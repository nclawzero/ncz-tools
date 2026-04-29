# ZTerm User Guide

Professional-grade terminal REPL interface for zeroclaw.

## Quick Start

### First Run (Onboarding)

```bash
zeroclaw tui
```

If this is your first run, ZTerm will prompt for:
1. **Gateway URL** (default: `http://localhost:8888`)
2. **Bearer Token** (from zeroclaw configuration)

Configuration is saved to `~/.zeroclaw/config.toml`.

### Normal Usage

```bash
zeroclaw tui
```

Displays:
- Model and provider (e.g., "Claude 3.5 Opus / Anthropic")
- Current session name
- Chat interface ready to accept input

## Chatting

### Basic Chat

```
📝 You: What is the capital of France?

🤖 Claude: (streaming response appears here in real-time)
The capital of France is Paris.
```

Just type your message and press Enter. Responses stream in real-time.

### Input History

- **↑** (Up arrow) — Navigate to previous message
- **↓** (Down arrow) — Navigate to next message
- **Ctrl+R** — Fuzzy search history (coming in Phase 3+)

Your input history is saved and persists across sessions.

## Commands

All commands start with a slash (`/`).

### `/help`
Shows available commands and usage.

```
📝 You: /help

Available commands:
  /model       - Switch model (shows available models)
  /session     - List/create/switch sessions
  /memory      - Search memory entries
  /skill       - Enable/disable skills
  /config      - Re-run setup wizard
  /clear       - Clear local transcript; backend context is retained
  /save [file] - Save session transcript
  /info        - Show current session info
  /exit        - Exit ZTerm
```

### `/info`
Shows current session information.

```
Session Information:
  ID:        session-main
  Name:      main
  Model:     claude-3.5-opus
  Provider:  anthropic
  Created:   2026-04-20T10:00:00Z
  Messages:  5
```

### `/model` (Coming in Phase 3+)
Switch to a different AI model.

```
📝 You: /model

Available models:
1. claude-3.5-opus (Claude, Anthropic)
2. gpt-4o (OpenAI)
3. gemma-4:e4b (Google, Local)

Select model (1-3): 2
```

### `/session` (Coming in Phase 3+)
List and switch between sessions.

```
📝 You: /session

Sessions:
1. main (5 messages)
2. research (12 messages)
3. debug (3 messages)

Select session (1-3): 2
```

### `/memory [query]` (Coming in Phase 3+)
Search your memory entries.

```
📝 You: /memory python patterns

Matching entries:
1. Python decorator pattern usage example
2. Best practices for Python typing
```

### `/skill` (Coming in Phase 3+)
Enable/disable skills and tools.

```
📝 You: /skill

Available skills:
[ ] Web search
[x] File analysis
[ ] Code execution
```

### `/config`
Re-run the configuration wizard to update settings.

### `/clear`
Clear zterm's local transcript for the current session. This does not reset or delete the backend session, so backend context is retained.

### `/save [filename]`
Save the current session transcript to a file.

```
📝 You: /save my-conversation.txt
✅ Session saved to my-conversation.txt
```

### `/exit`
Exit ZTerm and save the session.

```
📝 You: /exit

👋 Goodbye!
```

## Files and Directories

### Configuration

```
~/.zeroclaw/
├── config.toml              # Gateway URL, API token, model/provider
├── input_history.jsonl      # Your input history (one per line)
└── sessions/
    ├── main/                # Default session
    │   ├── meta.json        # Session metadata
    │   ├── history.jsonl    # Chat history
    │   └── transcript.md    # Human-readable transcript
    └── research/            # Another session
        └── ...
```

### Session Format

**meta.json** (Session metadata):
```json
{
    "id": "main",
    "name": "main",
    "model": "claude-3.5-opus",
    "provider": "anthropic",
    "created_at": "2026-04-20T10:00:00Z",
    "message_count": 5,
    "last_active": "2026-04-20T10:05:00Z"
}
```

**history.jsonl** (One JSON object per line):
```jsonl
{"role":"user","content":"What is Python?","timestamp":"2026-04-20T10:00:00Z"}
{"role":"assistant","content":"Python is a programming language...","timestamp":"2026-04-20T10:00:02Z"}
```

## Tips and Tricks

### Multi-line Input
For very long inputs, you can use:\
**Note**: Multi-line input coming in Phase 3+

### Code Blocks
Code blocks in responses are formatted with language detection:

```
┌─ python
│ def hello():
│     print("world")
└─────────────────────────
```

### Keyboard Shortcuts
- **Ctrl+C** — Interrupt streaming response
- **Ctrl+D** — Exit (same as `/exit`)
- **Ctrl+R** — Fuzzy search history (coming in Phase 3+)
- **↑/↓** — Navigate input history
- **Tab** — Auto-complete command/model/session (coming in Phase 3+)

### Performance Tips
- Use specific models for specific tasks (fast vs. reasoning)
- Keep sessions focused on one topic
- Use `/memory` to reference past conversations
- Enable only necessary skills

## Troubleshooting

### "Could not connect to gateway"
- Verify gateway URL in config: `~/.zeroclaw/config.toml`
- Check zeroclaw gateway is running
- Verify Bearer token is correct

### "Invalid API key"
Run `/config` to re-authenticate.

### Session lost
Sessions are saved server-side and persist. If you lose a session:
1. Check `~/.zeroclaw/sessions/`
2. Sessions can be restored manually

## Advanced Usage

### Multiple Sessions
Keep separate conversations for different topics:

```
📝 You: /session

Sessions:
1. main (default)
2. research-project
3. bug-debugging

Select session (1-3): 2
```

### Session Transcript Export
Export a session for sharing or archiving:

```
📝 You: /save my-research.txt
✅ Session saved to my-research.txt
```

### Configuration as Code
Edit `~/.zeroclaw/config.toml` directly to set defaults:

```toml
[agent]
model = "gpt-4o"              # Default model
provider = "openai"          # Default provider
max_turns = 20               # Max messages per session
```

## Known Limitations

- Code syntax highlighting: Comes in Phase 3+
- File uploads: Not yet supported
- Web search integration: Comes in Phase 3+
- Image generation: Depends on model capabilities

## Getting Help

- Type `/help` in ZTerm for command reference
- View configuration: `/info`
- Check gateway connectivity: Server will indicate network errors

## Next Steps

Explore:
1. Try different models with `/model`
2. Create topic-specific sessions with `/session`
3. Use `/memory` to reference past conversations
4. Share transcripts with `/save`

---

For more info, see README.md or IMPLEMENTATION.md.
