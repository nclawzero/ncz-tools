//! Output rendering. Each command builds a typed report struct that
//! implements `serde::Serialize` and the `Render` trait below; the binary
//! decides text vs JSON at the top level.
//!
//! Lock the JSON schema on day one. Every top-level output struct should
//! carry a `schema_version: u32 = 1` field so future Tier-2 additions can
//! evolve without breaking operator scripts.

use std::io::{self, Write};

use serde::Serialize;

pub trait Render: Serialize {
    /// Emit the human-readable form of this output.
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()>;
}

/// Top-level emission helper used by command handlers. Picks JSON vs text
/// based on the global `--json` flag.
pub fn emit<T: Render>(value: &T, json: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if json {
        serde_json::to_writer_pretty(&mut out, value)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        writeln!(out)?;
    } else {
        value.render_text(&mut out)?;
    }
    Ok(())
}
