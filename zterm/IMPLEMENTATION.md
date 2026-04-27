---
name: ZTerm Comprehensive Implementation Plan (All Components)
description: Full feature inventory with architecture, 4-phase breakdown, estimated effort, and integration details
type: solutions
originSessionId: 75142acb-e769-4ab2-a91e-6bb00a5ba842
---
# ZTerm: Complete Terminal REPL Implementation

**Goal**: Professional-grade terminal interface for the **claw-family** of agents (zeroclaw, openclaw, nemoclaw + API-compatible derivatives), with UX design-inspired by the Hermes terminal (credit in `NOTICE`) and enterprise polishing. v0.1 is zeroclaw-only; v0.2 adds `trait AgentClient` and a second concrete backend (openclaw).

**Scope**: ~2,500–3,200 LOC new code (up from 500 LOC due to full feature set)
**Timeline**: 3–4 weeks (was 5–8 days)
**Effort**: 80–100 hours (was 40–50 hours)

---

## v0.2 Backend Scope — claw-family only

zterm targets the **claw-family control plane** — agents that expose `/api/config`-style config discovery, a WebSocket gateway with pairing, and a shared slash-command vocabulary (`/agent`, `/doctor`, `/skill`, `/providers`, `/cron`, `/channels`, `/session`). Backends that share that contract drop in behind `trait AgentClient` without widening the abstraction.

### In scope

| Backend | Status | Implementation |
|---|---|---|
| zeroclaw | v0.1 (current) | `ZeroclawClient` — reference |
| openclaw | v0.2 (planned) | `OpenClawClient` — different gateway wire, same control plane |
| nemoclaw | v0.2 (planned) | Typically `ClawFamilyClient` shape with config differences |
| API-compatible derivatives | any | no code change — another `[[workspaces]]` entry in `~/.zterm/config.toml` |

### v0.2 smoke-test matrix

Sized to catch divergence without becoming a compat-testing project. Targets chosen from a 2026-04-22 audit of active openclaw forks:

