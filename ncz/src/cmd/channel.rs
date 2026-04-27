//! channel — read or set the update channel.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, channel as channel_state, Paths};

#[derive(Debug, Serialize)]
pub struct ChannelReport {
    pub schema_version: u32,
    pub channel: String,
    pub changed: bool,
}

impl Render for ChannelReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.changed {
            writeln!(w, "Update channel: {}", self.channel)
        } else {
            writeln!(w, "{}", self.channel)
        }
    }
}

pub fn run(ctx: &Context, channel: Option<&str>) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, channel)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    _ctx: &Context,
    paths: &Paths,
    channel: Option<&str>,
) -> Result<ChannelReport, NczError> {
    match channel {
        None => Ok(ChannelReport {
            schema_version: common::SCHEMA_VERSION,
            channel: channel_state::read(paths)?,
            changed: false,
        }),
        Some(channel) => {
            let _lock = state::acquire_lock(&paths.lock_path)?;
            channel_state::write(paths, channel)?;
            Ok(ChannelReport {
                schema_version: common::SCHEMA_VERSION,
                channel: channel.to_string(),
                changed: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Context;
    use crate::cmd::common::test_paths;
    use crate::error::NczError;
    use crate::sys::fake::FakeRunner;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    #[test]
    fn channel_happy_path_sets_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let report = run_with_paths(&ctx(&runner), &paths, Some("canary")).unwrap();
        assert_eq!(report.schema_version, 1);
        assert!(report.changed);
        assert_eq!(channel_state::read(&paths).unwrap(), "canary");
    }

    #[test]
    fn channel_error_path_rejects_unknown_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = run_with_paths(&ctx(&runner), &paths, Some("nightly")).unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }
}
