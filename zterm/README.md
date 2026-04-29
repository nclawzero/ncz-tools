# zterm

> **Light, thin, fast, memory-safe terminal for agentic systems.**
> Back to the 1970s. We talk to the API. The API is authoritative. No web nonsense.

A single-binary Rust terminal REPL for agent daemons. Works against a local daemon or a remote one over the network — same code, same config shape, same expectations.

```
Binary size:    ~10 MB (release, stripped)
Startup:        < 500 ms
Memory:         ~5 MB resident
Rust edition:   2021, 1.93+
unsafe code:    zero in zterm
```

---

## Status

**v0.3.1 — Turbo Vision UX release.** Ships the Paradox 4.5/dBASE V-inspired `turbo-vision-4-rust` TUI, real slash-command routing through `CommandHandler`, live workspace/model/context-token status, spinner/menu feedback, cached local connect splash, milestone welcome-back lines, opt-in beep-on-error, and a persisted `/resync` / `/resync --force` recovery fence for timed-out mutating slash commands. The v0.3.1 connect splash is intentionally local and cached; backend/LLM-generated splash text is deferred until zterm has a non-persistent generation endpoint or durable cleanup for scratch sessions.

Previous: v0.2.0 — multi-backend public release with zeroclaw and openclaw behind a shared `trait AgentClient`, runtime multi-workspace switching, and a `--workspace` CLI override.

Earlier: v0.1.x — zeroclaw-only, private builds on ARGONAS.

---

## What zterm is

A pure API client. zterm talks to an agent daemon over HTTP + WebSocket streaming, renders responses in a terminal, and gets out of the way.

- **The daemon is authoritative.** zterm does not cache server-side state, shadow sessions locally, or reimplement daemon logic. What the API says is what you see.
- **Local or remote, same surface.** Point zterm at `http://localhost:42617` or at a Pi on your LAN or at a remote host — same code path. Transport is not a special case.
- **No embedded web UI.** No browser, no webview, no HTML rendering. Terminal only, `turbo-vision-4-rust` + `crossterm`, with `rustyline` kept as a legacy fallback.
- **No per-project web junk.** Features exposed only via the daemon's API. Agent-specific web widgets are explicitly out of scope — zterm renders what the API returns, in a terminal, respectfully.

## What zterm is not

- An agent framework.
- A replacement for any project's desktop UI.
- A local model host or inference engine.
- An orchestration plane.
- A sync/mirror of daemon state.

---

## Supported backends — claw-family only

zterm is the terminal for the **claw-family** agent ecosystem. Backends share a gateway-style control plane — provider/model discovery via `/api/config`, WebSocket streaming, sessions, skills, cron, channels, and a common slash-command vocabulary. If an agent speaks that contract, zterm drops in. If it doesn't, zterm isn't the right client.

### v0.2 — zeroclaw + openclaw (shipping)

- **zeroclaw (HTTP + WS).** Original v0.1 backend. Also covers `nemoclawzero` and `nclawzero` since they run zeroclaw as their daemon — one backend, three distributions.
- **openclaw (pure WebSocket, ed25519 device-key auth).** `OpenClawClient` sits behind the shared `trait AgentClient` alongside `ZeroclawClient`. Canonical v3 handshake, live-smoke-tested against upstream.
- **Runtime workspace switching.** `~/.zterm/config.toml` lists `[[workspaces]]` entries (zeroclaw + openclaw side by side). `zterm tui --workspace <name>` picks the boot workspace; inside the REPL, `/workspace list | info | switch <name>` operate at runtime. `/memory` and `/cron` resolve against the active workspace's client on every call.
- **Legacy single-workspace mode.** If `~/.zterm/config.toml` is absent or has no `[[workspaces]]` entries, zterm synthesizes a one-workspace App from `--remote` + `--token` — v0.1 users see zero change.

### Roadmap — remaining for later v0.x

- **Any API-compatible derivative** — forks or variants that speak the claw-family contract drop in as another `[[workspaces]]` entry, no code change.
- **Per-workspace background streaming** — backgrounded workspaces keep accumulating scrollback while another is foregrounded. (Tracked as chunk D-4 in `docs/v0.2-roadmap.md`.)
- **Reconnect + tab-badge state** — chunk D-5.

#### v0.2 smoke-test matrix

zterm's v0.2 CI + release smoke targets, sized to catch divergence without becoming a compat-testing project:

