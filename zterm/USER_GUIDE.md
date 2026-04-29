# ZTerm User Guide - Terminal REPL for Zeroclaw

**Version**: 0.7.0  
**Status**: Production Ready  
**Theme**: Zeroclaw Brand (Cyan/Blue)

---

## Table of Contents

1. [Installation](#installation)
2. [Quick Start](#quick-start)
3. [Core Commands](#core-commands)
4. [Advanced Features](#advanced-features)
5. [Configuration](#configuration)
6. [Aliases](#aliases)
7. [Batch Mode](#batch-mode)
8. [Troubleshooting](#troubleshooting)

---

## Installation

### Build from Source

```bash
git clone <zterm-repo>
cd zterm
cargo build --release
```

The binary will be at `target/release/zterm`.

### First Run

```bash
zterm tui
```

First run will launch the onboarding wizard to configure your gateway connection.

---

## Quick Start

### Basic Chat

```bash
zterm tui
```

You'll see:
1. **Splash Screen** - Shows your session, gateway, and model info
2. **Status Bar** - Always visible, shows current model/provider/session
3. **REPL Prompt** - Type messages and press Enter

```
📝 You: What's the meaning of life?
🤖 Claude: The meaning of life is...
```

### Commands

All commands start with `/`:

```bash
/help           # Show all available commands
/info           # Display current session info
/models list    # Show available models
/session list   # List all sessions
/history        # Show conversation history
/exit           # Exit ZTerm
```

---

## Core Commands

### Model Management

```bash
/models list                    # List all available models
/models set <provider>/<model>  # Switch to a model

# Example:
/models set anthropic/claude-opus
/models set groq/mixtral-8x7b-32768
```

### Session Management

```bash
/session list                   # List all sessions
/session create <name>          # Create new session
/session switch <name>          # Switch to session
/session info                   # Show current session details
/session delete <name>          # Delete a session
```

### Chat History

```bash
/history        # Show entire conversation in current session
/clear          # Clear session history (warns before clearing)
/save [file]    # Export session to file
```

### Memory & Knowledge

```bash
/memory list                # Show recent memory entries
/memory get <id>            # Retrieve specific memory
/memory search <query>      # Search MNEMOS memory system
/memory stats               # Show memory system statistics
/memory clear --yes         # Clear all memory (destructive)
```

### Cron Jobs (Scheduled Tasks)

```bash
/cron list                  # Show all scheduled jobs
/cron add <name> <expr>     # Add cron job (5-field format)
/cron add-at <name> <time>  # Schedule one-time task
/cron pause <name>          # Pause a job
/cron resume <name>         # Resume a job
/cron remove <name>         # Delete a job

# Example:
/cron add daily-check "0 9 * * *"      # Every day at 9 AM
/cron add-at backup-now "2026-04-21T15:30:00"  # Specific time
```

### Skills

```bash
/skills list                # Show available skills
/skill <name>               # Execute a skill
/skill info <name>          # Show skill details
```

### Configuration

```bash
/config                     # View current configuration
/config set <key> <value>   # Update configuration
/config reset               # Reset to defaults
```

### Utility

```bash
/help           # Show this help
/info           # Session information
/clear          # Clear history
/exit           # Exit ZTerm (graceful shutdown)
```

---

## Advanced Features

### Tab Completion

Supported in rustyline mode:
- **Command completion**: Start typing `/` and press Tab
- **Model completion**: Type `/models set ` and press Tab
- **Session completion**: Type `/session ` and press Tab

### History Navigation

Use arrow keys in the REPL:
- **↑ Up Arrow**: Previous command
- **↓ Down Arrow**: Next command
- **Ctrl+R**: Reverse search history (if supported)

### Multi-line Input

For longer prompts:
```bash
# Method 1: Shift+Enter for new line in rustyline
# Method 2: Continue typing naturally, ZTerm handles line breaks
```

### Command Aliases

Create shortcuts for frequently used commands:

```bash
# Predefined aliases:
h           -> /help
ll          -> /session list
lm          -> /models list
m           -> /memory

# Create custom alias:
/alias create <alias> <command>

# Example:
/alias create sm session create main
/alias create ms models set anthropic/claude-opus
```

### Response Pagination

Long responses are automatically paginated:

```
[Page 1/5] (n)ext, (p)revious, (q)uit: n
```

Use `n`, `p`, or `q` to navigate.

---

## Configuration

### Configuration File

Location: `~/.zeroclaw/config.toml`

```toml
[gateway]
url = "http://127.0.0.1:18789"
token = "your-api-token-here"

[agent]
model = "claude-opus"
provider = "anthropic"

[ui]
splash_screen = true       # Show splash on startup
```

### Session Storage

Sessions are stored in `~/.zeroclaw/sessions/<session_name>/`:
- `meta.json` - Session metadata
- `history.jsonl` - Conversation history

### History File

Command history: `~/.zeroclaw/input_history.jsonl`

---

## Aliases

### Predefined Aliases

| Alias | Command | Purpose |
|-------|---------|---------|
| `h` | `/help` | Quick help |
| `ll` | `/session list` | List sessions |
| `lm` | `/models list` | List models |
| `m` | `/memory` | Search memory |
| `sm` | `/session create main` | Create main session |

### Create Custom Alias

```bash
/alias create <alias> <command>
```

Aliases are stored in `~/.zeroclaw/aliases.toml`.

### Restrictions

- Aliases cannot use reserved command names (help, exit, info, etc)
- Alias names must be alphanumeric + underscore
- Arguments are automatically appended to expanded commands

---

## Batch Mode

Execute multiple commands from a script file:

```bash
zterm batch script.zterm
```

### Script Format

```bash
# Lines starting with # are comments
/models set anthropic/claude-opus
/session create batch-test
/history

# Blank lines are ignored
# Messages (no leading /) are also supported
What's your name?
Tell me about yourself
```

### Example Script

```bash
# analytics-batch.zterm
/session create analytics
/models set anthropic/claude-opus
Analyze this data
Generate charts
Export results to CSV
/save analytics-session.txt
```

Run it:
```bash
zterm batch analytics-batch.zterm
```

---

## Keyboard Shortcuts

### Turbo Vision TUI (`zterm tui`, default)

Every F-key has a Ctrl-equivalent so the bindings stay reachable
when you're SSH'd in from a Mac — Terminal.app, iTerm2, and most
remote-desktop apps swallow F1 / F10 / F11 / F12 for OS-level
shortcuts before they ever reach the remote shell.

| Key(s) | Action |
|---|---|
| `F1` / `Ctrl-H` | Help — slash-command reference |
| `F10` / `Ctrl-T` | Activate the top menu bar |
| `Ctrl-P` | Cycle to the next color palette (Borland → PFS → mono → amber → green → …) — live preview + persisted to `~/.zterm/theme.toml` |
| `Ctrl-S` | Re-save the active palette to `~/.zterm/theme.toml` |
| `Ctrl-O` | Open the workspace switcher |
| `Ctrl-Z` / `Ctrl-Y` | Undo / redo on the input line |
| `Alt-X` | Quit |
| `↩` (Enter) | Submit the input line as a turn (or a `/slash` command) |
| `/` on an empty line / `Ctrl-K` | Open the slash-command popup |
| `Esc` | Close the active modal |

Mac-on-SSH demo path: every shortcut above also works as the
listed `Ctrl-…` combo, so you don't have to fight your terminal
emulator's F-key intercepts.

The status line shows `workspace · model · ctx used/total (%) · elapsed`;
when token usage includes a context window, zterm also renders a compact
budget bar. Theme presets live at `~/.zterm/theme.toml`; launch count and
the opt-in error bell live at `~/.zterm/state.toml`. Toggle the bell with
`/theme beep on` or `/theme beep off`.

### Legacy REPL (`zterm tui --legacy-repl`)

| Key | Action |
|-----|--------|
| `Tab` | Command/model/session completion |
| `↑` | Previous command in history |
| `↓` | Next command in history |
| `Ctrl+C` | Interrupt current operation |
| `Ctrl+D` | Exit (EOF) |
| `Ctrl+L` | Clear screen (in some terminals) |

---

## Troubleshooting

### Connection Issues

**Problem**: "Could not connect to gateway"

```bash
# Check if gateway is running
curl http://127.0.0.1:18789/health

# Verify config
cat ~/.zeroclaw/config.toml

# Check URL and token
```

**Solution**:
- Ensure zeroclaw gateway is running
- Verify URL in `config.toml`
- Confirm API token is valid

### Model Not Found

**Problem**: "Model not found" error

```bash
/models list
```

Choose an available model:
```bash
/models set anthropic/claude-opus
```

### Session Issues

**Problem**: "Session not found"

```bash
/session list                    # See all sessions
/session create my-new-session   # Create new
/session switch my-new-session   # Switch to it
```

### Memory Offline

**Problem**: "MNEMOS offline" or memory commands fail

- MNEMOS memory system is temporarily unavailable
- Local memory features will be disabled
- Commands will still work, just without memory integration
- Retry when system comes back online

### Slow Response

**Problem**: Commands taking too long

1. Check network latency
2. Check gateway status
3. Try smaller queries
4. Use pagination for long responses

---

## Environment Variables

```bash
# Override gateway URL
export ZEROCLAW_GATEWAY="http://custom-gateway:8888"

# Override API token
export ZEROCLAW_TOKEN="your-token"

# Enable debug logging
export RUST_LOG=debug
```

---

## Examples

### Daily Workflow

```bash
# Start
zterm tui

# Session 1: Research
📝 You: Create a session for today's research

/session create 2026-04-20-research
/models set anthropic/claude-opus

# Chat normally...

# Session 2: Quick task
/session create quick-task
/models set groq/mixtral

# Back to research
/session switch 2026-04-20-research
/history
```

### Batch Analysis

```bash
# Create script
cat > analysis.zterm << 'EOF'
/session create analysis-batch
/models set anthropic/claude-opus
Analyze the following dataset...
Generate summary statistics
Create visualization code
EOF

# Run
zterm batch analysis.zterm
```

### Scheduled Tasks

```bash
# Schedule daily summary
/cron add daily-summary "0 9 * * *"

# One-time reminder
/cron add-at meeting-prep "2026-04-21T14:00:00"

# View scheduled
/cron list
```

---

## Best Practices

1. **Use Sessions** - Organize work by session
2. **Tag Sessions** - Add tags for easier search
3. **Create Aliases** - Reduce typing for common commands
4. **Save Important Chats** - `/save` exports conversation
5. **Use Cron** - Automate recurring tasks
6. **Check /help** - Commands have detailed help

---

## Performance Tips

1. **Disable Splash** in config if launching frequently
2. **Use Tab Completion** instead of typing full commands
3. **Clear History** periodically for faster startup
4. **Batch Related Queries** - Session switching has overhead
5. **Monitor Memory** - Very long histories can impact performance

---

## Getting Help

1. **In ZTerm**: Type `/help` for command reference
2. **This Guide**: You're reading it!
3. **GitHub Issues**: Report bugs or request features
4. **Gateway Status**: Check `curl http://gateway:8888/health`

---

## What's New (v0.7.0)

✨ **Phase 8 Enhancements**:
- Tab completion for commands, models, sessions
- Arrow key history navigation
- Intelligent error suggestions
- Response pagination for long outputs
- Command aliases for custom shortcuts
- Session search and tagging
- Connection retry with exponential backoff
- Batch mode for script execution
- Splash screen with zeroclaw branding
- Color-themed entire interface

---

**Status**: ✅ **PRODUCTION READY - FULLY FEATURED REPL**

ZTerm is now a complete, professional terminal REPL for zeroclaw with all major features and polish.

---

*Generated: 2026-04-20*  
*ZTerm v0.7.0 - Terminal REPL for Zeroclaw*
