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
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
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
    Event, EventType, KB_ALT_F, KB_ALT_H, KB_ALT_M, KB_ALT_S, KB_ALT_W, KB_ALT_X, KB_CTRL_H,
    KB_CTRL_O, KB_CTRL_P, KB_CTRL_S, KB_CTRL_T, KB_CTRL_Y, KB_CTRL_Z, KB_DOWN, KB_ENTER, KB_ESC,
    KB_F1, KB_F10, KB_LEFT, KB_RIGHT, KB_UP, MB_LEFT_BUTTON,
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

use crate::cli::agent::{
    SessionPickerListResult, SessionPickerWorkspace, StreamSink, TurnChunk, TurnUsage,
};
use crate::cli::client::Session;
use crate::cli::commands::{tokenize_slash_command, CommandHandler};
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
    /// `Ok(Some(text))` output is forwarded to the chat pane.
    Command(String),
    /// Fetch backend sessions for the modal picker on the async
    /// worker path. The sync TUI thread must not call backend I/O.
    SessionPickerList(SessionPickerWorkspace),
}

#[derive(Debug, PartialEq, Eq)]
enum SubmissionStatus {
    Started,
    Busy,
    DispatchFailed,
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
/// ASCII character with no modifiers, so `c as u16` = 0x2F. Plain
/// slash is intentionally regular text input; the command popup is
/// reserved for Ctrl-K/menu paths so arbitrary slash commands remain
/// typeable.
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
const TURN_FORWARD_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const SESSION_PICKER_LIST_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_BUSY_TOAST: &str = "Busy: response in progress";
const UI_EVENT_CAPACITY: usize = 512;
const TURN_STREAM_CAPACITY: usize = 128;
const TURN_TOKEN_COALESCE_BYTES: usize = 4096;
const TURN_STREAM_MAX_BYTES: usize = 2 * 1024 * 1024;

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
    current_line: usize,
    completed: bool,
}

impl TypewriterState {
    fn new(text: impl Into<String>, after_lines: Vec<String>, current_line: usize) -> Self {
        Self {
            chars: sanitize_terminal_text(&text.into()).chars().collect(),
            pos: 0,
            last_emit: Instant::now(),
            after_lines: after_lines
                .into_iter()
                .map(|line| sanitize_terminal_text(&line))
                .collect(),
            current_line,
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
        if self.current_line > lines.len() {
            self.current_line = lines.len();
        }
        if self.current_line == lines.len() {
            lines.push(String::new());
        }

        for _ in 0..due {
            let Some(ch) = self.chars.get(self.pos).copied() else {
                self.completed = true;
                self.current_line += 1;
                lines.insert(self.current_line, String::new());
                for line in self.after_lines.drain(..) {
                    self.current_line += 1;
                    lines.insert(self.current_line, line);
                }
                return;
            };
            self.pos += 1;
            self.last_emit += TYPEWRITER_INTERVAL;
            if ch == '\n' {
                self.current_line += 1;
                lines.insert(self.current_line, String::new());
            } else if let Some(line) = lines.get_mut(self.current_line) {
                line.push(ch);
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
    let current_line = {
        let mut lines = lines.borrow_mut();
        let current_line = lines.len();
        lines.push(String::new());
        current_line
    };
    *state = Some(TypewriterState::new(text, after_lines, current_line));
}

fn typewriter_chars_due(elapsed: Duration, interval: Duration) -> usize {
    if interval.is_zero() {
        return usize::MAX;
    }
    (elapsed.as_millis() / interval.as_millis()) as usize
}

fn sanitize_terminal_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' => out.push('\n'),
            '\u{1b}' => out.push_str("<ESC>"),
            '\u{7f}' => out.push_str("^?"),
            '\u{00}'..='\u{1f}' => {
                out.push('^');
                out.push(((ch as u8) + b'@') as char);
            }
            '\u{80}'..='\u{9f}' => out.push_str(&format!("<0x{:02X}>", ch as u32)),
            _ if ch.is_control() => out.push_str(&format!("<U+{:04X}>", ch as u32)),
            _ => out.push(ch),
        }
    }
    out
}

/// Live state feeding the status line. Mutated on each event-loop
/// tick from the TV thread.
struct StatusState {
    /// Last known active workspace. Refreshed each tick by
    /// try-locking the shared `App`; falls back to this cached
    /// value when the worker is holding the mutex.
    workspace: String,
    /// Stable workspace id from config when available. The display
    /// name is still the primary status text, but picker caches use
    /// this alongside the name to reject stale async replies.
    workspace_id: Option<String>,
    /// Last known active model label. Refreshed through lightweight
    /// `TurnChunk::Status` frames after command-driven context
    /// changes.
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
            workspace_id: None,
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
        self.begin_busy(true);
    }

    fn begin_busy(&mut self, clear_usage: bool) {
        self.turn_start = Some(Instant::now());
        self.frozen_elapsed = None;
        if clear_usage {
            self.clear_usage();
        }
    }

    fn end_turn(&mut self) {
        if let Some(start) = self.turn_start.take() {
            self.frozen_elapsed = Some(start.elapsed());
        }
    }

    fn clear_usage(&mut self) {
        self.usage = None;
    }

    fn apply_status(&mut self, workspace: Option<String>, model: Option<String>) {
        let mut changed = false;
        if let Some(workspace) = workspace {
            if self.workspace != workspace {
                self.workspace = workspace;
                self.workspace_id = None;
                changed = true;
            }
        }
        if let Some(model) = model {
            if self.model != model {
                self.model = model;
                changed = true;
            }
        }
        if changed {
            self.clear_usage();
        }
    }

