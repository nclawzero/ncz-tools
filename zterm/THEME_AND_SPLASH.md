# ZTerm Theme & Splash Page Implementation

**Status**: ✅ Complete  
**Date**: April 20, 2026  
**Commits**: be2d6fa, a2fa042, 9dddeb5

> v0.3.1 note: the default interactive UI is now the Turbo
> Vision TUI in `src/cli/tui/tv_ui.rs`. The ANSI splash documented
> here remains relevant to the legacy/stdout path; the TV path adds
> runtime palette presets, `~/.zterm/theme.toml`, cached
> connect-splash text at `~/.zterm/cache/connect-splash/`, and
> `~/.zterm/state.toml` for launch count plus `beep_on_error`. The
> v0.3.1 connect splash asks the active backend for period-style text
> through a bounded scratch session, caches only confirmed generated
> output for 24h, and falls back to a local modem sequence on timeout
> or backend error. State writes use a
> bounded lock wait, and the TV path persists a mutation fence after
> timed-out mutating slash commands until `/resync --force`.

---

## Overview

Implemented a comprehensive color theme aligned with **Zeroclaw project branding** (cyan/blue). All UI elements now use consistent ANSI color codes for a professional, branded experience.

---

## Color Palette (Zeroclaw Brand)

| Color | ANSI Code | Usage |
|-------|-----------|-------|
| **Cyan** | `\x1b[36m` | Headers, borders, primary elements |
| **Bright Cyan** | `\x1b[96m` | Emphasis, titles |
| **Blue** | `\x1b[34m` | Secondary elements, borders |
| **Bright Blue** | `\x1b[94m` | Command labels, highlights |
| **Green** | `\x1b[32m` | Success messages |
| **Bright Green** | `\x1b[92m` | Success emphasis |
| **Yellow** | `\x1b[33m` | Warnings |
| **Bright Yellow** | `\x1b[93m` | Warning emphasis |
| **Red** | `\x1b[31m` | Errors |
| **Bright Red** | `\x1b[91m` | Error emphasis |

---

## Splash Screen

Displays on startup (configurable via `splash_screen` in config):

```
╔════════════════════════════════════════════════════════════════════╗
║                                                                    ║
║                  ✨ Welcome to ZTerm v0.7.0 ✨                     ║
║                                                                    ║
║              Terminal REPL for Zeroclaw Gateway                   ║
║                                                                    ║
╚════════════════════════════════════════════════════════════════════╝

┌─ Session Information ───────────────────────────────────────────┐
│ Session:  main                                                   │
│ Gateway:  http://127.0.0.1:18789                                │
│ Model:    claude-opus (anthropic)                                │
└─────────────────────────────────────────────────────────────────┘

╭─ Quick Help ────────────────────────────────────────────────────╮
│                                                                 │
│  💬 Chat:        Type your message and press Enter              │
│  ❓ Help:        /help              (show all commands)         │
│  🤖 Models:      /models list       (view available models)     │
│  📋 Sessions:    /session list      (view all sessions)         │
│  📝 History:     /history           (show conversation)         │
│  🧠 Memory:      /memory <query>    (search your memory)        │
│  🚀 Skills:      /skills list       (view available skills)     │
│  ⏰ Cron:        /cron list         (scheduled tasks)           │
│  🚪 Exit:        /exit              (exit gracefully)            │
│                                                                 │
╰─────────────────────────────────────────────────────────────────╯

💡 Tip: Type /help anytime for a complete command reference
```

**Colors Applied**:
- Cyan borders (`┌─`, `╔╚╗┘`, etc.)
- Bright cyan titles and emphasis
- Bright blue command labels
- Each command has an emoji prefix for quick visual scanning

---

## Configuration

Users can disable splash screen by adding to `~/.zeroclaw/config.toml`:

```toml
[ui]
splash_screen = false
```

Default is `true` (splash enabled).

---

## UI Elements with Theme

### Splash Screen (`src/cli/tui/splash.rs`)
- ✅ Cyan/blue borders
- ✅ Bright blue titled sections
- ✅ Session information display
- ✅ Quick command reference with emojis

### Onboarding Wizard (`src/cli/tui/onboarding.rs`)
- ✅ Cyan/blue title banner
- ✅ Bright blue prompts
- ✅ Bright green success message
- ✅ Consistent styling with splash

