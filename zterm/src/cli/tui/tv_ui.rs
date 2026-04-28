//! Turbo Vision UI (v0.3 E-1 scaffold + E-2 async bridge).
//!
//! Stands up a `TApplication` with a menu bar, a main window hosting
//! a scrollable chat pane and an input line, and a status line with
//! contextual key hints. Theme is stock cyan-on-blue Borland (E-6
//! adds the `pfs`/`mono`/`amber`/`green`/`borland` presets).
//!
//! E-2 wires the async agent path: a tokio worker task owns the
//! shared `App` and drains `WorkerRequest`s from an mpsc channel; the
//! TV event loop pushes a request on Enter and drains streamed
//! `TurnChunk`s from a companion channel on every tick, appending
//! tokens into the chat pane as they arrive.
//!
//! The legacy rustyline REPL is kept behind `--legacy-repl` as a
//! transition fallback while the Paradox 4.5-flavored UX lands in
//! the E-slice sequence.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use turbo_vision::app::Application;
use turbo_vision::core::command::CM_QUIT;
// Ctrl-key codes that turbo-vision's keyboard module doesn't export as
// public KB_* constants. Using raw values — these are ASCII control chars
// stable across every terminal emulator we care about.
const KB_CTRL_D: u16 = 0x0004; // EOT — exit (shell/REPL convention)
const KB_CTRL_K: u16 = 0x000B; // VT — command palette popup
const KB_CTRL_L: u16 = 0x000C; // FF — force redraw (curses convention)

use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{
    Event, EventType, KB_ALT_X, KB_CTRL_H, KB_CTRL_O, KB_CTRL_P, KB_CTRL_S, KB_CTRL_T, KB_CTRL_Y,
    KB_CTRL_Z, KB_DOWN, KB_ENTER, KB_ESC, KB_F1, KB_F10, KB_LEFT, KB_RIGHT, KB_UP,
};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::menu_data::{Menu, MenuItem};
use turbo_vision::core::palette::{Attr, Palette, TvColor};
use turbo_vision::core::state::{StateFlags, SF_SHADOW};
use turbo_vision::terminal::Terminal;
use turbo_vision::views::input_line::{InputLine, InputLineBuilder};
use turbo_vision::views::menu_bar::{MenuBar, SubMenu};
use turbo_vision::views::menu_box::MenuBox;
use turbo_vision::views::status_line::{StatusItem, StatusLine};
use turbo_vision::views::view::{write_line_to_terminal, View};
use turbo_vision::views::window::WindowBuilder;

use crate::cli::agent::{StreamSink, TurnChunk, TurnUsage};
use crate::cli::client::Session;
use crate::cli::commands::CommandHandler;
use crate::cli::storage;
use crate::cli::tui::delighters;
use crate::cli::tui::themes;
use crate::cli::workspace::App;

/// TUI → worker: user-initiated actions the worker should dispatch
/// against the active workspace. Unified envelope so the TUI holds
/// only one `Sender` and the worker matches on one type.
#[derive(Debug)]
enum WorkerRequest {
    /// Send `text` to the active agent backend via `submit_turn`.
    /// Streaming chunks flow back through the sink installed on the
    /// client at startup.
    Turn(String),
    /// Dispatch a slash command (e.g. `/help`, `/models list`)
    /// through the shared `CommandHandler`. Structured
    /// `Ok(Some(text))` output is forwarded to the chat pane; any
    /// legacy stdout-only command still gets a small advisory frame.
    Command(String),
}

// Custom commands. Menu and slash-popup entries route through the
// shared `CommandHandler` where a backend action exists; purely
// visual TUI concerns (theme presets/editing) stay local.
const CMD_HELP: u16 = 1001;
const CMD_ABOUT: u16 = 1002;
const CMD_WORKSPACE_LIST: u16 = 1003;
const CMD_WORKSPACE_SWITCH: u16 = 1004;
const CMD_MODELS_LIST: u16 = 1005;
const CMD_MEMORY_SEARCH: u16 = 1006;
const CMD_SESSION_NEW: u16 = 1007;
const CMD_SESSION_OPEN: u16 = 1008;
const CMD_PROVIDERS_LIST: u16 = 1009;
const CMD_MCP_STATUS: u16 = 1010;
const CMD_WORKSPACE_INFO: u16 = 1011;
const CMD_MODELS_STATUS: u16 = 1012;
const CMD_MEMORY_STATS: u16 = 1013;
const CMD_SESSION_LIST: u16 = 1014;
/// Slash-popup theme entries occupy `[CMD_THEME_BASE, +PRESETS.len())`.
/// The selected index is mapped back to `themes::PRESETS[i]` on
/// return so palette application lives in one place
/// (`handle_theme_command`).
const CMD_THEME_BASE: u16 = 1200;
/// Direct palette cycler (Ctrl-P). Steps through `themes::PRESETS`
/// in order, wrapping at the end. Distinct from `/theme <name>`
/// and the slash-popup theme entries — those are jump-to-named,
/// this is "give me the next one without thinking about names."
const CMD_PALETTE_NEXT: u16 = 1300;
/// Re-persist the currently active theme to `~/.zterm/theme.toml`.
/// Wired to Ctrl-S as a Mac-friendly F2 stand-in. No-op on the
/// palette itself — just a confirmation write.
const CMD_PERSIST_THEME: u16 = 1301;

/// `/` key maps to this KeyCode per `crossterm_to_keycode`: plain
/// ASCII character with no modifiers, so `c as u16` = 0x2F.
const KB_SLASH: u16 = b'/' as u16;

/// Depth of the input-line undo/redo ring. 64 snapshots is more
/// than enough for the longest slash command or turn prompt a
/// human is going to type while still keeping the memory
/// footprint trivial (avg <2 KiB).
const INPUT_UNDO_DEPTH: usize = 64;

/// Braille-dot spinner frames. Keeps visual continuity with the
/// legacy rustyline REPL's `cli::streaming::SPINNER_FRAMES` without
/// taking a cross-module dependency.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

/// How long a status-line toast (e.g. palette confirmation after
/// Ctrl-P) remains visible. ~1s is long enough to read but short
/// enough that mashing Ctrl-P repeatedly still feels live.
const TOAST_DURATION: Duration = Duration::from_millis(1000);
const TYPEWRITER_INTERVAL: Duration = Duration::from_millis(30);

struct TypewriterState {
    chars: Vec<char>,
    pos: usize,
    last_emit: Instant,
    after_lines: Vec<String>,
    completed: bool,
}

impl TypewriterState {
    fn new(text: impl Into<String>, after_lines: Vec<String>) -> Self {
        Self {
            chars: text.into().chars().collect(),
            pos: 0,
            last_emit: Instant::now(),
            after_lines,
            completed: false,
        }
    }

    fn tick(&mut self, lines: &Rc<RefCell<Vec<String>>>) {
        if self.completed {
            return;
        }

        let due = typewriter_chars_due(self.last_emit.elapsed(), TYPEWRITER_INTERVAL);
        if due == 0 {
            return;
        }

        let mut lines = lines.borrow_mut();
        if lines.is_empty() {
            lines.push(String::new());
        }

        for _ in 0..due {
            let Some(ch) = self.chars.get(self.pos).copied() else {
                self.completed = true;
                lines.push(String::new());
                lines.extend(self.after_lines.drain(..));
                return;
            };
            self.pos += 1;
            self.last_emit += TYPEWRITER_INTERVAL;
            if ch == '\n' {
                lines.push(String::new());
            } else if let Some(last) = lines.last_mut() {
                last.push(ch);
            }
        }
    }
}

fn start_typewriter(
    state: &mut Option<TypewriterState>,
    lines: &Rc<RefCell<Vec<String>>>,
    text: String,
    after_lines: Vec<String>,
) {
    lines.borrow_mut().push(String::new());
    *state = Some(TypewriterState::new(text, after_lines));
}

fn typewriter_chars_due(elapsed: Duration, interval: Duration) -> usize {
    if interval.is_zero() {
        return usize::MAX;
    }
    (elapsed.as_millis() / interval.as_millis()) as usize
}

/// Live state feeding the status line. Mutated on each event-loop
/// tick from the TV thread.
struct StatusState {
    /// Last known active workspace. Refreshed each tick by
    /// try-locking the shared `App`; falls back to this cached
    /// value when the worker is holding the mutex.
    workspace: String,
    /// Model name as snapshotted at boot. Updating in real time
    /// would require the agent client to surface the `model`
    /// field from turn responses — plumbed in a later slice.
    model: String,
    /// Instant the most recent turn was submitted. While `Some`
    /// the elapsed counter is live; cleared on `Finished` and the
    /// final duration gets frozen into `frozen_elapsed`.
    turn_start: Option<Instant>,
    /// Final elapsed value from the last completed turn. Rendered
    /// between turns so the status line doesn't snap back to
    /// `00:00` the instant a response finishes.
    frozen_elapsed: Option<Duration>,
    /// Frame index into `SPINNER_FRAMES`. Advanced per tick when
    /// `turn_start.is_some()`. Wraps around modulo frame count.
    spinner_frame: usize,
    /// Wall-clock of the last spinner advance. Caps the animation
    /// at ~12 frames/sec regardless of event-loop tick rate so
    /// the spinner looks steady on both an idle UI and a
    /// token-saturated one.
    last_spinner_tick: Instant,
    /// Transient message shown in the status-line summary slot
    /// for ~1s (e.g. "Palette: Paradox 4.5" after a Ctrl-P
    /// cycle). Cleared on tick once the deadline passes so the
    /// regular workspace/model/elapsed summary returns.
    toast: Option<(String, Instant)>,
    /// Canonical name of the currently active theme preset
    /// (`"borland"`, `"pfs"`, etc.). Drives Ctrl-P palette
    /// cycling so we always know "what comes next" without
    /// re-reading `~/.zterm/theme.toml`. Defaults to whatever
    /// `themes::load_persisted` returned at boot.
    current_theme: String,
    /// Token usage from the latest turn that reported it. Rendered
    /// as the `ctx used/total (%)` segment in the status line.
    usage: Option<TurnUsage>,
    /// Whether terminal bell should ring on error frames.
    beep_on_error: bool,
}