    fn session_picker_workspace(&self) -> SessionPickerWorkspace {
        SessionPickerWorkspace::new(self.workspace.clone(), self.workspace_id.clone())
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
/// `TurnChunk`s via bounded mpsc → TV thread drains on every
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
    let workspace_identity = {
        let locked = app.lock().await;
        locked
            .active_workspace()
            .map(session_picker_workspace_for_active_workspace)
            .unwrap_or_else(|| SessionPickerWorkspace::new("<unknown>", None))
    };
    let workspace_name = workspace_identity.name.clone();

    // Channels are bounded on both sides. If streamed chunks outrun
    // the UI drain loop, StreamSink closes the receiver so the active
    // turn fails instead of letting memory grow without bound.
    let (req_tx, mut req_rx) = mpsc::channel::<WorkerRequest>(32);
    let (event_tx, event_rx) = StreamSink::channel(UI_EVENT_CAPACITY);

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
    let worker_workspace_key = {
        let locked = app.lock().await;
        locked
            .active_workspace()
            .and_then(|workspace| local_storage_scope_for_workspace(workspace).ok())
            .map(|scope| scope.identity())
            .unwrap_or_else(|| workspace_name.clone())
    };
    let mut worker_sessions = HashMap::from([(
        worker_workspace_key,
        WorkerSessionBinding::from_session(&session),
    )]);
    let fallback_session_name = session.name.clone();
    let worker_sink = event_tx.clone();
    let worker_cmd_handler = CommandHandler::new(Arc::clone(&app));
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            match req {
                WorkerRequest::Turn(text) => {
                    let worker_session_id = match ensure_session_for_active_workspace(
                        &worker_app,
                        &mut worker_sessions,
                        &fallback_session_name,
                    )
                    .await
                    {
                        Ok(session_id) => session_id,
                        Err(e) => {
                            send_worker_finished(
                                &worker_sink,
                                Err(format!(
                                    "could not prepare session for active workspace: {e}"
                                )),
                            )
                            .await;
                            continue;
                        }
                    };
                    let client_opt = {
                        let guard = worker_app.lock().await;
                        guard.active_workspace().and_then(|w| w.client.clone())
                    };
                    match client_opt {
                        Some(client_arc) => {
                            let transcript_scope = match local_storage_scope_for_active_workspace(
                                &worker_app,
                            )
                            .await
                            {
                                Ok(scope) => scope,
                                Err(e) => {
                                    send_worker_finished(
                                        &worker_sink,
                                        Err(format!(
                                            "could not resolve transcript scope for session {}; turn not submitted: {e}",
                                            worker_session_id
                                        )),
                                    )
                                    .await;
                                    continue;
                                }
                            };
                            if let Err(e) = append_turn_transcript_entry(
                                &transcript_scope,
                                &worker_session_id,
                                "user",
                                &text,
                            ) {
                                send_worker_finished(
                                    &worker_sink,
                                    Err(format!("{e}; turn not submitted")),
                                )
                                .await;
                                continue;
                            }
                            let mut client = client_arc.lock().await;
                            let (turn_sink, turn_rx) =
                                StreamSink::turn_channel(TURN_STREAM_CAPACITY);
                            let observed_finished = Arc::new(AtomicBool::new(false));
                            let observed_finished_error = Arc::new(AtomicBool::new(false));
                            let forwarded_token = Arc::new(AtomicBool::new(false));
                            let mut forward_task = tokio::spawn(forward_turn_chunks(
                                turn_rx,
                                worker_sink.clone(),
                                Arc::clone(&observed_finished),
                                Arc::clone(&observed_finished_error),
                                Arc::clone(&forwarded_token),
                            ));
                            client.set_stream_sink(Some(turn_sink));
                            let submit_result = client.submit_turn(&worker_session_id, &text).await;
                            client.set_stream_sink(Some(worker_sink.clone()));
                            drop(client);
                            let submit_error_text =
                                submit_result.as_ref().err().map(|e| e.to_string());

                            let saw_finished = match tokio::time::timeout(
                                TURN_FORWARD_DRAIN_TIMEOUT,
                                &mut forward_task,
                            )
                            .await
                            {
                                Ok(Ok(saw_finished)) => saw_finished,
                                Ok(Err(e)) => {
                                    warn!("tv_ui: turn stream forwarder failed: {e}");
                                    observed_finished.load(Ordering::Acquire)
                                }
                                Err(_) => {
                                    warn!(
                                        "tv_ui: turn stream forwarder did not drain after \
                                         submit_turn returned; waiting for terminal frame"
                                    );
                                    match forward_task.await {
                                        Ok(saw_finished) => saw_finished,
                                        Err(e) => {
                                            warn!("tv_ui: turn stream forwarder failed: {e}");
                                            observed_finished.load(Ordering::Acquire)
                                        }
                                    }
                                }
                            };
                            let forwarded_any = forwarded_token.load(Ordering::Acquire);
                            let forwarded_terminal_error =
                                observed_finished_error.load(Ordering::Acquire);
                            if forwarded_terminal_error && submit_result.is_ok() {
                                let _ = mark_turn_transcript_incomplete_reason(
                                    &transcript_scope,
                                    &worker_session_id,
                                    "assistant response stream was rejected before transcript persistence",
                                );
                            } else {
                                match &submit_result {
                                    Ok(response) if !response.is_empty() => {
                                        if let Err(e) = append_turn_transcript_entry(
                                            &transcript_scope,
                                            &worker_session_id,
                                            "assistant",
                                            response,
                                        ) {
                                            let message =
                                                mark_turn_transcript_incomplete_after_append_failure(
                                                    &transcript_scope,
                                                    &worker_session_id,
                                                    &e,
                                                );
                                            send_worker_finished(&worker_sink, Err(message)).await;
                                        }
                                    }
                                    Err(e) => {
                                        let error_text = e.to_string();
                                        if let Err(append_error) = append_turn_transcript_entry(
                                            &transcript_scope,
                                            &worker_session_id,
                                            "error",
                                            &error_text,
                                        ) {
                                            let message =
                                                mark_turn_transcript_incomplete_after_append_failure(
                                                    &transcript_scope,
                                                    &worker_session_id,
                                                    &append_error,
                                                );
                                            send_worker_finished(&worker_sink, Err(message)).await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if let Some(error_text) = submit_error_text.as_deref() {
                                if submit_error_requires_incomplete_transcript(
                                    error_text,
                                    forwarded_any,
                                ) {
                                    let _ = mark_turn_transcript_incomplete_reason(
                                        &transcript_scope,
                                        &worker_session_id,
                                        error_text,
                                    );
                                }
                            }
                            send_worker_chunks_reliably(
                                &worker_sink,
                                submit_turn_fallback_chunks(
                                    &submit_result,
                                    saw_finished,
                                    forwarded_any,
                                ),
                            )
                            .await;
                        }
                        None => {
                            // Only branch where `submit_turn` was
                            // never called — emit the terminal
                            // frame ourselves so the UI can unstick
                            // the placeholder line.
                            send_worker_finished(
                                &worker_sink,
                                Err("no active workspace client".to_string()),
                            )
                            .await;
                        }
                    }
                }
                WorkerRequest::SessionPickerList(workspace) => {
                    let load_result = tokio::time::timeout(
                        SESSION_PICKER_LIST_TIMEOUT,
                        load_session_picker_sessions_for_worker(&worker_app, &workspace),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "timed out after {}s",
                            SESSION_PICKER_LIST_TIMEOUT.as_secs()
                        )
                    })
                    .and_then(|result| result)
                    .map_err(|e| e.to_string());
                    let (workspace, result) = match load_result {
                        Ok((loaded_workspace, sessions)) => (loaded_workspace, Ok(sessions)),
                        Err(message) => (workspace, Err(message)),
                    };
                    let _ =
                        worker_sink.send(TurnChunk::SessionPickerList(SessionPickerListResult {
                            workspace,
                            result,
                        }));
                }
                WorkerRequest::Command(cmdline) => {
                    // Route slash commands through the shared
                    // `CommandHandler`. Advertised commands return
                    // structured strings so side effects are visible
                    // inside the full-screen TUI.
                    if let Some(message) = stdout_only_slash_command_block_message(&cmdline) {
                        let _ = worker_sink.send(TurnChunk::Token(message));
                        send_worker_finished(&worker_sink, Ok(String::new())).await;
                        continue;
                    }
                    if let Some(target) = active_worker_session_delete_target(
                        &cmdline,
                        &worker_app,
                        &mut worker_sessions,
                    )
                    .await
                    {
                        send_worker_finished(
                            &worker_sink,
                            Err(format!(
                                "cannot delete active session `{target}`; switch to another session before deleting it"
                            )),
                        )
                        .await;
                        continue;
                    }
                    let preflight = command_session_preflight(&cmdline);
                    let workspace_before_dispatch =
                        if preflight == CommandSessionPreflight::AfterWorkspaceSwitch {
                            current_workspace_name(&worker_app).await.ok()
                        } else {
                            None
                        };
                    let mut command_session_id = remembered_session_id_for_active_workspace(
                        &worker_app,
                        &worker_sessions,
                        &fallback_session_name,
                    )
                    .await;
                    if preflight == CommandSessionPreflight::BeforeDispatch {
                        command_session_id = match ensure_session_for_active_workspace(
                            &worker_app,
                            &mut worker_sessions,
                            &fallback_session_name,
                        )
                        .await
                        {
                            Ok(session_id) => session_id,
                            Err(e) => {
                                send_worker_finished(
                                    &worker_sink,
                                    Err(format!(
                                        "could not prepare session for active workspace: {e}"
                                    )),
                                )
                                .await;
                                continue;
                            }
                        };
                    }
                    let mut session_switched = false;
                    if let Some(session_action) = session_action(&cmdline) {
                        let target_session = session_action.target().to_string();
                        let action_label = match &session_action {
                            SessionAction::Switch { .. } => "switch session to",
                            SessionAction::Create { .. } => "create session",
                        };
                        let session_result = match session_action {
                            SessionAction::Switch { target } => {
                                resolve_or_create_session_for_worker(&worker_app, &target).await
                            }
                            SessionAction::Create { target } => {
                                create_new_session_for_worker(&worker_app, &target).await
                            }
                        };
                        match session_result {
                            Ok(session) => {
                                command_session_id = session.id.clone();
                                match current_workspace_binding_key(&worker_app).await {
                                    Ok(workspace_key) => {
                                        remember_worker_session(
                                            &mut worker_sessions,
                                            workspace_key,
                                            &session,
                                        );
                                        session_switched = true;
                                    }
                                    Err(e) => {
                                        send_worker_finished(
                                            &worker_sink,
                                            Err(format!(
                                                "could not bind session `{target_session}` to workspace: {e}"
                                            )),
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                            }
                            Err(e) => {
                                send_worker_finished(
                                    &worker_sink,
                                    Err(format!(
                                        "could not {action_label} `{target_session}`: {e}"
                                    )),
                                )
                                .await;
                                continue;
                            }
                        }
                    }
                    match worker_cmd_handler
                        .handle(&cmdline, &command_session_id)
                        .await
                    {
                        Ok(Some(text)) => {
                            let model_switched = successful_model_switch_command(&cmdline, &text);
                            let _ = worker_sink.send(TurnChunk::Token(text));
                            let mut workspace_switched = false;
                            if preflight == CommandSessionPreflight::AfterWorkspaceSwitch {
                                let switched_workspace = {
                                    let guard = worker_app.lock().await;
                                    guard.active_workspace().map(|w| w.config.name.clone())
                                };
                                if switched_workspace != workspace_before_dispatch {
                                    workspace_switched = true;
                                    install_stream_sink_on_active_client(
                                        &worker_app,
                                        worker_sink.clone(),
                                    )
                                    .await;
                                    if let Err(e) = ensure_session_for_active_workspace(
                                        &worker_app,
                                        &mut worker_sessions,
                                        &fallback_session_name,
                                    )
                                    .await
                                    {
                                        let _ = worker_sink.send(TurnChunk::ClearUsage);
                                        send_worker_finished(
                                            &worker_sink,
                                            Err(format!(
                                                "workspace switched, but session setup failed: {e}"
                                            )),
                                        )
                                        .await;
                                        continue;
                                    }
                                    if let Some(name) = switched_workspace {
                                        let splash = connect_splash_for_workspace(&name);
                                        let _ = worker_sink.send(TurnChunk::Typewriter(splash));
                                    }
                                }
                            }
                            if should_clear_usage_after_command(
                                workspace_switched,
                                session_switched,
                                model_switched,
                            ) {
                                let _ = worker_sink.send(TurnChunk::ClearUsage);
                            }
                            if workspace_switched || model_switched {
                                if let Some((workspace, model)) =
                                    status_snapshot_for_worker(&worker_app).await
                                {
                                    let _ = worker_sink.send(TurnChunk::Status {
                                        workspace: Some(workspace),
                                        model: Some(model),
                                    });
                                }
                            }
                            send_worker_finished(&worker_sink, Ok(String::new())).await;
                        }
                        Ok(None) => {
                            let _ = worker_sink.send(TurnChunk::Token(format!(
                                "Command `{cmdline}` completed without structured TUI output."
                            )));
                            if should_clear_usage_after_command(false, session_switched, false) {
                                let _ = worker_sink.send(TurnChunk::ClearUsage);
                            }
                            send_worker_finished(&worker_sink, Ok(String::new())).await;
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
                            send_worker_finished(&worker_sink, Ok(String::new())).await;
                        }
                        Err(e) => {
                            if should_clear_usage_after_command(false, session_switched, false) {
                                let _ = worker_sink.send(TurnChunk::ClearUsage);
                            }
                            send_worker_finished(&worker_sink, Err(e.to_string())).await;
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
            workspace_identity.id,
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

async fn forward_turn_chunks(
    mut turn_rx: mpsc::Receiver<TurnChunk>,
    ui_sink: StreamSink,
    observed_finished: Arc<AtomicBool>,
    observed_finished_error: Arc<AtomicBool>,
    forwarded_token: Arc<AtomicBool>,
) -> bool {
    let mut saw_finished = false;
    let mut pending_token = String::new();
    let mut forwarded_bytes = 0usize;

    while let Some(chunk) = turn_rx.recv().await {
        match chunk {
            TurnChunk::Token(text) => {
                forwarded_bytes = forwarded_bytes.saturating_add(text.len());
                if forwarded_bytes > TURN_STREAM_MAX_BYTES {
                    if !flush_forwarded_token(&ui_sink, &forwarded_token, &mut pending_token).await
                    {
                        return saw_finished;
                    }
                    let message = format!(
                        "response exceeded {} byte TUI stream limit; turn closed",
                        TURN_STREAM_MAX_BYTES
                    );
                    if ui_sink
                        .send_async(TurnChunk::Finished(Err(message)))
                        .await
                        .is_ok()
                    {
                        saw_finished = true;
                        observed_finished.store(true, Ordering::Release);
                        observed_finished_error.store(true, Ordering::Release);
                    }
                    return saw_finished;
                }
                pending_token.push_str(&text);
                if pending_token.len() >= TURN_TOKEN_COALESCE_BYTES
                    && !flush_forwarded_token(&ui_sink, &forwarded_token, &mut pending_token).await
                {
                    return saw_finished;
                }
            }
            TurnChunk::Finished(result) => {
                if saw_finished {
                    continue;
                }
                let is_error = result.is_err();
                if !flush_forwarded_token(&ui_sink, &forwarded_token, &mut pending_token).await {
                    return saw_finished;
                }
                if ui_sink
                    .send_async(TurnChunk::Finished(result))
                    .await
                    .is_ok()
                {
                    saw_finished = true;
                    observed_finished.store(true, Ordering::Release);
                    if is_error {
                        observed_finished_error.store(true, Ordering::Release);
                    }
                } else {
                    return saw_finished;
                }
            }
            other => {
                if !flush_forwarded_token(&ui_sink, &forwarded_token, &mut pending_token).await {
                    return saw_finished;
                }
                if ui_sink.send_async(other).await.is_err() {
                    return saw_finished;
                }
            }
        }
    }
    let _ = flush_forwarded_token(&ui_sink, &forwarded_token, &mut pending_token).await;
    saw_finished
}

async fn flush_forwarded_token(
    ui_sink: &StreamSink,
    forwarded_token: &Arc<AtomicBool>,
    pending_token: &mut String,
) -> bool {
    if pending_token.is_empty() {
        return true;
    }
    let chunk = TurnChunk::Token(std::mem::take(pending_token));
    if ui_sink.send_async(chunk).await.is_ok() {
        forwarded_token.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

async fn send_worker_finished(ui_sink: &StreamSink, result: Result<String, String>) -> bool {
    send_worker_chunk_reliably(ui_sink, TurnChunk::Finished(result)).await
}

async fn send_worker_chunks_reliably(ui_sink: &StreamSink, chunks: Vec<TurnChunk>) -> bool {
    for chunk in chunks {
        if !send_worker_chunk_reliably(ui_sink, chunk).await {
            return false;
        }
    }
    true
}

async fn send_worker_chunk_reliably(ui_sink: &StreamSink, chunk: TurnChunk) -> bool {
    if ui_sink.send_async(chunk).await.is_ok() {
        true
    } else {
        warn!("tv_ui: worker event channel closed before terminal chunk delivery");
        false
    }
}

fn submit_turn_fallback_chunks(
    submit_result: &Result<String>,
    saw_finished: bool,
    forwarded_token: bool,
) -> Vec<TurnChunk> {
    if saw_finished {
        return Vec::new();
    }

    match submit_result {
        Ok(_) if forwarded_token => vec![TurnChunk::Finished(Err(
            "partial response incomplete; backend stream ended without a finished frame"
                .to_string(),
        ))],
        Ok(text) => {
            let mut chunks = Vec::new();
            if !text.is_empty() {
                chunks.push(TurnChunk::Token(text.clone()));
            }
            chunks.push(TurnChunk::Finished(Ok(String::new())));
            chunks
        }
        Err(e) => vec![TurnChunk::Finished(Err(e.to_string()))],
    }
}

fn successful_model_switch_command(cmdline: &str, output: &str) -> bool {
    model_switch_target(cmdline).is_some() && command_output_indicates_success(output)
}

fn should_clear_usage_after_command(
    workspace_switched: bool,
    session_switched: bool,
    model_switched: bool,
) -> bool {
    workspace_switched || session_switched || model_switched
}

fn model_switch_target(cmdline: &str) -> Option<&str> {
    let mut parts = cmdline.split_whitespace();
    let command = parts.next()?;
    if !matches!(command, "/model" | "/models") {
        return None;
    }
    if parts.next()? != "set" {
        return None;
    }
    parts.next()
}

fn command_output_indicates_success(output: &str) -> bool {
    let trimmed = output.trim_start();
    !trimmed.is_empty() && !trimmed.starts_with("Usage:") && !trimmed.starts_with("❌")
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

async fn load_session_picker_sessions_for_worker(
    app: &Arc<Mutex<App>>,
    requested_workspace: &SessionPickerWorkspace,
) -> Result<(SessionPickerWorkspace, Vec<Session>)> {
    let (active_workspace, client) = {
        let guard = app.lock().await;
        let workspace = guard
            .active_workspace()
            .ok_or_else(|| anyhow::anyhow!("no active workspace"))?;
        (
            session_picker_workspace_for_active_workspace(workspace),
            workspace.client.clone(),
        )
    };
    let client = client.ok_or_else(|| anyhow::anyhow!("no active workspace client"))?;
    if !session_picker_workspace_matches(requested_workspace, &active_workspace) {
        anyhow::bail!(
            "active workspace changed from `{}` to `{}` before sessions loaded",
            requested_workspace.name,
            active_workspace.name
        );
    }

    let locked = client.lock().await;
    let sessions = locked.list_sessions().await?;
    Ok((active_workspace, sessions))
}

fn local_storage_scope_for_workspace(
    workspace: &crate::cli::workspace::Workspace,
) -> Result<storage::LocalWorkspaceScope> {
    storage::workspace_scope(
        workspace.config.backend.as_str(),
        &workspace.config.name,
        workspace.config.id.as_deref(),
    )
}

async fn local_storage_scope_for_active_workspace(
    app: &Arc<Mutex<App>>,
) -> Result<storage::LocalWorkspaceScope> {
    let guard = app.lock().await;
    let workspace = guard
        .active_workspace()
        .ok_or_else(|| anyhow::anyhow!("no active workspace"))?;
    local_storage_scope_for_workspace(workspace)
}

fn session_picker_workspace_for_active_workspace(
    workspace: &crate::cli::workspace::Workspace,
) -> SessionPickerWorkspace {
    SessionPickerWorkspace::new(workspace.config.name.clone(), workspace.config.id.clone())
}

fn session_picker_workspace_matches(
    expected: &SessionPickerWorkspace,
    actual: &SessionPickerWorkspace,
) -> bool {
    expected.name == actual.name
        && match (&expected.id, &actual.id) {
            (Some(expected), Some(actual)) => expected == actual,
            _ => true,
        }
}

fn session_picker_load_matches_workspace(
    load: &SessionPickerLoad,
    workspace: &SessionPickerWorkspace,
) -> bool {
    match load {
        SessionPickerLoad::Idle => true,
        SessionPickerLoad::Loading(cached)
        | SessionPickerLoad::Ready(cached, _)
        | SessionPickerLoad::Error(cached, _) => {
            session_picker_workspace_matches(cached, workspace)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerSessionBinding {
    id: String,
    name: String,
}

impl WorkerSessionBinding {
    fn from_session(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
        }
    }
}

fn remember_worker_session(
    sessions: &mut HashMap<String, WorkerSessionBinding>,
    workspace_name: String,
    session: &Session,
) {
    sessions.insert(workspace_name, WorkerSessionBinding::from_session(session));
}

async fn current_workspace_name(app: &Arc<Mutex<App>>) -> Result<String> {
    let guard = app.lock().await;
    guard
        .active_workspace()
        .map(|w| w.config.name.clone())
        .ok_or_else(|| anyhow::anyhow!("no active workspace"))
}

async fn current_workspace_binding_key(app: &Arc<Mutex<App>>) -> Result<String> {
    Ok(local_storage_scope_for_active_workspace(app)
        .await?
        .identity())
}

async fn status_snapshot_for_worker(app: &Arc<Mutex<App>>) -> Option<(String, String)> {
    let (workspace, client) = {
        let guard = app.lock().await;
        let workspace = guard.active_workspace()?;
        (workspace.config.name.clone(), workspace.client.clone()?)
    };

    let model = client.lock().await.current_model_label();
    Some((workspace, model))
}

async fn remembered_session_id_for_active_workspace(
    app: &Arc<Mutex<App>>,
    sessions: &HashMap<String, WorkerSessionBinding>,
    fallback_session_name: &str,
) -> String {
    match current_workspace_name(app).await {
        Ok(_) => match current_workspace_binding_key(app).await {
            Ok(workspace_key) => sessions
                .get(&workspace_key)
                .map(|binding| binding.id.clone())
                .unwrap_or_else(|| fallback_session_name.to_string()),
            Err(_) => fallback_session_name.to_string(),
        },
        Err(_) => fallback_session_name.to_string(),
    }
}

async fn active_worker_session_delete_target(
    cmdline: &str,
    app: &Arc<Mutex<App>>,
    sessions: &mut HashMap<String, WorkerSessionBinding>,
) -> Option<String> {
    let target = session_delete_target(cmdline)?;
    let (workspace_key, client) = {
        let guard = app.lock().await;
        let workspace = guard.active_workspace()?;
        let workspace_key = local_storage_scope_for_workspace(workspace)
            .ok()?
            .identity();
        (workspace_key, workspace.client.clone())
    };
    let backend_sessions = match client {
        Some(client) => client.lock().await.list_sessions().await.ok(),
        None => None,
    };

    active_worker_session_delete_target_for_workspace(
        &target,
        &workspace_key,
        sessions,
        backend_sessions.as_deref(),
    )
}

fn active_worker_session_delete_target_for_workspace(
    target: &str,
    workspace_name: &str,
    sessions: &mut HashMap<String, WorkerSessionBinding>,
    backend_sessions: Option<&[Session]>,
) -> Option<String> {
    let binding = sessions.get(workspace_name)?.clone();
    let (active_binding, target_id) = if let Some(backend_sessions) = backend_sessions {
        let active_binding = refreshed_active_worker_session_binding(&binding, backend_sessions)?;
        let target_id = canonical_session_id_for_delete_target(target, backend_sessions)?;
        (active_binding, target_id)
    } else if binding.id == target || binding.name == target {
        let target_id = binding.id.clone();
        (binding, target_id)
    } else {
        return None;
    };

    if sessions.get(workspace_name) != Some(&active_binding) {
        sessions.insert(workspace_name.to_string(), active_binding.clone());
    }

    if active_binding.id == target_id {
        return Some(target.to_string());
    }
    None
}

fn refreshed_active_worker_session_binding(
    binding: &WorkerSessionBinding,
    backend_sessions: &[Session],
) -> Option<WorkerSessionBinding> {
    if let Some(session) = backend_sessions
        .iter()
        .find(|session| session.id == binding.id)
    {
        return Some(WorkerSessionBinding::from_session(session));
    }

    let mut name_matches = backend_sessions
        .iter()
        .filter(|session| session.name == binding.name);
    let session = name_matches.next()?;
    if name_matches.next().is_some() {
        return None;
    }
    Some(WorkerSessionBinding::from_session(session))
}

fn canonical_session_id_for_delete_target(
    target: &str,
    backend_sessions: &[Session],
) -> Option<String> {
    if let Some(session) = backend_sessions.iter().find(|session| session.id == target) {
        return Some(session.id.clone());
    }

    let mut name_matches = backend_sessions
        .iter()
        .filter(|session| session.name == target);
    let session = name_matches.next()?;
    if name_matches.next().is_some() {
        return None;
    }
    Some(session.id.clone())
}

async fn ensure_session_for_active_workspace(
    app: &Arc<Mutex<App>>,
    sessions: &mut HashMap<String, WorkerSessionBinding>,
    fallback_session_name: &str,
) -> Result<String> {
    let workspace_key = current_workspace_binding_key(app).await?;
    if let Some(binding) = sessions.get(&workspace_key).cloned() {
        if let Ok(session) = load_session_for_worker(app, &binding.id).await {
            remember_worker_session(sessions, workspace_key, &session);
            return Ok(session.id);
        }
        let session = resolve_or_create_session_for_worker(app, &binding.name).await?;
        remember_worker_session(sessions, workspace_key, &session);
        return Ok(session.id);
    }

    let session = resolve_or_create_session_for_worker(app, fallback_session_name).await?;
    remember_worker_session(sessions, workspace_key, &session);
    Ok(session.id)
}

async fn load_session_for_worker(app: &Arc<Mutex<App>>, session_id: &str) -> Result<Session> {
    let client = {
        let guard = app.lock().await;
        guard.active_workspace().and_then(|w| w.client.clone())
    }
    .ok_or_else(|| anyhow::anyhow!("no active workspace client"))?;

    let locked = client.lock().await;
    locked.load_session(session_id).await
}

async fn resolve_or_create_session_for_worker(
    app: &Arc<Mutex<App>>,
    session_name: &str,
) -> Result<Session> {
    let client = {
        let guard = app.lock().await;
        guard.active_workspace().and_then(|w| w.client.clone())
    }
    .ok_or_else(|| anyhow::anyhow!("no active workspace client"))?;

    let resolution = {
        let locked = client.lock().await;
        plan_worker_session_resolution(session_name, locked.list_sessions().await)?
    };

    if let WorkerSessionResolution::Existing(session) = resolution {
        return Ok(session);
    }

    let session = client.lock().await.create_session(session_name).await?;
    save_worker_session_metadata_best_effort(app, &session).await;
    Ok(session)
}

async fn create_new_session_for_worker(
    app: &Arc<Mutex<App>>,
    session_name: &str,
) -> Result<Session> {
    let client = {
        let guard = app.lock().await;
        guard.active_workspace().and_then(|w| w.client.clone())
    }
    .ok_or_else(|| anyhow::anyhow!("no active workspace client"))?;

    let session = client.lock().await.create_session(session_name).await?;
    save_worker_session_metadata_best_effort(app, &session).await;
    Ok(session)
}

async fn save_worker_session_metadata_best_effort(
    app: &Arc<Mutex<App>>,
    session: &Session,
) -> bool {
    let scope = match local_storage_scope_for_active_workspace(app).await {
        Ok(scope) => scope,
        Err(e) => {
            warn!(
                "backend session '{}' was created, but active workspace scope was unavailable: {e}",
                session.id
            );
            return false;
        }
    };
    save_worker_session_metadata_best_effort_with(
        &scope,
        session,
        storage::save_scoped_session_metadata,
    )
}

fn save_worker_session_metadata_best_effort_with<F>(
    scope: &storage::LocalWorkspaceScope,
    session: &Session,
    save: F,
) -> bool
where
    F: FnOnce(&storage::LocalWorkspaceScope, &storage::SessionMetadata) -> Result<()>,
{
    let metadata = storage::SessionMetadata {
        id: session.id.clone(),
        name: session.name.clone(),
        model: session.model.clone(),
        provider: session.provider.clone(),
        created_at: Utc::now().to_rfc3339(),
        message_count: 0,
        last_active: Utc::now().to_rfc3339(),
    };
    if storage::is_safe_session_id(&metadata.id) {
        if let Err(e) = save(scope, &metadata) {
            warn!(
                "backend session '{}' was created, but local metadata save failed: {e}",
                metadata.id
            );
            return false;
        }
    } else {
        warn!(
            "not saving local metadata for unsafe session id: {}",
            metadata.id
        );
        return false;
    }
    true
}

fn append_turn_transcript_entry(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    role: &str,
    content: &str,
) -> Result<()> {
    storage::append_scoped_session_history(scope, session_id, role, content).map_err(|e| {
        anyhow::anyhow!("could not append {role} transcript entry for session {session_id}: {e}")
    })
}

fn mark_turn_transcript_incomplete_after_append_failure(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    append_error: &anyhow::Error,
) -> String {
    mark_turn_transcript_incomplete_reason(scope, session_id, &append_error.to_string())
}

fn mark_turn_transcript_incomplete_reason(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    reason: &str,
) -> String {
    warn!("{reason}");
    match storage::mark_scoped_session_history_incomplete(scope, session_id, reason) {
        Ok(()) => {
            format!("{reason}; transcript marked incomplete and /save is disabled until /clear")
        }
        Err(marker_error) => {
            format!("{reason}; additionally failed to mark transcript incomplete: {marker_error}")
        }
    }
}

fn turn_collection_failure_requires_incomplete_transcript(message: &str) -> bool {
    message.contains("accepted assistant turn exceeded cap")
        || message.contains("buffered runId-less assistant messages exceeded cap")
}

fn submit_error_requires_incomplete_transcript(message: &str, forwarded_token: bool) -> bool {
    forwarded_token
        || turn_collection_failure_requires_incomplete_transcript(message)
        || response_size_failure_requires_incomplete_transcript(message)
}

fn response_size_failure_requires_incomplete_transcript(message: &str) -> bool {
    message.contains("response exceeded")
        || message.contains("response frame exceeded")
        || message.contains("TUI stream limit")
}

#[derive(Debug)]
enum WorkerSessionResolution {
    Existing(Session),
    Create,
}

fn plan_worker_session_resolution(
    requested: &str,
    list_result: Result<Vec<Session>>,
) -> Result<WorkerSessionResolution> {
    let sessions = list_result
        .map_err(|e| anyhow::anyhow!("could not list sessions from active backend: {e}"))?;
    match choose_worker_session_by_id_or_name(&sessions, requested)? {
        Some(session) => Ok(WorkerSessionResolution::Existing(session.clone())),
        None => Ok(WorkerSessionResolution::Create),
    }
}

fn choose_worker_session_by_id_or_name<'a>(
    sessions: &'a [Session],
    requested: &str,
) -> Result<Option<&'a Session>> {
    let id_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == requested)
        .collect();
    match id_matches.as_slice() {
        [session] => return Ok(Some(*session)),
        [] => {}
        _ => {
            return Err(ambiguous_worker_session_error(
                requested,
                "backend session id",
                id_matches,
            ));
        }
    }

    let name_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.name == requested)
        .collect();
    match name_matches.as_slice() {
        [session] => Ok(Some(*session)),
        [] => Ok(None),
        _ => Err(ambiguous_worker_session_error(
            requested,
            "session name",
            name_matches,
        )),
    }
}

fn ambiguous_worker_session_error(
    requested: &str,
    label: &str,
    candidates: Vec<&Session>,
) -> anyhow::Error {
    let candidates = candidates
        .iter()
        .map(|session| format!("backend id={} name={}", session.id, session.name))
        .collect::<Vec<_>>()
        .join("; ");

    anyhow::anyhow!("ambiguous {label} '{requested}'; use an explicit id. Candidates: {candidates}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandSessionPreflight {
    None,
    BeforeDispatch,
    AfterWorkspaceSwitch,
}

fn command_session_preflight(cmdline: &str) -> CommandSessionPreflight {
    let Ok(parts) = tokenize_slash_command(cmdline) else {
        return CommandSessionPreflight::None;
    };
    let Some(command) = parts.first().map(String::as_str) else {
        return CommandSessionPreflight::None;
    };
    let subcommand = parts.get(1).map(String::as_str);

    match command {
        "/info" | "/status" | "/clear" | "/save" => CommandSessionPreflight::BeforeDispatch,
        "/session" if matches!(subcommand, Some("info") | Some("delete")) => {
            CommandSessionPreflight::BeforeDispatch
        }
        "/workspace" | "/workspaces"
            if matches!(subcommand, Some("switch")) && parts.get(2).is_some() =>
        {
            CommandSessionPreflight::AfterWorkspaceSwitch
        }
        _ => CommandSessionPreflight::None,
    }
}

fn stdout_only_slash_command_block_message(cmdline: &str) -> Option<String> {
    cmdline.split_whitespace().next()?;
    None
}

fn session_delete_target(cmdline: &str) -> Option<String> {
    let parts = tokenize_slash_command(cmdline).ok()?;
    if parts.first()?.as_str() != "/session" {
        return None;
    }
    if parts.get(1)?.as_str() != "delete" {
        return None;
    }
    single_remaining_session_target(&parts[2..])
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionAction {
    Switch { target: String },
    Create { target: String },
}

impl SessionAction {
    fn target(&self) -> &str {
        match self {
            SessionAction::Switch { target } | SessionAction::Create { target } => target,
        }
    }
}

fn session_action(cmdline: &str) -> Option<SessionAction> {
    let parts = tokenize_slash_command(cmdline).ok()?;
    if parts.first()?.as_str() != "/session" {
        return None;
    }
    match parts.get(1).map(String::as_str)? {
        "list" | "info" | "delete" => None,
        "switch" => Some(SessionAction::Switch {
            target: single_remaining_session_target(&parts[2..])?,
        }),
        "create" => Some(SessionAction::Create {
            target: single_remaining_session_target(&parts[2..])?,
        }),
        name if parts.len() == 2 => Some(SessionAction::Switch {
            target: name.to_string(),
        }),
        _ => None,
    }
}

fn single_remaining_session_target(parts: &[String]) -> Option<String> {
    match parts {
        [target] if !target.is_empty() => Some(target.clone()),
        _ => None,
    }
}

fn new_session_command(now: DateTime<Utc>, nonce: uuid::Uuid) -> String {
    format!(
        "/session create session-{}-{}",
        now.format("%Y%m%d-%H%M%S"),
        nonce.simple()
    )
}

#[allow(clippy::too_many_arguments)]
fn run_blocking(
    app: Arc<Mutex<App>>,
    session: Session,
    model: String,
    provider: String,
    workspace_name: String,
    workspace_id: Option<String>,
    connect_splash: Option<String>,
    req_tx: mpsc::Sender<WorkerRequest>,
    mut event_rx: mpsc::Receiver<TurnChunk>,
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
    status_state.workspace_id = workspace_id;
    let initial_status_line = build_status_line(w, h, &status_state);
    tapp.set_status_line(initial_status_line);

    // Shared chat buffer. Rc<RefCell<_>> because the Turbo Vision
    // event loop is single-threaded; the custom ChatPane view reads
    // the buffer during its draw pass, and the event loop writes to
    // it on Enter.
    let welcome_lines = initial_chat_lines(&workspace_name, &model, &provider, welcome_back);
    let mut typewriter_state = None;
    let initial_lines = if let Some(text) = connect_splash {
        let state = TypewriterState::new(text, welcome_lines, 0);
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
            if ws.config.name != state.workspace || ws.config.id != state.workspace_id {
                state.workspace = ws.config.name.clone();
                state.workspace_id = ws.config.id.clone();
                state.clear_usage();
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
    event_rx: &mut mpsc::Receiver<TurnChunk>,
    status_state: &mut StatusState,
    typewriter_state: &mut Option<TypewriterState>,
    shared_app: &Arc<Mutex<App>>,
    w: i16,
    h: i16,
) -> Result<()> {
    app.running = true;
    let mut last_size = app.terminal.size();
    let mut response_in_flight = false;
    let mut session_picker_state = SessionPickerState::default();
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
        let error_frame = drain_stream_events(
            event_rx,
            &chat_lines,
            status_state,
            typewriter_state,
            &mut response_in_flight,
            &mut session_picker_state,
        );
        if error_frame && status_state.beep_on_error {
            let _ = app.terminal.beep();
        }
        maybe_open_pending_session_picker(
            app,
            &chat_lines,
            &req_tx,
            status_state,
            &mut response_in_flight,
            &mut session_picker_state,
        );
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

        if response_in_flight
            && should_block_modal_entry_while_busy(&event, input_data.borrow().is_empty())
        {
            note_response_busy(status_state);
            continue;
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

        // Slash-command popup: reserve Ctrl-K/menu paths for the
        // picker so plain `/` remains ordinary text input. That keeps
        // arbitrary slash commands typeable even when the popup is
        // dismissed.
        if event.what == EventType::Keyboard
            && should_open_slash_popup(event.key_code, input_data.borrow().is_empty())
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
                if quit_is_blocked_by_inflight_turn(response_in_flight) {
                    note_response_busy(status_state);
                    continue;
                }
                if is_exit_command(&submitted) {
                    append_prompt_placeholder(&submitted, &chat_lines);
                    input_line.borrow_mut().set_text(String::new());
                    app.running = false;
                    continue;
                }
                if response_in_flight {
                    note_response_busy(status_state);
                    continue;
                }
                // `/theme …` is a TUI-only concern — it toggles the
                // live `TPalette` and has no meaning on the
                // rustyline path. Intercept before routing to the
                // CommandHandler so the worker never sees it.
                if let Some(rest) = submitted.strip_prefix("/theme") {
                    append_prompt_placeholder(&submitted, &chat_lines);
                    // `set_text("")` resets `cursor_pos`, selection, and
                    // `first_pos` — safe to use mid-session unlike raw
                    // `data.clear()` which would leave `cursor_pos`
                    // pointing past the end of an empty string and panic
                    // in `String::insert` on the next keystroke.
                    input_line.borrow_mut().set_text(String::new());
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
                    WorkerRequest::Turn(submitted.clone())
                } else {
                    WorkerRequest::Command(submitted.clone())
                };
                let toast = (!is_turn).then(|| format!("Command: {submitted}"));
                let status = dispatch_worker_backed_submission(
                    &submitted,
                    request,
                    &chat_lines,
                    &req_tx,
                    status_state,
                    &mut response_in_flight,
                    is_turn,
                    toast,
                    "dispatch",
                );
                if status != SubmissionStatus::Busy {
                    // Clear after a non-busy submit attempt. Busy
                    // submissions keep the typed input intact so the
                    // user can send it once the active response finishes.
                    input_line.borrow_mut().set_text(String::new());
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
                &mut response_in_flight,
                &mut session_picker_state,
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

fn should_open_slash_popup(key_code: u16, input_empty: bool) -> bool {
    input_empty && key_code == KB_CTRL_K
}

fn should_block_modal_entry_while_busy(event: &Event, input_empty: bool) -> bool {
    match event.what {
        EventType::Keyboard => {
            is_menu_activation_key(event.key_code)
                || should_open_slash_popup(event.key_code, input_empty)
        }
        EventType::MouseDown => {
            event.mouse.pos.y == 0 && (event.mouse.buttons & MB_LEFT_BUTTON) != 0
        }
        _ => false,
    }
}

fn is_menu_activation_key(key_code: u16) -> bool {
    matches!(
        key_code,
        KB_F10 | KB_ALT_F | KB_ALT_S | KB_ALT_W | KB_ALT_M | KB_ALT_H
    )
}

fn is_exit_command(input: &str) -> bool {
    tokenize_slash_command(input)
        .ok()
        .and_then(|parts| parts.first().cloned())
        .map(|command| command == "/exit")
        .unwrap_or(false)
}

/// Non-blocking drain of the worker → TUI event channel. Called at
/// the top of every event-loop tick so streamed tokens land in the
/// chat pane even when the user isn't pressing keys. Also observes
/// `Finished` frames to stop the elapsed-turn timer so the status
/// line freezes at the final duration instead of ticking forever.
fn drain_stream_events(
    event_rx: &mut mpsc::Receiver<TurnChunk>,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    status_state: &mut StatusState,
    typewriter_state: &mut Option<TypewriterState>,
    response_in_flight: &mut bool,
    session_picker_state: &mut SessionPickerState,
) -> bool {
    let mut saw_error = false;
    loop {
        match event_rx.try_recv() {
            Ok(chunk) => {
                match &chunk {
                    TurnChunk::Usage(usage) => {
                        status_state.usage = Some(*usage);
                    }
                    TurnChunk::ClearUsage => {
                        status_state.clear_usage();
                    }
                    TurnChunk::Status { workspace, model } => {
                        status_state.apply_status(workspace.clone(), model.clone());
                    }
                    TurnChunk::SessionPickerList(result) => {
                        apply_session_picker_result(
                            result.clone(),
                            chat_lines,
                            status_state,
                            session_picker_state,
                        );
                    }
                    TurnChunk::Finished(result) => {
                        if result.is_err() {
                            saw_error = true;
                        }
                        status_state.end_turn();
                        *response_in_flight = false;
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
            // way, treat an in-flight request as terminal so the UI
            // does not stay permanently busy.
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if *response_in_flight {
                    saw_error = true;
                    status_state.end_turn();
                    *response_in_flight = false;
                    apply_chunk(
                        TurnChunk::Finished(Err(
                            "worker channel disconnected before the request completed".to_string(),
                        )),
                        chat_lines,
                    );
                }
                break;
            }
        }
    }
    saw_error
}

fn note_response_busy(status_state: &mut StatusState) {
    status_state.set_toast(RESPONSE_BUSY_TOAST);
}

fn quit_is_blocked_by_inflight_turn(response_in_flight: bool) -> bool {
    response_in_flight
}

#[allow(clippy::too_many_arguments)]
fn dispatch_worker_backed_submission(
    label: &str,
    request: WorkerRequest,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    status_state: &mut StatusState,
    response_in_flight: &mut bool,
    starts_turn_timer: bool,
    toast: Option<String>,
    error_context: &str,
) -> SubmissionStatus {
    if *response_in_flight {
        note_response_busy(status_state);
        return SubmissionStatus::Busy;
    }

    if let Some(toast) = toast {
        status_state.set_toast(toast);
    }
    append_prompt_placeholder(label, chat_lines);

    if starts_turn_timer {
        status_state.begin_turn();
    } else {
        status_state.begin_busy(false);
    }
    *response_in_flight = true;

    if let Err(e) = req_tx.blocking_send(request) {
        *response_in_flight = false;
        // Undo the busy indicator; the request never made it to the worker.
        status_state.turn_start = None;
        chat_lines
            .borrow_mut()
            .push(format!("[error] could not {error_context}: {e}"));
        return SubmissionStatus::DispatchFailed;
    }

    SubmissionStatus::Started
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
            let safe = sanitize_terminal_text(&s);
            let mut parts = safe.split('\n');
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
        TurnChunk::ClearUsage => {}
        TurnChunk::Status { .. } => {}
        TurnChunk::SessionPickerList(_) => {}
        TurnChunk::Finished(Ok(_)) => {
            lines.push(String::new());
        }
        TurnChunk::Finished(Err(e)) => {
            lines.push(format!("[error] {}", sanitize_terminal_text(&e)));
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

#[allow(clippy::too_many_arguments)]
fn handle_command(
    app: &mut Application,
    command: u16,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    shared_app: &Arc<Mutex<App>>,
    status_state: &mut StatusState,
    response_in_flight: &mut bool,
    session_picker_state: &mut SessionPickerState,
) {
    match command {
        CM_QUIT => {
            if quit_is_blocked_by_inflight_turn(*response_in_flight) {
                note_response_busy(status_state);
            } else {
                app.running = false;
            }
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
            dispatch_command(
                cmdline,
                chat_lines,
                req_tx,
                status_state,
                response_in_flight,
            );
        }
        // E-5: Workspace switch opens a modal picker populated from
        // the current App state. On selection, dispatch
        // `/workspace switch <name>` through the worker — the live
        // status-line read (E-4) picks up the new workspace on the
        // next tick.
        CMD_WORKSPACE_SWITCH => {
            if *response_in_flight {
                note_response_busy(status_state);
                return;
            }
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
                dispatch_command(
                    &cmdline,
                    chat_lines,
                    req_tx,
                    status_state,
                    response_in_flight,
                );
            }
        }
        // Theme preset slots from the slash popup. Map the id back
        // to a `themes::PRESETS` entry and apply via
        // `handle_theme_command` so the rendering path stays in
        // one place.
        cmd if (CMD_THEME_BASE..CMD_THEME_BASE + themes::PRESETS.len() as u16).contains(&cmd) => {
            if *response_in_flight {
                note_response_busy(status_state);
                return;
            }
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
            let cmdline = new_session_command(Utc::now(), uuid::Uuid::new_v4());
            dispatch_command(
                &cmdline,
                chat_lines,
                req_tx,
                status_state,
                response_in_flight,
            );
        }
        CMD_SESSION_OPEN => {
            if *response_in_flight {
                note_response_busy(status_state);
                return;
            }
            open_or_request_session_picker(
                app,
                chat_lines,
                req_tx,
                status_state,
                response_in_flight,
                session_picker_state,
            );
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
    response_in_flight: &mut bool,
) {
    let _ = dispatch_worker_backed_submission(
        cmdline,
        WorkerRequest::Command(cmdline.to_string()),
        chat_lines,
        req_tx,
        status_state,
        response_in_flight,
        false,
        Some(format!("Command: {cmdline}")),
        "dispatch command",
    );
}

fn append_prompt_placeholder(label: &str, chat_lines: &Rc<RefCell<Vec<String>>>) {
    let mut lines = chat_lines.borrow_mut();
    lines.push(format!("> {label}"));
    lines.push(String::new());
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPickerEntry {
    id: String,
    name: String,
    model: String,
    provider: String,
}

impl From<Session> for SessionPickerEntry {
    fn from(session: Session) -> Self {
        Self {
            id: session.id,
            name: session.name,
            model: session.model,
            provider: session.provider,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionPickerLoad {
    Idle,
    Loading(SessionPickerWorkspace),
    Ready(SessionPickerWorkspace, Vec<SessionPickerEntry>),
    Error(SessionPickerWorkspace, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPickerState {
    load: SessionPickerLoad,
    open_when_ready: bool,
}

impl Default for SessionPickerState {
    fn default() -> Self {
        Self {
            load: SessionPickerLoad::Idle,
            open_when_ready: false,
        }
    }
}

fn request_session_picker_load(
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    status_state: &mut StatusState,
    session_picker_state: &mut SessionPickerState,
) -> SubmissionStatus {
    session_picker_state.open_when_ready = true;
    let workspace = status_state.session_picker_workspace();
    if session_picker_load_matches_workspace(&session_picker_state.load, &workspace)
        && matches!(session_picker_state.load, SessionPickerLoad::Loading(_))
    {
        status_state.set_toast("Loading sessions...".to_string());
        return SubmissionStatus::Busy;
    }

    {
        let mut lines = chat_lines.borrow_mut();
        lines.push("[session] loading backend sessions...".to_string());
        lines.push(String::new());
    }
    session_picker_state.load = SessionPickerLoad::Loading(workspace.clone());
    status_state.set_toast("Loading sessions...".to_string());

    match req_tx.try_send(WorkerRequest::SessionPickerList(workspace)) {
        Ok(()) => SubmissionStatus::Started,
        Err(e) => {
            session_picker_state.load = SessionPickerLoad::Idle;
            session_picker_state.open_when_ready = false;
            chat_lines
                .borrow_mut()
                .push(format!("[session] could not request backend sessions: {e}"));
            SubmissionStatus::DispatchFailed
        }
    }
}

fn apply_session_picker_result(
    result: SessionPickerListResult,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    status_state: &mut StatusState,
    session_picker_state: &mut SessionPickerState,
) {
    let active_workspace = status_state.session_picker_workspace();
    if !session_picker_workspace_matches(&result.workspace, &active_workspace) {
        return;
    }

    match result.result {
        Ok(sessions) => {
            let entries: Vec<SessionPickerEntry> =
                sessions.into_iter().map(SessionPickerEntry::from).collect();
            status_state.set_toast(format!("Loaded {} sessions", entries.len()));
            session_picker_state.load = SessionPickerLoad::Ready(result.workspace, entries);
        }
        Err(message) => {
            chat_lines.borrow_mut().push(format!(
                "[session] could not load backend sessions: {message}"
            ));
            chat_lines.borrow_mut().push(String::new());
            status_state.set_toast("Session load failed".to_string());
            session_picker_state.load = SessionPickerLoad::Error(result.workspace, message);
            session_picker_state.open_when_ready = false;
        }
    }
}

fn open_or_request_session_picker(
    app: &mut Application,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    status_state: &mut StatusState,
    response_in_flight: &mut bool,
    session_picker_state: &mut SessionPickerState,
) {
    session_picker_state.open_when_ready = true;
    if request_session_picker_load_if_workspace_changed(
        chat_lines,
        req_tx,
        status_state,
        session_picker_state,
    )
    .is_some()
    {
        return;
    }
    maybe_open_pending_session_picker(
        app,
        chat_lines,
        req_tx,
        status_state,
        response_in_flight,
        session_picker_state,
    );
    if !session_picker_state.open_when_ready {
        return;
    }

    let workspace = status_state.session_picker_workspace();
    match session_picker_state.load.clone() {
        SessionPickerLoad::Idle | SessionPickerLoad::Error(_, _) => {
            request_session_picker_load(chat_lines, req_tx, status_state, session_picker_state);
        }
        SessionPickerLoad::Loading(cached) => {
            if session_picker_workspace_matches(&cached, &workspace) {
                status_state.set_toast("Loading sessions...".to_string());
            } else {
                request_session_picker_load(chat_lines, req_tx, status_state, session_picker_state);
            }
        }
        SessionPickerLoad::Ready(cached, _) => {
            if !session_picker_workspace_matches(&cached, &workspace) {
                request_session_picker_load(chat_lines, req_tx, status_state, session_picker_state);
            }
        }
    }
}

fn maybe_open_pending_session_picker(
    app: &mut Application,
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    status_state: &mut StatusState,
    response_in_flight: &mut bool,
    session_picker_state: &mut SessionPickerState,
) {
    if !session_picker_state.open_when_ready || *response_in_flight {
        return;
    }

    if request_session_picker_load_if_workspace_changed(
        chat_lines,
        req_tx,
        status_state,
        session_picker_state,
    )
    .is_some()
    {
        return;
    }

    match session_picker_state.load.clone() {
        SessionPickerLoad::Ready(_, entries) => {
            session_picker_state.open_when_ready = false;
            if entries.is_empty() {
                chat_lines
                    .borrow_mut()
                    .push("[session] no backend sessions returned".to_string());
                chat_lines.borrow_mut().push(String::new());
                return;
            }
            if let Some(entry) = run_session_picker(app, &entries) {
                match session_switch_command_for_picker_entry(&entry) {
                    Ok(cmdline) => dispatch_command(
                        &cmdline,
                        chat_lines,
                        req_tx,
                        status_state,
                        response_in_flight,
                    ),
                    Err(e) => {
                        chat_lines
                            .borrow_mut()
                            .push(format!("[session] cannot switch via picker: {e}"));
                        chat_lines.borrow_mut().push(String::new());
                    }
                }
            }
        }
        SessionPickerLoad::Error(_, message) => {
            session_picker_state.open_when_ready = false;
            chat_lines.borrow_mut().push(format!(
                "[session] could not load backend sessions: {message}"
            ));
            chat_lines.borrow_mut().push(String::new());
            session_picker_state.load = SessionPickerLoad::Idle;
        }
        SessionPickerLoad::Idle | SessionPickerLoad::Loading(_) => {}
    }
}

fn request_session_picker_load_if_workspace_changed(
    chat_lines: &Rc<RefCell<Vec<String>>>,
    req_tx: &mpsc::Sender<WorkerRequest>,
    status_state: &mut StatusState,
    session_picker_state: &mut SessionPickerState,
) -> Option<SubmissionStatus> {
    let workspace = status_state.session_picker_workspace();
    if session_picker_load_matches_workspace(&session_picker_state.load, &workspace) {
        return None;
    }
    session_picker_state.load = SessionPickerLoad::Idle;
    Some(request_session_picker_load(
        chat_lines,
        req_tx,
        status_state,
        session_picker_state,
    ))
}

fn session_switch_command_for_picker_entry(entry: &SessionPickerEntry) -> Result<String> {
    let id = entry.id.trim();
    if id.is_empty() || id.split_whitespace().count() != 1 {
        return Err(anyhow::anyhow!(
            "backend session id is not a single command token"
        ));
    }
    Ok(format!("/session switch {id}"))
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

/// Present a MenuBox-style modal picker of backend sessions.
///
/// The selected row maps back to the backend session id; the caller dispatches
/// `/session switch <id>` through the same worker/CommandHandler path as typed
/// slash commands so session binding remains centralized.
fn run_session_picker(
    app: &mut Application,
    entries: &[SessionPickerEntry],
) -> Option<SessionPickerEntry> {
    use turbo_vision::core::geometry::Point;

    const CMD_SESSION_SELECT_BASE: u16 = 1400;

    let items: Vec<MenuItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let label = format!(
                " {}  [{} / {}]  {}",
                e.name,
                empty_label(&e.provider),
                empty_label(&e.model),
                e.id
            );
            MenuItem::with_shortcut(&label, CMD_SESSION_SELECT_BASE + i as u16, 0, "", 0)
        })
        .collect();

    let (tw, th) = app.terminal.size();
    let w = tw;
    let h = th;
    let position = Point::new((w / 2) - 28, (h / 3).max(3));

    let menu = turbo_vision::core::menu_data::Menu::from_items(items);
    let mut menu_box = MenuBox::new(position, menu);
    let selected = menu_box.execute(&mut app.terminal);

    if selected == 0 {
        return None;
    }
    let idx = selected.checked_sub(CMD_SESSION_SELECT_BASE)? as usize;
    entries.get(idx).cloned()
}

fn empty_label(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value
    }
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
                let safe = sanitize_terminal_text(line);
                let truncated: String = safe.chars().take(width).collect();
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
    fn begin_turn_clears_stale_usage() {
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        state.usage = Some(TurnUsage {
            total_tokens: Some(2_000),
            context_window: Some(8_000),
            ..Default::default()
        });

        state.begin_turn();

        assert!(state.usage.is_none());
        assert!(state.turn_start.is_some());
    }

    #[test]
    fn clear_usage_chunk_resets_status_without_chat_output() {
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        state.usage = Some(TurnUsage {
            total_tokens: Some(2_000),
            context_window: Some(8_000),
            ..Default::default()
        });
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut typewriter_state = None;
        let mut response_in_flight = true;
        let mut session_picker_state = SessionPickerState::default();
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TurnChunk::ClearUsage).unwrap();

        assert!(!drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut session_picker_state
        ));

        assert!(state.usage.is_none());
        assert!(lines.borrow().is_empty());
        assert!(response_in_flight);
    }

    #[test]
    fn status_chunk_updates_workspace_and_model_without_chat_output() {
        let mut state = StatusState::new(
            "old".to_string(),
            "primary".to_string(),
            "borland".to_string(),
            false,
        );
        state.usage = Some(TurnUsage {
            total_tokens: Some(2_000),
            context_window: Some(8_000),
            ..Default::default()
        });
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut typewriter_state = None;
        let mut response_in_flight = true;
        let mut session_picker_state = SessionPickerState::default();
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TurnChunk::Status {
            workspace: Some("new".to_string()),
            model: Some("consult".to_string()),
        })
        .unwrap();

        assert!(!drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut session_picker_state
        ));

        assert_eq!(state.workspace, "new");
        assert_eq!(state.model, "consult");
        assert!(state.usage.is_none());
        assert!(lines.borrow().is_empty());
        assert!(response_in_flight);
    }

    #[test]
    fn submit_turn_result_gets_worker_fallback_chunks_only_without_finished() {
        let err: Result<String> = Err(anyhow::anyhow!("backend unavailable"));
        let err_chunks = submit_turn_fallback_chunks(&err, false, false);
        match err_chunks.as_slice() {
            [TurnChunk::Finished(Err(message))] => assert_eq!(message, "backend unavailable"),
            other => panic!("expected synthetic error Finished, got {other:?}"),
        }
        assert!(submit_turn_fallback_chunks(&err, true, false).is_empty());

        let ok: Result<String> = Ok("done".to_string());
        let ok_chunks = submit_turn_fallback_chunks(&ok, false, false);
        match ok_chunks.as_slice() {
            [TurnChunk::Token(text), TurnChunk::Finished(Ok(done))] => {
                assert_eq!(text, "done");
                assert!(done.is_empty());
            }
            other => panic!("expected synthetic success Token + Finished, got {other:?}"),
        }
        assert!(submit_turn_fallback_chunks(&ok, true, false).is_empty());

        let empty_ok: Result<String> = Ok(String::new());
        match submit_turn_fallback_chunks(&empty_ok, false, false).as_slice() {
            [TurnChunk::Finished(Ok(done))] => assert!(done.is_empty()),
            other => panic!("expected synthetic empty success Finished, got {other:?}"),
        }
    }

    #[test]
    fn ok_submit_turn_with_forwarded_tokens_finishes_with_partial_error() {
        let ok: Result<String> = Ok("complete response".to_string());

        match submit_turn_fallback_chunks(&ok, false, true).as_slice() {
            [TurnChunk::Finished(Err(message))] => {
                assert!(message.contains("partial response incomplete"));
            }
            other => panic!("expected synthetic partial error Finished, got {other:?}"),
        }
    }

    #[test]
    fn ok_submit_turn_fallback_renders_returned_text_before_finished() {
        let lines = Rc::new(RefCell::new(vec!["> hello".to_string(), String::new()]));
        let ok: Result<String> = Ok("complete response".to_string());

        for chunk in submit_turn_fallback_chunks(&ok, false, false) {
            apply_chunk(chunk, &lines);
        }

        assert_eq!(
            lines.borrow().as_slice(),
            ["> hello", "complete response", ""]
        );
    }

    #[test]
    fn token_chunks_render_terminal_controls_visibly() {
        let lines = Rc::new(RefCell::new(vec![String::new()]));

        apply_chunk(
            TurnChunk::Token("ok\u{1b}]52;c;owned\u{07}\nnext\u{9b}31m".to_string()),
            &lines,
        );

        let lines = lines.borrow();
        assert_eq!(lines[0], "ok<ESC>]52;c;owned^G");
        assert_eq!(lines[1], "next<0x9B>31m");
        assert!(!lines.iter().any(|line| {
            line.contains('\u{1b}') || line.contains('\u{07}') || line.contains('\u{9b}')
        }));
    }

    #[test]
    fn cached_typewriter_splash_sanitizes_terminal_controls() {
        let writer =
            TypewriterState::new("boot\u{1b}[31m\nready", vec!["after\u{07}".to_string()], 0);

        let text: String = writer.chars.iter().collect();
        assert_eq!(text, "boot<ESC>[31m\nready");
        assert_eq!(writer.after_lines, ["after^G"]);
    }

    #[test]
    fn session_picker_entry_builds_switch_command_from_backend_id() {
        let entry = SessionPickerEntry {
            id: "sess-123".to_string(),
            name: "scratch".to_string(),
            model: "gpt-test".to_string(),
            provider: "test".to_string(),
        };

        assert_eq!(
            session_switch_command_for_picker_entry(&entry).unwrap(),
            "/session switch sess-123"
        );
    }

    #[test]
    fn session_picker_entry_rejects_non_token_backend_id() {
        let entry = SessionPickerEntry {
            id: "bad id".to_string(),
            name: "scratch".to_string(),
            model: String::new(),
            provider: String::new(),
        };

        assert!(session_switch_command_for_picker_entry(&entry).is_err());
    }

    #[test]
    fn session_picker_open_enqueues_worker_load_without_backend_io_on_ui_thread() {
        let (tx, mut rx) = mpsc::channel(1);
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        let mut picker_state = SessionPickerState::default();

        let status = request_session_picker_load(&lines, &tx, &mut state, &mut picker_state);

        assert_eq!(status, SubmissionStatus::Started);
        let WorkerRequest::SessionPickerList(workspace) = rx.try_recv().unwrap() else {
            panic!("expected session picker load request");
        };
        assert_eq!(workspace.name, "default");
        assert_eq!(
            lines.borrow().as_slice(),
            [
                "[session] loading backend sessions...".to_string(),
                String::new()
            ]
        );
        assert_eq!(
            picker_state.load,
            SessionPickerLoad::Loading(SessionPickerWorkspace::new("default", None))
        );
        assert!(picker_state.open_when_ready);
    }

    #[test]
    fn session_picker_worker_chunk_populates_cached_results() {
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut typewriter_state = None;
        let mut response_in_flight = true;
        let mut picker_state = SessionPickerState {
            load: SessionPickerLoad::Loading(SessionPickerWorkspace::new("default", None)),
            open_when_ready: true,
        };
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TurnChunk::SessionPickerList(SessionPickerListResult {
            workspace: SessionPickerWorkspace::new("default", None),
            result: Ok(vec![Session {
                id: "sess-123".to_string(),
                name: "scratch".to_string(),
                model: "gpt-test".to_string(),
                provider: "test".to_string(),
            }]),
        }))
        .unwrap();

        assert!(!drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut picker_state
        ));

        match picker_state.load {
            SessionPickerLoad::Ready(workspace, entries) => {
                assert_eq!(workspace.name, "default");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].id, "sess-123");
            }
            other => panic!("expected ready picker entries, got {other:?}"),
        }
        assert!(picker_state.open_when_ready);
        assert!(response_in_flight);
        assert!(lines.borrow().is_empty());
    }

    #[test]
    fn session_picker_workspace_change_forces_fresh_load_instead_of_stale_ready_cache() {
        let (tx, mut rx) = mpsc::channel(1);
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut state = StatusState::new(
            "workspace-a".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        let mut picker_state = SessionPickerState {
            load: SessionPickerLoad::Loading(SessionPickerWorkspace::new("workspace-a", None)),
            open_when_ready: true,
        };

        apply_session_picker_result(
            SessionPickerListResult {
                workspace: SessionPickerWorkspace::new("workspace-a", None),
                result: Ok(vec![Session {
                    id: "stale-a-session".to_string(),
                    name: "scratch".to_string(),
                    model: "gpt-test".to_string(),
                    provider: "test".to_string(),
                }]),
            },
            &lines,
            &mut state,
            &mut picker_state,
        );
        assert!(matches!(&picker_state.load, SessionPickerLoad::Ready(_, _)));

        state.apply_status(Some("workspace-b".to_string()), None);

        let status = request_session_picker_load_if_workspace_changed(
            &lines,
            &tx,
            &mut state,
            &mut picker_state,
        );

        assert_eq!(status, Some(SubmissionStatus::Started));
        let WorkerRequest::SessionPickerList(workspace) = rx.try_recv().unwrap() else {
            panic!("expected fresh session picker load request");
        };
        assert_eq!(workspace.name, "workspace-b");
        assert_eq!(
            picker_state.load,
            SessionPickerLoad::Loading(SessionPickerWorkspace::new("workspace-b", None))
        );
        assert!(picker_state.open_when_ready);
    }

    #[test]
    fn session_picker_ignores_stale_worker_result_for_previous_workspace() {
        let mut state = StatusState::new(
            "workspace-b".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut typewriter_state = None;
        let mut response_in_flight = false;
        let mut picker_state = SessionPickerState {
            load: SessionPickerLoad::Loading(SessionPickerWorkspace::new("workspace-b", None)),
            open_when_ready: true,
        };
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TurnChunk::SessionPickerList(SessionPickerListResult {
            workspace: SessionPickerWorkspace::new("workspace-a", None),
            result: Ok(vec![Session {
                id: "stale-a-session".to_string(),
                name: "scratch".to_string(),
                model: "gpt-test".to_string(),
                provider: "test".to_string(),
            }]),
        }))
        .unwrap();

        assert!(!drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut picker_state
        ));

        assert_eq!(
            picker_state.load,
            SessionPickerLoad::Loading(SessionPickerWorkspace::new("workspace-b", None))
        );
        assert!(picker_state.open_when_ready);
        assert!(lines.borrow().is_empty());
    }

    #[test]
    fn empty_picker_labels_render_as_dash() {
        assert_eq!(empty_label(""), "-");
        assert_eq!(empty_label("  "), "-");
        assert_eq!(empty_label("openai"), "openai");
    }

    #[test]
    fn worker_submission_gate_blocks_second_command_placeholder() {
        let (tx, mut rx) = mpsc::channel(1);
        let lines = Rc::new(RefCell::new(vec![
            "> first".to_string(),
            "partial".to_string(),
        ]));
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        let mut response_in_flight = true;

        dispatch_command("/help", &lines, &tx, &mut state, &mut response_in_flight);

        assert!(response_in_flight);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            lines.borrow().as_slice(),
            ["> first".to_string(), "partial".to_string()]
        );
        assert_eq!(
            state.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some(RESPONSE_BUSY_TOAST)
        );
    }

    #[test]
    fn worker_backed_command_starts_busy_spinner_without_clearing_usage() {
        let (tx, mut rx) = mpsc::channel(1);
        let lines = Rc::new(RefCell::new(Vec::new()));
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        state.usage = Some(TurnUsage {
            total_tokens: Some(100),
            context_window: Some(1_000),
            ..Default::default()
        });
        let mut response_in_flight = false;

        dispatch_command("/help", &lines, &tx, &mut state, &mut response_in_flight);

        assert!(response_in_flight);
        assert!(state.spinner_char().is_some());
        assert!(state.usage.is_some());
        let WorkerRequest::Command(cmdline) = rx.try_recv().unwrap() else {
            panic!("expected command request");
        };
        assert_eq!(cmdline, "/help");
    }

    #[test]
    fn worker_submission_gate_blocks_second_turn_placeholder() {
        let (tx, mut rx) = mpsc::channel(1);
        let lines = Rc::new(RefCell::new(vec![
            "> first".to_string(),
            "partial".to_string(),
        ]));
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        let mut response_in_flight = true;

        let status = dispatch_worker_backed_submission(
            "second",
            WorkerRequest::Turn("second".to_string()),
            &lines,
            &tx,
            &mut state,
            &mut response_in_flight,
            true,
            None,
            "dispatch",
        );

        assert_eq!(status, SubmissionStatus::Busy);
        assert!(response_in_flight);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            lines.borrow().as_slice(),
            ["> first".to_string(), "partial".to_string()]
        );
        assert!(state.turn_start.is_none());
        assert_eq!(
            state.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some(RESPONSE_BUSY_TOAST)
        );
    }

    #[test]
    fn finished_chunk_clears_response_in_flight() {
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        state.begin_turn();
        let lines = Rc::new(RefCell::new(vec![
            "> first".to_string(),
            "done".to_string(),
        ]));
        let mut typewriter_state = None;
        let mut response_in_flight = true;
        let mut session_picker_state = SessionPickerState::default();
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TurnChunk::Finished(Ok(String::new()))).unwrap();

        assert!(!drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut session_picker_state
        ));

        assert!(!response_in_flight);
        assert!(state.turn_start.is_none());
        assert_eq!(
            lines.borrow().as_slice(),
            ["> first".to_string(), "done".to_string(), String::new()]
        );
    }

    #[test]
    fn worker_disconnect_clears_in_flight_turn() {
        let mut state = StatusState::new(
            "default".to_string(),
            "gpt-test".to_string(),
            "borland".to_string(),
            false,
        );
        state.begin_turn();
        let lines = Rc::new(RefCell::new(vec!["> prompt".to_string(), String::new()]));
        let mut typewriter_state = None;
        let mut response_in_flight = true;
        let mut session_picker_state = SessionPickerState::default();
        let (tx, mut rx) = mpsc::channel::<TurnChunk>(8);
        drop(tx);

        assert!(drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut session_picker_state
        ));

        assert!(!response_in_flight);
        assert!(state.turn_start.is_none());
        assert!(lines
            .borrow()
            .iter()
            .any(|line| line.contains("worker channel disconnected")));
    }

    #[tokio::test]
    async fn forward_turn_chunks_forwards_only_one_finished() {
        let (turn_tx, turn_rx) = mpsc::channel(8);
        let (ui_tx, mut ui_rx) = StreamSink::channel(8);
        let observed_finished = Arc::new(AtomicBool::new(false));
        let observed_finished_error = Arc::new(AtomicBool::new(false));
        let forwarded_token = Arc::new(AtomicBool::new(false));

        turn_tx
            .try_send(TurnChunk::Finished(Ok("first".to_string())))
            .unwrap();
        turn_tx
            .try_send(TurnChunk::Finished(Ok("second".to_string())))
            .unwrap();
        drop(turn_tx);

        assert!(
            forward_turn_chunks(
                turn_rx,
                ui_tx,
                Arc::clone(&observed_finished),
                Arc::clone(&observed_finished_error),
                Arc::clone(&forwarded_token),
            )
            .await
        );
        assert!(observed_finished.load(Ordering::Acquire));
        assert!(!observed_finished_error.load(Ordering::Acquire));
        assert!(!forwarded_token.load(Ordering::Acquire));

        match ui_rx.recv().await {
            Some(TurnChunk::Finished(Ok(text))) => assert_eq!(text, "first"),
            other => panic!("expected first Finished, got {other:?}"),
        }
        assert!(ui_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn forward_turn_chunks_tracks_forwarded_tokens_without_finished() {
        let (turn_tx, turn_rx) = mpsc::channel(8);
        let (ui_tx, mut ui_rx) = StreamSink::channel(8);
        let observed_finished = Arc::new(AtomicBool::new(false));
        let observed_finished_error = Arc::new(AtomicBool::new(false));
        let forwarded_token = Arc::new(AtomicBool::new(false));

        turn_tx
            .try_send(TurnChunk::Token("partial".to_string()))
            .unwrap();
        drop(turn_tx);

        assert!(
            !forward_turn_chunks(
                turn_rx,
                ui_tx,
                Arc::clone(&observed_finished),
                Arc::clone(&observed_finished_error),
                Arc::clone(&forwarded_token),
            )
            .await
        );
        assert!(!observed_finished.load(Ordering::Acquire));
        assert!(!observed_finished_error.load(Ordering::Acquire));
        assert!(forwarded_token.load(Ordering::Acquire));

        match ui_rx.recv().await {
            Some(TurnChunk::Token(text)) => assert_eq!(text, "partial"),
            other => panic!("expected forwarded token, got {other:?}"),
        }
        assert!(ui_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn stream_sink_full_queue_does_not_poison_shared_sender() {
        let (sink, mut rx) = StreamSink::channel(1);

        assert!(sink.send(TurnChunk::Token("queued".to_string())).is_ok());
        assert!(sink.send(TurnChunk::Token("overflow".to_string())).is_err());
        assert!(!sink.is_closed());

        match rx.recv().await {
            Some(TurnChunk::Token(text)) => assert_eq!(text, "queued"),
            other => panic!("expected queued token, got {other:?}"),
        }

        assert!(sink
            .send(TurnChunk::Token("after-drain".to_string()))
            .is_ok());
        match rx.recv().await {
            Some(TurnChunk::Token(text)) => assert_eq!(text, "after-drain"),
            other => panic!("expected token after drain, got {other:?}"),
        }

        drop(sink);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn reliable_finished_delivery_waits_for_saturated_ui_queue_and_clears_inflight() {
        let (sink, mut rx) = StreamSink::channel(1);
        sink.send(TurnChunk::Token("queued".to_string())).unwrap();

        let send_sink = sink.clone();
        let send_task =
            tokio::spawn(async move { send_worker_finished(&send_sink, Ok(String::new())).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!send_task.is_finished());

        match rx.recv().await {
            Some(TurnChunk::Token(text)) => assert_eq!(text, "queued"),
            other => panic!("expected saturated queue token, got {other:?}"),
        }
        assert!(send_task.await.unwrap());

        let lines = Rc::new(RefCell::new(vec![String::new()]));
        let mut state = StatusState::new(
            "workspace".to_string(),
            "primary".to_string(),
            "borland".to_string(),
            false,
        );
        let mut typewriter_state = None;
        let mut response_in_flight = true;
        let mut session_picker_state = SessionPickerState::default();

        assert!(!drain_stream_events(
            &mut rx,
            &lines,
            &mut state,
            &mut typewriter_state,
            &mut response_in_flight,
            &mut session_picker_state
        ));
        assert!(!response_in_flight);
    }

    #[tokio::test]
    async fn turn_stream_sink_overflow_rejects_later_finished_as_incomplete() {
        let (turn_sink, turn_rx) = StreamSink::turn_channel(1);
        let (ui_tx, mut ui_rx) = StreamSink::channel(8);
        let observed_finished = Arc::new(AtomicBool::new(false));
        let observed_finished_error = Arc::new(AtomicBool::new(false));
        let forwarded_token = Arc::new(AtomicBool::new(false));

        assert!(turn_sink
            .send(TurnChunk::Token("queued".to_string()))
            .is_ok());
        assert!(turn_sink
            .send(TurnChunk::Token("dropped".to_string()))
            .is_err());
        assert!(turn_sink
            .send(TurnChunk::Finished(Ok("".to_string())))
            .is_err());
        drop(turn_sink);

        let saw_finished = forward_turn_chunks(
            turn_rx,
            ui_tx,
            Arc::clone(&observed_finished),
            Arc::clone(&observed_finished_error),
            Arc::clone(&forwarded_token),
        )
        .await;
        assert!(!saw_finished);
        assert!(!observed_finished.load(Ordering::Acquire));
        assert!(!observed_finished_error.load(Ordering::Acquire));
        assert!(forwarded_token.load(Ordering::Acquire));

        match ui_rx.recv().await {
            Some(TurnChunk::Token(text)) => assert_eq!(text, "queued"),
            other => panic!("expected queued token, got {other:?}"),
        }

        let fallback = submit_turn_fallback_chunks(
            &Ok("complete backend text".to_string()),
            saw_finished,
            true,
        );
        assert!(
            matches!(fallback.as_slice(), [TurnChunk::Finished(Err(message))] if message.contains("partial response incomplete"))
        );
    }

    #[tokio::test]
    async fn forward_turn_chunks_coalesces_token_bursts() {
        let (turn_tx, turn_rx) = mpsc::channel(8);
        let (ui_tx, mut ui_rx) = StreamSink::channel(8);
        let observed_finished = Arc::new(AtomicBool::new(false));
        let observed_finished_error = Arc::new(AtomicBool::new(false));
        let forwarded_token = Arc::new(AtomicBool::new(false));

        turn_tx
            .try_send(TurnChunk::Token("hel".to_string()))
            .unwrap();
        turn_tx
            .try_send(TurnChunk::Token("lo".to_string()))
            .unwrap();
        drop(turn_tx);

        assert!(
            !forward_turn_chunks(
                turn_rx,
                ui_tx,
                Arc::clone(&observed_finished),
                Arc::clone(&observed_finished_error),
                Arc::clone(&forwarded_token),
            )
            .await
        );
        assert!(!observed_finished_error.load(Ordering::Acquire));
        assert!(forwarded_token.load(Ordering::Acquire));
        match ui_rx.recv().await {
            Some(TurnChunk::Token(text)) => assert_eq!(text, "hello"),
            other => panic!("expected coalesced token, got {other:?}"),
        }
        assert!(ui_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn forward_turn_chunks_caps_token_bytes_with_error() {
        let (turn_tx, turn_rx) = mpsc::channel(2);
        let (ui_tx, mut ui_rx) = StreamSink::channel(8);
        let observed_finished = Arc::new(AtomicBool::new(false));
        let observed_finished_error = Arc::new(AtomicBool::new(false));
        let forwarded_token = Arc::new(AtomicBool::new(false));

        turn_tx
            .try_send(TurnChunk::Token("x".repeat(TURN_STREAM_MAX_BYTES + 1)))
            .unwrap();
        drop(turn_tx);

        assert!(
            forward_turn_chunks(
                turn_rx,
                ui_tx,
                Arc::clone(&observed_finished),
                Arc::clone(&observed_finished_error),
                Arc::clone(&forwarded_token),
            )
            .await
        );
        assert!(observed_finished.load(Ordering::Acquire));
        assert!(observed_finished_error.load(Ordering::Acquire));
        assert!(!forwarded_token.load(Ordering::Acquire));
        match ui_rx.recv().await {
            Some(TurnChunk::Finished(Err(message))) => {
                assert!(message.contains("TUI stream limit"));
            }
            other => panic!("expected stream-limit error, got {other:?}"),
        }
        assert!(ui_rx.recv().await.is_none());
    }

    #[test]
    fn usage_clear_boundaries_include_session_workspace_and_model_switches() {
        assert!(should_clear_usage_after_command(true, false, false));
        assert!(should_clear_usage_after_command(false, true, false));
        assert!(should_clear_usage_after_command(false, false, true));
        assert!(!should_clear_usage_after_command(false, false, false));

        assert_eq!(model_switch_target("/models set primary"), Some("primary"));
        assert_eq!(model_switch_target("/model set fast"), Some("fast"));
        assert_eq!(model_switch_target("/models list"), None);
        assert!(successful_model_switch_command(
            "/models set primary",
            "✅ Active model key: primary\n"
        ));
        assert!(!successful_model_switch_command(
            "/models set missing",
            "❌ Failed to set model key: missing\n"
        ));
        assert!(!successful_model_switch_command(
            "/models set",
            "Usage: /models set <key>\n"
        ));
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
    fn typewriter_keeps_splash_lines_before_prompt_appended_mid_stream() {
        let lines = Rc::new(RefCell::new(vec![String::new()]));
        let mut writer = TypewriterState::new(
            "ab\ncd",
            vec!["workspace: default".to_string(), "model: test".to_string()],
            0,
        );

        writer.last_emit = Instant::now() - Duration::from_millis(90);
        writer.tick(&lines);
        {
            let mut lines = lines.borrow_mut();
            lines.push("> hello".to_string());
            lines.push(String::new());
        }

        writer.last_emit = Instant::now() - Duration::from_millis(300);
        writer.tick(&lines);

        let expected = vec![
            "ab".to_string(),
            "cd".to_string(),
            String::new(),
            "workspace: default".to_string(),
            "model: test".to_string(),
            "> hello".to_string(),
            String::new(),
        ];

        assert!(writer.completed);
        assert_eq!(lines.borrow().clone(), expected);
    }

    #[test]
    fn parses_theme_beep_toggle() {
        assert_eq!(parse_beep_toggle("beep on"), Some(true));
        assert_eq!(parse_beep_toggle("BEEP OFF"), Some(false));
        assert_eq!(parse_beep_toggle("amber"), None);
    }

    #[test]
    fn slash_popup_ignores_plain_slash() {
        assert!(should_open_slash_popup(KB_CTRL_K, true));
        assert!(!should_open_slash_popup(KB_SLASH, true));
        assert!(!should_open_slash_popup(KB_CTRL_K, false));
    }

    #[test]
    fn busy_modal_gate_blocks_menu_and_slash_popup_entry() {
        assert!(should_block_modal_entry_while_busy(
            &Event::keyboard(KB_F10),
            true
        ));
        assert!(should_block_modal_entry_while_busy(
            &Event::keyboard(KB_ALT_W),
            true
        ));
        assert!(should_block_modal_entry_while_busy(
            &Event::keyboard(KB_CTRL_K),
            true
        ));
        assert!(!should_block_modal_entry_while_busy(
            &Event::keyboard(KB_CTRL_K),
            false
        ));

        let mut click_menu_bar = Event::nothing();
        click_menu_bar.what = EventType::MouseDown;
        click_menu_bar.mouse.pos = turbo_vision::core::geometry::Point::new(2, 0);
        click_menu_bar.mouse.buttons = MB_LEFT_BUTTON;
        assert!(should_block_modal_entry_while_busy(&click_menu_bar, true));

        let mut click_chat = click_menu_bar;
        click_chat.mouse.pos = turbo_vision::core::geometry::Point::new(2, 3);
        assert!(!should_block_modal_entry_while_busy(&click_chat, true));
    }

    #[test]
    fn typed_exit_command_is_tui_local_quit() {
        assert!(is_exit_command("/exit"));
        assert!(is_exit_command("/exit now"));
        assert!(is_exit_command(" /exit"));
        assert!(!is_exit_command("/exitnow"));
        assert!(!is_exit_command("hello /exit"));
    }

    #[test]
    fn quit_paths_are_blocked_while_turn_is_in_flight() {
        assert!(quit_is_blocked_by_inflight_turn(true));
        assert!(!quit_is_blocked_by_inflight_turn(false));
    }

    #[test]
    fn session_action_carries_switch_and_create_intent() {
        assert_eq!(
            session_action("/session research"),
            Some(SessionAction::Switch {
                target: "research".to_string()
            })
        );
        assert_eq!(
            session_action("/session switch research"),
            Some(SessionAction::Switch {
                target: "research".to_string()
            })
        );
        assert_eq!(
            session_action("/session create scratch"),
            Some(SessionAction::Create {
                target: "scratch".to_string()
            })
        );
        assert_eq!(session_action("/session switch 'Research"), None);
        assert_eq!(
            session_action("/session switch 'Research Notes'"),
            Some(SessionAction::Switch {
                target: "Research Notes".to_string()
            })
        );
        assert_eq!(session_action("/session research notes"), None);
        assert_eq!(session_action("/session switch research notes"), None);
        assert_eq!(session_action("/session create scratch copy"), None);
        assert_eq!(session_action("/session list"), None);
        assert_eq!(session_action("/session info"), None);
        assert_eq!(session_action("/workspace switch prod"), None);
    }

    #[test]
    fn new_session_command_uses_explicit_create_and_nonce() {
        let now = DateTime::parse_from_rfc3339("2026-04-28T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        let first = new_session_command(now, uuid::Uuid::from_u128(1));
        let second = new_session_command(now, uuid::Uuid::from_u128(2));

        assert_eq!(
            first,
            "/session create session-20260428-123456-00000000000000000000000000000001"
        );
        assert_ne!(first, second);
        assert!(second.starts_with("/session create session-20260428-123456-"));
    }

    #[test]
    fn session_delete_target_parses_only_delete_commands() {
        assert_eq!(
            session_delete_target("/session delete sess-123"),
            Some("sess-123".to_string())
        );
        assert_eq!(
            session_delete_target("/session delete Research"),
            Some("Research".to_string())
        );
        assert_eq!(
            session_delete_target("/session delete 'Research Notes'"),
            Some("Research Notes".to_string())
        );
        assert_eq!(session_delete_target("/session delete 'Research"), None);
        assert_eq!(
            session_delete_target("/session delete Research Notes"),
            None
        );
        assert_eq!(session_delete_target("/session switch Research"), None);
        assert_eq!(session_delete_target("/workspace switch prod"), None);
        assert_eq!(session_delete_target("/session delete"), None);
    }

    #[test]
    fn active_worker_session_delete_target_resolves_backend_alias_before_matching_active() {
        let mut bindings = HashMap::new();
        remember_worker_session(
            &mut bindings,
            "default".to_string(),
            &Session {
                id: "sess-123".to_string(),
                name: "Research".to_string(),
                model: "m".to_string(),
                provider: "p".to_string(),
            },
        );
        let backend_sessions = vec![Session {
            id: "sess-123".to_string(),
            name: "Renamed Display".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
        }];

        assert_eq!(
            active_worker_session_delete_target_for_workspace(
                "sess-123",
                "default",
                &mut bindings,
                Some(&backend_sessions)
            ),
            Some("sess-123".to_string())
        );
        assert_eq!(
            active_worker_session_delete_target_for_workspace(
                "Renamed Display",
                "default",
                &mut bindings,
                Some(&backend_sessions)
            ),
            Some("Renamed Display".to_string())
        );
        assert_eq!(bindings["default"].name, "Renamed Display");
        assert_eq!(
            active_worker_session_delete_target_for_workspace(
                "Research",
                "default",
                &mut bindings,
                Some(&backend_sessions)
            ),
            None
        );
        assert_eq!(
            active_worker_session_delete_target_for_workspace(
                "sess-123",
                "other-workspace",
                &mut bindings,
                Some(&backend_sessions)
            ),
            None
        );
    }

    #[test]
    fn active_worker_session_delete_target_keeps_cached_fallback_without_backend() {
        let mut bindings = HashMap::new();
        remember_worker_session(
            &mut bindings,
            "default".to_string(),
            &Session {
                id: "sess-123".to_string(),
                name: "Research".to_string(),
                model: "m".to_string(),
                provider: "p".to_string(),
            },
        );

        assert_eq!(
            active_worker_session_delete_target_for_workspace(
                "Research",
                "default",
                &mut bindings,
                None
            ),
            Some("Research".to_string())
        );
    }

    #[test]
    fn workspace_switch_detection_requires_target() {
        assert_eq!(
            command_session_preflight("/workspace switch prod"),
            CommandSessionPreflight::AfterWorkspaceSwitch
        );
        assert_eq!(
            command_session_preflight("/workspaces switch prod"),
            CommandSessionPreflight::AfterWorkspaceSwitch
        );
        assert_eq!(
            command_session_preflight("/workspace switch"),
            CommandSessionPreflight::None
        );
        assert_eq!(
            command_session_preflight("/workspace list"),
            CommandSessionPreflight::None
        );
    }

    #[test]
    fn command_session_preflight_is_limited_to_session_dependent_commands() {
        for cmdline in [
            "/help",
            "/workspace list",
            "/workspace info",
            "/session list",
            "/session switch research",
            "/session research",
            "/models list",
            "/memory stats",
        ] {
            assert_eq!(
                command_session_preflight(cmdline),
                CommandSessionPreflight::None,
                "{cmdline}"
            );
        }

        assert_eq!(
            command_session_preflight("/info"),
            CommandSessionPreflight::BeforeDispatch
        );
        assert_eq!(
            command_session_preflight("/status"),
            CommandSessionPreflight::BeforeDispatch
        );
        assert_eq!(
            command_session_preflight("/session info"),
            CommandSessionPreflight::BeforeDispatch
        );
        assert_eq!(
            command_session_preflight("/session delete Research"),
            CommandSessionPreflight::BeforeDispatch
        );
        assert_eq!(
            command_session_preflight("/session delete 'Research"),
            CommandSessionPreflight::None
        );
        assert_eq!(
            command_session_preflight("/clear"),
            CommandSessionPreflight::BeforeDispatch
        );
        assert_eq!(
            command_session_preflight("/save out.txt"),
            CommandSessionPreflight::BeforeDispatch
        );
        assert_eq!(
            command_session_preflight("/workspace switch prod"),
            CommandSessionPreflight::AfterWorkspaceSwitch
        );
    }

    #[test]
    fn stdout_only_slash_blocker_allows_advertised_tui_commands() {
        for cmdline in [
            "/help",
            "/info",
            "/agent",
            "/cron list",
            "/clear",
            "/save out.txt",
            "/history",
            "/config",
            "/session delete sess-123",
            "/workspace switch prod",
            "/models set primary",
            "/memory list",
            "/doctor",
            "/skill list",
            "/channels list",
            "/hardware discover",
            "/peripheral list",
            "/estop status",
        ] {
            assert!(
                stdout_only_slash_command_block_message(cmdline).is_none(),
                "{cmdline} should route through CommandHandler instead of the stdout-only blocker"
            );
        }
    }

    #[test]
    fn worker_session_bindings_are_per_workspace() {
        let mut bindings = HashMap::new();
        let alpha = Session {
            id: "alpha-id".to_string(),
            name: "main".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
        };
        let beta = Session {
            id: "beta-id".to_string(),
            name: "main".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
        };

        remember_worker_session(&mut bindings, "alpha".to_string(), &alpha);
        remember_worker_session(&mut bindings, "beta".to_string(), &beta);

        assert_eq!(bindings["alpha"].id, "alpha-id");
        assert_eq!(bindings["beta"].id, "beta-id");
    }

    #[test]
    fn worker_session_resolution_fails_closed_when_backend_listing_fails() {
        let err =
            plan_worker_session_resolution("Research", Err(anyhow::anyhow!("backend unavailable")))
                .unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("could not list sessions from active backend"));
        assert!(msg.contains("backend unavailable"));
    }

    #[test]
    fn worker_session_resolution_creates_only_after_successful_absent_listing() {
        let resolution = plan_worker_session_resolution("Research", Ok(Vec::new()))
            .expect("successful empty backend listing should permit create");

        match resolution {
            WorkerSessionResolution::Create => {}
            WorkerSessionResolution::Existing(session) => {
                panic!("expected create plan, got existing session {}", session.id)
            }
        }
    }

    #[test]
    fn worker_session_resolution_switch_and_bare_prefer_existing_backend_match() {
        let sessions = vec![Session {
            id: "sess-123".to_string(),
            name: "Research".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
        }];

        let resolution = plan_worker_session_resolution("Research", Ok(sessions))
            .expect("successful backend listing should resolve existing session");

        match resolution {
            WorkerSessionResolution::Existing(session) => assert_eq!(session.id, "sess-123"),
            WorkerSessionResolution::Create => panic!("expected existing session resolution"),
        }
    }

    #[test]
    fn worker_session_metadata_save_failure_is_best_effort() {
        let session = Session {
            id: "sess-123".to_string(),
            name: "Research".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
        };
        let scope = storage::workspace_scope("zeroclaw", "default", None).unwrap();

        let saved = save_worker_session_metadata_best_effort_with(&scope, &session, |_, _| {
            Err(anyhow::anyhow!("disk full"))
        });

        assert!(!saved);
    }

    #[test]
    fn user_transcript_append_failure_is_returned_to_block_submit() {
        let scope = storage::workspace_scope("zeroclaw", "default", None).unwrap();

        let err = append_turn_transcript_entry(&scope, "../unsafe", "user", "secret").unwrap_err();

        assert!(err
            .to_string()
            .contains("could not append user transcript entry"));
        assert!(err.to_string().contains("unsafe session id"));
    }

    #[test]
    fn post_submit_transcript_failure_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "zeroclaw",
            &format!("transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let append_error = anyhow::anyhow!(
            "could not append assistant transcript entry for session main: disk full"
        );

        let message =
            mark_turn_transcript_incomplete_after_append_failure(&scope, "main", &append_error);

        assert!(message.contains("transcript marked incomplete"));
        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn turn_collection_overflow_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "openclaw",
            &format!("overflow-transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let reason =
            "openclaw: turn collection failed; accepted assistant turn exceeded cap".to_string();

        assert!(turn_collection_failure_requires_incomplete_transcript(
            &reason
        ));
        let message = mark_turn_transcript_incomplete_reason(&scope, "main", &reason);

        assert!(message.contains("transcript marked incomplete"));
        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn post_token_submit_error_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "zeroclaw",
            &format!("partial-error-transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let reason = "WebSocket read failed: reset".to_string();

        assert!(submit_error_requires_incomplete_transcript(&reason, true));
        let message = mark_turn_transcript_incomplete_reason(&scope, "main", &reason);

        assert!(message.contains("transcript marked incomplete"));
        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn oversized_response_submit_error_marks_history_incomplete_without_tokens() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "zeroclaw",
            &format!("oversized-webhook-transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let reason = "Webhook response exceeded 16 byte limit".to_string();

        assert!(submit_error_requires_incomplete_transcript(&reason, false));
        let message = mark_turn_transcript_incomplete_reason(&scope, "main", &reason);

        assert!(message.contains("transcript marked incomplete"));
        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn session_switch_resolver_prefers_exact_backend_id_over_duplicate_names() {
        let sessions = vec![
            Session {
                id: "sess-123".to_string(),
                name: "Research".to_string(),
                model: "m".to_string(),
                provider: "p".to_string(),
            },
            Session {
                id: "sess-456".to_string(),
                name: "Research".to_string(),
                model: "m".to_string(),
                provider: "p".to_string(),
            },
        ];

        let resolved = choose_worker_session_by_id_or_name(&sessions, "sess-456")
            .expect("id lookup should not be ambiguous")
            .expect("id should resolve");

        assert_eq!(resolved.id, "sess-456");
    }

    #[test]
    fn session_switch_resolver_fails_closed_on_duplicate_backend_names() {
        let sessions = vec![
            Session {
                id: "sess-123".to_string(),
                name: "Research".to_string(),
                model: "m".to_string(),
                provider: "p".to_string(),
            },
            Session {
                id: "sess-456".to_string(),
                name: "Research".to_string(),
                model: "m".to_string(),
                provider: "p".to_string(),
            },
        ];

        let err = choose_worker_session_by_id_or_name(&sessions, "Research").unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("ambiguous session name 'Research'"));
        assert!(msg.contains("sess-123"));
        assert!(msg.contains("sess-456"));
        assert!(msg.contains("explicit id"));
    }
}
