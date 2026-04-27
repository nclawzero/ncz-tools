//! v0.3 theme presets (E-6 slice).
//!
//! Each preset is a 63-byte `TPalette` payload that feeds
//! `turbo_vision::core::palette::palettes::set_custom_palette` via
//! `Application::set_palette`. The palette layout follows Borland
//! Turbo Vision's `cpColor` convention:
//!
//! ```text
//!     1      TBackground (desktop)
//!     2-7    TMenuView / TStatusLine
//!     8-15   TWindow(Blue)
//!     16-23  TWindow(Cyan)     ← the workspace frame uses this slot
//!     24-31  TWindow(Gray)
//!     32-63  TDialog (32 slots)
//! ```
//!
//! Each byte encodes a foreground/background pair: low nibble = fg,
//! high nibble = bg, matching the PC BIOS attribute byte. Colors are
//! 0..=15 per `TvColor::from_u8`.
//!
//! Themes deliberately stay within the indexed 16-color space so
//! they work in plain xterm/tmux/screen without extended-color
//! support. Terminal emulators render the physical amber/green tint
//! from their own 16-color palette settings.

/// A named theme preset.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Canonical short name used by `/theme <name>` and by
    /// persistence in `~/.zterm/theme.toml` (E-8).
    pub name: &'static str,
    /// Human-readable description for `/theme list` and menus.
    pub display_name: &'static str,
    /// 63-byte palette payload. Copied into a `Vec<u8>` when handed
    /// to `Application::set_palette`.
    pub palette: &'static [u8; 63],
}

/// Lookup by name (case-insensitive). `None` for unknown names.
pub fn find(name: &str) -> Option<&'static Theme> {
    let target = name.to_ascii_lowercase();
    PRESETS.iter().find(|t| t.name == target)
}

/// Return the preset that comes after `current` in `PRESETS`,
/// wrapping at the end. Used by the Ctrl-P direct-cycle binding
/// so the user doesn't have to memorize preset names.
///
/// Unknown / `"custom"` / unmatched names fall through to the
/// first preset — a reasonable "start over" behavior that also
/// gracefully covers the edge case where the user is on the
/// custom-edited palette and presses Ctrl-P to drop back into
/// the named-preset cycle.
pub fn next_preset(current: &str) -> &'static Theme {
    let target = current.to_ascii_lowercase();
    let idx = PRESETS
        .iter()
        .position(|t| t.name == target)
        .map(|i| (i + 1) % PRESETS.len())
        .unwrap_or(0);
    &PRESETS[idx]
}

/// Default theme when no persisted selection is found.
pub const DEFAULT: &Theme = &BORLAND;

// ---------------------------------------------------------------
// Persistence (E-8a)
// ---------------------------------------------------------------
//
// `~/.zterm/theme.toml` is a small TOML document the TUI reads on
// boot and writes whenever the user picks a theme. Shape:
//
// ```toml
// theme = "amber"              # canonical preset name
// # OR
// theme = "custom"
// custom = [0x07, 0x70, ...]   # 63 raw palette bytes (E-8b)
// ```
//
// The file is loose-coupled: missing / malformed / mismatched
// length → fall back to `DEFAULT` without surfacing an error. That
// keeps the TUI boot resilient when a user hand-edits the file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Canonical path: `~/.zterm/theme.toml`.
pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".zterm").join("theme.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTheme {
    /// Preset name (`"borland" | "pfs" | "mono" | "amber" | "green"
    /// | "custom"`).
    pub theme: String,
    /// 63-byte palette — only meaningful when `theme == "custom"`.
    #[serde(default)]
    pub custom: Option<Vec<u8>>,
}

/// Load the persisted theme, returning `(palette, label)` where
/// `label` is the user-visible preset name. Returns `(DEFAULT
/// palette, "borland")` if the file is missing or invalid — the
/// error is logged to `tracing` at debug level but never bubbled
/// up to the caller.
pub fn load_persisted() -> (Vec<u8>, String) {
    let Some(path) = default_path() else {
        return (DEFAULT.palette.to_vec(), DEFAULT.name.to_string());
    };
    let Ok(bytes) = std::fs::read_to_string(&path) else {
        return (DEFAULT.palette.to_vec(), DEFAULT.name.to_string());
    };
    let Ok(cfg) = toml::from_str::<PersistedTheme>(&bytes) else {
        tracing::debug!("theme.toml parse failed, using default");
        return (DEFAULT.palette.to_vec(), DEFAULT.name.to_string());
    };
    if cfg.theme == "custom" {
        match cfg.custom {
            Some(bytes) if bytes.len() == 63 => (bytes, "custom".to_string()),
            _ => {
                tracing::debug!("custom theme payload invalid, using default");
                (DEFAULT.palette.to_vec(), DEFAULT.name.to_string())
            }
        }
    } else if let Some(preset) = find(&cfg.theme) {
        (preset.palette.to_vec(), preset.name.to_string())
    } else {
        tracing::debug!(
            "theme.toml references unknown preset '{}', using default",
            cfg.theme
        );
        (DEFAULT.palette.to_vec(), DEFAULT.name.to_string())
    }
}