impl StatusState {
    fn new(workspace: String, model: String, current_theme: String, beep_on_error: bool) -> Self {
        Self {
            workspace,
            model,
            turn_start: None,
            frozen_elapsed: None,
            spinner_frame: 0,
            last_spinner_tick: Instant::now(),
            toast: None,
            current_theme,
            usage: None,
            beep_on_error,
        }
    }

    /// Show a transient message in the status summary for
    /// `TOAST_DURATION` (~1s). Replaces any existing toast.
    fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Drop the toast if its deadline has passed. Called every
    /// tick before the status line is rebuilt so stale text
    /// doesn't linger.
    fn expire_toast(&mut self) {
        if let Some((_, t)) = &self.toast {
            if t.elapsed() >= TOAST_DURATION {
                self.toast = None;
            }
        }
    }

    /// Advance the spinner frame if at least 80ms have passed
    /// since the last advance. Called at the top of every event-
    /// loop tick; no-op while `turn_start.is_none()`.
    fn tick_spinner(&mut self) {
        if self.turn_start.is_none() {
            return;
        }
        if should_advance_spinner(self.last_spinner_tick.elapsed()) {
            self.spinner_frame = next_spinner_frame(self.spinner_frame, SPINNER_FRAMES.len());
            self.last_spinner_tick = Instant::now();
        }
    }

    fn spinner_char(&self) -> Option<&'static str> {
        if self.turn_start.is_some() {
            Some(SPINNER_FRAMES[self.spinner_frame])
        } else {
            None
        }
    }

    fn begin_turn(&mut self) {
        self.turn_start = Some(Instant::now());
        self.frozen_elapsed = None;
    }

    fn end_turn(&mut self) {
        if let Some(start) = self.turn_start.take() {
            self.frozen_elapsed = Some(start.elapsed());
        }
    }

    /// Current elapsed duration: live while a turn is in flight,
    /// last-turn's frozen value between turns, zero at boot.
    fn elapsed(&self) -> Duration {
        self.turn_start
            .map(|s| s.elapsed())
            .or(self.frozen_elapsed)
            .unwrap_or_default()
    }

    fn elapsed_mmss(&self) -> String {
        let total = self.elapsed().as_secs();
        format!("{:02}:{:02}", total / 60, total % 60)
    }

    /// Build the status-line content string for the right-most
    /// segment. The spinner prefix only renders while a turn is in
    /// flight; token usage reflects the latest `TurnChunk::Usage`
    /// observed from the active backend.
    ///
    /// While a toast is active (set via `set_toast`, e.g. on a
    /// Ctrl-P palette cycle), its text replaces the workspace/model
    /// summary so the user gets immediate visual feedback.
    fn render_summary(&self) -> String {
        if let Some((msg, _)) = &self.toast {
            return msg.clone();
        }
        let lead = match self.spinner_char() {
            Some(s) => format!("{s} "),
            None => String::new(),
        };
        format!(
            "{}{} · {} · {} · {} elapsed",
            lead,
            self.workspace,
            self.model,
            render_ctx_usage(self.usage),
            self.elapsed_mmss()
        )
    }
}

fn should_advance_spinner(elapsed: Duration) -> bool {
    elapsed >= SPINNER_INTERVAL
}

fn next_spinner_frame(current: usize, frame_count: usize) -> usize {
    if frame_count == 0 {
        return 0;
    }
    (current + 1) % frame_count
}

fn render_ctx_usage(usage: Option<TurnUsage>) -> String {
    let Some(usage) = usage else {
        return "ctx --/--".to_string();
    };
    let Some(used) = usage.used_tokens() else {
        return "ctx --/--".to_string();
    };
    match usage.context_window {
        Some(total) if total > 0 => {
            let pct = usage.budget_pct().unwrap_or(0);
            format!("ctx {used}/{total} ({pct}%) {}", token_budget_bar(pct, 10))
        }
        _ => format!("ctx {used}/--"),
    }
}

fn token_budget_bar(pct: u8, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = (pct.min(100) as usize * width).div_ceil(100);
    format!(
        "[{}{}]",
        "#".repeat(filled),
        "-".repeat(width.saturating_sub(filled))
    )
}

fn parse_beep_toggle(arg: &str) -> Option<bool> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "beep on" | "beep true" | "beep 1" => Some(true),
        "beep off" | "beep false" | "beep 0" => Some(false),
        _ => None,
    }
}

