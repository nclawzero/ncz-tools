# ZeroClaw API Parity Roadmap

**Status:** Phase 1 (model selection) shipped — see commit history.
**Phase 2 scope:** the rest of `/api/*` that zeroclaw exposes today and zterm has not yet wired.

This doc tracks zterm's coverage of the zeroclaw gateway HTTP/WS surface.
Endpoint state was verified against `zeroclaw-demo-typhon` on TYPHON
(127.0.0.1:42617) on 2026-04-24. Items marked `503` return that on the
current build — those are an upstream gap, not a zterm backlog item.

## Endpoint matrix

| Endpoint | Verb | Daemon state | zterm coverage | Phase 2 work |
|---|---|---|---|---|
| `/health` | GET | 200 | startup health-check (`tui::run`) | keep — surface in status line indicator (`E-9?`) |
| `/metrics` | GET | 200 (Prometheus text) | none | metrics panel (P2-d) |
| `/api/config` | GET | 200, returns full TOML envelope | Phase 1: `refresh_models` parses `[providers.models.*]` | settings editor screen (P2-a) — read+edit other sections |
| `/api/config` | PUT | undefined in this build | none | persist settings-editor changes via PUT once daemon honors it |
| `/api/sessions` | GET | 200, returns session list | none | session browser pane (P2-b) |
| `/api/sessions/<id>` | GET | (assumed) | `load_session` (legacy REPL only) | wire into session browser |
| `/api/sessions/<id>` | DELETE | (assumed) | `delete_session` (legacy REPL only) | confirm-dialog from browser pane |
| `/api/status` | GET | 200, daemon health | none | live status-line indicator (P2-d) |
| `/api/events` | GET | 200, suspected SSE | none | event-stream panel (P2-c); confirm SSE shape first |
| `/api/skills` | GET | 503 | none | needs-zeroclaw-update — defer until daemon implements |
| `/api/workspaces` | GET | 503 | none | needs-zeroclaw-update |
| `/api/providers` | GET | 503 | derived from `/api/config` | needs-zeroclaw-update for canonical answer; Phase 1 already covers via config-derived list |
| `/api/agents` | GET | 503 | none | needs-zeroclaw-update |
| `/api/plugins` | GET | 503 | none | needs-zeroclaw-update |
| `/api/autonomy` | GET | 503 | none | needs-zeroclaw-update — autonomy/policy editor depends on this |
| `/api/metrics` | GET | 503 | none | needs-zeroclaw-update — distinct from `/metrics` (Prometheus) |
| `/api/logs` | GET | 503 | none | needs-zeroclaw-update — log tail panel depends on this |
| `/api/cron/*` | GET/POST | (active) | `list_cron_jobs`, `create_cron_job`, `pause_cron`, `resume_cron`, `delete_cron`, `create_cron_at` | cron browser pane (P2-e) |
| `/webhook` | POST | 200 | current chat path (`submit_turn`) | keep |
| `/ws/chat` | GET | 200 | legacy streaming demo (`websocket.rs`) | future: replace webhook chat path with WS streaming |
| `/pair` | POST | 200 | `pairing::PairingManager` | already on the path when `require_pairing=true` |

## Phase 1 — landed (this commit)

- `ZeroclawClient::refresh_models` GETs `/api/config`, parses
  `[providers.models.*]`, builds `Vec<ModelInfo { key, provider, model }>`.
- `current_model_key()` resolves `ZTERM_MODEL` env → `/models set <key>`
  → daemon's `[providers] fallback` → static `"primary"` fallback.
- `submit_turn` and the websocket envelope send the live key, not a
  hardcoded literal.
- `/models` lists keys from the live daemon, marks the active one with
  `*`, and `/models set <key>` validates against the cached list.
- Status line and splash already render the model name; they now show
  the seeded key (`primary` etc.) instead of `mixtral-8x7b-32768`.
- Test fixtures and bootstrap defaults scrubbed of vendor-brand strings
  per `feedback_anthropic_tos.md`.

## Phase 2 — priorities

Ranked by operational value to a zterm user. Only items that depend
solely on zterm code; daemon-503 endpoints stay out of this list until
the daemon implements them.

### P2-a. Settings editor (read/write `/api/config`)

- New TUI screen, F5-bound (per v0.3 roadmap reservation).
- `GET /api/config` we already do — Phase 1 `fetch_config_toml` is the
  read half.
- Edit-and-save needs `PUT /api/config` to land daemon-side first; for
  now we can ship read-only with a banner explaining the round-trip
  isn't supported yet.
- Section list: `[providers.models]`, `[autonomy]`, `[security]`,
  `[gateway]`, `[reliability]`, `[memory]` are the most operator-
  relevant.

### P2-b. Session browser pane

- Calls `GET /api/sessions` on demand (Ctrl-F8 popup, modal list).
- Selecting an entry calls `GET /api/sessions/<id>` and switches the
  active chat-pane scrollback to that session's history.
- `DELETE /api/sessions/<id>` from the row's context menu (with
  confirm-dialog).
- Loose dependency: needs the daemon to emit per-session metadata in
  the list response (current `Session` struct is sparse — `id`, `name`,
  `model`, `provider`); we may need to lobby for `created_at`, `last_active`, `message_count`.

### P2-c. Event-stream panel

- `GET /api/events` is suspected SSE; first task is to verify the
  Content-Type and event shape.
- Once confirmed, add an `eventsource-stream`-driven background task
  (we already depend on this crate in `Cargo.toml`) that pushes events
  into a ring-buffered side panel (Ctrl-F7?).
- Filtering by event type is a v2 add; v1 is just "show me the firehose
  in chronological order."

### P2-d. Live status indicators

- Status line currently shows model + workspace + ctx placeholder.
- Add a tick-driven probe of `GET /api/status` (every 5–10s) → status-
  line green/amber/red dot for daemon health.
- `GET /metrics` parsing for token-rate / cost numbers is a stretch
  goal; needs a Prometheus text-format mini-parser.

### P2-e. Cron browser pane

- All the necessary client methods already exist
  (`list_cron_jobs`, `pause_cron`, `resume_cron`, etc.).
- Just needs a TV view: scrollable list, row-action context menu
  (pause/resume/delete), modal "add cron" form.
- `/cron` slash-command remains the keyboardless equivalent.

### Deferred — needs-zeroclaw-update

These return `503 Not Implemented` on the current daemon. zterm
gracefully ignores them today; once daemon ships:

- `/api/skills` — skill catalog browser
- `/api/workspaces` — multi-workspace dashboard (zterm already has its
  own client-side multi-workspace; this would mirror the daemon's view)
- `/api/agents` — agent definition viewer
- `/api/plugins` — plugin manager
- `/api/autonomy` — autonomy-policy editor (overlaps P2-a)
- `/api/logs` — log-tail panel

## Out of scope for Phase 2

- Replacing webhook chat with WS streaming. Webhook works, latency is
  acceptable, and a backend rewrite isn't worth doing before settings
  editor + session browser ship.
- Pair flow rework. `PairingManager` already covers `require_pairing=true`.
- TLS / mTLS for the gateway. Still a daemon-side decision.

---

**Last updated:** 2026-04-24
**Authors:** zterm maintainers
**Companion docs:** `docs/v0.3-ux-roadmap.md` (UI chunk plan), `docs/ARCHITECTURE.md`