/// Persist a preset by name. Errors (missing home dir, IO, serde)
/// are returned so the caller can surface them in the chat pane.
pub fn save_preset(name: &str) -> std::io::Result<()> {
    let Some(path) = default_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist theme",
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cfg = PersistedTheme {
        theme: name.to_string(),
        custom: None,
    };
    let body = toml::to_string_pretty(&cfg).map_err(std::io::Error::other)?;
    std::fs::write(&path, body)
}

/// Persist a raw 63-byte custom palette (E-8b).
pub fn save_custom(palette: &[u8; 63]) -> std::io::Result<()> {
    let Some(path) = default_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist theme",
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cfg = PersistedTheme {
        theme: "custom".to_string(),
        custom: Some(palette.to_vec()),
    };
    let body = toml::to_string_pretty(&cfg).map_err(std::io::Error::other)?;
    std::fs::write(&path, body)
}

/// All bundled themes, in presentation order for `/theme list` and
/// menu rendering.
pub const PRESETS: &[Theme] = &[BORLAND, PFS, MONO, AMBER, GREEN];

// ---------- Borland default (Turbo Vision cyan on blue) ----------
//
// Exact copy of `CP_APP_COLOR` from turbo-vision-1.1.0. Kept in
// sync by hand; any cpColor drift upstream should bump the crate
// dep and re-copy.
#[rustfmt::skip]
const BORLAND_PALETTE: [u8; 63] = [
    0x71, 0x70, 0x78, 0x74, 0x20, 0x28, 0x24, 0x17, // 1-8 desktop
    0x1F, 0x1A, 0x31, 0x31, 0x1E, 0x71, 0x00,       // 9-15 menu + status
    0x30, 0x3F, 0x3A, 0x13, 0x13, 0x3E, 0x21, 0x00, // 16-23 cyan window
    0x70, 0x7F, 0x7A, 0x13, 0x13, 0x70, 0x7F, 0x00, // 24-31 gray window
    0x70, 0x7F, 0x7A, 0x13, 0x13, 0x70, 0x70, 0x7F, // 32-39 dialog frame
    0x7E, 0x20, 0x2B, 0x2F, 0x78, 0x2E, 0x70, 0x30, // 40-47 dialog ctrl
    0x3F, 0x3E, 0x1F, 0x2F, 0x1A, 0x20, 0x72, 0x31, // 48-55 dialog input/button
    0x31, 0x30, 0x2F, 0x3E, 0x31, 0x13, 0x38, 0x00, // 56-63 dialog remaining
];
pub const BORLAND: Theme = Theme {
    name: "borland",
    display_name: "Borland Turbo Vision — cyan on blue (default)",
    palette: &BORLAND_PALETTE,
};

// ---------- Paradox 4.5 / dBASE V calibration ----------
//
// Differentiated from `borland` by:
//   - yellow-on-blue menu bar (Paradox top bar is white-on-cyan; we
//     approximate with yellow to pick up the Paradox accelerator
//     accent even on terminals with muted cyans),
//   - yellow selection bar in menus + listboxes (index slots that
//     feed TMenuView selected + TListViewer selected),
//   - deeper blue desktop background dots.
//
// Reference screenshots (the ones we locked the aesthetic against)
// can't be perfectly reproduced in 16-color indexed mode, but the
// eye picks up the brighter-yellow accents immediately on theme
// switch, which is the point.
#[rustfmt::skip]
const PFS_PALETTE: [u8; 63] = [
    0x17, 0x70, 0x7E, 0x7E, 0x20, 0x2E, 0x24, 0x17, // 1-8  desktop + menu normal/select
    0x1F, 0x1A, 0x31, 0x31, 0x1E, 0x71, 0x00,       // 9-15 menu + status
    0x30, 0x3F, 0x3E, 0x1E, 0x1E, 0x3E, 0x21, 0x00, // 16-23 cyan window — yellow selection bar
    0x70, 0x7F, 0x7E, 0x1E, 0x1E, 0x70, 0x7F, 0x00, // 24-31 gray window
    0x70, 0x7F, 0x7E, 0x1E, 0x1E, 0x70, 0x70, 0x7F, // 32-39 dialog frame + selection
    0x7E, 0x20, 0x2B, 0x2F, 0x78, 0x2E, 0x70, 0x30, // 40-47 dialog ctrl
    0x3F, 0x3E, 0x1E, 0x2F, 0x1A, 0x20, 0x72, 0x31, // 48-55 dialog input/button
    0x31, 0x30, 0x2F, 0x3E, 0x31, 0x13, 0x38, 0x00, // 56-63 dialog remaining
];
pub const PFS: Theme = Theme {
    name: "pfs",
    display_name: "Paradox 4.5 / dBASE V — navy & yellow accents",
    palette: &PFS_PALETTE,
};

