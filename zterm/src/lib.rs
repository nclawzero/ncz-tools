//! zterm — terminal REPL for claw-family agentic daemons.
//!
//! Library crate exposing `cli::*` for use by the `zterm` bin target
//! and by integration tests in `tests/*.rs`. v0.2 openclaw backend work
//! needs library-scope access to run async integration flows against
//! a live gateway; keeping everything behind `pub mod cli;` is the
//! minimum split to enable that without reshaping call sites.

// Planned-feature modules (batch executor, pagination, rusty-line REPL,
// streaming, session search, etc.) compile as unreachable scaffolding
// until the corresponding dispatch path lands. Flip these back to errors
// once each module is wired up.
#![allow(dead_code, clippy::new_without_default)]

pub mod cli;
