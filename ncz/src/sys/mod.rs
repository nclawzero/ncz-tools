//! Subprocess + HTTP abstraction. A single `CommandRunner` trait sits at
//! the bottom; per-tool wrappers (`systemd`, `podman`, `apt`) call into it
//! so handlers test cleanly against a `FakeRunner`.
//!
//! `LANG=C LC_ALL=C` is set on every `RealRunner` invocation so output is
//! locale-stable across hosts (journalctl/apt format under non-C locales).

pub mod apt;
pub mod podman;
pub mod systemd;

use std::process::Command;

use crate::error::NczError;

#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ProcessOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, cmd: &str, args: &[&str]) -> Result<ProcessOutput, NczError>;

    /// Blocking GET against `http://127.0.0.1:<port><path>` for health
    /// probes. Default impl uses `ureq`. Override in fakes.
    fn http_get_local(&self, port: u16, path: &str, timeout_secs: u64) -> Result<u16, NczError> {
        let url = format!("http://127.0.0.1:{port}{path}");
        match ureq::get(&url)
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .call()
        {
            Ok(resp) => Ok(resp.status()),
            Err(ureq::Error::Status(code, _)) => Ok(code),
            Err(e) => Err(NczError::Exec {
                cmd: "http_get_local".into(),
                msg: e.to_string(),
            }),
        }
    }
}

/// Production implementation: `std::process::Command` with a forced C
/// locale and no shell interpretation.
pub struct RealRunner;

impl RealRunner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for RealRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> Result<ProcessOutput, NczError> {
        let out = Command::new(cmd)
            .args(args)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| NczError::Exec {
                cmd: cmd.into(),
                msg: e.to_string(),
            })?;
        Ok(ProcessOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Mutex;

    /// In-memory canned-response runner keyed on `(cmd, argv)`. Tests can
    /// register expectations and assert on call order.
    pub struct FakeRunner {
        pub responses: Mutex<HashMap<String, VecDeque<ProcessOutput>>>,
        pub http_responses: Mutex<HashMap<String, VecDeque<u16>>>,
        repeatable: Mutex<HashSet<String>>,
        http_repeatable: Mutex<HashSet<String>>,
        pub calls: Mutex<Vec<String>>,
        unexpected: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                http_responses: Mutex::new(HashMap::new()),
                repeatable: Mutex::new(HashSet::new()),
                http_repeatable: Mutex::new(HashSet::new()),
                calls: Mutex::new(vec![]),
                unexpected: Mutex::new(vec![]),
            }
        }

        pub fn expect(&self, cmd: &str, args: &[&str], reply: ProcessOutput) {
            let key = format!("{} {}", cmd, args.join(" "));
            self.responses
                .lock()
                .unwrap()
                .entry(key)
                .or_default()
                .push_back(reply);
        }

        #[allow(dead_code)]
        pub fn expect_repeating(&self, cmd: &str, args: &[&str], reply: ProcessOutput) {
            let key = format!("{} {}", cmd, args.join(" "));
            self.responses
                .lock()
                .unwrap()
                .insert(key.clone(), VecDeque::from([reply]));
            self.repeatable.lock().unwrap().insert(key);
        }

        #[allow(dead_code)]
        pub fn expect_http(&self, port: u16, path: &str, status: u16) {
            let key = format!("http {port} {path}");
            self.http_responses
                .lock()
                .unwrap()
                .entry(key)
                .or_default()
                .push_back(status);
        }

        #[allow(dead_code)]
        pub fn expect_http_repeating(&self, port: u16, path: &str, status: u16) {
            let key = format!("http {port} {path}");
            self.http_responses
                .lock()
                .unwrap()
                .insert(key.clone(), VecDeque::from([status]));
            self.http_repeatable.lock().unwrap().insert(key);
        }

        #[allow(dead_code)]
        pub fn assert_done(&self) {
            let repeatable = self.repeatable.lock().unwrap();
            let mut pending: Vec<String> = self
                .responses
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, replies)| !repeatable.contains(*key) && !replies.is_empty())
                .map(|(key, _)| key.clone())
                .collect();
            drop(repeatable);

            let http_repeatable = self.http_repeatable.lock().unwrap();
            pending.extend(
                self.http_responses
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(key, replies)| {
                        !http_repeatable.contains(*key) && !replies.is_empty()
                    })
                    .map(|(key, _)| key.clone()),
            );
            pending.sort();
            let unexpected = self.unexpected.lock().unwrap().clone();
            assert!(
                unexpected.is_empty(),
                "FakeRunner: unexpected calls: {unexpected:?}"
            );
            assert!(pending.is_empty(), "FakeRunner: unused expectations: {pending:?}");
        }

        fn unexpected_call(&self, cmd: &str, key: String) -> NczError {
            self.unexpected.lock().unwrap().push(key.clone());
            NczError::Exec {
                cmd: cmd.into(),
                msg: format!("FakeRunner: unexpected call: {key}"),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<ProcessOutput, NczError> {
            let key = format!("{} {}", cmd, args.join(" "));
            self.calls.lock().unwrap().push(key.clone());
            let mut responses = self.responses.lock().unwrap();
            let Some(replies) = responses.get_mut(&key) else {
                return Err(self.unexpected_call(cmd, key));
            };
            if self.repeatable.lock().unwrap().contains(&key) {
                replies
                    .front()
                    .cloned()
                    .ok_or_else(|| self.unexpected_call(cmd, key))
            } else {
                replies
                    .pop_front()
                    .ok_or_else(|| self.unexpected_call(cmd, key))
            }
        }

        fn http_get_local(
            &self,
            port: u16,
            path: &str,
            _timeout_secs: u64,
        ) -> Result<u16, NczError> {
            let key = format!("http {port} {path}");
            self.calls.lock().unwrap().push(key.clone());
            let mut responses = self.http_responses.lock().unwrap();
            let Some(replies) = responses.get_mut(&key) else {
                return Err(self.unexpected_call("http_get_local", key));
            };
            if self.http_repeatable.lock().unwrap().contains(&key) {
                replies
                    .front()
                    .copied()
                    .ok_or_else(|| self.unexpected_call("http_get_local", key))
            } else {
                replies
                    .pop_front()
                    .ok_or_else(|| self.unexpected_call("http_get_local", key))
            }
        }
    }
}