// ---------- Single-color terminal themes ----------
//
// Classic CRT looks: a single foreground color on a black
// background, with reverse-video for highlighted / focused / menu
// selection slots. All three (mono / amber / green) share the same
// structural layout, just swapping the bright-foreground color
// token.
//
// Per-slot color intent:
//   - Normal:    fg on black               → `fg`
//   - Reverse:   black on fg (selection)   → `rev = (fg << 4)`
//   - Bright:    bright-variant on black   → `br`  (fg+8 typically)
//   - Accent:    bright + reverse          → `brev = (br << 4)`
//
// The layout is derived from the Borland cpColor groupings:
// whichever slot carries "selected" / "highlighted" semantics gets
// the reverse variant, everything else gets plain `fg`.
const fn make_mono_like_palette(fg: u8) -> [u8; 63] {
    let br = fg | 0x08; // bright variant of the base color
    let norm = fg; // fg on black
    let rev = fg << 4; // black on fg
    let bright = br; // bright fg on black
    let brev = br << 4; // black on bright fg

    [
        // 1-8 desktop + early menu
        norm,   // 1  background pattern
        norm,   // 2  menu normal
        bright, // 3  menu disabled (a bit dimmer still readable)
        rev,    // 4  menu selected
        bright, // 5  menu shortcut
        brev,   // 6  menu selected shortcut
        norm,   // 7  status
        norm,   // 8  (spare)
        // 9-15 menu + status line remaining
        rev,    // 9  menu accelerator selected
        norm,   // 10 menu disabled separator
        rev,    // 11 status normal
        bright, // 12 status selected
        rev,    // 13 status accent
        norm,   // 14 status disabled
        0x00,   // 15 (reserved / spacer — black on black, invisible)
        // 16-23 cyan window (our workspace frame)
        norm, bright, rev, brev, rev, bright, norm, 0x00, // 24-31 gray window
        norm, bright, rev, brev, rev, bright, norm, 0x00, // 32-39 dialog frame + headers
        norm, bright, rev, brev, rev, norm, norm, bright,
        // 40-47 dialog controls (buttons, etc.)
        bright, norm, rev, bright, rev, bright, norm, norm,
        // 48-55 dialog input / selection
        bright, bright, rev, bright, norm, norm, rev, rev, // 56-63 remaining dialog
        rev, norm, bright, bright, rev, rev, rev, 0x00,
    ]
}

// Foreground color indices (matches `TvColor::from_u8`):
//   7 = LightGray   (mono — neutral white-ish)
//   6 = Brown       (amber CRT — brown in the base palette; bright
//                    variant 0xE = Yellow renders closer to amber on
//                    most 16-color terminal palettes)
//   2 = Green       (phosphor — bright variant 0xA = LightGreen)
const MONO_PALETTE: [u8; 63] = make_mono_like_palette(0x07);
const AMBER_PALETTE: [u8; 63] = make_mono_like_palette(0x06);
const GREEN_PALETTE: [u8; 63] = make_mono_like_palette(0x02);

pub const MONO: Theme = Theme {
    name: "mono",
    display_name: "dBASE III — monochrome reverse-video",
    palette: &MONO_PALETTE,
};

pub const AMBER: Theme = Theme {
    name: "amber",
    display_name: "IBM 3270 — amber CRT",
    palette: &AMBER_PALETTE,
};

pub const GREEN: Theme = Theme {
    name: "green",
    display_name: "VT100 — green phosphor",
    palette: &GREEN_PALETTE,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_63_bytes() {
        for theme in PRESETS {
            assert_eq!(
                theme.palette.len(),
                63,
                "theme '{}' palette is not 63 bytes",
                theme.name
            );
        }
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(find("borland").unwrap().name, "borland");
        assert_eq!(find("MONO").unwrap().name, "mono");
        assert_eq!(find("Amber").unwrap().name, "amber");
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("does-not-exist").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn presets_have_distinct_names() {
        let mut seen: Vec<&'static str> = Vec::new();
        for theme in PRESETS {
            assert!(
                !seen.contains(&theme.name),
                "duplicate preset name: {}",
                theme.name
            );
            seen.push(theme.name);
        }
    }

    #[test]
    fn next_preset_advances_in_order() {
        // Walking forward from the first preset should hit every
        // subsequent preset and wrap back to the first.
        let mut name = PRESETS[0].name.to_string();
        for expected in PRESETS.iter().skip(1).chain(std::iter::once(&PRESETS[0])) {
            let next = next_preset(&name);
            assert_eq!(next.name, expected.name);
            name = next.name.to_string();
        }
    }

    #[test]
    fn next_preset_unknown_name_falls_back_to_first() {
        // Custom palette / typo / empty string — all return the
        // first preset so the cycle "starts over" gracefully.
        assert_eq!(next_preset("custom").name, PRESETS[0].name);
        assert_eq!(next_preset("not-a-real-theme").name, PRESETS[0].name);
        assert_eq!(next_preset("").name, PRESETS[0].name);
    }

    #[test]
    fn next_preset_is_case_insensitive() {
        // Match `find`'s case-insensitivity so callers can pass
        // whatever's in the theme.toml without normalizing.
        let upper = PRESETS[0].name.to_ascii_uppercase();
        assert_eq!(next_preset(&upper).name, PRESETS[1].name);
    }
}