| Target | What it exercises |
|---|---|
| [`openclaw/openclaw`](https://github.com/openclaw/openclaw) | Upstream anchor |
| [`OpenBMB/EdgeClaw`](https://github.com/OpenBMB/EdgeClaw) | Largest active distribution — edge-cloud split with extension-heavy gateway |
| [`romiluz13/ClawMongo`](https://github.com/romiluz13/ClawMongo) | Storage-backend swap (SQLite → MongoDB) with explicit "same wire protocol" contract — compat canary |
| [`jiulingyun/openclaw-cn`](https://github.com/jiulingyun/openclaw-cn) | Highest-star community fork; regional channel pack on unchanged protocol |

Other active distributions (`DenchHQ/DenchClaw`, `QVerisAI/QVerisBot`, `AtomicBot-ai/atomicbot`, `jomafilms/openclaw-multitenant`) are out of the smoke matrix either because they track upstream too closely to add signal or because they diverge into product / tenancy layers that v0.2 isn't sized to cover.

### Explicitly out of scope

- **Hermes (Nous Research agent).** Hermes does ship a documented OpenAI-compatible HTTP API (`/v1/chat/completions` + SSE on `:8642`), so the exclusion is *architectural*, not *capability*. Hermes has the chat-and-stream slot but none of the claw-family control-plane surfaces (no `/api/config`, no pairing, no shared skills registry, no common slash vocabulary). Wiring `HermesClient` would either leave half of zterm's slash commands silently stubbed ("not supported in this workspace") or force `trait AgentClient` to split into `ChatClient` + `GatewayClient`, at which point zterm is competing with `aichat` / `llm-cli` / `openai-cli` for OpenAI-compat terminal users — a fight not worth picking. zterm's UX is *design-inspired* by the Hermes terminal (credit in `NOTICE`), but zterm is original Rust and does not connect to Hermes.
- **OpenAI-compatible chat backends in general.** If you need a terminal for `/v1/chat/completions`, there are good tools for that; zterm is not one of them.

---

## Install

### From source (only path in v0.1)

```bash
git clone https://github.com/perlowja/zterm.git
cd zterm
cargo build --release
# Binary at target/release/zterm
```

Rust **1.93+** required (matches upstream zeroclaw CI toolchain).

---

## Configure

zterm reads configuration from, in order of precedence:

1. CLI flags
2. Environment variables (loaded from `.env` if present via `dotenvy`)
3. Defaults

### Environment variables

| Var | Default | Purpose |
|---|---|---|
| `ZEROCLAW_URL` | `http://localhost:42617` | Gateway URL for the zeroclaw daemon |
| `ZEROCLAW_TOKEN` | *(prompted on first run)* | Bearer token for daemon auth |
| `MNEMOS_URL` | *(unset = memory disabled)* | Optional: MNEMOS memory-daemon URL |
| `MNEMOS_TOKEN` | *(unset = memory disabled)* | Optional: MNEMOS bearer token |

Copy `.env.example` to `.env` and fill in your values for local dev. `.env` is gitignored — **never commit it**.

If `MNEMOS_URL` and `MNEMOS_TOKEN` are both set, the `/memory` command is active. If either is unset or empty, memory commands cleanly disable themselves and zterm continues to work.

---

## Run

```bash
# Interactive TUI (default) — uses ~/.zterm/config.toml if present,
# otherwise falls back to single-workspace synth from flags/env
zterm tui

# Point at a remote daemon (single-workspace mode)
ZEROCLAW_URL=http://192.168.1.100:42617 zterm tui

# With explicit token (override env)
zterm tui --token my-bearer-token

# Multi-workspace: boot into a named workspace from config.toml
zterm tui --workspace openclaw-typhon

# Inside the REPL:
#   /workspace list              show configured workspaces, active marked
#   /workspace info              active-workspace details
#   /workspace switch <name>     activate another workspace at runtime
```

See `docs/config.toml.example` for a commented multi-workspace template (zeroclaw + openclaw side by side, inline + env-var tokens, optional labels). In multi-workspace mode, each `zeroclaw` workspace must resolve a bearer token from `token_env` or `token`; use `token = ""` only for an explicitly unauthenticated local gateway.

---

## Architecture

```
src/
├── main.rs                  entry; dotenv auto-load; CLI dispatch
└── cli/
    ├── client.rs            ZeroclawClient — HTTP + WS client
    ├── websocket.rs         WS handler, streaming events
    ├── streaming.rs         SSE / token streaming
    ├── pairing.rs           first-run daemon pairing
    ├── storage.rs           zterm-local state: input history, config
    ├── commands.rs          command palette (/model, /session, /memory, ...)
    ├── tui/                 Turbo Vision TUI + legacy rustyline fallback
    │   ├── mod.rs
    │   ├── delighters.rs
    │   ├── repl.rs
    │   ├── tv_ui.rs
    │   ├── themes.rs
    │   ├── splash.rs
    │   └── onboarding.rs
    ├── theme.rs             color theme
    ├── pagination.rs        response pagination
    ├── aliases.rs           command aliases
    ├── session_search.rs    fuzzy session search
    ├── retry.rs             transient-failure retry
    ├── batch.rs             non-interactive batch mode
    ├── input.rs             input handling
    ├── error_handler.rs     error messages
    └── ui.rs                terminal output utilities
```

**Opinionated boundaries:**

- `storage.rs` and the `~/.zterm/*.toml` files store **only zterm-local state** — input history, aliases, theme, daemon URL config, launch counter, and UI preferences. They must not shadow server-side state (sessions, messages, models). The daemon is authoritative.
- `client.rs` will become `trait AgentClient` + concrete impl(s) in v0.2.
- MNEMOS support is config-driven — not a runtime dependency. URL and token come from env; no hardcoded endpoints or credentials anywhere in source.

---

## Doctrine

zterm is built on a deliberate design doctrine:

- **Light. Thin. Fast. Memory-safe.** Constraints that ship.
- **API is authoritative.** We don't replicate daemon logic. We don't shadow state.
- **Local or remote, same code.** Transport is a URL, not a special case.
- **Terminal only.** No webview, no browser, no HTML rendering.
- **Add dependencies grudgingly.** Every crate in `Cargo.toml` earns its place.
- **1970s Unix values.** Compose, pipe, respect the user's shell, don't phone home, don't spam output, don't fork processes the user didn't ask for.

See the upstream design notes in the project repository for the longer version.

---

## Contributing

Currently a private project in development. External contribution pathway and issue tracker will land with the public release.

---

## Credits

zterm's **code is original Rust** by the author (Jason Perlow). Its **UX design is inspired by the Hermes terminal** — a separate agent-ecosystem project whose terminal interface set the bar for what a lean agent REPL should feel like. zterm is a clean-room reimplementation; no code or resources are forked or derived from Hermes. See `NOTICE` for attribution.

---

## License

Apache 2.0 — see `LICENSE`.

---

## Disclaimer

Personal project by Jason Perlow.

---

*"Back to the 1970s. We talk to the API. The API is authoritative. No web nonsense."*