/// Entry point from `cli::tui::run` when `--legacy-repl` is not set.
///
/// Turbo Vision's event loop is synchronous and must own the main
/// thread, so the UI runs inside `spawn_blocking`. The async agent
/// path lives in a tokio worker task: TV thread → `WorkerRequest` via
/// bounded mpsc → worker → active client's `submit_turn` → streamed
/// `TurnChunk`s via unbounded mpsc → TV thread drains on every
/// poll-timeout tick.
pub async fn run(
    app: Arc<Mutex<App>>,
    session: Session,
    model: String,
    provider: String,
) -> Result<()> {
    info!("Starting tv_ui (E-2 async bridge)");

    // Snapshot the active workspace name for the status line. A later
    // slice replaces this one-shot read with a live subscription so
    // `/workspace switch` updates the status bar in place.
    let workspace_name = {
        let locked = app.lock().await;
        locked
            .active_workspace()
            .map(|w| w.config.name.clone())
            .unwrap_or_else(|| "<unknown>".to_string())
    };

    // Channels: bounded for requests (backpressure on a spammy user
    // isn't a bad thing), unbounded for streamed chunks (the UI
    // drains on every tick so this should never grow).
    let (req_tx, mut req_rx) = mpsc::channel::<WorkerRequest>(32);
    let (event_tx, event_rx) = mpsc::unbounded_channel::<TurnChunk>();

    let connect_splash = Some(connect_splash_for_workspace(&workspace_name));

    // Install the streaming sink on the boot workspace. The worker
    // also reinstalls before every submitted turn and after every
    // workspace switch, so cached splash reads cannot leave the
    // newly-active client detached from the TUI stream.
    if !install_stream_sink_on_active_client(&app, event_tx.clone()).await {
        warn!("tv_ui: active workspace has no client; turn submits will fail cleanly");
    }

    // Worker task: owns the shared App reference and processes one
    // request at a time. Errors are surfaced to the UI via the sink's
    // `TurnChunk::Finished(Err(_))`; the worker itself never panics
    // the whole process.
    let worker_app = Arc::clone(&app);
    let mut worker_session_id = session.id.clone();
    let worker_sink = event_tx.clone();
    let worker_cmd_handler = CommandHandler::new(Arc::clone(&app));
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            match req {
                WorkerRequest::Turn(text) => {
                    let client_opt = {
                        let guard = worker_app.lock().await;
                        guard.active_workspace().and_then(|w| w.client.clone())
                    };
                    match client_opt {
                        Some(client_arc) => {
                            let mut client = client_arc.lock().await;
                            client.set_stream_sink(Some(worker_sink.clone()));
                            // `submit_turn` is responsible for
                            // emitting exactly one `Finished(_)`
                            // through the installed sink on both
                            // success and failure. The worker
                            // swallows the Err return (already
                            // reflected on the sink) and waits for
                            // the next request.
                            let _ = client.submit_turn(&worker_session_id, &text).await;
                        }
                        None => {
                            // Only branch where `submit_turn` was
                            // never called — emit the terminal
                            // frame ourselves so the UI can unstick
                            // the placeholder line.
                            let _ = worker_sink.send(TurnChunk::Finished(Err(
                                "no active workspace client".to_string(),
                            )));
                        }
                    }
                }
                WorkerRequest::Command(cmdline) => {
                    // Route slash commands through the shared
                    // `CommandHandler`. v0.3.1 refactors the TV
                    // menu/popup command paths to return structured
                    // strings; legacy stdout-only commands still get
                    // an advisory rather than silently disappearing.
                    let is_workspace_switch = is_workspace_switch_command(&cmdline);
                    if let Some(target_session) = session_switch_target(&cmdline) {
                        match resolve_or_create_session_for_worker(&worker_app, target_session)
                            .await
                        {
                            Ok(session) => {
                                worker_session_id = session.id;
                            }
                            Err(e) => {
                                let _ = worker_sink.send(TurnChunk::Finished(Err(format!(
                                    "could not switch session to `{target_session}`: {e}"
                                ))));
                                continue;
                            }
                        }
                    }
                    match worker_cmd_handler
                        .handle(&cmdline, &worker_session_id)
                        .await
                    {
                        Ok(Some(text)) => {
                            let _ = worker_sink.send(TurnChunk::Token(text));
                            if is_workspace_switch {
                                install_stream_sink_on_active_client(
                                    &worker_app,
                                    worker_sink.clone(),
                                )
                                .await;
                                let switched_workspace = {
                                    let guard = worker_app.lock().await;
                                    guard.active_workspace().map(|w| w.config.name.clone())
                                };
                                if let Some(name) = switched_workspace {
                                    let splash = connect_splash_for_workspace(&name);
                                    let _ = worker_sink.send(TurnChunk::Typewriter(splash));
                                }
                            }
                            let _ = worker_sink.send(TurnChunk::Finished(Ok(String::new())));
                        }
                        Ok(None) => {
                            let _ = worker_sink.send(TurnChunk::Token(format!(
                                "(command `{cmdline}` — stdout renderer lands \
                                 in a later slice; structured output not yet \
                                 available)"
                            )));
                            let _ = worker_sink.send(TurnChunk::Finished(Ok(String::new())));
                        }
                        Err(e) if e.to_string() == "EXIT" => {
                            // `/exit` bubbled up; mirror the
                            // rustyline behavior by signalling a
                            // clean shutdown via Finished(Ok).
                            // E-6 introduces a dedicated Quit
                            // TurnChunk; for now the user just
                            // sees the command acknowledged and
                            // Alt-X actually closes the UI.
                            let _ = worker_sink.send(TurnChunk::Token(
                                "(use Alt-X or F10 → File → Exit to leave \
                                 the TUI)"
                                    .to_string(),
                            ));
                            let _ = worker_sink.send(TurnChunk::Finished(Ok(String::new())));
                        }
                        Err(e) => {
                            let _ = worker_sink.send(TurnChunk::Finished(Err(e.to_string())));
                        }
                    }
                }
            }
        }
    });

    let blocking_app = Arc::clone(&app);
    tokio::task::spawn_blocking(move || {
        run_blocking(
            blocking_app,
            session,
            model,
            provider,
            workspace_name,
            connect_splash,
            req_tx,
            event_rx,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("tv_ui join error: {e}"))?
}

fn connect_splash_for_workspace(workspace_name: &str) -> String {
    let cache_path = delighters::default_connect_splash_cache_path(workspace_name);
    if let Some(path) = &cache_path {
        if let Some(cached) = delighters::read_cached_connect_splash(
            path,
            std::time::SystemTime::now(),
            delighters::CONNECT_SPLASH_TTL,
        ) {
            return cached;
        }
    }

    let fallback = delighters::local_connect_splash(workspace_name);
    if let Some(path) = cache_path {
        let fallback_for_cache = fallback.clone();
        let _ = std::thread::spawn(move || {
            if let Err(e) = delighters::write_connect_splash_cache(&path, &fallback_for_cache) {
                warn!("connect-splash cache write failed: {e}");
            }
        });
    }
    fallback
}

async fn install_stream_sink_on_active_client(app: &Arc<Mutex<App>>, sink: StreamSink) -> bool {
    let client = {
        let guard = app.lock().await;
        guard.active_workspace().and_then(|w| w.client.clone())
    };
    let Some(client) = client else {
        return false;
    };
    client.lock().await.set_stream_sink(Some(sink));
    true
}

async fn resolve_or_create_session_for_worker(
    app: &Arc<Mutex<App>>,
    session_name: &str,
) -> Result<Session> {
    if let Ok(metadata) = load_session_metadata_by_id_or_name(session_name) {
        return Ok(Session {
            id: metadata.id,
            name: metadata.name,
            model: metadata.model,
            provider: metadata.provider,
        });
    }

    let client = {
        let guard = app.lock().await;
        guard.active_workspace().and_then(|w| w.client.clone())
    }
    .ok_or_else(|| anyhow::anyhow!("no active workspace client"))?;

    let session = client.lock().await.create_session(session_name).await?;
    let metadata = storage::SessionMetadata {
        id: session.id.clone(),
        name: session.name.clone(),
        model: session.model.clone(),
        provider: session.provider.clone(),
        created_at: Utc::now().to_rfc3339(),
        message_count: 0,
        last_active: Utc::now().to_rfc3339(),
    };
    storage::save_session_metadata(&metadata)?;
    Ok(session)
}

fn load_session_metadata_by_id_or_name(session: &str) -> Result<storage::SessionMetadata> {
    if let Ok(metadata) = storage::load_session_metadata(session) {
        return Ok(metadata);
    }
    storage::list_sessions()?
        .into_iter()
        .find(|metadata| metadata.id == session || metadata.name == session)
        .ok_or_else(|| anyhow::anyhow!("session metadata not found: {session}"))
}

fn is_workspace_switch_command(cmdline: &str) -> bool {
    let mut parts = cmdline.split_whitespace();
    matches!(parts.next(), Some("/workspace" | "/workspaces"))
        && matches!(parts.next(), Some("switch"))
        && parts.next().is_some()
}

fn session_switch_target(cmdline: &str) -> Option<&str> {
    let mut parts = cmdline.split_whitespace();
    if parts.next()? != "/session" {
        return None;
    }
    match parts.next()? {
        "list" | "info" | "delete" => None,
        "switch" | "create" => parts.next(),
        name => Some(name),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_blocking(
    app: Arc<Mutex<App>>,
    session: Session,
    model: String,
    provider: String,
    workspace_name: String,
    connect_splash: Option<String>,
    req_tx: mpsc::Sender<WorkerRequest>,
    mut event_rx: mpsc::UnboundedReceiver<TurnChunk>,
) -> Result<()> {
    let mut tapp =
        Application::new().map_err(|e| anyhow::anyhow!("turbo-vision init failed: {e:?}"))?;

    let (tw, th) = tapp.terminal.size();
    let w = tw;
    let h = th;

    let menu_bar = build_menu_bar(w);
    tapp.set_menu_bar(menu_bar);

    // Apply the persisted theme (E-8a) before the UI draws its
    // first frame. The default is `borland` (CP_APP_COLOR) if
    // there's no `~/.zterm/theme.toml` or the file is malformed.
    // `Application::set_palette` also flips `needs_redraw`, which
    // we don't depend on (our event loop always redraws) but is
    // harmless.
    let (persisted_palette, persisted_name) = themes::load_persisted();
    tapp.set_palette(Some(persisted_palette));
    info!("tv_ui: applied persisted theme '{}'", persisted_name);

    let welcome_back = match delighters::record_launch() {
        Ok((_, quote)) => quote,
        Err(e) => {
            warn!("welcome-back state update failed: {e}");
            None
        }
    };
    let beep_on_error = delighters::default_state_path()
        .map(|path| delighters::load_state(&path).beep_on_error)
        .unwrap_or(false);

    // Live status state. The status line is rebuilt on every
    // redraw tick by `refresh_status_line` rather than built once
    // here, so the elapsed counter and workspace label stay
    // current without per-field mutation APIs.
    let mut status_state = StatusState::new(
        workspace_name.clone(),
        model.clone(),
        persisted_name.clone(),
        beep_on_error,
    );
    let initial_status_line = build_status_line(w, h, &status_state);
    tapp.set_status_line(initial_status_line);

    // Shared chat buffer. Rc<RefCell<_>> because the Turbo Vision
    // event loop is single-threaded; the custom ChatPane view reads
    // the buffer during its draw pass, and the event loop writes to
    // it on Enter.
    let welcome_lines = initial_chat_lines(&workspace_name, &model, &provider, welcome_back);
    let mut typewriter_state = None;
    let initial_lines = if let Some(text) = connect_splash {
        let state = TypewriterState::new(text, welcome_lines);
        typewriter_state = Some(state);
        vec![String::new()]
    } else {
        welcome_lines
    };
    let chat_lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(initial_lines));

    // Shared input buffer for the InputLine. Turbo Vision's
    // InputLine is already Rc<RefCell<String>>-backed; we hold an
    // extra clone so the event loop can pull the text on Enter.
    let input_data: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // Undo / redo stacks for the input line (E-6b). Each slot is a
    // committed snapshot of `input_data` *before* a modifying
    // keystroke was applied. Ring-buffered at `INPUT_UNDO_DEPTH`
    // (old entries drop off the front when full) so a user who
    // types a long essay can still rewind the last ~64 changes
    // without unbounded memory growth.
    let input_undo: Rc<RefCell<VecDeque<String>>> =
        Rc::new(RefCell::new(VecDeque::with_capacity(INPUT_UNDO_DEPTH)));
    let input_redo: Rc<RefCell<VecDeque<String>>> =
        Rc::new(RefCell::new(VecDeque::with_capacity(INPUT_UNDO_DEPTH)));

    // Screen layout:
    //   row 0         menu bar
    //   row 1         desktop background gap (classic Borland)
    //   row 2         window top frame
    //   rows 3..=h-4  chat pane
    //   row h-3       input line
    //   row h-2       window bottom frame
    //   row h-1       status line
    //
    // Turbo Vision applies *two* transforms when a window is added
    // to the desktop: `constrain_to_limits` (which clamps against
    // the desktop rect, bumping negative/off-desktop offsets) and
    // `Group::add` (which translates by the desktop origin). Both
    // fire even when the caller's bounds are already "correct," so
    // a window that nominally fills the desktop ends up shifted
    // down one row. Rather than fight the API, we honor the 1-row
    // gap between menu bar and window frame — matches the classic
    // Borland look where panels sit in a visible desktop field
    // rather than flush against the menu.
    //
    // Child bounds passed to `win.add` are window-interior-relative;
    // `Group::add` inside the window translates them to absolute
    // terminal coords. Interior size is (w-2) × (h-5).
    let win_bounds = Rect::new(0, 0, w, h - 3);
    let mut win = WindowBuilder::new()
        .bounds(win_bounds)
        .title(format!("zterm — session: {}", session.name))
        .resizable(false)
        .build();
    // Drop shadow would force another `constrain_to_limits` shift
    // (2 cols wide, 1 row tall) that shoves the window up/left
    // off-screen. Modal dialogs keep their shadows (E-5); the
    // full-width workspace frame doesn't need one.
    win.set_state_flag(SF_SHADOW, false);

    let interior_w = w - 2;
    let interior_h = h - 5;
    let input_row = interior_h - 1;

    // Chat pane goes *inside* the window via `win.add` so focus and
    // palette chain flow through the interior Group normally.
    let chat_rect = Rect::new(0, 0, interior_w, input_row);
    let chat = ChatPane::new(chat_rect, Rc::clone(&chat_lines));
    win.add(Box::new(chat));

    tapp.desktop.add(Box::new(win));

    // The input line is deliberately NOT added to the window. Turbo
    // Vision's `InputLine` tracks an internal `cursor_pos` in bytes
    // against the shared `Rc<RefCell<String>>`, and there's no public
    // hook to let us reset it when we clear the buffer externally on
    // Enter. Clearing the buffer while leaving `cursor_pos` pointing
    // past the (now empty) string panics on the next keystroke with
    // `assertion failed: self.is_char_boundary(idx)` inside
    // `String::insert`. Holding the `InputLine` directly lets us
    // call its `set_text("")` method, which resets `cursor_pos`,
    // selection, and view scroll atomically.
    //
    // Side effect: we draw and dispatch events for it ourselves in
    // the event loop, outside the window's interior Group. Bounds
    // are screen-absolute since no Group::add translation fires.
    // The absolute position matches what win.add would have produced
    // with interior-relative `(0, input_row)` — one row above the
    // window's bottom frame — so the visual layout is unchanged
    // from E-1.
    let input_bounds = Rect::new(1, h - 3, w - 1, h - 2);
    let input_line: Rc<RefCell<InputLine>> = Rc::new(RefCell::new(
        InputLineBuilder::new()
            .bounds(input_bounds)
            .max_length(1024)
            .data(Rc::clone(&input_data))
            .build(),
    ));
    input_line.borrow_mut().set_focus(true);

    run_event_loop(
        &mut tapp,
        chat_lines,
        input_data,
        input_line,
        input_undo,
        input_redo,
        req_tx,
        &mut event_rx,
        &mut status_state,
        &mut typewriter_state,
        &app,
        w,
        h,
    )?;

    Ok(())
}

fn initial_chat_lines(
    workspace: &str,
    model: &str,
    provider: &str,
    welcome_back: Option<String>,
) -> Vec<String> {
    let mut lines = vec![
        "zterm — Turbo Vision UI (E-2 async bridge)".to_string(),
        format!("workspace: {workspace}  ·  model: {model}  ·  provider: {provider}"),
        String::new(),
        "Type a message and press Enter to submit a turn to the agent.".to_string(),
        "Press Ctrl-D (or Alt-X, F10→File→Exit, /exit) to quit. Ctrl-L to redraw.".to_string(),
        String::new(),
    ];
    if let Some(quote) = welcome_back {
        lines.insert(2, quote);
    }
    lines
}

fn build_menu_bar(w: i16) -> MenuBar {
    let file_menu = SubMenu::new(
        "~F~ile",
        Menu::from_items(vec![
            MenuItem::with_shortcut("~N~ew session", CMD_SESSION_NEW, 0, "", 0),
            MenuItem::with_shortcut("~O~pen session", CMD_SESSION_OPEN, 0, "", 0),
            MenuItem::with_shortcut("~L~ist sessions", CMD_SESSION_LIST, 0, "", 0),
            MenuItem::separator(),
            MenuItem::with_shortcut("E~x~it", CM_QUIT, 0, "Alt+X", 0),
        ]),
    );

    let session_menu = SubMenu::new(
        "~S~ession",
        Menu::from_items(vec![MenuItem::with_shortcut("~I~nfo", CMD_ABOUT, 0, "", 0)]),
    );

    let workspace_menu = SubMenu::new(
        "~W~orkspace",
        Menu::from_items(vec![
            MenuItem::with_shortcut("~L~ist", CMD_WORKSPACE_LIST, 0, "", 0),
            MenuItem::with_shortcut("~I~nfo", CMD_WORKSPACE_INFO, 0, "", 0),
            MenuItem::with_shortcut("~S~witch…", CMD_WORKSPACE_SWITCH, 0, "", 0),
        ]),
    );

    let model_menu = SubMenu::new(
        "~M~odel",
        Menu::from_items(vec![
            MenuItem::with_shortcut("~L~ist", CMD_MODELS_LIST, 0, "", 0),
            MenuItem::with_shortcut("~S~tatus", CMD_MODELS_STATUS, 0, "", 0),
            MenuItem::with_shortcut("~P~roviders", CMD_PROVIDERS_LIST, 0, "", 0),
        ]),
    );

    let memory_menu = SubMenu::new(
        "Me~m~ory",
        Menu::from_items(vec![
            MenuItem::with_shortcut("~R~ecent", CMD_MEMORY_SEARCH, 0, "", 0),
            MenuItem::with_shortcut("~S~tats", CMD_MEMORY_STATS, 0, "", 0),
            MenuItem::with_shortcut("~M~CP status", CMD_MCP_STATUS, 0, "", 0),
        ]),
    );

    let help_menu = SubMenu::new(
        "~H~elp",
        Menu::from_items(vec![
            MenuItem::with_shortcut("~T~opics", CMD_HELP, 0, "F1", 0),
            MenuItem::separator(),
            MenuItem::with_shortcut("~A~bout", CMD_ABOUT, 0, "", 0),
        ]),
    );

    let mut menu_bar = MenuBar::new(Rect::new(0, 0, w, 1));
    menu_bar.add_submenu(file_menu);
    menu_bar.add_submenu(session_menu);
    menu_bar.add_submenu(workspace_menu);
    menu_bar.add_submenu(model_menu);
    menu_bar.add_submenu(memory_menu);
    menu_bar.add_submenu(help_menu);
    menu_bar
}

fn build_status_line(w: i16, h: i16, state: &StatusState) -> StatusLine {
    // The `↩ Send` hint uses key=0 so `StatusLine::handle_event`
    // doesn't absorb KB_ENTER. Input submission is intercepted in
    // `run_event_loop` before the desktop dispatches to the
    // focused `InputLine`.
    //
    // F-keys get Ctrl-equivalent hints baked into the visible label
    // (`F1/^H Help` etc.) so Mac/SSH users — whose terminal often
    // intercepts F1/F10/F11/F12 — see the alternate path at a
    // glance. The actual key dispatch for the Ctrl variants happens
    // in `run_event_loop` ahead of the InputLine, since stacking
    // multiple StatusItems wired to the same command would clutter
    // the bar visually.
    //
    // The last segment renders a live `workspace · model · ctx · elapsed`
    // summary. E-4 ships everything except the ctx token counts —
    // zeroclaw's `/webhook` path doesn't surface usage today, so
    // `ctx --/--` is a placeholder until a turn-result usage field
    // is plumbed. While a toast is active (e.g. after Ctrl-P) the
    // summary slot is overlaid with the toast text — see
    // `StatusState::render_summary`.
    let summary = state.render_summary();
    StatusLine::new(
        Rect::new(0, h - 1, w, h),
        vec![
            StatusItem::new("~Alt-X~/~^D~ Exit", KB_ALT_X, CM_QUIT),
            StatusItem::new("~F1/^H~ Help", KB_F1, CMD_HELP),
            StatusItem::new("~F10/^T~ Menu", KB_F10, 0),
            StatusItem::new("~^P~ Palette", KB_CTRL_P, CMD_PALETTE_NEXT),
            StatusItem::new("~↩~ Send", 0, 0),
            StatusItem::new(&summary, 0, 0),
        ],
    )
}

/// Refresh the status line to reflect the current `StatusState`.
/// Called on every event-loop tick. Also tries (non-blockingly) to
/// refresh the active workspace name from the shared `App` so
/// `/workspace switch` (E-5) picks up live without extra plumbing.
fn refresh_status_line(
    tapp: &mut Application,
    w: i16,
    h: i16,
    state: &mut StatusState,
    app: &Arc<Mutex<App>>,
) {
    // Non-blocking workspace refresh. If the worker holds the mutex
    // this tick, reuse the cached value — skipping one frame of
    // updates is preferable to blocking the UI.
    if let Ok(guard) = app.try_lock() {
        if let Some(ws) = guard.active_workspace() {
            if ws.config.name != state.workspace {
                state.workspace = ws.config.name.clone();
            }
        }
    }
    tapp.set_status_line(build_status_line(w, h, state));
}

#[allow(clippy::too_many_arguments)]
fn run_event_loop(
    app: &mut Application,
    chat_lines: Rc<RefCell<Vec<String>>>,
    input_data: Rc<RefCell<String>>,
    input_line: Rc<RefCell<InputLine>>,
    input_undo: Rc<RefCell<VecDeque<String>>>,
    input_redo: Rc<RefCell<VecDeque<String>>>,
    req_tx: mpsc::Sender<WorkerRequest>,
    event_rx: &mut mpsc::UnboundedReceiver<TurnChunk>,
    status_state: &mut StatusState,
    typewriter_state: &mut Option<TypewriterState>,
    shared_app: &Arc<Mutex<App>>,
    w: i16,
    h: i16,
) -> Result<()> {
    app.running = true;
    let mut last_size = app.terminal.size();
    while app.running {
        // Poll terminal size once per tick. On change, print a
        // user-facing notice and exit so the caller can relaunch
        // at the new size. Live-resize (rebuilding all view
        // bounds on the fly) is a larger refactor — pending.
        let cur_size = app.terminal.size();
        if cur_size != last_size {
            app.terminal.clear();
            eprintln!(
                "\n⚠️  Terminal resized ({}x{} → {}x{}). Please rerun `zterm tui` \
                 at the new size.",
                last_size.0, last_size.1, cur_size.0, cur_size.1
            );
            app.running = false;
            break;
        }
        last_size = cur_size;

        // Drain any streamed chunks that arrived since the last tick
        // *before* redrawing, so new tokens are visible this frame.
        // Also lets the status-state timer observe `Finished` frames
        // inline with the rest of the UI update.
        let error_frame =
            drain_stream_events(event_rx, &chat_lines, status_state, typewriter_state);
        if error_frame && status_state.beep_on_error {
            let _ = app.terminal.beep();
        }
        if let Some(writer) = typewriter_state.as_mut() {
            writer.tick(&chat_lines);
        }
        if typewriter_state
            .as_ref()
            .map(|writer| writer.completed)
            .unwrap_or(false)
        {
            *typewriter_state = None;
        }

        // Advance the in-flight spinner (no-op while idle).
        status_state.tick_spinner();

        // Drop any toast (Ctrl-P palette confirmation, Ctrl-S
        // save confirmation, …) whose 1-second deadline has
        // passed so the regular workspace summary returns.
        status_state.expire_toast();

        // Rebuild the status line so the elapsed counter and any
        // workspace change are reflected in this frame. The build
        // is cheap (one Vec<StatusItem> + a couple of String
        // allocations).
        refresh_status_line(app, w, h, status_state, shared_app);

        redraw(app, &input_line);

        let Ok(Some(mut event)) = app.terminal.poll_event(Duration::from_millis(50)) else {
            continue;
        };

        // Global shortcut: Alt-X → quit, even when menu is closed.
        if event.what == EventType::Keyboard && event.key_code == KB_ALT_X {
            event = Event::command(CM_QUIT);
        }

        // Mac-friendly Ctrl-equivalents for the F-key hint set.
        // macOS terminals (Terminal.app, iTerm, plus most SSH
        // clients on macOS) intercept F1/F10/F11/F12 for Mission
        // Control, dock zoom, etc., so Shane-on-his-Mac demoing
        // over SSH never actually reaches us. Translating the
        // common Ctrl combos into the same Command events keeps
        // the F-key visual flavor while giving Mac-SSH users a
        // working path. Intercepted *before* the InputLine sees
        // them so e.g. Ctrl-P doesn't type the character.
        //
        // Ctrl-T → menu bar. We synthesize an F10 keypress rather
        // than firing a command because turbo-vision's MenuBar
        // owns its activation logic on KB_F10 — easier to feed it
        // the key it expects than to replicate the cascade dance.
        if event.what == EventType::Keyboard {
            match event.key_code {
                KB_CTRL_H => {
                    event = Event::command(CMD_HELP);
                }
                KB_CTRL_S => {
                    // Mirror F2-style "save current settings".
                    // No `/save` command exists yet, so we
                    // re-persist the active theme — equivalent
                    // to F2 in WordPerfect/Borland editors and
                    // a no-cost confirmation that settings have
                    // been written.
                    event = Event::command(CMD_PERSIST_THEME);
                }
                KB_CTRL_O => {
                    // Mirror F3-style "open" → workspace picker
                    // is the closest existing affordance until
                    // session-open lands as a real picker.
                    event = Event::command(CMD_WORKSPACE_SWITCH);
                }
                KB_CTRL_T => {
                    // Synthesize an F10 keypress so the menu_bar
                    // dispatcher below opens the cascading menu
                    // exactly as if the user had a real F10.
                    event.key_code = KB_F10;
                }
                KB_CTRL_P => {
                    event = Event::command(CMD_PALETTE_NEXT);
                }
                KB_CTRL_D => {
                    // Universal REPL/shell exit — Mac Terminal + iTerm
                    // + every SSH client pass this through cleanly,
                    // unlike Alt-X (needs Meta-key enabled on Mac) or
                    // F10 (Mac intercepts for system functions).
                    event = Event::command(CM_QUIT);
                }
                KB_CTRL_L => {
                    // Force a full repaint. Standard curses/readline
                    // convention. Useful when scrollback / stream
                    // state left artifacts on the canvas.
                    app.terminal.clear();
                    event.what = EventType::Nothing;
                }
                _ => {}
            }
        }

        // Menu bar gets first crack at navigation keys (F10, Alt+F,
        // arrow navigation inside open menus).
        if let Some(menu_bar) = app.menu_bar.as_mut() {
            menu_bar.handle_event(&mut event);
            if event.what == EventType::Keyboard || event.what == EventType::MouseUp {
                if let Some(cmd) = menu_bar.check_cascading_submenu(&mut app.terminal) {
                    if cmd != 0 {
                        event = Event::command(cmd);
                    }
                }
            }
        }

        if let Some(status_line) = app.status_line.as_mut() {
            status_line.handle_event(&mut event);
        }

        // Slash-command popup: `/` on an empty input line matches
        // the v0.3 spec; Ctrl-K remains as a command-palette
        // fallback for users who want to type a literal slash command.
        if event.what == EventType::Keyboard
            && (event.key_code == KB_CTRL_K || event.key_code == KB_SLASH)
            && input_data.borrow().is_empty()
        {
            let selected = run_slash_popup(app);
            if selected != 0 {
                event = Event::command(selected);
            } else {
                event.what = EventType::Nothing;
            }
        }

        // Enter → submit the input line as a turn. Append the user's
        // `> {text}` record plus an empty placeholder line that
        // incoming `TurnChunk::Token` will extend, then hand off to
        // the worker via `blocking_send`. That's safe here because
        // the TV event loop lives on a `spawn_blocking` thread, not
        // on a tokio worker.
        if event.what == EventType::Keyboard && event.key_code == KB_ENTER {
            let submitted = input_data.borrow().clone();
            if !submitted.is_empty() {
                {
                    let mut lines = chat_lines.borrow_mut();
                    lines.push(format!("> {submitted}"));
                    lines.push(String::new());
                }
                // `set_text("")` resets `cursor_pos`, selection, and
                // `first_pos` — safe to use mid-session unlike raw
                // `data.clear()` which would leave `cursor_pos`
                // pointing past the end of an empty string and panic
                // in `String::insert` on the next keystroke.
                input_line.borrow_mut().set_text(String::new());
                // `/theme …` is a TUI-only concern — it toggles the
                // live `TPalette` and has no meaning on the
                // rustyline path. Intercept before routing to the
                // CommandHandler so the worker never sees it.
                if let Some(rest) = submitted.strip_prefix("/theme") {
                    handle_theme_command(
                        rest.trim(),
                        &chat_lines,
                        app,
                        &mut status_state.current_theme,
                        &mut status_state.beep_on_error,
                    );
                    status_state.set_toast(format!("Command: {submitted}"));
                    // `continue` skips desktop.handle_event +
                    // command dispatch for this tick; the Enter
                    // was fully consumed.
                    continue;
                }
                // Slash-prefixed input routes to CommandHandler;
                // everything else submits as an agent turn.
                let is_turn = !submitted.starts_with('/');
                let request = if is_turn {
                    WorkerRequest::Turn(submitted)
                } else {
                    status_state.set_toast(format!("Command: {submitted}"));
                    WorkerRequest::Command(submitted)
                };
                // Only agent turns drive the elapsed counter —
                // slash commands are nearly instantaneous and
                // jittering the timer on each /help would look
                // wrong.
                if is_turn {
                    status_state.begin_turn();
                }
                if let Err(e) = req_tx.blocking_send(request) {
                    chat_lines
                        .borrow_mut()
                        .push(format!("[error] could not dispatch: {e}"));
                    if is_turn {
                        // Undo the timer start; the turn never
                        // made it to the worker.
                        status_state.turn_start = None;
                    }
                }
                // Consume the Enter so the input line itself (which
                // would otherwise ignore it anyway, but be explicit)
                // doesn't also see it.
                event.what = EventType::Nothing;
            }
        }

        // Undo / redo (E-6b). Intercepted before the InputLine sees
        // them so Ctrl-Z / Ctrl-Y don't try to be "typed." Snapshots
        // live on the TV thread since the input buffer is Rc-bound
        // anyway; no locking concerns.
        if event.what == EventType::Keyboard && event.key_code == KB_CTRL_Z {
            apply_undo(&input_line, &input_data, &input_undo, &input_redo);
            // `continue` skips remaining handlers for this tick —
            // no need to mark the event consumed first.
            continue;
        }
        if event.what == EventType::Keyboard && event.key_code == KB_CTRL_Y {
            apply_redo(&input_line, &input_data, &input_undo, &input_redo);
            continue;
        }

        // Wrap InputLine::handle_event in a pre/post snapshot so
        // any text-modifying keystroke (character insert, backspace,
        // delete, selection-replace) pushes the prior text onto the
        // undo stack. Non-modifying events (arrow keys, Home/End,
        // selection-only) leave pre == post and don't grow the
        // stack.
        let pre_text = input_data.borrow().clone();
        input_line.borrow_mut().handle_event(&mut event);
        let post_text = input_data.borrow().clone();
        if pre_text != post_text {
            push_undo_snapshot(&input_undo, pre_text);
            // Fresh input invalidates the redo stack — the classic
            // "branch on edit" semantics users expect.
            input_redo.borrow_mut().clear();
        }

        app.desktop.handle_event(&mut event);

        if event.what == EventType::Command {
            redraw(app, &input_line);
            handle_command(
                app,
                event.command,
                &chat_lines,
                &req_tx,
                shared_app,
                status_state,
            );
        }
    }

    Ok(())
}

/// Build and run the slash-command popup as a modal MenuBox.
///
/// Returns the selected command ID (0 on Esc/cancel). Positions the
/// popup near the center of the terminal, offset up by a couple of
/// rows so the input line stays visible underneath — matches the
/// Paradox/dBASE-style "pinned floating picker" feel. Shadows are
/// on by default for MenuBox, which fits the Borland modal idiom.
fn run_slash_popup(app: &mut Application) -> u16 {
    use turbo_vision::core::geometry::Point;

    let mut items = vec![
        MenuItem::with_shortcut("~H~elp — slash command reference", CMD_HELP, 0, "/help", 0),
        MenuItem::with_shortcut("~I~nfo — current session details", CMD_ABOUT, 0, "/info", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut(
            "~W~orkspace list",
            CMD_WORKSPACE_LIST,
            0,
            "/workspace list",
            0,
        ),
        MenuItem::with_shortcut(
            "Workspace ~i~nfo",
            CMD_WORKSPACE_INFO,
            0,
            "/workspace info",
            0,
        ),
        MenuItem::with_shortcut(
            "Workspace ~s~witch…",
            CMD_WORKSPACE_SWITCH,
            0,
            "/workspace switch",
            0,
        ),
        MenuItem::with_shortcut("~M~odel list", CMD_MODELS_LIST, 0, "/models list", 0),
        MenuItem::with_shortcut("Model s~t~atus", CMD_MODELS_STATUS, 0, "/models status", 0),
        MenuItem::with_shortcut("~P~roviders list", CMD_PROVIDERS_LIST, 0, "/providers", 0),
        MenuItem::with_shortcut("Memor~y~ recent", CMD_MEMORY_SEARCH, 0, "/memory list", 0),
        MenuItem::with_shortcut("Memory s~t~ats", CMD_MEMORY_STATS, 0, "/memory stats", 0),
        MenuItem::with_shortcut("~M~CP status", CMD_MCP_STATUS, 0, "/mcp status", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut("Session ~l~ist", CMD_SESSION_LIST, 0, "/session list", 0),
        MenuItem::with_shortcut("Session ~n~ew", CMD_SESSION_NEW, 0, "/session <new>", 0),
        MenuItem::with_shortcut("Session ~o~pen", CMD_SESSION_OPEN, 0, "/session open", 0),
        MenuItem::separator(),
    ];
    // Theme presets — each carries an index-encoded command id so
    // `handle_command` can map the selection back to a
    // `themes::PRESETS[i]` entry without growing a const per
    // preset.
    for (i, theme) in themes::PRESETS.iter().enumerate() {
        let label = format!("Theme: {:<8} {}", theme.name, theme.display_name);
        items.push(MenuItem::with_shortcut(
            &label,
            CMD_THEME_BASE + i as u16,
            0,
            &format!("/theme {}", theme.name),
            0,
        ));
    }

    let (tw, th) = app.terminal.size();
    let w = tw;
    let h = th;
    // MenuBox auto-sizes width from content. Place it roughly
    // centered horizontally and one-third from the top.
    let position = Point::new((w / 2) - 20, (h / 3).max(3));

    let menu = turbo_vision::core::menu_data::Menu::from_items(items);
    let mut menu_box = MenuBox::new(position, menu);
    // The MenuBox leaves the terminal dirty under its footprint;
    // the outer event loop's next `redraw()` tick will repaint the
    // desktop, chat, input line, menu bar, and status line in the
    // right order, so there's nothing to clean up here.
    menu_box.execute(&mut app.terminal)
}

/// Non-blocking drain of the worker → TUI event channel. Called at
/// the top of every event-loop tick so streamed tokens land in the
/// chat pane even when the user isn't pressing keys. Also observes
/// `Finished` frames to stop the elapsed-turn timer so the status
/// line freezes at the final duration instead of ticking forever.
fn drain_stream_events(
    event_rx: &mut mpsc::UnboundedReceiver<TurnChunk>,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    status_state: &mut StatusState,
    typewriter_state: &mut Option<TypewriterState>,
) -> bool {
    let mut saw_error = false;
    loop {
        match event_rx.try_recv() {
            Ok(chunk) => {
                match &chunk {
                    TurnChunk::Usage(usage) => {
                        status_state.usage = Some(*usage);
                    }
                    TurnChunk::Finished(result) => {
                        if result.is_err() {
                            saw_error = true;
                        }
                        status_state.end_turn();
                    }
                    TurnChunk::Typewriter(text) => {
                        start_typewriter(typewriter_state, chat_lines, text.clone(), Vec::new());
                    }
                    TurnChunk::Token(_) => {}
                }
                apply_chunk(chunk, chat_lines);
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            // Worker dropped — either shutting down or crashed. Either
            // way there's nothing productive to do in the UI loop
            // beyond letting Alt-X exit naturally.
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    saw_error
}

/// Apply a single streamed chunk to the chat buffer.
///
/// Tokens append to the *current* response line (the empty string
/// pushed when the user hit Enter). Embedded newlines split into new
/// buffer lines. `Finished(Ok(_))` just emits a blank separator;
/// `Finished(Err(_))` appends an `[error]` line with the message.
fn apply_chunk(chunk: TurnChunk, chat_lines: &Rc<RefCell<Vec<String>>>) {
    let mut lines = chat_lines.borrow_mut();
    match chunk {
        TurnChunk::Token(s) => {
            let mut parts = s.split('\n');
            if let Some(first) = parts.next() {
                if let Some(last) = lines.last_mut() {
                    last.push_str(first);
                } else {
                    lines.push(first.to_string());
                }
            }
            for piece in parts {
                lines.push(piece.to_string());
            }
        }
        TurnChunk::Typewriter(_) => {}
        TurnChunk::Usage(_) => {}
        TurnChunk::Finished(Ok(_)) => {
            lines.push(String::new());
        }
        TurnChunk::Finished(Err(e)) => {
            lines.push(format!("[error] {e}"));
            lines.push(String::new());
        }
    }
}

fn redraw(app: &mut Application, input_line: &Rc<RefCell<InputLine>>) {
    app.desktop.draw(&mut app.terminal);
    if let Some(menu_bar) = app.menu_bar.as_mut() {
        menu_bar.draw(&mut app.terminal);
    }
    if let Some(status_line) = app.status_line.as_mut() {
        status_line.draw(&mut app.terminal);
    }
    // InputLine draws last so it overlays the window frame's bottom
    // row. Its bounds were picked to match where a win.add'd input
    // line would have landed, so the visual effect is identical.
    input_line.borrow_mut().draw(&mut app.terminal);
    let _ = app.terminal.flush();
}

fn handle_command(
    app: &mut Application,
    command: u16,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    shared_app: &Arc<Mutex<App>>,
    status_state: &mut StatusState,
) {
    match command {
        CM_QUIT => {
            app.running = false;
        }
        // Ctrl-P: cycle to the next palette preset, apply it
        // live, persist to ~/.zterm/theme.toml, and post a
        // ~1s toast in the status summary so the user sees
        // which one is now active without guessing.
        CMD_PALETTE_NEXT => {
            let next = themes::next_preset(&status_state.current_theme);
            app.set_palette(Some(next.palette.to_vec()));
            status_state.current_theme = next.name.to_string();
            // Persist; failures are advisory — the live
            // change still happened, the user just won't
            // get it back on next launch.
            let persist_note = match themes::save_preset(next.name) {
                Ok(_) => String::new(),
                Err(e) => format!(" (not persisted: {e})"),
            };
            status_state.set_toast(format!(
                "🎨 Palette: {} — {}{}",
                next.name, next.display_name, persist_note
            ));
        }
        // Ctrl-S: re-write the current theme to disk.
        // Confirms to the user that settings are saved
        // without changing what's on screen.
        CMD_PERSIST_THEME => {
            let name = status_state.current_theme.clone();
            match themes::save_preset(&name) {
                Ok(_) => {
                    status_state.set_toast(format!("💾 Saved: theme = {name}"));
                }
                Err(e) => {
                    status_state.set_toast(format!("💾 Save failed: {e}"));
                }
            }
        }
        // Commands whose CommandHandler implementations return
        // `Ok(Some(String))` route cleanly through the worker and
        // append into the chat pane.
        CMD_HELP | CMD_ABOUT | CMD_WORKSPACE_LIST | CMD_WORKSPACE_INFO | CMD_MODELS_LIST
        | CMD_MODELS_STATUS | CMD_PROVIDERS_LIST | CMD_MEMORY_SEARCH | CMD_MEMORY_STATS
        | CMD_MCP_STATUS | CMD_SESSION_LIST => {
            let cmdline = match command {
                CMD_HELP => "/help",
                CMD_ABOUT => "/info",
                CMD_WORKSPACE_LIST => "/workspace list",
                CMD_WORKSPACE_INFO => "/workspace info",
                CMD_MODELS_LIST => "/models list",
                CMD_MODELS_STATUS => "/models status",
                CMD_PROVIDERS_LIST => "/providers",
                CMD_MEMORY_SEARCH => "/memory list",
                CMD_MEMORY_STATS => "/memory stats",
                CMD_MCP_STATUS => "/mcp status",
                CMD_SESSION_LIST => "/session list",
                _ => return,
            };
            dispatch_command(cmdline, chat_lines, req_tx, status_state);
        }
        // E-5: Workspace switch opens a modal picker populated from
        // the current App state. On selection, dispatch
        // `/workspace switch <name>` through the worker — the live
        // status-line read (E-4) picks up the new workspace on the
        // next tick.
        CMD_WORKSPACE_SWITCH => {
            let workspaces = snapshot_workspace_names(shared_app);
            if workspaces.is_empty() {
                chat_lines.borrow_mut().push(
                    "[workspace] no workspaces configured — add [[workspaces]] \
                     entries to ~/.zterm/config.toml"
                        .to_string(),
                );
                chat_lines.borrow_mut().push(String::new());
                return;
            }
            if let Some(selected_name) = run_workspace_picker(app, &workspaces) {
                let cmdline = format!("/workspace switch {selected_name}");
                dispatch_command(&cmdline, chat_lines, req_tx, status_state);
            }
        }
        // Theme preset slots from the slash popup. Map the id back
        // to a `themes::PRESETS` entry and apply via
        // `handle_theme_command` so the rendering path stays in
        // one place.
        cmd if (CMD_THEME_BASE..CMD_THEME_BASE + themes::PRESETS.len() as u16).contains(&cmd) => {
            let idx = (cmd - CMD_THEME_BASE) as usize;
            if let Some(theme) = themes::PRESETS.get(idx) {
                handle_theme_command(
                    theme.name,
                    chat_lines,
                    app,
                    &mut status_state.current_theme,
                    &mut status_state.beep_on_error,
                );
                status_state.set_toast(format!("Command: /theme {}", theme.name));
            }
        }
        CMD_SESSION_NEW => {
            let name = format!("session-{}", Utc::now().format("%Y%m%d-%H%M%S"));
            let cmdline = format!("/session {name}");
            dispatch_command(&cmdline, chat_lines, req_tx, status_state);
        }
        CMD_SESSION_OPEN => {
            let sessions = snapshot_sessions();
            if sessions.is_empty() {
                dispatch_command("/session list", chat_lines, req_tx, status_state);
                return;
            }
            if let Some(selected_name) = run_session_picker(app, &sessions) {
                let cmdline = format!("/session {selected_name}");
                dispatch_command(&cmdline, chat_lines, req_tx, status_state);
            }
        }
        _ => {}
    }
}

/// Push `snapshot` onto the undo ring, evicting the oldest entry
/// when the ring is full. Duplicate consecutive snapshots are
/// collapsed — InputLine sometimes fires modifying events that
/// produce identical text (e.g. typing past `max_length`).
fn push_undo_snapshot(input_undo: &Rc<RefCell<VecDeque<String>>>, snapshot: String) {
    let mut stack = input_undo.borrow_mut();
    if stack.back().map(|s| s == &snapshot).unwrap_or(false) {
        return;
    }
    if stack.len() >= INPUT_UNDO_DEPTH {
        stack.pop_front();
    }
    stack.push_back(snapshot);
}

/// Ctrl-Z handler: pop the top of the undo ring, push the *current*
/// text onto the redo ring, and `set_text` the recovered snapshot
/// onto the InputLine. `set_text` resets `cursor_pos` / selection
/// (the same reason we couldn't use raw `data.clear()` for
/// mid-session clears).
fn apply_undo(
    input_line: &Rc<RefCell<InputLine>>,
    input_data: &Rc<RefCell<String>>,
    input_undo: &Rc<RefCell<VecDeque<String>>>,
    input_redo: &Rc<RefCell<VecDeque<String>>>,
) {
    let Some(prev) = input_undo.borrow_mut().pop_back() else {
        return;
    };
    let current = input_data.borrow().clone();
    {
        let mut redo = input_redo.borrow_mut();
        if redo.len() >= INPUT_UNDO_DEPTH {
            redo.pop_front();
        }
        redo.push_back(current);
    }
    input_line.borrow_mut().set_text(prev);
}

/// Ctrl-Y handler: pop the top of the redo ring, push the *current*
/// text onto the undo ring, apply the recovered snapshot.
fn apply_redo(
    input_line: &Rc<RefCell<InputLine>>,
    input_data: &Rc<RefCell<String>>,
    input_undo: &Rc<RefCell<VecDeque<String>>>,
    input_redo: &Rc<RefCell<VecDeque<String>>>,
) {
    let Some(next) = input_redo.borrow_mut().pop_back() else {
        return;
    };
    let current = input_data.borrow().clone();
    {
        let mut undo = input_undo.borrow_mut();
        if undo.len() >= INPUT_UNDO_DEPTH {
            undo.pop_front();
        }
        undo.push_back(current);
    }
    input_line.borrow_mut().set_text(next);
}

/// Handle a `/theme …` command locally. Supports:
///
/// - `/theme` or `/theme list` — enumerate available presets in
///   the chat pane.
/// - `/theme <name>` — apply the named preset via
///   `Application::set_palette`. The next event-loop tick redraws
///   with the new palette.
///
/// Unknown names produce a friendly `[theme] …` error line and
/// leave the current palette untouched. Theme selection isn't
/// persisted in E-6 — E-8 picks that up alongside the custom
/// color editor.
fn handle_theme_command(
    rest: &str,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    app: &mut Application,
    current_theme: &mut String,
    beep_on_error: &mut bool,
) {
    let arg = rest.trim();
    if let Some(enabled) = parse_beep_toggle(arg) {
        let mut lines = chat_lines.borrow_mut();
        match delighters::set_beep_on_error(enabled) {
            Ok(_) => {
                *beep_on_error = enabled;
                lines.push(format!(
                    "🔔 theme beep: {}",
                    if enabled { "on" } else { "off" }
                ));
            }
            Err(e) => {
                lines.push(format!("[theme] beep setting not persisted: {e}"));
            }
        }
        lines.push(String::new());
        return;
    }
    match arg {
        "" | "list" => {
            let mut lines = chat_lines.borrow_mut();
            lines.push("Available themes:".to_string());
            for theme in themes::PRESETS {
                lines.push(format!("  {:<8} {}", theme.name, theme.display_name));
            }
            lines.push(
                "Apply via `/theme <name>`; edit with `/theme edit`; toggle bell with \
                 `/theme beep on|off`."
                    .to_string(),
            );
            lines.push(String::new());
        }
        "edit" => {
            let snapshot = turbo_vision::core::palette::palettes::get_app_palette();
            let initial: [u8; 63] = match snapshot.as_slice().try_into() {
                Ok(arr) => arr,
                Err(_) => *themes::DEFAULT.palette,
            };
            match run_theme_editor(app, initial) {
                Some(final_palette) => {
                    // Already applied live during editing; commit
                    // to disk as a `custom` payload.
                    *current_theme = "custom".to_string();
                    let mut lines = chat_lines.borrow_mut();
                    match themes::save_custom(&final_palette) {
                        Ok(_) => {
                            lines.push(
                                "🎨 theme: custom (saved to ~/.zterm/theme.toml)".to_string(),
                            );
                        }
                        Err(e) => {
                            lines.push(format!("🎨 theme: custom (applied but not saved: {e})"));
                        }
                    }
                    lines.push(String::new());
                }
                None => {
                    // Cancel path — restore the snapshot taken on
                    // editor entry so the user sees no change.
                    app.set_palette(Some(snapshot));
                    let mut lines = chat_lines.borrow_mut();
                    lines.push("🎨 theme edit cancelled — palette reverted".to_string());
                    lines.push(String::new());
                }
            }
        }
        name => match themes::find(name) {
            Some(theme) => {
                app.set_palette(Some(theme.palette.to_vec()));
                *current_theme = theme.name.to_string();
                let mut lines = chat_lines.borrow_mut();
                lines.push(format!("🎨 theme: {} ({})", theme.name, theme.display_name));
                // Persist so the choice survives a restart. IO
                // errors get surfaced as a chat advisory but don't
                // roll back the palette — the user still gets
                // their visual change this session.
                if let Err(e) = themes::save_preset(theme.name) {
                    lines.push(format!("[theme] (not persisted: {e})"));
                }
                lines.push(String::new());
            }
            None => {
                let known: Vec<&str> = themes::PRESETS.iter().map(|t| t.name).collect();
                let mut lines = chat_lines.borrow_mut();
                lines.push(format!(
                    "[theme] unknown theme `{name}` (try: {})",
                    known.join(", ")
                ));
                lines.push(String::new());
            }
        },
    }
}

/// Common helper: push a `> {cmdline}` marker + empty placeholder
/// line into the chat buffer, then hand the command off to the
/// worker. Keeps the menu-bar and popup dispatch paths symmetric
/// with Enter-on-`/…`-text input.
fn dispatch_command(
    cmdline: &str,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    status_state: &mut StatusState,
) {
    status_state.set_toast(format!("Command: {cmdline}"));
    {
        let mut lines = chat_lines.borrow_mut();
        lines.push(format!("> {cmdline}"));
        lines.push(String::new());
    }
    if let Err(e) = req_tx.blocking_send(WorkerRequest::Command(cmdline.to_string())) {
        chat_lines
            .borrow_mut()
            .push(format!("[error] could not dispatch command: {e}"));
    }
}

/// Read a snapshot of configured workspaces from the shared App.
///
/// Captures name, optional label, and active flag for the workspace-switch
/// picker. Uses `try_lock` so a stalled worker cannot hang the TV thread;
/// returns an empty vec in that case.
fn snapshot_workspace_names(shared_app: &Arc<Mutex<App>>) -> Vec<WorkspacePickerEntry> {
    let Ok(guard) = shared_app.try_lock() else {
        return Vec::new();
    };
    guard
        .workspaces
        .iter()
        .enumerate()
        .map(|(i, w)| WorkspacePickerEntry {
            name: w.config.name.clone(),
            label: w.config.display_label().to_string(),
            backend: w.config.backend.as_str().to_string(),
            active: i == guard.active,
        })
        .collect()
}

#[derive(Debug, Clone)]
struct WorkspacePickerEntry {
    name: String,
    label: String,
    backend: String,
    active: bool,
}

#[derive(Debug, Clone)]
struct SessionPickerEntry {
    name: String,
    id: String,
    model: String,
    last_active: String,
}

fn snapshot_sessions() -> Vec<SessionPickerEntry> {
    storage::list_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| SessionPickerEntry {
            name: s.name,
            id: s.id,
            model: format!("{}/{}", s.provider, s.model),
            last_active: s.last_active,
        })
        .collect()
}

/// Theme color editor modal (E-8b MVP).
///
/// Presents a small framed dialog stuck at the top-right corner so
/// the user can see their edits reflected in the workspace frame
/// and chat pane underneath. The editor state is a 63-byte palette
/// plus a focused index. Keys:
///
/// - `←` / `→` — previous / next index (wraps)
/// - `↑` / `↓` — cycle foreground color (wraps 0..=15)
/// - `PgUp` / `PgDn` — cycle background color (wraps 0..=15)
/// - `Enter` — commit (persist + return the palette)
/// - `Esc` — cancel (caller restores the entry snapshot)
///
/// Every byte change re-applies the whole palette via
/// `Application::set_palette`, so the rest of the UI repaints with
/// the new colors on the next event-loop tick. The editor's own
/// frame redraws every tick so it itself is also affected by the
/// user's choices — a useful sanity check that they haven't
/// accidentally made the dialog frame invisible.
fn run_theme_editor(app: &mut Application, initial: [u8; 63]) -> Option<[u8; 63]> {
    let mut palette = initial;
    let mut idx: usize = 0;

    // Apply once up front so the editor starts from a known state
    // even if the caller hadn't yet committed `initial`.
    app.set_palette(Some(palette.to_vec()));

    loop {
        // Rebuild the editor frame + labels before polling so the
        // user sees their edits immediately. We draw directly onto
        // the terminal — no Desktop/Group involved, which keeps
        // coordinate math trivial.
        draw_theme_editor(app, &palette, idx);
        let _ = app.terminal.flush();

        let Ok(Some(event)) = app.terminal.poll_event(Duration::from_millis(50)) else {
            continue;
        };

        if event.what != EventType::Keyboard {
            continue;
        }

        match event.key_code {
            KB_ESC => return None,
            KB_ENTER => return Some(palette),
            KB_LEFT => idx = if idx == 0 { 62 } else { idx - 1 },
            KB_RIGHT => idx = (idx + 1) % 63,
            KB_UP => {
                let (fg, bg) = split_attr(palette[idx]);
                palette[idx] = join_attr((fg + 1) & 0x0F, bg);
                app.set_palette(Some(palette.to_vec()));
            }
            KB_DOWN => {
                let (fg, bg) = split_attr(palette[idx]);
                palette[idx] = join_attr((fg + 15) & 0x0F, bg);
                app.set_palette(Some(palette.to_vec()));
            }
            0x4900 => {
                // KB_PGUP
                let (fg, bg) = split_attr(palette[idx]);
                palette[idx] = join_attr(fg, (bg + 1) & 0x0F);
                app.set_palette(Some(palette.to_vec()));
            }
            0x5100 => {
                // KB_PGDN
                let (fg, bg) = split_attr(palette[idx]);
                palette[idx] = join_attr(fg, (bg + 15) & 0x0F);
                app.set_palette(Some(palette.to_vec()));
            }
            _ => {}
        }
    }
}

fn split_attr(byte: u8) -> (u8, u8) {
    (byte & 0x0F, (byte >> 4) & 0x0F)
}

fn join_attr(fg: u8, bg: u8) -> u8 {
    (fg & 0x0F) | ((bg & 0x0F) << 4)
}

fn color_name(idx: u8) -> &'static str {
    match idx & 0x0F {
        0 => "Black",
        1 => "Blue",
        2 => "Green",
        3 => "Cyan",
        4 => "Red",
        5 => "Magenta",
        6 => "Brown",
        7 => "LightGray",
        8 => "DarkGray",
        9 => "LightBlue",
        10 => "LightGreen",
        11 => "LightCyan",
        12 => "LightRed",
        13 => "LightMagenta",
        14 => "Yellow",
        15 => "White",
        _ => unreachable!(),
    }
}

/// Paint the editor frame + current-index readout in the top-right
/// corner. The dialog is deliberately small so the user can see
/// their palette changes reflected in the big window/desktop/chat
/// area underneath.
fn draw_theme_editor(app: &mut Application, palette: &[u8; 63], idx: usize) {
    use turbo_vision::core::draw::DrawBuffer;
    use turbo_vision::core::palette::{Attr, TvColor};
    use turbo_vision::views::view::write_line_to_terminal;

    let (tw, _th) = app.terminal.size();
    let w = tw;
    let dx: i16 = w - 52; // top-right corner with a small right margin
    let dy: i16 = 2;
    let dw: i16 = 50;
    let dh: i16 = 6;

    let normal = Attr::new(TvColor::White, TvColor::Blue);
    let accent = Attr::new(TvColor::Yellow, TvColor::Blue);

    for row in 0..dh {
        let mut buf = DrawBuffer::new(dw as usize);
        buf.move_char(0, ' ', normal, dw as usize);
        let line: String = match row {
            0 => format!(" 🎨 Theme color editor — index {idx:2} / 62 "),
            1 => format!(
                "   fg: {:>14}   bg: {:>14}      ",
                color_name(palette[idx] & 0x0F),
                color_name((palette[idx] >> 4) & 0x0F)
            ),
            2 => "   ← →  index     ↑ ↓  fg     PgUp/Dn  bg     ".to_string(),
            3 => "   Enter  apply + save as custom               ".to_string(),
            4 => "   Esc    cancel and restore                   ".to_string(),
            _ => String::new(),
        };
        let truncated: String = line.chars().take(dw as usize).collect();
        let attr = if row == 0 { accent } else { normal };
        buf.move_str(0, &truncated, attr);
        write_line_to_terminal(&mut app.terminal, dx, dy + row, &buf);
    }
}

/// Present a MenuBox-style modal picker of workspaces.
///
/// Each entry's MenuItem carries an index-encoded command ID
/// (`CMD_WS_SELECT_BASE + i`) so the caller can map the return value back to a
/// workspace name. Returns the selected workspace name, or `None` on Esc/cancel.
fn run_workspace_picker(app: &mut Application, entries: &[WorkspacePickerEntry]) -> Option<String> {
    use turbo_vision::core::geometry::Point;

    const CMD_WS_SELECT_BASE: u16 = 1100;

    let items: Vec<MenuItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            // Each entry gets a unique slot command in the
            // [1100, 1200) range. We don't expose these outside of
            // this picker — they're immediately mapped back to a
            // workspace name after `execute` returns.
            let active_marker = if e.active { "●" } else { " " };
            let label = format!(
                " {} {}  [{}]  {}",
                active_marker, e.name, e.backend, e.label
            );
            MenuItem::with_shortcut(&label, CMD_WS_SELECT_BASE + i as u16, 0, "", 0)
        })
        .collect();

    let (tw, th) = app.terminal.size();
    let w = tw;
    let h = th;
    // Narrower and lower than the slash popup — this one hangs
    // just below the menu bar to feel like a drop-down rather
    // than a centered decision dialog.
    let position = Point::new((w / 2) - 24, (h / 3).max(3));

    let menu = turbo_vision::core::menu_data::Menu::from_items(items);
    let mut menu_box = MenuBox::new(position, menu);
    let selected = menu_box.execute(&mut app.terminal);

    if selected == 0 {
        return None;
    }
    let idx = selected.checked_sub(CMD_WS_SELECT_BASE)? as usize;
    entries.get(idx).map(|e| e.name.clone())
}

/// Present a modal picker of locally-known sessions.
///
/// This intentionally reuses `MenuBox`, matching the existing
/// workspace picker. It dispatches back to `/session <name>` so
/// session behavior remains owned by `CommandHandler`.
fn run_session_picker(app: &mut Application, entries: &[SessionPickerEntry]) -> Option<String> {
    use turbo_vision::core::geometry::Point;

    const CMD_SESSION_SELECT_BASE: u16 = 1400;

    let items: Vec<MenuItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let short_id = &e.id[..8.min(e.id.len())];
            let last = &e.last_active[..10.min(e.last_active.len())];
            let label = format!(" {}  ({})  {}  {}", e.name, short_id, e.model, last);
            MenuItem::with_shortcut(&label, CMD_SESSION_SELECT_BASE + i as u16, 0, "", 0)
        })
        .collect();

    let (tw, th) = app.terminal.size();
    let position = Point::new((tw / 2) - 30, (th / 3).max(3));
    let menu = turbo_vision::core::menu_data::Menu::from_items(items);
    let mut menu_box = MenuBox::new(position, menu);
    let selected = menu_box.execute(&mut app.terminal);

    if selected == 0 {
        return None;
    }
    let idx = selected.checked_sub(CMD_SESSION_SELECT_BASE)? as usize;
    entries.get(idx).map(|e| e.id.clone())
}

/// Custom read-only scrolling text view over a shared `Vec<String>`
/// buffer. Purposely narrow for E-1 — no scrollbars, no selection,
/// no word wrap beyond hard truncation. E-2 replaces this with a
/// proper streaming buffer driven by `TApplication::put_event`.
struct ChatPane {
    bounds: Rect,
    state: StateFlags,
    lines: Rc<RefCell<Vec<String>>>,
}

impl ChatPane {
    fn new(bounds: Rect, lines: Rc<RefCell<Vec<String>>>) -> Self {
        Self {
            bounds,
            state: 0,
            lines,
        }
    }
}

impl View for ChatPane {
    fn bounds(&self) -> Rect {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }

    fn state(&self) -> StateFlags {
        self.state
    }

    fn set_state(&mut self, state: StateFlags) {
        self.state = state;
    }

    fn draw(&mut self, terminal: &mut Terminal) {
        let width = (self.bounds.b.x - self.bounds.a.x).max(0) as usize;
        let height = (self.bounds.b.y - self.bounds.a.y).max(0) as usize;
        if width == 0 || height == 0 {
            return;
        }

        // Pull the window-body color live from the application
        // palette so theme switches (E-6) and custom editors (E-8b)
        // flow through here automatically. Borland's blue-window
        // scroller slot is index 13 (`CP_BLUE_WINDOW` maps 13 → app
        // palette 13, which is the "scroller normal" cell in
        // `CP_APP_COLOR`). Falling back to LightGray-on-Blue if the
        // palette byte is invalid keeps the UI readable when
        // someone hand-edits an empty `theme.toml`.
        let app_palette = turbo_vision::core::palette::palettes::get_app_palette();
        let attr = app_palette
            .get(12)
            .copied()
            .filter(|&b| b != 0)
            .map(Attr::from_u8)
            .unwrap_or_else(|| Attr::new(TvColor::LightGray, TvColor::Blue));

        let lines = self.lines.borrow();
        // Show the tail of the buffer that fits in `height` rows.
        let start = lines.len().saturating_sub(height);
        let visible = &lines[start..];

        for row in 0..height {
            let mut buf = DrawBuffer::new(width);
            buf.move_char(0, ' ', attr, width);
            if let Some(line) = visible.get(row) {
                let truncated: String = line.chars().take(width).collect();
                buf.move_str(0, &truncated, attr);
            }
            write_line_to_terminal(
                terminal,
                self.bounds.a.x,
                self.bounds.a.y + row as i16,
                &buf,
            );
        }
    }

    fn handle_event(&mut self, _event: &mut Event) {
        // Read-only pane — no input handling yet. Scrollback
        // bindings (PgUp/PgDn/Home/End) arrive in E-2.
    }

    fn get_palette(&self) -> Option<Palette> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_bar_clamps_and_sizes() {
        assert_eq!(token_budget_bar(0, 10), "[----------]");
        assert_eq!(token_budget_bar(25, 10), "[###-------]");
        assert_eq!(token_budget_bar(150, 10), "[##########]");
    }

    #[test]
    fn ctx_usage_renders_used_total_pct_and_bar() {
        let usage = TurnUsage {
            total_tokens: Some(2_000),
            context_window: Some(8_000),
            ..Default::default()
        };

        assert_eq!(
            render_ctx_usage(Some(usage)),
            "ctx 2000/8000 (25%) [###-------]"
        );
    }

    #[test]
    fn spinner_advances_only_after_interval_and_wraps() {
        assert!(!should_advance_spinner(
            SPINNER_INTERVAL - Duration::from_millis(1)
        ));
        assert!(should_advance_spinner(SPINNER_INTERVAL));
        assert_eq!(next_spinner_frame(0, SPINNER_FRAMES.len()), 1);
        assert_eq!(
            next_spinner_frame(SPINNER_FRAMES.len() - 1, SPINNER_FRAMES.len()),
            0
        );
    }

    #[test]
    fn typewriter_due_uses_thirty_ms_cadence() {
        assert_eq!(
            typewriter_chars_due(Duration::from_millis(29), TYPEWRITER_INTERVAL),
            0
        );
        assert_eq!(
            typewriter_chars_due(Duration::from_millis(30), TYPEWRITER_INTERVAL),
            1
        );
        assert_eq!(
            typewriter_chars_due(Duration::from_millis(95), TYPEWRITER_INTERVAL),
            3
        );
    }

    #[test]
    fn parses_theme_beep_toggle() {
        assert_eq!(parse_beep_toggle("beep on"), Some(true));
        assert_eq!(parse_beep_toggle("BEEP OFF"), Some(false));
        assert_eq!(parse_beep_toggle("amber"), None);
    }

    #[test]
    fn session_switch_target_only_matches_real_switches() {
        assert_eq!(session_switch_target("/session research"), Some("research"));
        assert_eq!(
            session_switch_target("/session switch research"),
            Some("research")
        );
        assert_eq!(
            session_switch_target("/session create scratch"),
            Some("scratch")
        );
        assert_eq!(session_switch_target("/session list"), None);
        assert_eq!(session_switch_target("/session info"), None);
        assert_eq!(session_switch_target("/workspace switch prod"), None);
    }

    #[test]
    fn workspace_switch_detection_requires_target() {
        assert!(is_workspace_switch_command("/workspace switch prod"));
        assert!(is_workspace_switch_command("/workspaces switch prod"));
        assert!(!is_workspace_switch_command("/workspace switch"));
        assert!(!is_workspace_switch_command("/workspace list"));
    }
}