| Target | Stars | Why in the matrix |
|---|---|---|
| [`openclaw/openclaw`](https://github.com/openclaw/openclaw) | 362k | Upstream anchor |
| [`OpenBMB/EdgeClaw`](https://github.com/OpenBMB/EdgeClaw) | 1.2k | Largest active distribution (edge-cloud split, extension-heavy gateway) |
| [`romiluz13/ClawMongo`](https://github.com/romiluz13/ClawMongo) | 18 | Storage-backend swap (SQLite → MongoDB); explicit "same wire protocol" README — compat canary |
| [`jiulingyun/openclaw-cn`](https://github.com/jiulingyun/openclaw-cn) | 4.7k | Highest-star community fork; regional channel pack on unchanged protocol |

Other active distributions (`DenchHQ/DenchClaw`, `QVerisAI/QVerisBot`, `AtomicBot-ai/atomicbot`, `jomafilms/openclaw-multitenant`) are out of scope either because they track upstream too closely to add signal or because they diverge into product / tenancy layers that v0.2 isn't sized to cover.

### Out of scope

- **Hermes** (Nous Research agent). Has a documented OpenAI-compatible HTTP API on `:8642` with `/v1/chat/completions` + SSE, so the exclusion is *architectural*, not *capability*. Hermes has the chat-and-stream slot but none of the claw-family control-plane surfaces. Wiring `HermesClient` would either stub out half the slash commands per workspace or force `trait AgentClient` to split into `ChatClient` + `GatewayClient`, putting zterm in commodity-aichat territory. UX credit only (see `NOTICE`); no code path.
- OpenAI-compatible chat backends in general. Use `aichat`/`llm-cli` for those.

### Trait surface (preview)

The v0.2 trait is sized against the two-backend reality (zeroclaw + openclaw) rather than pre-generalizing. Rough shape:

```rust
#[async_trait]
pub trait AgentClient: Send + Sync {
    async fn fetch_config(&self) -> Result<AgentConfig>;
    async fn list_providers(&self) -> Result<Vec<Provider>>;
    async fn list_models(&self, provider: &str) -> Result<Vec<Model>>;
    async fn send_turn(&self, session_id: &str, message: &str)
        -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    async fn create_session(&self, name: &str) -> Result<SessionSummary>;
    async fn get_session(&self, id: &str) -> Result<Session>;
    async fn delete_session(&self, id: &str) -> Result<()>;
    // cron / skills / channels land behind optional sub-traits so derivatives
    // that don't implement those surfaces don't have to stub them.
}
```

MNEMOS `/memory` commands stay outside the trait — MNEMOS is user-global, not agent-scoped, and the client lives in a shared module used from every workspace.

---

## Complete Feature Matrix

### TIER 1: Must-Have (MVP)
| Feature | Source | Category | Status |
|---------|--------|----------|--------|
| Onboarding wizard (31 screens) | zeroclaw-tui | Setup | ✅ Reuse |
| Chat REPL loop | Hermes | Core | 🔨 Build |
| Real-time streaming responses | Hermes | Core | 🔨 Build |
| Model switching (`/model`) | Hermes | Commands | 🔨 Build |
| Session management (`/session`) | Hermes | Commands | 🔨 Build |
| Input history (↑/↓ navigation) | Hermes | Input | 🔨 Build |
| Persistent status bar | zeroclaw-tui + custom | UI | 🔨 Build |
| Error handling + graceful exit | Both | Reliability | 🔨 Build |
| `/help` command | Hermes | Discovery | 🔨 Build |
| REST API client (SSE streaming) | Custom | Backend | 🔨 Build |

### TIER 2: Nice-to-Have (Polish)
| Feature | Source | Category | Status |
|---------|--------|----------|--------|
| Tab completion (`/mo[TAB]`) | Hermes | Input | 🔨 Build |
| Fuzzy search in history (`Ctrl+R`) | Hermes | Input | 🔨 Build |
| Spinner animations | zeroclaw-tui | UI | 🔨 Adapt |
| Pagination for long lists | zeroclaw-tui | UI | 🔨 Build |
| Memory search (`/memory`) | Hermes | Commands | 🔨 Build |
| Skill management (`/skill`) | Hermes | Commands | 🔨 Build |
| Config editing (`/config`) | Hermes | Commands | 🔨 Build |
| Code block formatting | Hermes | Output | 🔨 Build |

### TIER 3: Enterprise (Phase 4)
| Feature | Source | Category | Status |
|---------|--------|----------|--------|
| Session metadata (created, msg count) | Custom | Sessions | 🔨 Build |
| Hot-reload config on `/config` save | Custom | Config | 🔨 Build |
| API key re-entry flow | Custom | Security | 🔨 Build |
| Help system with inline docs | zeroclaw-tui pattern | Help | 🔨 Build |
| Clipboard integration (copy response) | Terminal | UX | 🔨 Build |
| Non-TTY graceful degradation | Custom | Robustness | 🔨 Build |
| Response buffering strategy | Custom | Performance | 🔨 Build |

---

## Architecture Overview

```
zeroclaw tui [--session-name] [--remote] [--token]
  ↓
┌─────────────────────────────────────────────────────────────┐
│                    ZTerm Main Module                         │
│                 src/cli/tui/mod.rs (~100 LOC)                │
├─────────────────────────────────────────────────────────────┤
│
├─→ [1] Entry Point (100 LOC)
│   ├─ Check: config exists?
│   │  ├─ No → run_tui_onboarding() [REUSE zeroclaw-tui]
│   │  └─ Yes → skip to chat
│   ├─ Check: TTY or piped?
│   ├─ Load session (create/switch)
│   └─ Enter chat_repl_loop()
│
├─→ [2] Chat REPL Loop (300 LOC) ← src/cli/tui/repl.rs
│   ├─ Render: status bar (model, provider, session)
│   ├─ Read: input with history & completion
│   ├─ Dispatch: `/command` or submit turn
│   ├─ Stream: response via SSE
│   ├─ Display: formatted with code blocks
│   └─ Save: to session history
│
├─→ [3] Input Handler (250 LOC) ← src/cli/tui/input.rs
│   ├─ Input history manager (push/navigate)
│   ├─ Tab completion (commands, models, sessions)
│   ├─ Fuzzy search (Ctrl+R)
│   ├─ Multi-line input buffer
│   ├─ Validation & feedback
│   └─ Keyboard event dispatch
│
├─→ [4] Command Palette (400 LOC) ← src/cli/tui/commands.rs
│   ├─ /model → SelectableList widget (REUSE)
│   ├─ /session → SelectableList + metadata
│   ├─ /memory → search + formatted display
│   ├─ /skill → checkbox list widget
│   ├─ /config → re-open onboarding flow
│   ├─ /clear → clear history
│   ├─ /help → command reference
│   ├─ /save → export transcript
│   └─ /exit → save session, cleanup
│
├─→ [5] Streaming Handler (250 LOC) ← src/cli/tui/streaming.rs
│   ├─ SSE event listener (non-blocking)
│   ├─ Chunk accumulator (per token, per line, or per sentence)
│   ├─ ANSI formatting preservation
│   ├─ Spinner animation (concurrent with streaming)
│   ├─ Error detection (network, timeout, API error)
│   ├─ Reconnection logic (exponential backoff)
│   └─ Response buffering strategy
│
├─→ [6] Session Manager (300 LOC) ← src/cli/tui/session.rs
│   ├─ Session metadata (created, model, msg count)
│   ├─ Load session from server
│   ├─ Create new session
│   ├─ Switch active session
│   ├─ Persist session to disk (optional cache)
│   ├─ List sessions with pagination
│   └─ Delete session
│
├─→ [7] REST API Client (400 LOC) ← src/cli/tui/client.rs
│   ├─ Bearer token auth
│   ├─ TOML config get/put
│   ├─ Session CRUD (list, create, switch)
│   ├─ Turn submission (streaming via SSE)
│   ├─ Memory queries
│   ├─ Provider/model catalog fetch
│   ├─ Skill management
│   ├─ Error mapping (friendly messages)
│   └─ Retry logic (transient failures)
│
├─→ [8] UI Components (400 LOC) ← src/cli/tui/ui.rs
│   ├─ Status bar renderer (persistent, updates)
│   ├─ Spinner animation (multiple faces)
│   ├─ Error panel (styled with theme)
│   ├─ Code block formatter (with language indicator)
│   ├─ List paginator (with scroll indicators)
│   ├─ Help panel (command reference)
│   ├─ Confirmation dialog
│   ├─ Progress indicator (multi-step operations)
│   └─ Theme application (reuse zeroclaw-tui colors)
│
└─→ [9] Config & Session Storage (150 LOC) ← src/cli/tui/storage.rs
    ├─ Session directory: ~/.zeroclaw/sessions/{session-id}/
    │  ├─ meta.json (created, model, msg_count, last_active)
    │  ├─ history.jsonl (one message per line)
    │  └─ transcript.md (human-readable export)
    ├─ History file cache (local input history)
    ├─ Session index (for list operations)
    └─ Config hot-reload on changes
```

---

## Phase-by-Phase Implementation

### Phase 1: Foundation & Onboarding (3–4 days)

**Goal**: Get onboarding working, establish core structure.

**Deliverables:**
- `src/cli/tui/mod.rs` — entry point, TTY detection, config check
- `src/cli/tui/client.rs` — REST API wrapper (basic)
- `src/cli/tui/storage.rs` — session directory structure
- Reuse: `zeroclaw-tui::onboarding`

**Success Criteria:**
- `zeroclaw tui` runs, detects missing config
- Onboarding wizard runs (all 31 screens)
- Config saved to `~/.zeroclaw/config.toml`
- Session directory created
- Returns to entry point on completion

**Effort**: ~200 LOC (mostly reuse + integration)

---

### Phase 2: REST Client & Streaming (3–4 days)

**Goal**: Connect to zeroclaw gateway, test response streaming.

**Deliverables:**
- `src/cli/tui/client.rs` — full REST wrapper with SSE
- `src/cli/tui/streaming.rs` — SSE event listener + chunk display
- `src/cli/tui/repl.rs` — basic REPL loop (read → submit → stream)
- Integration tests with zeroclaw gateway

**Success Criteria:**
- `zeroclaw tui` connects to gateway with Bearer token
- Submit turn, receive streaming response
- Chunks display in real-time (no buffering)
- Timeout handling + reconnection logic works
- Error messages are user-friendly

**Effort**: ~600 LOC (client, streaming, tests)

---

### Phase 3: Commands & Input (4–5 days)

**Goal**: Implement command palette, input history, completion.

**Deliverables:**
- `src/cli/tui/commands.rs` — command dispatch (/model, /session, /memory, etc.)
- `src/cli/tui/input.rs` — history, tab completion, fuzzy search
- `src/cli/tui/session.rs` — session management
- `src/cli/tui/ui.rs` — status bar, spinners, error panels
- Widget adaptations (SelectableList for model picker)

**Success Criteria:**
- `/model` shows SelectableList, switches model
- `/session` lists sessions, allows switching
- `/memory` searches memory, displays results
- `/skill` shows checkbox list, enables/disables
- `/config` re-opens onboarding
- Input history works (↑/↓), fuzzy search (Ctrl+R)
- Tab completion for commands & model IDs
- Status bar updates on model/session change
- Help displays all commands + descriptions

**Effort**: ~1,200 LOC (commands, input handler, UI)

---

### Phase 4: Polish & Enterprise (4–5 days)

**Goal**: Error handling, session persistence, performance, documentation.

**Deliverables:**
- `src/cli/tui/session.rs` — metadata, persistence
- Retry logic in client (exponential backoff)
- Hot-reload config on `/config` save
- Code block formatting
- Pagination for long lists
- Non-TTY fallback (graceful degradation)
- Comprehensive error handling
- Documentation & examples
- Integration tests (end-to-end)

**Success Criteria:**
- Session history persists across restarts
- Session metadata (created, msg count) tracked
- Network errors trigger retry with backoff
- Code blocks display with language indicator
- Lists paginate with scroll indicators (▲▼)
- Piped input handled gracefully
- Ctrl+C exits cleanly, saves session
- All commands have help text
- Examples in README

**Effort**: ~1,000 LOC (polish, tests, docs)

---

## Detailed Component Specifications

### 1. Input History Manager

```rust
// src/cli/tui/input.rs

pub struct InputHistory {
    entries: Vec<String>,
    current_index: Option<usize>,
}

impl InputHistory {
    pub fn push(&mut self, entry: String);
    pub fn navigate_up(&mut self) -> Option<String>;
    pub fn navigate_down(&mut self) -> Option<String>;
    pub fn search(&self, query: &str) -> Vec<(usize, &str)>;  // Fuzzy search
    pub fn load_from_file(path: &Path) -> Result<Self>;
    pub fn save_to_file(&self, path: &Path) -> Result<()>;
}
```

**Storage**: `~/.zeroclaw/input_history.jsonl` (one entry per line)

**Behavior**:
- ↑ navigates backward through history
- ↓ navigates forward
- Ctrl+R opens fuzzy search
- New input resets position
- History persists across sessions

---

### 2. Tab Completion Engine

```rust
// src/cli/tui/input.rs (completion module)

pub struct CompletionProvider {
    commands: Vec<&'static str>,  // ["/model", "/session", ...]
    models: Vec<String>,           // ["claude-3.5-opus", ...]
    sessions: Vec<String>,         // ["main", "research", ...]
}

impl CompletionProvider {
    pub fn complete(&self, input: &str) -> Vec<String>;
    // Returns matching completions for current prefix
}
```

**Trigger**: Tab key
**Priority**: 
1. Commands (if starts with `/`)
2. Model IDs (if after `/model `)
3. Session names (if after `/session `)
4. Memory keywords (if after `/memory `)

**Display**: Show matches inline or in popup list

---

### 3. Streaming Response Handler

```rust
// src/cli/tui/streaming.rs

pub struct StreamHandler {
    buffer: String,
    chunk_timeout: Duration,
    spinner: Spinner,
}

impl StreamHandler {
    pub async fn stream_response(&mut self, rx: EventReceiver) -> Result<String>;
    
    // Handles:
    // - SSE events → chunks
    // - Buffering strategy (per-token vs per-sentence)
    // - Display updates without blocking
    // - Spinner animation during streaming
    // - Error detection (network, timeout, API error)
    // - Reconnection logic (exponential backoff, max 3 retries)
}
```

**Buffering Strategy**:
- Display every token as it arrives (no buffering)
- For slower responses, aggregate into 100ms batches
- Preserve ANSI color codes and formatting

**Spinner**: Animated while streaming (faces: ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏)

**Error Handling**:
```
Network error: Connection timeout
Retrying in 500ms... (attempt 1/3)
[retry succeeds → continue streaming]
```

---

### 4. Command Dispatcher

```rust
// src/cli/tui/commands.rs

pub enum Command {
    Model,
    Session,
    Memory(String),  // query
    Skill,
    Config,
    Clear,
    Help,
    Save(Option<String>),  // optional filename
    Exit,
}

pub async fn dispatch(cmd: Command, client: &ZeroclawClient, ui: &mut UI) -> Result<()>;

// /model → show SelectableList, get selection, submit turn with new model
// /session → show SelectableList with metadata, switch session
// /memory → search, paginate results, display formatted
// /skill → show checkbox list, submit changes
// /config → re-run onboarding, validate, save
// /clear → clear local history (not server-side)
// /help → show command reference
// /save → export session transcript to file
// /exit → save session, cleanup, exit
```

---

### 5. Session Manager

```rust
// src/cli/tui/session.rs

pub struct Session {
    pub id: String,
    pub name: String,
    pub model: String,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub message_count: usize,
    pub last_active: DateTime<Utc>,
}

pub struct SessionManager {
    client: ZeroclawClient,
    current_session: Session,
    session_dir: PathBuf,
}

impl SessionManager {
    pub async fn create(&mut self, name: &str) -> Result<Session>;
    pub async fn switch(&mut self, id: &str) -> Result<()>;
    pub async fn list(&self) -> Result<Vec<Session>>;
    pub async fn delete(&self, id: &str) -> Result<()>;
    pub fn load_metadata(&self, id: &str) -> Result<SessionMetadata>;
    pub fn save_metadata(&self, metadata: SessionMetadata) -> Result<()>;
}
```

**Storage**:
```
~/.zeroclaw/sessions/
├── main/
│   ├── meta.json
│   │   {
│   │     "id": "main",
│   │     "name": "main",
│   │     "model": "claude-3.5-opus",
│   │     "provider": "anthropic",
│   │     "created_at": "2026-04-20T10:30:00Z",
│   │     "message_count": 15,
│   │     "last_active": "2026-04-20T14:45:00Z"
│   │   }
│   ├── history.jsonl
│   │   {"role": "user", "content": "...", "timestamp": "..."}
│   │   {"role": "assistant", "content": "...", "timestamp": "..."}
│   └── transcript.md (auto-generated)
└── research/
    └── ... (similar structure)
```

---

### 6. REST API Client (Detailed)

```rust
// src/cli/tui/client.rs

pub struct ZeroclawClient {
    base_url: String,
    token: String,
    http_client: HttpClient,
}

impl ZeroclawClient {
    // Config
    pub async fn get_config(&self) -> Result<Config>;
    pub async fn put_config(&self, config: Config) -> Result<()>;
    
    // Models & Providers
    pub async fn list_providers(&self) -> Result<Vec<Provider>>;
    pub async fn get_models(&self, provider: &str) -> Result<Vec<Model>>;
    pub async fn probe_models(&self, provider: &str, api_key: Option<&str>) -> Result<Vec<Model>>;
    
    // Sessions
    pub async fn list_sessions(&self) -> Result<Vec<Session>>;
    pub async fn create_session(&self, name: &str) -> Result<Session>;
    pub async fn get_session(&self, id: &str) -> Result<Session>;
    pub async fn delete_session(&self, id: &str) -> Result<()>;
    
    // Turns (chat)
    pub async fn submit_turn(
        &self,
        session_id: &str,
        message: &str,
    ) -> Result<EventReceiver>;  // SSE stream
    
    // Memory
    pub async fn search_memory(&self, query: &str) -> Result<Vec<MemoryEntry>>;
    pub async fn store_memory(&self, entry: MemoryEntry) -> Result<()>;
    
    // Skills
    pub async fn list_skills(&self) -> Result<Vec<Skill>>;
    pub async fn enable_skill(&self, skill_id: &str) -> Result<()>;
    pub async fn disable_skill(&self, skill_id: &str) -> Result<()>;
}

// Error handling
pub enum ClientError {
    Network(String),
    Auth(String),      // 401, 403
    NotFound(String),  // 404
    Server(String),    // 5xx
    Timeout,
    Invalid(String),   // 4xx non-auth
}

impl Display for ClientError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::Auth(_) => write!(f, "Authentication failed. Check your API key."),
            Self::NotFound(_) => write!(f, "Resource not found."),
            Self::Timeout => write!(f, "Request timed out. Check your connection."),
            _ => write!(f, "Error: {:?}", self),
        }
    }
}
```

---

### 7. UI Components

```rust
// src/cli/tui/ui.rs

pub struct StatusBar {
    model: String,
    provider: String,
    session_name: String,
}

impl StatusBar {
    pub fn render(&self) -> String;
    // Output: "Model: claude-3.5-opus  Provider: anthropic  Session: main"
    //         "─────────────────────────────────────────────────────────"
}

pub struct Spinner {
    frames: Vec<&'static str>,
    current_frame: usize,
}

impl Spinner {
    pub fn next_frame(&mut self) -> &'static str;
}

pub struct Paginator {
    items: Vec<String>,
    page_size: usize,
    current_page: usize,
}

impl Paginator {
    pub fn render(&self) -> String;
    // Shows: "Item 1\nItem 2\nItem 3  [▲ Page 1/5 ▼]"
}

pub struct CodeBlockFormatter;

impl CodeBlockFormatter {
    pub fn format(text: &str) -> String;
    // Detects ``` blocks, adds language indicator
    // Example output:
    // ┌─ rust
    // │ fn main() { ... }
    // └─────────────────
}

pub struct ErrorPanel {
    message: String,
    suggestion: Option<String>,
}

impl ErrorPanel {
    pub fn render(&self) -> String;
    // Shows: "❌ Authentication failed\n💡 Check your API key at: https://..."
}
```

---

### 8. Configuration Management

```rust
// src/cli/tui/client.rs (config section)

pub async fn hot_reload_config(path: &Path) -> Result<Config> {
    // Watch config file, reload on change
    // Re-populate completion providers with new models/skills
    // Update session manager's available models
}

pub async fn validate_api_key(key: &str, provider: &str) -> Result<()> {
    // Make test request to provider
    // Return friendly error if invalid
    // Used during onboarding + `/config` re-entry
}
```

---

### 9. Non-TTY Graceful Degradation

```rust
// src/cli/tui/mod.rs (entry point section)

if atty::isnt(atty::Stream::Stdin) {
    // Piped input (not interactive)
    
    // Option 1: Simple JSON input mode
    // Example: echo '{"message": "Hello"}' | zeroclaw tui
    
    // Option 2: Num-pad fallback for model selection
    // When /model is requested, show:
    // 1: claude-3.5-opus
    // 2: gpt-4o
    // [Enter number]: 1
    
    // Option 3: Error with suggestion
    // "Interactive mode requires a TTY.\nTry: zeroclaw tui --non-interactive < input.json"
}
```

---

## Complete LOC Breakdown

| Component | File | Est. LOC | Notes |
|-----------|------|----------|-------|
| **Entry Point** | `tui/mod.rs` | 100 | Config check, TTY detect, session load |
| **REPL Loop** | `tui/repl.rs` | 300 | Main read→dispatch→stream loop |
| **Input Handler** | `tui/input.rs` | 250 | History, completion, fuzzy search |
| **Commands** | `tui/commands.rs` | 400 | /model, /session, /memory, etc. |
| **Streaming** | `tui/streaming.rs` | 250 | SSE, spinner, buffering, retry |
| **Sessions** | `tui/session.rs` | 300 | CRUD, metadata, persistence |
| **REST Client** | `tui/client.rs` | 400 | API wrapper, auth, error handling |
| **UI Components** | `tui/ui.rs` | 400 | Status bar, spinner, paginator, code formatter |
| **Storage** | `tui/storage.rs` | 150 | Session dir, history cache, config |
| **Tests** | `tui/tests/` | 600 | Unit, integration, E2E tests |
| **Docs & Examples** | README, examples/ | 200 | Usage guide, command reference |
| — | — | — | — |
| **TOTAL NEW CODE** | — | **3,350 LOC** | |
| **REUSED (zeroclaw-tui)** | — | **1,000 LOC** | Onboarding, widgets, theme |

---

## Integration Checklist

### Phase 1 (Onboarding)
- [ ] Wire zeroclaw-tui::onboarding into entry point
- [ ] Detect first run (no config)
- [ ] Save config to ~/.zeroclaw/config.toml
- [ ] Create session directory
- [ ] Return to entry point on completion

### Phase 2 (Streaming)
- [ ] Build REST client with auth
- [ ] Implement SSE listener
- [ ] Test with zeroclaw gateway
- [ ] Timeout handling (30s)
- [ ] Reconnection logic (3 retries, 500ms backoff)

### Phase 3 (Commands)
- [ ] Wire command dispatcher
- [ ] Implement /model (SelectableList)
- [ ] Implement /session (list, switch)
- [ ] Implement /memory (search, paginate)
- [ ] Implement /skill (checkbox)
- [ ] Implement /config (re-run onboarding)
- [ ] Input history (↑/↓)
- [ ] Tab completion
- [ ] Status bar updates

### Phase 4 (Polish)
- [ ] Session persistence (meta.json, history.jsonl)
- [ ] Session metadata tracking
- [ ] Code block formatting
- [ ] Pagination for long lists
- [ ] Non-TTY fallback
- [ ] Comprehensive error messages
- [ ] Help system
- [ ] Integration tests (end-to-end)
- [ ] Documentation

---

## Test Strategy

### Unit Tests
- InputHistory (push, navigate, search)
- CompletionProvider (complete commands, models)
- Paginator (render, page navigation)
- CodeBlockFormatter (detect, format blocks)
- SessionMetadata (serialize, deserialize)

### Integration Tests
- Client → zeroclaw gateway (submit turn, stream)
- Command dispatch → effect (model switch, session change)
- Onboarding → config save → REPL start
- Session persistence → load → history intact

### End-to-End Tests
- Full flow: onboarding → chat → switch model → /memory → /config → exit
- Network failure → retry → recovery
- Non-TTY input → graceful fallback

---

## Success Criteria (All Phases)

### Phase 1
- ✅ First run triggers onboarding
- ✅ Config saved correctly
- ✅ Session directory created
- ✅ Returns to REPL on completion

### Phase 2
- ✅ Connects to gateway with token
- ✅ Submit turn, receive streaming response
- ✅ Chunks display in real-time
- ✅ Timeout + reconnection works
- ✅ Errors are user-friendly

### Phase 3
- ✅ /model shows list, switches model
- ✅ /session lists sessions, switches
- ✅ /memory searches, displays results
- ✅ /skill shows checklist, enables/disables
- ✅ Input history works (↑/↓)
- ✅ Tab completion works
- ✅ Status bar updates on change
- ✅ /help shows commands

### Phase 4
- ✅ Session history persists
- ✅ Metadata tracked (created, msg count)
- ✅ Network errors retry with backoff
- ✅ Code blocks display with language
- ✅ Long lists paginate
- ✅ Piped input handled gracefully
- ✅ Ctrl+C exits cleanly
- ✅ All commands have help text
- ✅ Examples in README
- ✅ E2E tests passing

---

## Effort & Timeline (Revised)

| Phase | Duration | FTE | Deliverables |
|-------|----------|-----|--------------|
| Phase 1 | 3–4 days | 1.0 | Onboarding, storage, client (basic) |
| Phase 2 | 3–4 days | 1.0 | REST client (full), streaming, REPL (basic) |
| Phase 3 | 4–5 days | 1.0 | Commands, input handler, UI, session mgr |
| Phase 4 | 4–5 days | 1.0 | Polish, tests, docs, enterprise features |
| **Total** | **14–18 days** | **4 FTE-days** | **All features** |

**If parallelized with 2 developers**: **7–9 days**
**If serialized (1 developer)**: **14–18 days**

---

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| SSE connection loss | Exponential backoff retry (3×), graceful fallback |
| Large response streams | Chunked display, pagination for results |
| Command incompleteness | Help system, error messages guide users |
| Config hot-reload issues | Validation on load, revert on error |
| Session persistence conflicts | Lock file or server-side coordination |
| Non-TTY complexity | Clear error message, documented fallback |

---

## Future Enhancements (Post-MVP)

- [ ] Web UI parity (browser interface using same backend)
- [ ] Collaboration mode (shared sessions, multi-user)
- [ ] Plugin system (custom commands via WASM)
- [ ] Voice input (transcription + streaming)
- [ ] Syntax highlighting via `syntect` crate
- [ ] Session sharing (export/import with encryption)
- [ ] Batch mode (scripted interactions)
- [ ] Theme customization (color schemes)

