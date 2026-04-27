//! Parse Podman quadlet `.container` files for the bits ncz cares about
//! (currently just the `Image=` line).

use std::fs;
use std::path::Path;

use crate::error::NczError;

pub fn image_for(quadlet_path: &Path) -> Result<Option<String>, NczError> {
    if !quadlet_path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(quadlet_path)?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Image") {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    return Ok(Some(v));
                }
            }
        }
    }
    Ok(None)
}