### Help Panel (`src/cli/ui.rs::print_help`)
- ✅ Cyan header
- ✅ Bright blue command labels
- ✅ Emoji prefixes for each command
- ✅ Color-coded for quick scanning

### Error/Success Messages (`src/cli/ui.rs`)
- ✅ Bright red for errors + ❌ icon
- ✅ Bright yellow for suggestions + 💡 icon
- ✅ Bright green for success + ✅ icon

### Code Blocks (`src/cli/ui.rs::CodeBlockFormatter`)
- ✅ Cyan borders (`┌─`, `│`, `└─`)
- ✅ Bright blue language label
- ✅ Clean visual separation from text

### Status Bar (`src/cli/ui.rs::StatusBar`)
- ✅ Bright cyan/blue labels
- ✅ Bright blue values for emphasis
- ✅ Blue separator line
- ✅ Always visible during REPL session

### REPL Interface (`src/cli/tui/repl.rs`)
- ✅ REPL banner: cyan borders with bright blue title
- ✅ User prompt: `📝 You:` in bright blue, input area in cyan
- ✅ Claude response: `🤖 Claude:` in bright green, response area in cyan
- ✅ Help message: bright cyan header with blue command labels and emojis
- ✅ Session info display: bright cyan header with blue labels

---

## Theme System (`src/cli/theme.rs`)

Centralized color definitions:

```rust
pub struct Theme;

impl Theme {
    pub const CYAN: &'static str = "\x1b[36m";
    pub const BRIGHT_CYAN: &'static str = "\x1b[96m";
    pub const BLUE: &'static str = "\x1b[34m";
    pub const BRIGHT_BLUE: &'static str = "\x1b[94m";
    // ... more colors
}

// Helper functions
pub fn colored(text: &str, color: &str) -> String { ... }
pub fn bold(text: &str) -> String { ... }
pub fn bold_colored(text: &str, color: &str) -> String { ... }
```

Benefits:
- **Centralized**: Change theme colors in one place
- **Consistent**: All UI uses same color constants
- **Reusable**: Helper functions for common patterns
- **Easy to extend**: Add new colors/modifiers as needed

---

## Files Modified

| File | Changes |
|------|---------|
| `src/cli/theme.rs` | **NEW** - Theme system with color palette + helpers |
| `src/cli/tui/splash.rs` | **NEW** - Splash screen with cyan/blue branding |
| `src/cli/tui/repl.rs` | Apply theme to REPL banner, prompts, help, info |
| `src/cli/tui/mod.rs` | Import splash module, call on startup, add config check |
| `src/cli/tui/onboarding.rs` | Apply theme colors to onboarding wizard |
| `src/cli/ui.rs` | Apply theme to help, error, success, status bar, code blocks |
| `src/cli/mod.rs` | Export theme module |

---

## Build Status

✅ **Clean build** - 0 errors, 34 warnings (pre-existing)

```bash
cargo build 2>&1 | grep -E "(error|Finished)"
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.26s
```

---

## Demo Experience

**First Time User**:
1. Run `zterm tui`
2. Onboarding wizard appears (blue-themed prompts)
3. Config saved
4. Splash screen displays (cyan/blue branded)
5. REPL prompt ready
6. Type `/help` to see color-coded command list
7. Errors/success shown with themed colors

**Subsequent Sessions**:
1. Run `zterm tui`
2. Splash screen displays immediately (unless disabled in config)
3. REPL prompt ready with colored status bar

---

## Next Steps (Optional Enhancements)

1. **REPL Prompt Coloring** - Color the input prompt line
2. **Response Highlighting** - Color-code response types (agent response vs. command result)
3. **Syntax Highlighting** - Add syntax highlighting for code blocks in responses
4. **Theme Switching** - Allow users to pick between themes (dark/light/custom)
5. **Disable Colors** - Auto-detect terminal support, allow `--no-color` flag

---

## Testing

All code compiles cleanly and is ready for:
- ✅ Live testing on zeroclaw gateway
- ✅ Integration with Phase 7 features
- ✅ Demo to zeroclaw team

---

**Status**: ✅ **SPLASH PAGE + THEME COMPLETE - READY FOR DEMO**

The ZTerm now has a professional, branded appearance aligned with zeroclaw project identity.
