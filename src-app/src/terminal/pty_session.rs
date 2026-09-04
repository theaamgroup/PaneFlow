//! `TerminalState` and its PTY lifecycle - spawn, notifier wiring, event
//! processing, OSC channel drains, CWD resolution, scrollback I/O, and the
//! drop-time force-kill path.
//!
//! macOS only: POSIX syscalls (`libc::kill`, `proc_pidinfo`) sit behind
//! `#[cfg(unix)]` / `#[cfg(target_os = "macos")]`.
//!
//! Extracted from `terminal.rs` per US-012 of the src-app refactor PRD.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use futures::channel::mpsc::UnboundedReceiver;

use super::clipboard_gate::ClipboardGate;
use super::ghostty_session::{
    GhosttyInputSendResult, GhosttyRuntimePending, GhosttySession, GhosttyUiEvent,
    ProgramNotification, SpawnedGhostty,
};
use super::marks::SharedMarkRing;
use super::service_detector::{ServiceInfo, detect_framework, parse_service_line};
use super::shell::{resolve_default_shell, setup_shell_integration};
use super::types::{
    Content, GridLineText, GridMetrics, HyperlinkZone, Line, Modes, Point, SelectionGeometry,
    SelectionKind, SelectionRange, ShellQuoting, TerminalWindowSize,
};
use crate::agents::parent_guard::INHERITED_AGENT_SESSION_ENV;
use crate::limits::MAX_OSC52_BYTES;
use paneflow_config::schema::{Osc52ClipboardConfig, TerminalConfig, TerminalSurfaceProfile};
use paneflow_terminal_ghostty::Scroll as GhosttyScroll;

const ZDOTDIR_ENV: &str = "ZDOTDIR";
const PANEFLOW_ORIG_ZDOTDIR_ENV: &str = "PANEFLOW_ORIG_ZDOTDIR";

/// Default scrollback history length, in lines. Paneflow keeps this standard
/// for predictable terminal memory use. `TermConfig::default()` is `0`, which
/// disables scrollback entirely. Overridable via
/// `terminal.scrollback_lines` in `paneflow.json` - see
/// [`paneflow_config::TerminalConfig::resolved_scrollback_lines`].
const DEFAULT_SCROLLBACK_LINES: usize = TerminalConfig::DEFAULT_SCROLLBACK_LINES;
/// Host-terminal identity markers Paneflow inherits from whatever launched it.
///
/// A pane must look like a Paneflow surface, never like the terminal that
/// happened to start Paneflow. These are how TUI programs answer "which
/// emulator am I talking to", and Paneflow already owns the answer through
/// `TERM` / `TERM_PROGRAM` / `TERM_PROGRAM_VERSION`. Leaving a stale one in
/// place makes the child believe something else entirely, and two agent CLIs
/// change their wire behavior on exactly these bytes:
///
/// - `TMUX` / `STY` / `ZELLIJ`: Claude Code and Codex both wrap their OSC
///   notifications in multiplexer passthrough (`ESC P tmux ; ESC …`), which
///   libghostty does not unwrap - the notification is swallowed whole.
/// - `KITTY_WINDOW_ID` / `TERMINAL_EMULATOR` / `VTE_VERSION` / `ConEmu*`:
///   same class of capability probe, no known agent-visible breakage today,
///   dropped for the same reason - the answer they give is about a terminal
///   that is not rendering this pane.
///
/// `ConEmu*` is matched by prefix (`ConEmuANSI`, `ConEmuPID`, `ConEmuTask`,
/// `ConEmuBuild`, …). Matching is ASCII-case-insensitive because these arrive
/// by INHERITANCE, in whatever casing the host set them, not through the
/// upper-cased user-env merge.
const INHERITED_HOST_TERMINAL_ENV: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "STY",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
    "KITTY_WINDOW_ID",
    "KITTY_LISTEN_ON",
    "TERMINAL_EMULATOR",
    "VTE_VERSION",
    "ITERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "ALACRITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
];
const MAX_PENDING_CLIPBOARD_OPS: usize = 8;
const MAX_PENDING_NOTIFICATIONS: usize = 8;

/// Read the user's configured scrollback length, clamped to the
/// [`paneflow_config::TerminalConfig`] allowed range. Falls back to
/// [`DEFAULT_SCROLLBACK_LINES`] when no `terminal` block exists.
fn resolved_scrollback_lines(profile: TerminalSurfaceProfile) -> usize {
    paneflow_config::loader::load_config()
        .terminal
        .unwrap_or(TerminalConfig {
            scrollback_lines: Some(DEFAULT_SCROLLBACK_LINES),
            ..Default::default()
        })
        .resolved_scrollback_lines_for_profile(profile)
}

/// Cloneable renderer-facing session facade. The concrete Ghostty handles stay
/// private to this backend module; GPUI receives only Paneflow-owned commands
/// and snapshots.
#[derive(Clone)]
pub(crate) struct TerminalSessionBackend {
    ghostty: GhosttySession,
}

/// A UI-facing event published by the terminal engine.
pub(crate) struct TerminalBackendEvent(GhosttyUiEvent);

impl TerminalBackendEvent {
    pub(crate) fn is_wakeup(&self) -> bool {
        self.0.is_wakeup()
    }
}

/// The event stream a view polls, taken once from [`TerminalState`].
pub(crate) struct TerminalBackendEvents(Option<UnboundedReceiver<GhosttyUiEvent>>);

impl futures::Stream for TerminalBackendEvents {
    type Item = TerminalBackendEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let Some(receiver) = self.0.as_mut() else {
            return std::task::Poll::Pending;
        };
        match std::pin::Pin::new(receiver).poll_next(cx) {
            std::task::Poll::Ready(Some(event)) => {
                std::task::Poll::Ready(Some(TerminalBackendEvent(event)))
            }
            // A closed engine channel must not end the view's stream: the
            // surface stays mounted so the exit overlay remains visible.
            std::task::Poll::Ready(None) | std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl futures::stream::FusedStream for TerminalBackendEvents {
    fn is_terminated(&self) -> bool {
        false
    }
}

/// The runtime handle produced alongside a not-yet-started terminal, handed to
/// the background half of a spawn.
pub(crate) struct PendingTerminalBackend {
    pub(super) ghostty: GhosttyRuntimePending,
}

#[cfg(test)]
static RENDER_CONTENT_TIMING_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static RENDER_CONTENT_LOCK_DURATIONS: std::sync::Mutex<Vec<std::time::Duration>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn start_render_content_timing_probe() {
    let mut durations = RENDER_CONTENT_LOCK_DURATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    durations.clear();
    RENDER_CONTENT_TIMING_ENABLED.store(true, std::sync::atomic::Ordering::Release);
}

#[cfg(test)]
pub(crate) fn take_render_content_lock_durations() -> Vec<std::time::Duration> {
    RENDER_CONTENT_TIMING_ENABLED.store(false, std::sync::atomic::Ordering::Release);
    let mut durations = RENDER_CONTENT_LOCK_DURATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *durations)
}

impl TerminalSessionBackend {
    fn new(ghostty: GhosttySession) -> Self {
        Self { ghostty }
    }

    /// Resize and snapshot in one runtime round-trip, then return owned neutral
    /// content. No engine handle or borrowed grid data crosses this call.
    pub(crate) fn render_content(
        &self,
        window_size: TerminalWindowSize,
        first_visible_row: i32,
        last_visible_row: i32,
        clear_on_resize: bool,
    ) -> (Content, bool) {
        // The render thread pays for this call once per pane per frame, and the
        // Ghostty state lock is held inside it. Time the whole round-trip so the
        // eight-pane performance gate keeps a lock-contention signal.
        #[cfg(test)]
        let snapshot_started_at = RENDER_CONTENT_TIMING_ENABLED
            .load(std::sync::atomic::Ordering::Acquire)
            .then(std::time::Instant::now);
        let rendered = self.ghostty.render_content(
            window_size,
            first_visible_row,
            last_visible_row,
            clear_on_resize,
        );
        #[cfg(test)]
        if let Some(snapshot_started_at) = snapshot_started_at {
            RENDER_CONTENT_LOCK_DURATIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(snapshot_started_at.elapsed());
        }
        rendered
    }

    pub(crate) fn notify_window_size(&self, size: TerminalWindowSize) {
        self.ghostty.resize(size);
    }

    pub(crate) fn modes(&self) -> Modes {
        self.ghostty.modes()
    }

    pub(crate) fn grid_metrics(&self) -> GridMetrics {
        self.ghostty.grid_metrics()
    }

    pub(crate) fn clear_history(&self) {
        self.ghostty.clear_history();
    }

    pub(crate) fn scroll_to_bottom(&self) -> bool {
        self.scroll(GhosttyScroll::Bottom)
    }

    pub(crate) fn scroll_delta(&self, delta: i32) -> bool {
        self.scroll(GhosttyScroll::Delta(delta))
    }

    pub(crate) fn scroll_page_up(&self) -> bool {
        let lines = i32::try_from(self.grid_metrics().screen_lines).unwrap_or(i32::MAX);
        self.scroll(GhosttyScroll::Delta(lines))
    }

    pub(crate) fn scroll_page_down(&self) -> bool {
        let lines = i32::try_from(self.grid_metrics().screen_lines).unwrap_or(i32::MAX);
        self.scroll(GhosttyScroll::Delta(-lines))
    }

    fn scroll(&self, scroll: GhosttyScroll) -> bool {
        // Shared metrics can lag commands already accepted by the worker.
        // Queue every non-zero gesture and let Ghostty's live viewport perform
        // the authoritative boundary clamp in FIFO order.
        if matches!(scroll, GhosttyScroll::Delta(0)) {
            return false;
        }
        self.ghostty.scroll(scroll)
    }

    pub(crate) fn restore_display_offset(&self, target: usize) -> bool {
        let metrics = self.ghostty.grid_metrics();
        let history_size = usize::try_from(-i64::from(metrics.topmost_line.0)).unwrap_or(0);
        let row = history_size.saturating_sub(target.min(history_size));
        self.ghostty.scroll_to_viewport_row(row)
    }

    pub(crate) fn scroll_to_viewport_row(&self, row: usize) -> bool {
        self.ghostty.scroll_to_viewport_row(row)
    }

    /// Where the grid sits right now, as one snapshot for a pointer event.
    pub(crate) fn selection_geometry(
        &self,
        cell_width: f32,
        line_height: f32,
    ) -> SelectionGeometry {
        let metrics = self.ghostty.grid_metrics();
        SelectionGeometry {
            columns: metrics.columns,
            screen_lines: metrics.screen_lines,
            display_offset: metrics.display_offset,
            cell_width,
            line_height,
        }
    }

    pub(crate) fn press_selection(&self, kind: SelectionKind, point: Point, position: (f32, f32)) {
        self.ghostty.press_selection(kind, point, position);
    }

    pub(crate) fn drag_selection(
        &self,
        point: Point,
        position: (f32, f32),
        geometry: SelectionGeometry,
        rectangle: bool,
    ) {
        self.ghostty
            .drag_selection(point, position, geometry, rectangle);
    }

    pub(crate) fn release_selection(&self, point: Option<Point>) {
        self.ghostty.release_selection(point);
    }

    pub(crate) fn selection_text(&self) -> Option<String> {
        self.ghostty.selection_text()
    }

    pub(crate) fn finish_selection(&self) -> (bool, Option<String>) {
        let copied = self.ghostty.selection_text();
        let is_empty = copied.as_ref().is_none_or(String::is_empty);
        self.ghostty.clear_selection();
        (is_empty, copied)
    }

    pub(crate) fn clear_selection(&self) {
        self.ghostty.clear_selection();
    }

    /// Select the whole grid (history and screen). Backs Edit ' Select All.
    pub(crate) fn select_all(&self) {
        self.ghostty.select_all();
    }

    /// Start resolving the OSC 8 hyperlink under `point`; the answer lands in
    /// [`TerminalState::take_resolved_hover_link`] on a later event batch.
    pub(crate) fn request_osc8_hyperlink_at(&self, point: Point) -> bool {
        self.ghostty.request_hyperlink_at(point)
    }

    pub(crate) fn line_text_at(&self, point: Point) -> Option<GridLineText> {
        self.ghostty.line_text_at(point)
    }

    pub(crate) fn move_copy_cursor(&self, current: Point, dx: i32, dy: i32, extend: bool) -> Point {
        let metrics = self.ghostty.grid_metrics();
        let column = (current.column.0 as i32 + dx)
            .clamp(0, metrics.columns.saturating_sub(1) as i32) as usize;
        let line = (current.line.0 + dy).clamp(metrics.topmost_line.0, metrics.bottommost_line.0);
        let next = Point::new(line, column);
        if extend {
            // The keyboard has no pointer geometry, so the cursor drives the
            // gesture through a synthetic one-pixel-per-cell grid: geometry is
            // only ever read to arbitrate half-cells and viewport exits, and a
            // keyboard extend does neither.
            let geometry = self.selection_geometry(1.0, 1.0);
            if self.ghostty.selection_range().is_none() {
                self.ghostty
                    .press_selection(SelectionKind::Simple, current, (0.0, 0.0));
            }
            self.ghostty
                .drag_selection(next, (0.0, 0.0), geometry, false);
        } else {
            self.ghostty.clear_selection();
        }
        next
    }

    pub(crate) fn selection_range(&self) -> Option<SelectionRange> {
        self.ghostty.selection_range()
    }

    pub(crate) fn bottommost_line(&self) -> Line {
        self.ghostty.grid_metrics().bottommost_line
    }

    /// Uncancellable twin of [`Self::search_with_cancel`]; production paths
    /// (find bar, fleet search, IPC) all carry a cancel flag.
    #[cfg(test)]
    pub(crate) fn search(&self, query: &str, regex: bool) -> crate::search::SearchResult {
        self.search_with_cancel(query, regex, &std::sync::atomic::AtomicBool::new(false))
    }

    pub(crate) fn search_with_cancel(
        &self,
        query: &str,
        regex: bool,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> crate::search::SearchResult {
        self.ghostty.search_with_cancel(query, regex, cancelled)
    }

    pub(crate) fn set_default_cursor(
        &self,
        shape: paneflow_terminal_ghostty::CursorShape,
        blink: bool,
    ) -> bool {
        self.ghostty.set_default_cursor(shape, blink)
    }

    /// Kitty graphics placements for the published frame, already resolved
    /// and uploaded by the session runtime.
    pub(crate) fn kitty_placements(
        &self,
    ) -> std::sync::Arc<[crate::terminal::kitty::KittyPlacement]> {
        self.ghostty.kitty_placements()
    }

    pub(crate) fn refresh_appearance(&self) -> bool {
        self.ghostty.refresh_appearance()
    }

    pub(crate) fn scroll_to_match(&self, search_match: &crate::search::SearchMatch) -> usize {
        let metrics = self.ghostty.grid_metrics();
        let target = (metrics.bottommost_line.0 - search_match.start.line.0).max(0) as usize;
        let _ = self.restore_display_offset(target);
        target
    }
}

// ---------------------------------------------------------------------------
// OSC 52 clipboard mode
// ---------------------------------------------------------------------------

/// Controls OSC 52 clipboard access. Default: CopyOnly (write-only).
/// Read path (CopyPaste) is a security risk - clipboard exfiltration.
#[derive(Clone, Copy, PartialEq)]
pub enum Osc52Mode {
    Disabled,
    CopyOnly,
    CopyPaste,
}

impl Osc52Mode {
    /// Resolve the user's `terminal.osc52_clipboard` policy from a config
    /// snapshot. `None` (block or key absent) keeps the copy-only default.
    pub(super) fn from_config(config: &paneflow_config::schema::PaneFlowConfig) -> Self {
        let policy = config
            .terminal
            .as_ref()
            .map(TerminalConfig::resolved_osc52_clipboard)
            .unwrap_or_default();
        match policy {
            Osc52ClipboardConfig::CopyOnly => Self::CopyOnly,
            Osc52ClipboardConfig::Disabled => Self::Disabled,
        }
    }
}
// ---------------------------------------------------------------------------
// Terminal state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhosttyBuildDiagnostics {
    pub version: &'static str,
    pub source_sha: &'static str,
    pub api_version: &'static str,
    pub zig_version: &'static str,
    pub optimization: &'static str,
    pub simd: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "mirrors the engine's failure phases in full; not every phase is reachable on macOS"
)]
pub enum TerminalBackendFailurePhase {
    Initialization,
    OpenPty,
    Spawn,
    PostSpawn,
}

impl TerminalBackendFailurePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialization => "initialization",
            Self::OpenPty => "open_pty",
            Self::Spawn => "spawn",
            Self::PostSpawn => "post_spawn",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalBackendFailureDiagnostics {
    pub phase: TerminalBackendFailurePhase,
    pub reason_code: &'static str,
    pub os_error: Option<i32>,
}

#[allow(
    dead_code,
    reason = "mirrors the engine's reason codes in full; not every code is reachable on macOS"
)]
impl TerminalBackendFailureDiagnostics {
    pub(super) const GHOSTTY_INITIALIZATION_FAILED: &'static str = "ghostty_initialization_failed";
    pub(super) const GHOSTTY_OPEN_PTY_FAILED: &'static str = "ghostty_open_pty_failed";
    pub(super) const GHOSTTY_SPAWN_FAILED: &'static str = "ghostty_spawn_failed";
    pub(super) const GHOSTTY_POST_SPAWN_FAILED: &'static str = "ghostty_post_spawn_failed";

    pub(super) fn new(
        phase: TerminalBackendFailurePhase,
        reason_code: &'static str,
        os_error: Option<i32>,
    ) -> Self {
        Self {
            phase,
            reason_code,
            os_error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalBackendDiagnostics {
    pub failure: Option<TerminalBackendFailureDiagnostics>,
    pub target_triple: &'static str,
    pub ghostty: GhosttyBuildDiagnostics,
}

impl std::fmt::Display for TerminalBackendDiagnostics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (failure_phase, reason_code, os_error) =
            self.failure
                .as_ref()
                .map_or(("none", "none", None), |failure| {
                    (
                        failure.phase.as_str(),
                        failure.reason_code,
                        failure.os_error,
                    )
                });
        write!(
            formatter,
            "backend=ghostty failure_phase={failure_phase} reason_code={reason_code} target={} os_error=",
            self.target_triple
        )?;
        match os_error {
            Some(code) => write!(formatter, "{code}")?,
            None => formatter.write_str("none")?,
        }
        write!(
            formatter,
            " ghostty_version={} ghostty_source_sha={} ghostty_api_version={} zig_version={} optimization={} simd={}",
            self.ghostty.version,
            self.ghostty.source_sha,
            self.ghostty.api_version,
            self.ghostty.zig_version,
            self.ghostty.optimization,
            self.ghostty.simd,
        )
    }
}

pub(super) fn raw_os_error_from_anyhow(error: &anyhow::Error) -> Option<i32> {
    error.chain().find_map(|source| {
        source
            .downcast_ref::<io::Error>()
            .and_then(io::Error::raw_os_error)
    })
}

#[derive(Clone)]
enum PendingTerminalInput {
    Raw(Cow<'static, [u8]>),
    Key(paneflow_terminal_ghostty::KeyInput),
    Mouse {
        input: paneflow_terminal_ghostty::MouseInput,
        repeat: usize,
    },
    Focus(paneflow_terminal_ghostty::FocusEvent),
    Paste {
        text: String,
        allow_unsafe: bool,
    },
}

impl PendingTerminalInput {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Raw(bytes) => bytes.len(),
            Self::Key(input) => std::mem::size_of::<paneflow_terminal_ghostty::KeyInput>()
                .saturating_add(input.text.len()),
            Self::Mouse { repeat, .. } => {
                std::mem::size_of::<paneflow_terminal_ghostty::MouseInput>().saturating_add(*repeat)
            }
            Self::Focus(_) => std::mem::size_of::<paneflow_terminal_ghostty::FocusEvent>(),
            Self::Paste { text, .. } => text.len(),
        }
    }

    /// Control events (key and mouse *releases*, focus reports) may use the
    /// whole queue; text-bearing input stops one reserve short. A flood of
    /// typing or a huge paste therefore can never starve the events that
    /// unstick a held modifier or end a drag.
    fn queue_limit(&self) -> usize {
        match self {
            Self::Raw(_) | Self::Paste { .. } => {
                MAX_PENDING_INPUT_BYTES - INPUT_CONTROL_RESERVE_BYTES
            }
            Self::Key(input) if input.action == paneflow_terminal_ghostty::KeyAction::Release => {
                MAX_PENDING_INPUT_BYTES
            }
            Self::Mouse { input, .. }
                if input.action == paneflow_terminal_ghostty::MouseAction::Release =>
            {
                MAX_PENDING_INPUT_BYTES
            }
            Self::Focus(_) => MAX_PENDING_INPUT_BYTES,
            Self::Key(_) | Self::Mouse { .. } => {
                MAX_PENDING_INPUT_BYTES - INPUT_CONTROL_RESERVE_BYTES
            }
        }
    }

    fn fits_after(&self, queued_bytes: usize) -> bool {
        queued_bytes.saturating_add(self.queued_bytes()) <= self.queue_limit()
    }

    fn try_send(&self, ghostty: &GhosttySession) -> GhosttyInputSendResult {
        match self {
            Self::Raw(bytes) => ghostty.write(bytes.clone().into_owned()),
            Self::Key(input) => ghostty.write_key(input.clone()),
            Self::Mouse { input, repeat } => ghostty.write_mouse(*input, *repeat),
            Self::Focus(event) => ghostty.write_focus(*event),
            Self::Paste { text, allow_unsafe } => ghostty.write_paste(text.clone(), *allow_unsafe),
        }
    }
}

/// Why an IPC write was not queued (see [`TerminalState::try_write_to_pty`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtyWriteError {
    /// The engine rejected the bytes: queue limit reached, runtime gone, the
    /// pending-input lock poisoned, or the spawn failed and there is no child
    /// to receive them.
    Rejected,
}

impl std::fmt::Display for PtyWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("terminal input rejected")
    }
}

impl std::error::Error for PtyWriteError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendInputResult {
    Accepted,
    Rejected,
}

pub struct TerminalState {
    /// The libghostty engine: VT parser, grid, PTY, and child process. Built
    /// display-only by [`build_display_only`](Self::build_display_only), then
    /// either started against a real child ([`GhosttySession::start`]) or kept
    /// as a pure grid ([`GhosttySession::start_display`]) for restored
    /// scrollback and the spawn-failure pane.
    ghostty: GhosttySession,
    /// UI event stream published by the engine, taken once by the view
    /// through [`take_backend_events`](Self::take_backend_events).
    ghostty_events_rx: Option<UnboundedReceiver<GhosttyUiEvent>>,
    /// Set when a spawn failed, so the surface can report *why* the pane holds
    /// an error instead of a shell.
    backend_failure: Option<TerminalBackendFailureDiagnostics>,
    /// The one warning [`Self::dispatch_ghostty_input`] logs for input that
    /// reaches a spawn-failed pane; every later rejection is silent.
    spawn_failure_input_warned: std::sync::atomic::AtomicBool,
    pub(crate) marks: SharedMarkRing,
    /// Watermark over [`super::marks::MarkRing::prompt_start_seq`], read by
    /// [`Self::take_shell_prompt_ready`].
    last_prompt_seq: u64,
    pub exited: Option<i32>,
    /// Latest answer to a hover hyperlink lookup, until the view takes it.
    resolved_hover_link: Option<(Point, Option<HyperlinkZone>)>,
    /// US-002: set true once any user input (keystroke, paste, mouse report,
    /// IME commit, user scroll) has been written via `write_to_pty`.
    /// Distinguishes a user-initiated exit (always close the pane) from a
    /// spawn/launch failure (keep the pane open so the exit overlay is
    /// visible). Atomic because `write_to_pty` takes `&self`. Mirrors Zed's
    /// keyboard_input_sent (crates/terminal/src/terminal.rs:2572-2576).
    keyboard_input_sent: std::sync::atomic::AtomicBool,
    /// EP-002 US-005: numeric signal + name if the child was terminated by a
    /// signal (crash), formatted "N (Name)" e.g. "11 (Segmentation fault)".
    /// `None` for a normal code exit. The engine resolves both from the
    /// child's wait status and publishes them with `ChildExited`. Rendered by
    /// the exit overlay to flag a crash.
    pub exit_signal: Option<String>,
    /// PID of the shell child process, used for port detection.
    pub child_pid: u32,
    /// Spawn-time start pin for `child_pid` (`pbi_start_tvsec`/`tvusec`), so a
    /// recycled PID can never be signalled as if it were ours.
    pub child_proc_start: Option<u64>,
    /// App-owned duplicate of the PTY master. The runtime thread owns the
    /// engine's copy; this one keeps `tcgetpgrp` and session enumeration
    /// valid in `Drop` after the runtime has closed its own.
    pty_master_fd: Option<OwnedFd>,
    /// GPUI executor for the 100 ms SIGKILL escalation in `Drop`; a detached
    /// thread is the fallback when none was wired (tests, display-only).
    background_executor: Option<gpui::BackgroundExecutor>,
    /// Per-surface cursor colour override mirrored from the view so the
    /// element paints it without a view round-trip.
    pub(super) cursor_color_override: Option<gpui::Hsla>,
    /// Terminal title set via OSC 0/2 escape sequences (e.g. shell prompt, Claude Code).
    pub title: String,
    /// Current working directory of the shell process. the
    /// engine decodes OSC 7 and publishes it as a UI event; Unix/macOS also
    /// refresh from the process table via `cwd_now()`.
    pub current_cwd: Option<String>,
    /// Latest OSC 9;4 progress report the running program published. Returns
    /// to `None` as soon as the program asks for the indicator to be removed
    /// or the child exits.
    pub progress: Option<paneflow_terminal_ghostty::ProgressReport>,
    /// User-assigned custom name (US-013). When `Some`, it overrides the
    /// auto-derived surface name in `surface.list` / MCP / the sidebar, and is
    /// persisted to `session.json`. `None` falls back to derivation.
    pub custom_name: Option<String>,
    /// EP-005 US-013: agent CLI detected in this terminal's PTY subtree by
    /// the per-pane scan - PID-authoritative, never the spoofable OSC
    /// title. Drives the tab identity pill; persisted to `session.json`
    /// as the agent's stable `tag()`.
    pub detected_agent: Option<crate::agent_launcher::TerminalAgent>,
    /// US-013: `false` while `detected_agent` is a declared or
    /// session-restored "last known" value awaiting its first scan
    /// confirmation; flipped `true` (or the agent cleared) by every scan
    /// deposit.
    pub agent_confirmed: bool,
    /// Deadline protecting a *launch-declared* `detected_agent` from being
    /// cleared by a scan that ran before the CLI process existed.
    ///
    /// Paneflow knows which agent a launch is about to run (cmux's declared
    /// `SessionAgent`), so the surface carries its identity from frame zero -
    /// but the shell still needs a moment to start and `exec` the binary, and
    /// the first scan lands inside that window with an empty subtree. Without
    /// a grace period the deposit would clear the declaration and the logo
    /// would flicker off then back on.
    ///
    /// Set by [`TerminalView::declare_agent`] only. A restored "last known"
    /// value leaves it `None` and is still cleared by the first scan, because
    /// nothing is being launched there. Cleared as soon as any scan resolves
    /// the surface either way.
    pub agent_declared_until: Option<std::time::Instant>,
    /// EP-005 US-014: LISTEN ports attributed to this terminal's PTY
    /// subtree by the per-pane scan, each with the clickable frontend URL
    /// when the workspace's `service_labels` knows one. Sorted by port,
    /// deduplicated, and kept as live resource state rather than persisted.
    pub detected_ports: Vec<(u16, Option<String>)>,
    /// US-014: ports whose service URL was announced in THIS terminal
    /// while the LISTEN socket belongs to another pane's subtree -
    /// `(port, owner pane display name)`. Info-level heuristic; known
    /// false positives (proxies, port-forwards, re-announcements) are
    /// tolerated in v1.
    pub port_conflicts: Vec<(u16, String)>,
    /// US-014: ports announced by service URLs detected in this terminal's
    /// own output (untrusted text - used only to cross-check against the
    /// scan's LISTEN attribution, never to open anything). Bounded.
    pub announced_ports: Vec<u16>,
    /// EP-006 US-019: per-pane font-size override in points. `None` follows
    /// the global config (and live global changes); `Some` is clamped to
    /// [8.0, 32.0] at every write site (zoom actions, session ingress) and
    /// wins over the global. Persisted to `session.json`.
    pub font_size_override: Option<f32>,
    /// OSC 52 clipboard access mode (default: copy-only for security).
    pub osc52_mode: Osc52Mode,
    /// The configured `terminal.osc52_clipboard` policy, resolved from the
    /// spawn's config snapshot and installed by
    /// [`promote_ghostty`](Self::promote_ghostty) once the child exists.
    spawn_osc52_mode: Osc52Mode,
    /// OSC 52 is accepted only while this terminal owns focus. Updated from
    /// the GPUI focus transition before any focus protocol report is queued.
    terminal_focused: bool,
    /// Shared with the engine so focus and policy are checked when OSC 52 is
    /// emitted, before the asynchronous UI event queue.
    clipboard_gate: Arc<ClipboardGate>,
    /// Shell syntax used when Paneflow inserts OS file paths into the PTY.
    pub(super) shell_quoting: ShellQuoting,
    /// Clipboard payloads deferred from sync() - drained in the poll loop
    /// where cx is available for the clipboard write.
    pub(super) pending_clipboard_ops: Vec<String>,
    /// Desktop notifications the running program asked for with OSC 9 or
    /// OSC 777, drained by the view, which is where the config gate and the
    /// background executor live.
    pub(super) pending_notifications: Vec<ProgramNotification>,
    /// Foreground command cached by the off-thread pane process scanner.
    /// `surface.list` reads this synchronously, so it must never perform
    /// process-table I/O on the GPUI thread.
    pub cached_foreground_command: Option<String>,
    #[cfg(all(unix, not(test)))]
    pty_guard: Option<crate::agents::parent_guard::PtyGuardHandle>,
    /// Whether the terminal wants the cursor to blink.
    pub cursor_blinking: bool,
    /// Set when PTY output has been processed (Wakeup event received).
    /// Cleared after cx.notify() triggers a repaint.
    pub dirty: bool,
    /// US-010 (cli-agent-orchestration): monotonic count of processed
    /// PTY-output events. Never reset. `workspace.up` polls this as a
    /// readiness signal for prompt prefill - it is the only screen-agnostic
    /// "the agent produced output" signal available: `dirty` is cleared on
    /// every repaint, and `extract_scrollback` misses content painted on the
    /// alternate screen (where TUI agents live).
    pub output_generation: u64,
    /// Leading-edge throttle for ActivityBurst/service-scan emission
    /// (view.rs): when the last burst was emitted for this terminal.
    pub(super) last_activity_burst: Option<std::time::Instant>,
    /// EP-002 US-007: throttle counter for the proc-based CWD refresh in
    /// `sync_channels`, the fallback for shells that never emit OSC 7.
    cwd_poll_ticks: u32,
    /// Ports already reported via ServiceDetected (dedup guard).
    /// Cleared on ChildExit so a restarted server is re-detected.
    /// U-052: a `HashSet` bounds membership to O(1) and the structure to a
    /// flat per-distinct-port cost, vs. the old `Vec` whose linear `.contains`
    /// and unbounded growth scaled with every detected service.
    reported_ports: std::collections::HashSet<u16>,
    /// Timestamp of the most recent keystroke, used by latency probes
    /// to measure total keystroke-to-pixel time. Debug builds only.
    /// Note: on rapid keystrokes before a render frame, earlier timestamps are overwritten.
    #[cfg(debug_assertions)]
    pub(crate) last_keystroke_at: Option<std::time::Instant>,
    /// input written while the engine has no child yet (the PTY opens
    /// on a background thread and the session is promoted later). Without this
    /// queue an auto-launch command issued the instant a terminal mounts - or
    /// a keystroke typed in the brief pre-promotion window - would be lost.
    /// [`promote_ghostty`](Self::promote_ghostty) flushes it in order.
    /// `Mutex` (not `RefCell`) keeps `TerminalState` `Send` and matches the
    /// crate's interior-mutability idiom; the lock is uncontended (main thread
    /// only).
    pending_input: std::sync::Mutex<VecDeque<PendingTerminalInput>>,
}

/// Cap on input buffered before the engine owns a child. Generous for a launch
/// command plus a burst of typing, tight enough that a terminal that never
/// promotes (spawn failure) cannot accumulate input without bound.
const MAX_PENDING_INPUT_BYTES: usize = 1024 * 1024;
const INPUT_CONTROL_RESERVE_BYTES: usize = 64 * 1024;

/// Scrollback budget for the display-only grid that renders a spawn failure.
/// The pane holds one error message, so it never needs history - and reading
/// the user's configured length here would put file I/O on the render thread.
const SPAWN_FAILURE_SCROLLBACK_LINES: usize = 256;

/// The cheap, render-thread-safe half of a spawn: resolved shell, assembled
/// child env, cwd, and grid size. Produced by
/// [`TerminalState::resolve_spawn_params`] and consumed by
/// [`GhosttySession::start`], which may run on a background thread. All fields
/// are `Send`.
#[derive(Clone)]
pub(super) struct SpawnParams {
    pub(super) shell: String,
    pub(super) shell_quoting: ShellQuoting,
    pub(super) extra_args: Vec<String>,
    pub(super) env: std::collections::HashMap<String, String>,
    pub(super) cwd: std::path::PathBuf,
    pub(super) cols: usize,
    pub(super) rows: usize,
    pub(super) profile: TerminalSurfaceProfile,
}

/// Foreground (main-thread) signal mask, captured so an off-thread PTY spawn
/// doesn't hand the child the background executor's mask (which blocks
/// SIGINT/SIGTSTP and would break Ctrl-C / Ctrl-Z).
pub type ForegroundSignalMask = libc::sigset_t;

/// Capture the calling thread's signal mask. Call on the main thread before
/// scheduling an off-thread spawn; thread the result through to
/// [`GhosttySession::start`].
pub(super) fn capture_foreground_signal_mask() -> Option<ForegroundSignalMask> {
    // SAFETY: `pthread_sigmask` with a null `set` only reads the current
    // mask into `oldset`; nothing is changed.
    unsafe {
        let mut oldset: libc::sigset_t = std::mem::zeroed();
        if libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut oldset) == 0 {
            Some(oldset)
        } else {
            None
        }
    }
}

/// Install `mask` on the current thread, returning the previous mask to restore.
/// Brackets the child fork so it inherits the foreground signal disposition
/// even when the spawn runs on a background thread .
#[cfg(unix)]
pub(super) fn apply_thread_signal_mask(
    mask: Option<ForegroundSignalMask>,
) -> Option<libc::sigset_t> {
    let fg = mask?;
    // SAFETY: set this thread's mask to the captured foreground mask, saving the
    // previous one into `saved` for `restore_thread_signal_mask`.
    unsafe {
        let mut saved: libc::sigset_t = std::mem::zeroed();
        if libc::pthread_sigmask(libc::SIG_SETMASK, &fg, &mut saved) == 0 {
            Some(saved)
        } else {
            None
        }
    }
}

/// Restore a thread mask saved by [`apply_thread_signal_mask`].
#[cfg(unix)]
pub(super) fn restore_thread_signal_mask(saved: Option<libc::sigset_t>) {
    if let Some(saved) = saved {
        // SAFETY: restore the previously-saved mask on this thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut());
        }
    }
}

impl TerminalState {
    /// PTY bytes the runtime has parsed so far. The benchmark's idle probe
    /// uses it to wait for a shell to finish printing its prompt.
    #[cfg(test)]
    pub(super) fn processed_output_bytes_for_test(&self) -> usize {
        self.ghostty.processed_output_bytes_for_test()
    }

    pub(crate) fn session_backend(&self) -> TerminalSessionBackend {
        TerminalSessionBackend::new(self.ghostty.clone())
    }

    /// Clone of the engine handle, for the background half of a spawn.
    pub(super) fn ghostty_session(&self) -> GhosttySession {
        self.ghostty.clone()
    }

    pub(crate) fn take_backend_events(&mut self) -> TerminalBackendEvents {
        TerminalBackendEvents(self.ghostty_events_rx.take())
    }

    /// OSC 0/2 is terminal-controlled input and this retained field is cloned
    /// by view, IPC and event consumers. Scrub controls/bidi and cap it at
    /// ingestion so no later sink or clone observes the raw, unbounded payload.
    fn ingest_title(&mut self, raw: String) {
        self.title = crate::sidebar_title::clean_sidebar_title(&raw).unwrap_or_default();
    }

    pub(crate) fn process_backend_event(&mut self, event: TerminalBackendEvent) {
        self.process_ghostty_event(event.0);
    }

    pub(crate) fn process_backend_wakeup(&mut self) {
        self.dirty = true;
        self.output_generation = self.output_generation.saturating_add(1);
        self.flush_ghostty_pending_input();
        self.ghostty.retry_backpressured_commands();
    }

    pub(crate) fn notify_window_size(&self, size: TerminalWindowSize) {
        self.ghostty.resize(size);
    }

    /// Install the child produced by [`GhosttySession::start`] and switch the
    /// surface to interactive defaults. The grid is unchanged - the engine was
    /// already rendering into it - so this only opens the write side and lets
    /// `Drop` reach the child.
    pub(super) fn promote_ghostty(&mut self, spawned: SpawnedGhostty) {
        self.ghostty.promote();
        self.child_pid = spawned.child_pid;
        self.child_proc_start = child_pid_start_time(spawned.child_pid);
        self.current_cwd = Some(spawned.cwd.to_string_lossy().into_owned());
        // The guard dups the fd again for its own child (FD_CLOEXEC cleared),
        // so this app-owned copy stays ours for `Drop`'s session snapshot.
        #[cfg(all(unix, not(test)))]
        {
            self.pty_guard = crate::agents::parent_guard::spawn_pty_guard(
                spawned.child_pid,
                self.child_proc_start,
                spawned.master_fd.as_raw_fd(),
            );
        }
        self.pty_master_fd = Some(spawned.master_fd);
        self.set_osc52_mode(self.spawn_osc52_mode);
        self.cursor_blinking = true;
        self.dirty = true;
        self.flush_ghostty_pending_input();
    }

    /// Route the Drop-time SIGKILL escalation through GPUI's background
    /// executor instead of a detached OS thread (no thread leak per closed pane).
    pub fn set_background_executor(&mut self, executor: gpui::BackgroundExecutor) {
        self.background_executor = Some(executor);
    }

    fn flush_ghostty_pending_input(&self) {
        if !self.ghostty.is_promoted() {
            return;
        }
        let Ok(mut pending) = self.pending_input.lock() else {
            return;
        };
        while let Some(input) = pending.front().cloned() {
            match input.try_send(&self.ghostty) {
                GhosttyInputSendResult::Sent => {
                    pending.pop_front();
                }
                GhosttyInputSendResult::Full => break,
                GhosttyInputSendResult::Closed => {
                    let discarded = pending.len();
                    pending.clear();
                    log::warn!(
                        target: "paneflow::terminal::ghostty",
                        "Ghostty input closed with {discarded} deferred events"
                    );
                    break;
                }
            }
        }
    }

    /// Turn the surface into a static error pane after a failed spawn.
    ///
    /// [`GhosttySession::start`] consumes its runtime handle, and a failure
    /// leaves the runtime thread returned and the mailbox closed - that session
    /// can no longer render anything. A fresh display-only session takes its
    /// place so the message is visible, and the diagnostics explain why the
    /// pane holds text instead of a shell.
    ///
    /// The replacement is never promoted, so the input queued for the child
    /// that never came is dropped here and [`Self::dispatch_ghostty_input`]
    /// refuses everything after: `surface.send_text` gets `sent: false`
    /// instead of a queue that fills up behind `sent: true`.
    pub(super) fn report_spawn_failure(
        &mut self,
        failure: TerminalBackendFailureDiagnostics,
        message: &str,
    ) {
        self.backend_failure = Some(failure);
        self.ghostty.shutdown();
        let discarded = {
            let mut pending = self
                .pending_input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let discarded = pending.len();
            pending.clear();
            discarded
        };
        if discarded > 0 {
            log::debug!(
                target: "paneflow::terminal::ghostty",
                "spawn failure dropped {discarded} input events queued for the child"
            );
        }

        let size = self.ghostty.requested_window_size();
        let (session, pending, events_rx) =
            GhosttySession::pending_with_clipboard_gate(size, self.clipboard_gate.clone());
        if let Err(error) = session.start_display(pending, SPAWN_FAILURE_SCROLLBACK_LINES) {
            log::error!(
                target: "paneflow::terminal::ghostty",
                "could not open the spawn-failure pane: {error}"
            );
            return;
        }

        self.marks = session.marks();
        self.ghostty = session;
        self.ghostty_events_rx = Some(events_rx);
        self.write_output(message.as_bytes());
        self.dirty = true;
    }

    pub fn backend_diagnostics(&self) -> TerminalBackendDiagnostics {
        let identity = paneflow_terminal_ghostty::build_identity();
        TerminalBackendDiagnostics {
            failure: self.backend_failure.clone(),
            target_triple: env!("PANEFLOW_TARGET_TRIPLE"),
            ghostty: GhosttyBuildDiagnostics {
                version: paneflow_terminal_ghostty::GHOSTTY_APP_VERSION,
                source_sha: identity.source_sha,
                api_version: identity.api_version,
                zig_version: identity.zig_version,
                optimization: identity.optimization,
                simd: identity.simd,
            },
        }
    }

    /// Spawn a real PTY-backed terminal synchronously. Resolves the shell + env
    /// ([`resolve_spawn_params`]), builds a display-only session
    /// ([`new_pending`]), starts the engine against a child
    /// ([`GhosttySession::start`]), and promotes it
    /// ([`promote_ghostty`](Self::promote_ghostty)). The off-thread path
    /// (`TerminalView::with_cwd_and_env`,) runs the same steps but
    /// spreads the blocking one across the background executor with a
    /// `signal_mask` so the render thread never blocks on the spawn.
    ///
    /// `signal_mask` is `None` on the synchronous main-thread path (the
    /// foreground mask is already active); the off-thread path passes the
    /// captured foreground mask so the child still gets correct Ctrl-C.
    ///
    /// The production GUI path spawns off-thread; this synchronous composition
    /// is the reference path, exercised end-to-end by the live PTY smoke tests
    /// and available to any future non-GUI (headless) caller.
    #[allow(dead_code)]
    pub fn new(
        working_directory: Option<std::path::PathBuf>,
        workspace_id: u64,
        surface_id: u64,
        initial_size: Option<(usize, usize)>,
        user_env: Option<std::collections::HashMap<String, String>>,
        signal_mask: Option<ForegroundSignalMask>,
    ) -> anyhow::Result<Self> {
        Self::new_with_profile(
            working_directory,
            workspace_id,
            surface_id,
            initial_size,
            user_env,
            TerminalSurfaceProfile::Normal,
            signal_mask,
        )
    }

    #[allow(dead_code)]
    pub fn new_with_profile(
        working_directory: Option<std::path::PathBuf>,
        workspace_id: u64,
        surface_id: u64,
        initial_size: Option<(usize, usize)>,
        user_env: Option<std::collections::HashMap<String, String>>,
        profile: TerminalSurfaceProfile,
        signal_mask: Option<ForegroundSignalMask>,
    ) -> anyhow::Result<Self> {
        let params = Self::resolve_spawn_params_with_profile(
            working_directory,
            workspace_id,
            surface_id,
            initial_size,
            user_env,
            profile,
            &paneflow_config::loader::load_config(),
        );
        let max_scrollback = resolved_scrollback_lines(params.profile);
        let (mut state, pending) = Self::new_pending_with_profile_and_shell_quoting(
            params.cols,
            params.rows,
            params.profile,
            params.shell_quoting,
        );
        state.set_spawn_osc52_mode(Osc52Mode::from_config(
            &paneflow_config::loader::load_config(),
        ));
        let spawned = state
            .ghostty_session()
            .start(pending.ghostty, params, signal_mask, max_scrollback)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        state.promote_ghostty(spawned);
        Ok(state)
    }

    /// Resolve the shell, the merged + assembled child env, the cwd, and the
    /// grid size - the cheap, render-thread-safe half of a spawn. Factored out
    /// of `new` so the off-thread path (US-012) runs the *blocking* half
    /// ([`GhosttySession::start`]) on the background executor.
    #[allow(dead_code)]
    pub(super) fn resolve_spawn_params(
        working_directory: Option<std::path::PathBuf>,
        workspace_id: u64,
        surface_id: u64,
        initial_size: Option<(usize, usize)>,
        user_env: Option<std::collections::HashMap<String, String>>,
    ) -> SpawnParams {
        Self::resolve_spawn_params_with_profile(
            working_directory,
            workspace_id,
            surface_id,
            initial_size,
            user_env,
            TerminalSurfaceProfile::Normal,
            &paneflow_config::loader::load_config(),
        )
    }

    /// `config` is the caller's snapshot of the live config (issue #298:
    /// settings persist is cache-first, so a fresh `load_config()` here can
    /// still read the value a change is about to overwrite on disk).
    pub(super) fn resolve_spawn_params_with_profile(
        working_directory: Option<std::path::PathBuf>,
        workspace_id: u64,
        surface_id: u64,
        initial_size: Option<(usize, usize)>,
        user_env: Option<std::collections::HashMap<String, String>>,
        profile: TerminalSurfaceProfile,
        config: &paneflow_config::schema::PaneFlowConfig,
    ) -> SpawnParams {
        // Fallback chain handled by `resolve_default_shell` (US-006):
        // config → $SHELL → /bin/sh
        let shell = {
            let configured = config
                .default_shell
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let resolved = resolve_default_shell(configured);
            // The configured value and what actually got launched are the two
            // facts needed to tell "my shell setting is ignored" apart from "my
            // shell is just slow to start". `resolve_default_shell` only warns
            // on the rejection path, which says nothing when resolution
            // succeeded but picked something else than expected.
            log::info!(
                target: "paneflow::terminal::backend",
                "{}",
                shell_resolution_log_line(&resolved, configured)
            );
            resolved
        };
        let shell_quoting = ShellQuoting::for_shell(&shell);
        // US-014: layer the per-surface `user_env` on top of the global
        // `terminal.env` default (surface wins on key collision).
        let global_env = config.terminal.as_ref().and_then(|t| t.env.clone());
        let merged_env = match (global_env, user_env) {
            (None, None) => None,
            (Some(g), None) => Some(g),
            (None, Some(s)) => Some(s),
            (Some(mut g), Some(s)) => {
                g.extend(s);
                Some(g)
            }
        };
        let mut env = std::collections::HashMap::new();
        // EP-003 US-007: clean opt-out - with `shell_integration: false` no
        // rc snippet is written or wired and the shell starts untouched.
        let extra_args = if config.shell_integration.unwrap_or(true) {
            setup_shell_integration(&shell, &mut env, profile)
        } else {
            vec![]
        };
        // Assemble the child environment (identity vars, TERM, AI-hook PATH
        // prepend, user-env merge with protected keys). Pure function so the env
        // contract stays unit-testable (the mockable `PtyBackend::spawn` seam is
        // gone).
        let env = assemble_pty_env(env, workspace_id, surface_id, merged_env);
        // Keep terminal.env and identity propagation independent from shell
        // integration: opting out disables rc hooks, not the terminal env contract.
        // U-026 + issue #11: when no cwd is explicit, avoid inheriting a GUI
        // launch cwd that is the filesystem root. Explicit root cwd requests
        // still arrive through `working_directory` and are preserved.
        let cwd = working_directory.unwrap_or_else(crate::launch_cwd::implicit_launch_cwd);
        let (cols, rows) = initial_size.unwrap_or((120, 40));
        SpawnParams {
            shell,
            shell_quoting,
            extra_args,
            env,
            cwd,
            cols,
            rows,
            profile,
        }
    }

    /// Build a terminal whose engine exists but has not been started yet, so a
    /// background spawn can start the same session against a real child and
    /// [`promote_ghostty`](Self::promote_ghostty) it . The returned
    /// opaque pending handle is what [`GhosttySession::start`] consumes.
    #[allow(dead_code)]
    pub(super) fn new_pending(cols: usize, rows: usize) -> (Self, PendingTerminalBackend) {
        Self::new_pending_with_profile(cols, rows, TerminalSurfaceProfile::Normal)
    }

    pub(super) fn new_pending_with_profile(
        cols: usize,
        rows: usize,
        profile: TerminalSurfaceProfile,
    ) -> (Self, PendingTerminalBackend) {
        Self::new_pending_with_profile_and_shell_quoting(
            cols,
            rows,
            profile,
            ShellQuoting::default_for_platform(),
        )
    }

    pub(super) fn new_pending_with_profile_and_shell_quoting(
        cols: usize,
        rows: usize,
        _profile: TerminalSurfaceProfile,
        shell_quoting: ShellQuoting,
    ) -> (Self, PendingTerminalBackend) {
        Self::build_display_only(cols, rows, shell_quoting)
    }

    /// Create a display-only terminal with no PTY and no child process.
    /// Content is rendered via `write_output()`, which feeds bytes straight
    /// into the grid. The terminal supports full ANSI rendering but does not
    /// accept keyboard input. Used by tests and by the spawn-failure pane.
    #[allow(dead_code)]
    pub fn new_display_only(rows: usize, cols: usize) -> Self {
        Self::new_display_only_with_profile(rows, cols, TerminalSurfaceProfile::Normal)
    }

    #[allow(dead_code)]
    pub fn new_display_only_with_profile(
        rows: usize,
        cols: usize,
        profile: TerminalSurfaceProfile,
    ) -> Self {
        let (state, pending) =
            Self::build_display_only(cols, rows, ShellQuoting::default_for_platform());
        if let Err(error) = state
            .ghostty
            .start_display(pending.ghostty, resolved_scrollback_lines(profile))
        {
            log::error!(
                target: "paneflow::terminal::ghostty",
                "could not start the display-only runtime: {error}"
            );
        }
        state
    }

    /// Shared constructor for the not-yet-started state. Returns the terminal
    /// plus the opaque runtime handle its engine still needs, so either
    /// [`GhosttySession::start`] (real child) or
    /// [`GhosttySession::start_display`] (grid only) can bring it up.
    fn build_display_only(
        cols: usize,
        rows: usize,
        shell_quoting: ShellQuoting,
    ) -> (Self, PendingTerminalBackend) {
        let clipboard_gate = Arc::new(ClipboardGate::default());
        let (ghostty, runtime_pending, events_rx) = GhosttySession::pending_with_clipboard_gate(
            TerminalWindowSize::new(cols, rows, 0, 0),
            clipboard_gate.clone(),
        );
        let marks = ghostty.marks();
        let state = Self {
            ghostty,
            ghostty_events_rx: Some(events_rx),
            backend_failure: None,
            spawn_failure_input_warned: std::sync::atomic::AtomicBool::new(false),
            marks,
            last_prompt_seq: 0,
            exited: None,
            resolved_hover_link: None,
            keyboard_input_sent: std::sync::atomic::AtomicBool::new(false),
            exit_signal: None,
            child_pid: 0,
            current_cwd: None,
            progress: None,
            custom_name: None,
            detected_agent: None,
            agent_confirmed: false,
            agent_declared_until: None,
            detected_ports: Vec::new(),
            port_conflicts: Vec::new(),
            announced_ports: Vec::new(),
            font_size_override: None,
            osc52_mode: Osc52Mode::Disabled,
            spawn_osc52_mode: Osc52Mode::CopyOnly,
            terminal_focused: false,
            clipboard_gate,
            shell_quoting,
            pending_clipboard_ops: Vec::new(),
            pending_notifications: Vec::new(),
            cached_foreground_command: None,
            child_proc_start: None,
            pty_master_fd: None,
            background_executor: None,
            cursor_color_override: None,
            #[cfg(all(unix, not(test)))]
            pty_guard: None,
            cursor_blinking: false,
            title: String::from("Terminal"),
            dirty: true,
            output_generation: 0,
            last_activity_burst: None,
            cwd_poll_ticks: 0,
            reported_ports: std::collections::HashSet::new(),
            #[cfg(debug_assertions)]
            last_keystroke_at: None,
            pending_input: std::sync::Mutex::new(VecDeque::new()),
        };
        (
            state,
            PendingTerminalBackend {
                ghostty: runtime_pending,
            },
        )
    }

    /// Write ANSI-formatted content to a display-only terminal.
    /// Converts bare `\n` to `\r\n` (since there is no PTY to perform CR insertion).
    /// Note: callers must not split a `\r\n` pair across two calls (the second call
    /// would insert an extra `\r`, producing `\r\r\n`). Prefer complete chunks.
    #[allow(dead_code)]
    pub fn write_output(&self, bytes: &[u8]) {
        // Convert \n to \r\n - bare LF without preceding CR needs CR insertion
        let mut converted = Vec::with_capacity(bytes.len());
        let mut prev = 0u8;
        for &b in bytes {
            if b == b'\n' && prev != b'\r' {
                converted.push(b'\r');
            }
            converted.push(b);
            prev = b;
        }
        self.ghostty.write_output(&converted);
    }

    /// Drain the CWD fallback, then drain any pending engine events.
    /// Sets `dirty = true` when PTY output was processed.
    #[allow(dead_code)]
    pub fn sync(&mut self) {
        self.sync_channels();
        if let Some(mut rx) = self.ghostty_events_rx.take() {
            while let Ok(event) = rx.try_recv() {
                self.process_ghostty_event(event);
            }
            self.ghostty_events_rx = Some(rx);
        }
    }

    /// Refresh the shell CWD from the process table (EP-002 US-007).
    ///
    /// The engine decodes OSC 7 itself and publishes it as a UI event.
    /// Unix/macOS additionally refresh from process-table state via
    /// `cwd_now()`, for shells that never emit the sequence. The fallback is
    /// throttled so we don't `readlink` on every poll tick.
    pub fn sync_channels(&mut self) {
        self.cwd_poll_ticks = self.cwd_poll_ticks.wrapping_add(1);
        if self.cwd_poll_ticks.is_multiple_of(25)
            && let Some(cwd) = self.cwd_now()
        {
            self.current_cwd = Some(cwd.to_string_lossy().into_owned());
        }
    }

    /// Whether the shell returned to its prompt since the last call.
    ///
    /// Reads the OSC 133 `PromptStart` sequence the engine's mark scanner
    /// already maintains, so it costs one mutex read per sync tick.
    ///
    /// A prompt is proof that no foreground command owns the terminal any
    /// more, which is how the app reaps a finished agent's session without
    /// waiting for the periodic PID sweep. The first prompt of a fresh shell
    /// fires it too; the consumer is a no-op when the surface owns no session.
    /// The latest answer to [`TerminalSessionBackend::request_osc8_hyperlink_at`],
    /// once.
    pub(crate) fn take_resolved_hover_link(&mut self) -> Option<(Point, Option<HyperlinkZone>)> {
        self.resolved_hover_link.take()
    }

    pub(crate) fn take_shell_prompt_ready(&mut self) -> bool {
        let seq = self
            .marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prompt_start_seq();
        let fired = seq != self.last_prompt_seq;
        self.last_prompt_seq = seq;
        fired
    }

    fn process_ghostty_event(&mut self, event: GhosttyUiEvent) {
        match event {
            GhosttyUiEvent::Wakeup(events) => {
                events.acknowledge_wakeup();
                self.dirty = true;
                // US-010: advance the readiness signal `workspace.up` polls.
                // Saturating (not wrapping) so the count is monotone for the
                // lifetime of a pane; u64 never realistically saturates.
                self.output_generation = self.output_generation.saturating_add(1);
            }
            GhosttyUiEvent::Title(events) => {
                if let Some(title) = events.take_title() {
                    self.ingest_title(title);
                }
            }
            GhosttyUiEvent::WorkingDirectory(events) => {
                if let Some(cwd) = events.take_working_directory() {
                    self.current_cwd = Some(cwd);
                }
            }
            GhosttyUiEvent::Progress(events) => {
                if let Some(report) = events.take_progress() {
                    self.progress = match report.state {
                        paneflow_terminal_ghostty::ProgressState::Remove => None,
                        _ => Some(report),
                    };
                }
            }
            GhosttyUiEvent::Notification(events) => {
                for notification in events.take_notifications() {
                    if self.pending_notifications.len() >= MAX_PENDING_NOTIFICATIONS {
                        self.pending_notifications.remove(0);
                    }
                    self.pending_notifications.push(notification);
                }
            }
            GhosttyUiEvent::Clipboard(events) => {
                for text in events.take_clipboard() {
                    self.deliver_clipboard_text(text);
                }
            }
            GhosttyUiEvent::ServiceOutputReady(events) => {
                events.acknowledge_service_output();
                self.last_activity_burst = None;
                self.dirty = true;
            }
            GhosttyUiEvent::ChildExited { code, signal } => {
                if self.exited.is_none() {
                    self.exited = Some(code);
                    self.exit_signal = signal;
                }
                self.dirty = true;
                self.progress = None;
                self.cached_foreground_command = None;
                self.reported_ports.clear();
                #[cfg(all(unix, not(test)))]
                {
                    self.pty_guard = None;
                }
            }
            GhosttyUiEvent::HyperlinkResolved { point, link } => {
                // Only the latest answer matters: the pointer has moved on
                // from any older one.
                self.resolved_hover_link = Some((point, link));
            }
            GhosttyUiEvent::InputRejected(error) => {
                log::warn!(target: "paneflow::terminal::ghostty", "{error}");
            }
            GhosttyUiEvent::RuntimeFailed(error) => {
                log::error!(target: "paneflow::terminal::ghostty", "{error}");
                if self.exited.is_none() {
                    self.exited = Some(-1);
                }
                self.dirty = true;
            }
        }
    }

    /// Gate an OSC 52 store: the pane must own focus, the policy must allow
    /// writes, and the payload is capped to prevent a memory DoS from a
    /// malicious program (`crate::limits`).
    fn deliver_clipboard_text(&mut self, text: String) {
        if self.terminal_focused
            && self.osc52_mode != Osc52Mode::Disabled
            && text.len() <= MAX_OSC52_BYTES
        {
            self.queue_clipboard_op(text);
        }
    }

    fn queue_clipboard_op(&mut self, text: String) {
        if self.pending_clipboard_ops.len() >= MAX_PENDING_CLIPBOARD_OPS {
            self.pending_clipboard_ops.remove(0);
        }
        self.pending_clipboard_ops.push(text);
    }

    /// Read the shell's CWD from the OS on demand.
    /// Fallback for shells that don't emit OSC 7 - used at split time.
    ///
    /// Reads the PTY child shell's current working directory from the kernel
    /// via `proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &buf, size)`.
    #[cfg(target_os = "macos")]
    pub fn cwd_now(&self) -> Option<std::path::PathBuf> {
        use std::ffi::CStr;
        use std::mem::MaybeUninit;
        use std::os::raw::c_void;

        // US-034: after exit, `child_pid` may have been reused - `proc_pidinfo`
        // would return an unrelated process's CWD. Bail.
        if self.exited.is_some() {
            return None;
        }

        // Display-only terminal (no real PTY) or a child whose pid hasn't been
        // resolved yet: `child_pid` is 0. Bail before the FFI - `proc_pidinfo(0,
        // …)` targets the kernel swapper (pid 0), fails with EPERM, and would
        // spam a misleading "shell may have exited" warning on every poll tick.
        // Mirrors the `foreground_command` guards on every platform.
        if self.child_pid == 0 {
            return None;
        }

        let pid = self.child_pid as libc::c_int;
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;

        // SAFETY: `info` is a stack-allocated MaybeUninit zeroed above; we
        // only read from it if the syscall reports the full struct size
        // was written. Zeroing first leaves it in a defined state on any
        // partial-write error path.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr() as *mut c_void,
                size,
            )
        };

        if written <= 0 {
            let err = std::io::Error::last_os_error();
            log::warn!(
                "cwd_now: proc_pidinfo(pid={pid}) returned {written} ({err}) - shell may have exited or SIP / sandbox is denying the read"
            );
            return None;
        }

        if written < size {
            log::warn!(
                "cwd_now: proc_pidinfo(pid={pid}) wrote {written} of {size} bytes - truncated result discarded"
            );
            return None;
        }

        // SAFETY: `written == size` implies the kernel fully populated the
        // buffer with a valid `proc_vnodepathinfo`.
        let info = unsafe { info.assume_init() };

        let ptr = info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char;
        // SAFETY: the kernel guarantees `vip_path` holds a NUL-terminated
        // C string not exceeding `MAXPATHLEN` bytes when the syscall
        // succeeds with full size.
        let cstr = unsafe { CStr::from_ptr(ptr) };
        match cstr.to_str() {
            Ok(s) if !s.is_empty() => Some(std::path::PathBuf::from(s)),
            _ => None,
        }
    }

    /// Scan the most recent terminal output for server/service patterns.
    /// Returns newly detected services (deduped against previously reported
    /// ports). The engine materializes the lines on its own thread, so the
    /// render thread never touches the grid here.
    pub fn scan_output(&mut self) -> Vec<ServiceInfo> {
        let lines = self.ghostty.recent_output_lines();
        self.detect_services_in_lines(&lines)
    }

    fn detect_services_in_lines(&mut self, lines: &[String]) -> Vec<ServiceInfo> {
        // Detect framework from ALL lines (context-wide), not just the port line.
        // Next.js prints "▲ Next.js 16.1.6" on one line and "localhost:3000" on another.
        let all_text = lines.join(" ");
        let (global_label, global_is_frontend) = detect_framework(&all_text);

        let mut results = Vec::new();
        for line in lines {
            if let Some(mut info) = parse_service_line(line)
                && !self.reported_ports.contains(&info.port)
            {
                if info.label.is_none() {
                    info.label = global_label.clone();
                    info.is_frontend = global_is_frontend;
                }
                self.reported_ports.insert(info.port);
                results.push(info);
            }
        }

        results
    }

    pub fn write_to_pty(&self, input: impl Into<Cow<'static, [u8]>>) {
        // any path through write_to_pty is genuine user input
        // (keystroke, paste, mouse report, IME commit, user scroll). Mark the
        // session user-initiated so a later exit closes the pane. Automated
        // protocol writes (focus reports, search RIS reset, OSC responses)
        // deliberately go through `write_to_pty_silent`.
        self.keyboard_input_sent
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.notify_or_buffer(input.into());
    }

    /// Fallible twin of [`Self::write_to_pty`] for IPC. Queuing onto the live
    /// engine or the pre-promotion pending buffer is `Ok`; a queue-limit
    /// rejection, a gone runtime, a poisoned pending lock, or a pane whose
    /// spawn failed is `Err`, so `surface.send_text` never reports
    /// `sent: true` while dropping bytes.
    pub(crate) fn try_write_to_pty(
        &self,
        input: impl Into<Cow<'static, [u8]>>,
    ) -> Result<(), PtyWriteError> {
        let input: Cow<'static, [u8]> = input.into();
        if input.is_empty() {
            return Ok(());
        }
        self.keyboard_input_sent
            .store(true, std::sync::atomic::Ordering::Relaxed);
        match self.dispatch_ghostty_input(PendingTerminalInput::Raw(input), true) {
            BackendInputResult::Accepted => Ok(()),
            BackendInputResult::Rejected => Err(PtyWriteError::Rejected),
        }
    }

    pub(super) fn set_terminal_focused(&mut self, focused: bool) {
        self.terminal_focused = focused;
        self.clipboard_gate.set_focused(focused);
    }

    fn set_osc52_mode(&mut self, mode: Osc52Mode) {
        self.osc52_mode = mode;
        self.clipboard_gate
            .set_policy(mode != Osc52Mode::Disabled, mode == Osc52Mode::CopyPaste);
    }

    /// Record the configured OSC 52 policy for this spawn. Takes effect when
    /// the child is promoted; the gate stays closed until then.
    pub(super) fn set_spawn_osc52_mode(&mut self, mode: Osc52Mode) {
        self.spawn_osc52_mode = mode;
    }

    fn dispatch_ghostty_input(
        &self,
        input: PendingTerminalInput,
        user_initiated: bool,
    ) -> BackendInputResult {
        if let Some(failure) = &self.backend_failure {
            // The error pane is display-only and never promotes, so queuing
            // here would accept bytes nothing will ever flush.
            if !self
                .spawn_failure_input_warned
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                log::warn!(
                    target: "paneflow::terminal::ghostty",
                    "input to a pane whose spawn failed ({} / {}) is rejected: there is no child to receive it",
                    failure.phase.as_str(),
                    failure.reason_code
                );
            }
            return BackendInputResult::Rejected;
        }
        let Ok(mut pending) = self.pending_input.lock() else {
            return BackendInputResult::Rejected;
        };
        let pending_bytes = pending.iter().fold(0usize, |total, item| {
            total.saturating_add(item.queued_bytes())
        });
        let total = pending_bytes.saturating_add(self.ghostty.queued_input_bytes());
        let queue_limit = input.queue_limit();
        if !input.fits_after(total) {
            log::warn!(
                target: "paneflow::terminal::ghostty",
                "Ghostty input rejected at the {} byte queue limit",
                queue_limit
            );
            return BackendInputResult::Rejected;
        }

        if self.ghostty.is_promoted() && pending.is_empty() {
            match input.try_send(&self.ghostty) {
                GhosttyInputSendResult::Sent => {
                    if user_initiated {
                        self.keyboard_input_sent
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    return BackendInputResult::Accepted;
                }
                GhosttyInputSendResult::Full => {}
                GhosttyInputSendResult::Closed => return BackendInputResult::Rejected,
            }
        }

        pending.push_back(input);
        if user_initiated {
            self.keyboard_input_sent
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        BackendInputResult::Accepted
    }

    pub(super) fn write_ghostty_key(
        &self,
        input: paneflow_terminal_ghostty::KeyInput,
    ) -> BackendInputResult {
        self.dispatch_ghostty_input(PendingTerminalInput::Key(input), true)
    }

    pub(super) fn write_ghostty_mouse(
        &self,
        input: paneflow_terminal_ghostty::MouseInput,
        repeat: usize,
    ) -> BackendInputResult {
        self.dispatch_ghostty_input(PendingTerminalInput::Mouse { input, repeat }, true)
    }

    pub(super) fn write_ghostty_focus(
        &self,
        event: paneflow_terminal_ghostty::FocusEvent,
    ) -> BackendInputResult {
        self.dispatch_ghostty_input(PendingTerminalInput::Focus(event), false)
    }

    pub(super) fn write_ghostty_paste(&self, text: String) -> BackendInputResult {
        self.dispatch_ghostty_input(
            PendingTerminalInput::Paste {
                text,
                allow_unsafe: true,
            },
            true,
        )
    }

    /// Send input to the engine, or queue it while the engine still has no
    /// child (pre-promotion window): the launch command an auto-launched
    /// agent issues the instant it mounts, plus any keystroke typed before the
    /// off-thread spawn resolved. [`promote_ghostty`](Self::promote_ghostty)
    /// flushes the queue in order. Bounded by [`MAX_PENDING_INPUT_BYTES`] so a
    /// never-promoted terminal can't grow it without bound.
    fn notify_or_buffer(&self, input: Cow<'static, [u8]>) {
        if input.is_empty() {
            return;
        }
        self.dispatch_ghostty_input(PendingTerminalInput::Raw(input), false);
    }

    /// US-002: write to the PTY WITHOUT marking the session user-initiated.
    /// For automated protocol writes that must not flip `keyboard_input_sent`,
    /// otherwise a failed-spawn pane that merely gains focus would wrongly
    /// close on exit. Focus reports go through [`Self::write_ghostty_focus`]
    /// and the Shift+Cmd+R reset no longer types at the child, so no
    /// production path uses this today; it stays as the entry point for the
    /// next protocol write.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "protocol-write entry point; the reset stopped typing at the child"
        )
    )]
    pub fn write_to_pty_silent(&self, input: impl Into<Cow<'static, [u8]>>) {
        self.notify_or_buffer(input.into());
    }

    /// US-002: whether a child exit should close the pane. A user-initiated
    /// session (any input was sent) always closes; otherwise only a clean exit
    /// (code 0) closes - a non-zero exit with no input is a spawn/launch
    /// failure and stays open so the exit overlay shows the code. Mirrors Zed's
    /// discriminator (crates/terminal/src/terminal.rs:2572-2576).
    pub fn should_close_on_exit(&self) -> bool {
        self.keyboard_input_sent
            .load(std::sync::atomic::Ordering::Relaxed)
            || self.exited == Some(0)
    }

    /// Extract terminal history as plain text (ANSI stripped) for session
    /// persistence. The active viewport is deliberately excluded so restoring a
    /// session cannot replay the previous visible frame ahead of fresh shell
    /// output. Caps at 4000 lines and 400,000 characters. Returns None if
    /// history is empty.
    pub fn extract_scrollback(&self) -> Option<String> {
        self.ghostty.extract_scrollback()
    }

    /// [`Self::extract_scrollback_window`] bounded to the newest `lines`
    /// rows: the tail of the history followed by the screen, so the undo
    /// shows what was on screen when the pane closed.
    ///
    /// The undo-close record (issue #83) takes this instead of the full
    /// extract: undo replays it as inert text into a brand-new PTY, and
    /// `enforce_closed_pane_scrollback_budget` strips most of a full extract
    /// back to 2 MiB milliseconds later anyway.
    pub(crate) fn extract_scrollback_capped(&self, lines: usize) -> Option<String> {
        let (mut result, returned, _, _) = self.extract_scrollback_window(lines, 0)?;
        if returned == 0 {
            return None;
        }
        cap_scrollback_at_char_boundary(&mut result, crate::limits::MAX_CHARS);
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// Windowed extract for `surface.read` (issue #29).
    ///
    /// The rows are the retained history followed by the screen the program
    /// is painting (#184 Phase 3.6: a full-screen TUI has no history at all,
    /// so without the screen a reader saw nothing). `offset` skips lines
    /// from the newest end; `lines` is the window size. `total` is the row
    /// count (trailing empty rows trimmed; the history part is capped at
    /// [`crate::limits::MAX_SCROLLBACK_EXTRACT_LINES`]). The window is cut
    /// in the engine (`DisplayTerminal::transcript_window`), which reads the
    /// screen plus the `lines` rows the window covers and nothing else.
    ///
    /// Returns `Some((text, returned, total, eof))`, or `None` when the
    /// runtime did not answer (mailbox full or closed, no reply within a
    /// second) or the engine failed the read. `None` is not a blank pane:
    /// `surface.read` turns it into an error so `wait`/`flow` do not settle
    /// on a wedged runtime as if it had gone quiet.
    pub(crate) fn extract_scrollback_window(
        &self,
        lines: usize,
        offset: usize,
    ) -> Option<(String, usize, usize, bool)> {
        self.scrollback_reader()
            .extract_scrollback_window(lines, offset)
    }

    /// A `Send` view of this surface's runtime for the two scrollback reads
    /// whose wait must not run on the GPUI thread (issue #363).
    ///
    /// Cloning this on the render thread costs an `Arc` bump; the blocking
    /// wait then happens on a background worker.
    pub(crate) fn scrollback_reader(&self) -> ScrollbackReader {
        ScrollbackReader {
            ghostty: self.ghostty.clone(),
            title: self.title.clone(),
            child_pid: self.child_pid,
        }
    }

    /// The screen half of a read on its own, trailing blank rows trimmed.
    ///
    /// `surface.read` takes both halves through one
    /// [`GhosttySession::transcript`] so they cannot tear; tests use this to
    /// pin what that read appends after the history.
    #[cfg(test)]
    pub fn screen_text(&self) -> Option<String> {
        self.ghostty.screen_text()
    }

    /// Reset the emulator the way a program-emitted RIS (`ESC c`) does:
    /// modes, screen, scrollback and tab stops go. This runs against the grid
    /// in the runtime and writes nothing to the PTY, so the program in the
    /// pane is not interrupted (Shift+Cmd+R used to type `ESC c` at it).
    pub(crate) fn reset_terminal(&self) {
        self.ghostty.reset();
    }

    /// Capture the screen and its recent history as VT sequences.
    ///
    /// Unlike [`Self::extract_scrollback`] this keeps the styling and the
    /// cursor, so a restored pane looks like the one that was closed rather
    /// than like a transcript of it. The bytes are produced by libghostty's
    /// own formatter over this process's terminal under
    /// `TerminalExtra::replay`, which is what keeps a closed pane's OSC 52,
    /// OSC 7, title, and mode state out of the pane undo brings back (#195).
    pub fn capture_replay(&self) -> Option<Vec<u8>> {
        self.ghostty.capture_replay()
    }

    /// Best-effort foreground command of this surface, cached by the off-thread
    /// pane process scanner. Returns the shell command while idle or the current
    /// child approximation while busy. `None` means callers fall back to the OSC
    /// title.
    pub fn foreground_command(&self) -> Option<String> {
        self.cached_foreground_command.clone()
    }

    /// EP-005 US-014: remember a port announced by a service URL in this
    /// terminal's output, for the collision cross-check against the scan's
    /// LISTEN attribution. The source text is UNTRUSTED terminal output, so
    /// the list is bounded: a flood of fake announcements can at most fill
    /// 16 slots (oldest kept - the legitimate dev-server line is printed
    /// once at startup, i.e. first).
    /// Drop announce-dedup state for ports that are no longer LISTEN
    /// anywhere in the workspace. Without this, `reported_ports` was only
    /// cleared on ChildExit - a dev server restarted inside a live shell
    /// (nodemon, plain re-run) re-printed its banner but could never re-fire
    /// `ServiceDetected`, leaving the sidebar chip stale until the pane died.
    pub fn retain_reported_ports(&mut self, live: &[u16]) {
        self.reported_ports.retain(|p| live.contains(p));
    }

    pub fn note_announced_port(&mut self, port: u16) {
        const MAX_ANNOUNCED_PORTS: usize = 16;
        if !self.announced_ports.contains(&port) && self.announced_ports.len() < MAX_ANNOUNCED_PORTS
        {
            self.announced_ports.push(port);
        }
    }

    /// Feed saved scrollback text back into the grid. Called during session
    /// restore, before the shell has produced output.
    ///
    /// The engine enforces the "plain, ANSI stripped" invariant on its side: a
    /// tampered or imported `session.json` can carry raw VT bytes in
    /// `surface.scrollback`, and feeding them verbatim would allow single-line
    /// title-spoof / OSC 8 clickable-link injection into the restored grid.
    pub fn restore_scrollback(&self, text: &str) {
        self.ghostty.restore_scrollback(text);
    }

    /// Replay bytes produced by [`Self::capture_replay`] in this process.
    ///
    /// These go into the grid verbatim, escapes included, which is the whole
    /// point: the styling has to survive. That makes this the wrong entry
    /// point for anything read from disk or from a config file, where
    /// [`Self::restore_scrollback`] and its ANSI stripping is the only safe
    /// path. Only pass bytes this process captured from its own terminal.
    pub fn restore_replay(&self, bytes: &[u8]) {
        self.ghostty.write_output(bytes);
    }
}

/// Compute the PaneFlow IPC socket path, delegating to `runtime_paths` so
/// the fallback chain stays in sync with `ipc::socket_path`.
fn paneflow_socket_path() -> Option<String> {
    crate::runtime_paths::socket_path().map(|p| p.display().to_string())
}

/// US-009 - extract the embedded AI-hook binaries into the user's cache
/// dir, then expose that dir via `PANEFLOW_BIN_DIR` and prepend it to
/// the child shell's `PATH`.
///
/// Silent-fail: any error (extraction IO failure, unresolvable
/// `cache_dir`) is logged at `warn` and then swallowed so the terminal
/// opens normally without the AI-hook loader for this session. PRD
/// constraint C4 mandates the terminal must never fail to open because
/// of AI-hook wiring.
///
/// Factored out of `TerminalState::new` so the helper is independently
/// testable - the extraction side-effect lives in `ai_hooks::extract`
/// (already unit-tested in US-008); this glue only layers the env
/// mutations on top of a returned `PathBuf`.
fn inject_ai_hook_env(env: &mut std::collections::HashMap<String, String>) {
    let bin_dir = match crate::ai_hooks::extract::ensure_binaries_extracted() {
        Ok(p) => p,
        Err(e) => {
            // `{e:#}` emits the full anyhow context chain (each
            // `.with_context()` frame) rather than just the outermost
            // message - crucial for diagnosing cache-dir permission
            // errors that arrive with a useful inner IO error.
            log::warn!(
                "paneflow: AI-hook binary extraction failed ({e:#}); sidebar loader will not activate for this terminal session"
            );
            return;
        }
    };

    // `PANEFLOW_BIN_DIR` is the source-of-truth the shim uses for its
    // self-exclusion PATH walk (US-004). Set it even in the unlikely
    // event the PATH-prepend below fails, so the shim can still
    // identify its own dir if a later code path routes into it.
    env.insert("PANEFLOW_BIN_DIR".into(), bin_dir.display().to_string());

    prepend_bin_dir_to_path(env, &bin_dir);
}

fn reassert_paneflow_bin_dir_first(env: &mut std::collections::HashMap<String, String>) {
    let Some(bin_dir) = env.get("PANEFLOW_BIN_DIR").cloned() else {
        return;
    };
    if bin_dir.is_empty() {
        return;
    }
    prepend_bin_dir_to_path(env, std::path::Path::new(&bin_dir));
}

/// Prepend `bin_dir` to `env["PATH"]` (or to the process `PATH` if the
/// env map does not yet carry one). Uses `std::env::join_paths`, which
/// emits `:` between entries.
///
/// If join-paths fails (e.g. a `PATH` entry contains a platform
/// separator byte - invalid but physically possible), logs a warning
/// and leaves the env map unchanged. Better "no prepend" than "broken
/// PATH".
fn prepend_bin_dir_to_path(
    env: &mut std::collections::HashMap<String, String>,
    bin_dir: &std::path::Path,
) {
    // Order of precedence: explicit map entry first, then process env.
    // `setup_shell_integration` (shell.rs) does not set PATH, so in
    // practice this always falls through to the process PATH - but the
    // explicit-map branch makes the helper reusable and keeps tests
    // decoupled from the process environment.
    let existing: Option<std::ffi::OsString> = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"));

    let mut components: Vec<std::path::PathBuf> = vec![bin_dir.to_path_buf()];
    // Guard against an empty `PATH` string: on Unix, `split_paths("")`
    // yields a single `PathBuf::from("")` which `execvp` resolves as the
    // current working directory - that would silently put `.` on the
    // child's PATH (a classic shell-injection surface). Treat empty and
    // absent identically.
    if let Some(existing) = existing.as_deref()
        && !existing.is_empty()
    {
        components.extend(std::env::split_paths(existing));
    }

    match std::env::join_paths(components) {
        Ok(joined) => {
            // `join_paths` always produces valid UTF-8 when all inputs
            // were UTF-8 PathBufs + an OsString PATH - on all three
            // supported OSes, PATH is conventionally UTF-8 so the
            // `to_string_lossy` round-trip is safe. If a real-world PATH
            // entry contains non-UTF-8 bytes, we lose those in the
            // lossy conversion - but the env map is keyed on
            // `HashMap<String, String>` to begin with, so this is a
            // pre-existing constraint inherited from
            // `PtyBackend::spawn`, not introduced here.
            env.insert("PATH".into(), joined.to_string_lossy().into_owned());
        }
        Err(e) => {
            log::warn!(
                "paneflow: could not prepend AI-hook bin dir {} to PATH: {e}",
                bin_dir.display()
            );
        }
    }
}

/// True if `key` names a dynamic-loader-influencing environment variable that
/// an untrusted source (an imported `session.json` surface env, or the global
/// `terminal.env` config) must NOT be allowed to inject into a spawned child:
/// `LD_PRELOAD` / `LD_LIBRARY_PATH` / `LD_AUDIT` and any `LD_*` on Linux, plus
/// any `DYLD_*` on macOS. Letting these through is an RCE vector - the operator
/// treats imported sessions as untrusted, and the child is always the
/// configured shell. The match is case-sensitive on purpose: the unix loaders
/// only honour the exact upper-case spelling, so a lower-case `ld_preload` is
/// inert and need not be dropped.
fn is_loader_influencing_env_key(key: &str) -> bool {
    key.starts_with("LD_") || key.starts_with("DYLD_")
}

/// True if `key` is one of the launching agent session's identity/credential
/// markers - see [`INHERITED_AGENT_SESSION_ENV`].
fn is_inherited_agent_session_env_key(key: &str) -> bool {
    INHERITED_AGENT_SESSION_ENV.contains(&key)
}

fn is_forbidden_child_env_key(key: &str) -> bool {
    is_inherited_agent_session_env_key(key)
        || key == ZDOTDIR_ENV
        || key == PANEFLOW_ORIG_ZDOTDIR_ENV
        || is_loader_influencing_env_key(key)
}

/// True if `key` names a host-terminal identity marker - see
/// [`INHERITED_HOST_TERMINAL_ENV`]. Pure, so the list is unit-tested without
/// touching the process environment.
pub(super) fn is_inherited_host_terminal_env_key(key: &str) -> bool {
    INHERITED_HOST_TERMINAL_ENV
        .iter()
        .any(|known| key.eq_ignore_ascii_case(known))
}

/// The names Paneflow inherited that must not reach a pane: host-terminal
/// identity markers, plus the launching agent session's own markers.
///
/// `portable_pty::CommandBuilder` seeds itself from `std::env::vars_os()` and
/// Paneflow only layers overrides on top, so removing a key from the assembled
/// env map cannot unset an INHERITED variable - only an explicit
/// `env_remove` at the spawn boundary can. That is why
/// [`assemble_pty_env`]'s `retain` is not enough on its own for
/// [`INHERITED_AGENT_SESSION_ENV`]: it stops a merge from reintroducing a
/// marker, it cannot unset one Paneflow was started with. Enumerating the real
/// environment is also what lets the `ConEmu*` prefix rule resolve to concrete
/// names to remove.
///
/// Non-UTF-8 names are left alone: none of the targets can be spelled that way,
/// and guessing at a lossy comparison would risk unsetting an unrelated var.
/// The one line that says which shell a pane launched, next to the one the
/// config asked for. Logged at info level by `spawn_params` under the
/// `paneflow::terminal::backend` target, so `RUST_LOG=info` tells an ignored
/// `default_shell` apart from a shell that is merely slow to start.
fn shell_resolution_log_line(resolved: &str, configured: Option<&str>) -> String {
    format!("Terminal shell resolved: {resolved:?} (default_shell={configured:?})")
}

pub(super) fn inherited_env_keys_to_strip() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key.to_str().is_some_and(|key| {
                is_inherited_host_terminal_env_key(key) || is_inherited_agent_session_env_key(key)
            })
        })
        .collect()
}

/// True if `key` is a well-formed environment variable name safe to insert into
/// a child env block: non-empty and free of `=` and NUL, which would otherwise
/// corrupt the name/value framing. Charset is intentionally NOT restricted to a
/// strict POSIX set - legitimate user vars (e.g. `ANTHROPIC_API_KEY`) are
/// already all-caps `[A-Z0-9_]`, and over-restricting would silently drop valid
/// keys.
fn is_valid_env_name(key: &str) -> bool {
    !key.is_empty() && !key.contains('=') && !key.contains('\0')
}

/// Locale names installed on this machine, read once per process. macOS builds
/// `locale -a` from the entries of `/usr/share/locale` (plus the builtin `C` /
/// `POSIX`), so reading that directory lists the same locale names without
/// spawning a subprocess on the render thread. Empty when the directory cannot
/// be read, which makes the UTF-8 override a no-op rather than naming a locale
/// the system may not have.
fn installed_locale_names() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| match std::fs::read_dir("/usr/share/locale") {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(err) => {
            log::warn!(
                target: "paneflow::terminal::backend",
                "could not enumerate installed locales: {err}"
            );
            Vec::new()
        }
    })
}

/// The language/territory part of a locale name: `fr_FR` from
/// `fr_FR.ISO8859-1@euro`.
fn locale_base(locale: &str) -> &str {
    let base = locale.split('.').next().unwrap_or(locale);
    base.split('@').next().unwrap_or(base)
}

/// True if `locale` selects the UTF-8 codeset (`en_US.UTF-8`, `C.UTF-8`, ...).
fn is_utf8_locale(locale: &str) -> bool {
    locale
        .split_once('.')
        .map(|(_, codeset)| codeset.split('@').next().unwrap_or(codeset))
        .is_some_and(|codeset| {
            codeset.eq_ignore_ascii_case("UTF-8") || codeset.eq_ignore_ascii_case("UTF8")
        })
}

/// Pick the locale to force on a child PTY, or `None` when the inherited block
/// already selects UTF-8 (or no listed candidate exists).
///
/// The character encoding a child ends up with is `LC_ALL`, else `LC_CTYPE`,
/// else `LANG` - so a non-UTF-8 `LC_ALL` has to be overridden, not merely
/// out-voted by a `LANG` insert. Candidates, first one `available` wins: the
/// inherited language/territory re-spelled as UTF-8 (keeps a non-English
/// machine in its own language), then `C.UTF-8`, then `en_US.UTF-8`. Nothing
/// listed means nothing is forced: naming an uninstalled locale is what makes a
/// shell print `setlocale: LC_ALL: cannot change locale`.
fn utf8_locale_override(
    lang: Option<&str>,
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    available: &[String],
) -> Option<String> {
    fn set(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }
    let effective = set(lc_all).or(set(lc_ctype)).or(set(lang));
    if effective.is_some_and(is_utf8_locale) {
        return None;
    }
    let preferred = [set(lc_all), set(lc_ctype), set(lang)]
        .into_iter()
        .flatten()
        .map(locale_base)
        .find(|base| !matches!(*base, "C" | "POSIX"))
        .map(|base| format!("{base}.UTF-8"));
    preferred
        .into_iter()
        .chain(["C.UTF-8".to_string(), "en_US.UTF-8".to_string()])
        .find(|candidate| available.iter().any(|name| name == candidate))
}

/// Assemble the child PTY environment: PaneFlow identity vars, explicit TERM /
/// locale / terminal-program identification, the AI-hook PATH prepend, and the
/// user-env merge (a user var wins on collision EXCEPT the protected keys
/// PaneFlow owns and `PANEFLOW_BIN_DIR` is re-prepended after any user PATH).
/// Pure except for `inject_ai_hook_env` staging the shim
/// binaries, so the env contract stays unit-testable now that the mockable
/// `PtyBackend::spawn` seam is gone (EP-002 US-004). Mirrors Zed's
/// `insert_zed_terminal_env`.
fn assemble_pty_env(
    mut env: std::collections::HashMap<String, String>,
    workspace_id: u64,
    surface_id: u64,
    user_env: Option<std::collections::HashMap<String, String>>,
) -> std::collections::HashMap<String, String> {
    // PaneFlow identity vars (AI-hook + MCP bridge integration).
    // `0` is reserved for detached terminals such as discovered worktree Review
    // terminals. Do not advertise a fake workspace id to the IPC hook.
    if workspace_id != 0 {
        env.insert("PANEFLOW_WORKSPACE_ID".into(), workspace_id.to_string());
    }
    env.insert("PANEFLOW_SURFACE_ID".into(), surface_id.to_string());
    if let Some(socket_path) = paneflow_socket_path() {
        env.insert("PANEFLOW_SOCKET_PATH".into(), socket_path);
    }

    // Propagate the opt-in hook-diagnostic log path explicitly so the whole
    // chain (shell → shim → agent → ai-hook) appends to the same file even if
    // a PTY backend ever clears the inherited env. No-op when unset.
    if let Some(log_path) = std::env::var_os("PANEFLOW_HOOK_LOG")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
    {
        env.insert("PANEFLOW_HOOK_LOG".into(), log_path);
    }

    // Explicit TERM so TUI apps detect capabilities correctly.
    env.insert("TERM".into(), "xterm-256color".into());

    // Ensure a UTF-8 locale in minimal environments (GUI launches with no
    // locale block, containers, etc.). POSIX resolves the character encoding as
    // LC_ALL > LC_CTYPE > LANG, so setting LANG alone silently lost to an
    // inherited `LC_ALL=C`; the override sets both, and only ever names a
    // locale this machine actually has installed.
    if let Some(locale) = utf8_locale_override(
        std::env::var("LANG").ok().as_deref(),
        std::env::var("LC_ALL").ok().as_deref(),
        std::env::var("LC_CTYPE").ok().as_deref(),
        installed_locale_names(),
    ) {
        env.insert("LANG".into(), locale.clone());
        env.insert("LC_ALL".into(), locale);
    }

    // Standard terminal identification for capability detection.
    // `TERM_PROGRAM=ghostty` (#184): the agent CLIs key their OSC 9;4 and
    // Kitty-graphics behaviour on it, and the engine really is Ghostty's. The
    // runtime re-asserts both at spawn (`ghostty_session.rs`), so there is one
    // answer whichever layer a reader looks at.
    env.insert("TERM_PROGRAM".into(), "ghostty".into());
    env.insert(
        "TERM_PROGRAM_VERSION".into(),
        paneflow_terminal_ghostty::GHOSTTY_APP_VERSION.into(),
    );
    env.insert("COLORTERM".into(), "truecolor".into());

    // Reset SHLVL so the child shell starts fresh at 1. The child inherits the
    // parent environment (no `env_clear`), so the value Paneflow itself
    // inherited (typically >= 2 when launched from a terminal) has to be
    // actively overridden; otherwise nested-shell prompt detection breaks
    // (oh-my-zsh subshell banner, fish $SHLVL gating). "0" makes the shell
    // initialize it to 1.
    env.insert("SHLVL".into(), "0".into());

    // Cross-platform AI-hook PATH-prepend: stage the embedded shim binaries and
    // prepend their dir to `$PATH` so `claude`/`codex` route through the shim.
    // Silent-fail (the terminal still opens). Sets `PANEFLOW_BIN_DIR`.
    inject_ai_hook_env(&mut env);

    // Snapshot zsh-integration keys before the untrusted merge so a hostile
    // ZDOTDIR / PANEFLOW_ORIG_ZDOTDIR cannot stick even if the skip lists drift.
    let integration_zdotdir = env.get(ZDOTDIR_ENV).cloned();
    let integration_orig_zdotdir = env.get(PANEFLOW_ORIG_ZDOTDIR_ENV).cloned();

    // Merge user-supplied env on top, EXCEPT the protected keys PaneFlow owns:
    // TERM/COLORTERM/TERM_PROGRAM drive capability detection; SHLVL is reset so
    // shells start fresh; the PANEFLOW_* identity vars are how the MCP bridge
    // and the AI-hook shim find PaneFlow - letting a user clobber them would
    // silently break those features.
    if let Some(user_vars) = user_env {
        const PROTECTED: &[&str] = &[
            "TERM",
            "COLORTERM",
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "SHLVL",
            "PANEFLOW_WORKSPACE_ID",
            "PANEFLOW_SURFACE_ID",
            "PANEFLOW_SOCKET_PATH",
            "PANEFLOW_BIN_DIR",
            ZDOTDIR_ENV,
            PANEFLOW_ORIG_ZDOTDIR_ENV,
        ];
        for (k, v) in user_vars {
            // Reject malformed env names (empty / `=` / NUL) and drop
            // dynamic-loader-influencing keys (LD_* / DYLD_*) outright: an
            // imported `session.json` surface env or the global `terminal.env`
            // is untrusted, and these inject a bundled `.so` into the spawned
            // shell (RCE). `PATH` is deliberately still mergeable here (a
            // documented US-014 use case), but PANEFLOW_BIN_DIR is re-prepended
            // after the merge so agent commands still route through the shim.
            if !is_valid_env_name(&k) || is_forbidden_child_env_key(&k) {
                continue;
            }
            if PROTECTED.contains(&k.as_str()) {
                continue;
            }
            env.insert(k, v);
        }
    }

    // Runs after the user/session merge so neither an inherited value nor a
    // hand-written `terminal.env` entry can put these back.
    env.retain(|k, _| !is_inherited_agent_session_env_key(k));
    reassert_paneflow_bin_dir_first(&mut env);
    reassert_shell_integration_zdotdir(&mut env, integration_zdotdir, integration_orig_zdotdir);

    env
}

/// Restore zsh-integration `ZDOTDIR` / `PANEFLOW_ORIG_ZDOTDIR` after the
/// untrusted env merge. If integration did not set a key, drop any value that
/// slipped through so a hostile pane env cannot point zsh at attacker rc.
fn reassert_shell_integration_zdotdir(
    env: &mut std::collections::HashMap<String, String>,
    zdotdir: Option<String>,
    orig_zdotdir: Option<String>,
) {
    restore_or_drop_env_key(env, ZDOTDIR_ENV, zdotdir);
    restore_or_drop_env_key(env, PANEFLOW_ORIG_ZDOTDIR_ENV, orig_zdotdir);
}

fn restore_or_drop_env_key(
    env: &mut std::collections::HashMap<String, String>,
    key: &str,
    value: Option<String>,
) {
    match value {
        Some(value) => {
            env.insert(key.into(), value);
        }
        None => {
            env.remove(key);
        }
    }
}

/// A `Send` handle over one surface's runtime for the scrollback reads that
/// must not block the GPUI thread (issue #363).
///
/// [`GhosttySession::request`] parks its caller on the runtime's reply for up
/// to a second, so a slow or silent runtime froze painting for that long on
/// every `surface.read` (`wait`/`flow` poll one every 500 ms) and once per
/// `SearchChunk` of a `surface.search`. The handle is cloned out of the
/// entity on the render thread and the wait happens on a background worker.
#[derive(Clone)]
pub(crate) struct ScrollbackReader {
    ghostty: GhosttySession,
    title: String,
    child_pid: u32,
}

impl ScrollbackReader {
    /// Windowed extract for `surface.read`, the body
    /// [`TerminalState::extract_scrollback_window`] delegates to.
    ///
    /// **Blocking**: waits on the runtime thread. Call it from inside
    /// `smol::unblock`, never on the GPUI thread.
    pub(crate) fn extract_scrollback_window(
        &self,
        lines: usize,
        offset: usize,
    ) -> Option<(String, usize, usize, bool)> {
        let reason = match self.ghostty.transcript(lines, offset) {
            Some(Ok(window)) => {
                return Some((window.text, window.returned, window.total, window.eof));
            }
            Some(Err(error)) => format!("engine error: {error}"),
            None => "the runtime did not answer (mailbox full or closed, or no reply within 1 s)"
                .to_owned(),
        };
        log::warn!(
            target: "paneflow::terminal::ghostty",
            "transcript of surface {:?} (child pid {}) unavailable: {reason}",
            self.title,
            self.child_pid
        );
        None
    }

    /// Search the scrollback for `pattern` (plain-text, case-insensitive) and
    /// return matching lines as `(grid_line, text)` pairs, deduped by line and
    /// capped at `max_matches`. The bool is `true` when the cap (or the cell
    /// budget) truncated an otherwise finished scan. Backs the
    /// `surface.search` IPC method. The engine performs the search and the
    /// matched-line extraction atomically on its runtime thread.
    ///
    /// `Err` is a runtime that did not answer (mailbox full or closed, no
    /// reply within a second) or an engine that failed the scan. Issue #362:
    /// that is not a finished scan with no hits, and it is not the cap
    /// either - `surface.search` turns it into an error, the way
    /// `surface.read` already does, so a caller cannot read a wedged pane as
    /// "pattern absent" or "raise max_matches".
    ///
    /// **Blocking**: one runtime wait per chunk. Same rule as
    /// [`Self::extract_scrollback_window`].
    pub(crate) fn search_scrollback(
        &self,
        pattern: &str,
        max_matches: usize,
    ) -> Result<(Vec<(i32, String)>, bool), String> {
        if pattern.is_empty() || max_matches == 0 {
            return Ok((Vec::new(), false));
        }
        self.ghostty.search_scrollback(pattern, max_matches)
    }
}

/// Spawn-time pin for `child_pid`. Same encoding as session `proc_start`
/// (`pbi_start_tvsec`/`pbi_start_tvusec`). EPERM and dead-pid races degrade
/// to `None`.
#[cfg(target_os = "macos")]
fn child_pid_start_time(pid: u32) -> Option<u64> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let info = pidinfo::<BSDInfo>(pid as i32, 0).ok()?;
    Some(
        info.pbi_start_tvsec
            .wrapping_mul(1_000_000)
            .wrapping_add(info.pbi_start_tvusec),
    )
}

/// Cap `result` at `max_chars` bytes keeping the NEWEST text: the cut lands
/// on a UTF-8 char boundary and then moves forward to the next line start, so
/// the kept text begins on a complete row (one oversized line with no newline
/// is dropped whole). Mirrors the engine's `cap_complete_lines`, because
/// [`TerminalState::extract_scrollback_capped`] hands this the tail of the
/// transcript for undo to replay - cutting the head kept the oldest rows and
/// dropped the screen.
///
/// `String::drain` panics if the byte index is not on a char boundary, and
/// scrollback is real grid text (CJK, emoji, box drawing are routine agent
/// output), so the index is raised to a boundary first. The tail is the
/// original tail, so the partial-escape strip is only a guard against grid
/// text that already ended mid-sequence.
pub(super) fn cap_scrollback_at_char_boundary(result: &mut String, max_chars: usize) {
    if result.len() <= max_chars {
        return;
    }
    let mut start = result.ceil_char_boundary(result.len() - max_chars);
    if start > 0 && result.as_bytes()[start - 1] != b'\n' {
        match result[start..].find('\n') {
            Some(newline) => start += newline + 1,
            None => {
                result.clear();
                return;
            }
        }
    }
    result.drain(..start);
    strip_partial_ansi_tail(result);
}

/// Strip any partial ANSI escape sequence from the end of a truncated string.
///
/// Scans backward from the end for an ESC (`\x1b`) that starts a CSI (`\x1b[`),
/// OSC (`\x1b]`), or DCS (`\x1bP`) sequence. If the sequence is unterminated
/// (no final byte in the valid range), it is removed. Plain text is untouched.
pub(super) fn strip_partial_ansi_tail(text: &mut String) {
    let Some(esc_pos) = text.rfind('\x1b') else {
        return;
    };

    let tail = &text[esc_pos..];
    let bytes = tail.as_bytes();

    if bytes.len() < 2 {
        text.truncate(esc_pos);
        return;
    }

    match bytes[1] {
        b'[' => {
            // CSI: terminated by a byte in 0x40..=0x7E
            let terminated = bytes[2..].iter().any(|&b| (0x40..=0x7E).contains(&b));
            if !terminated {
                text.truncate(esc_pos);
            }
        }
        b']' => {
            // OSC: terminated by BEL or ST
            let terminated = bytes[2..].contains(&0x07) || tail[2..].contains("\x1b\\");
            if !terminated {
                text.truncate(esc_pos);
            }
        }
        b'P' => {
            // DCS: terminated by ST
            let terminated = tail[2..].contains("\x1b\\");
            if !terminated {
                text.truncate(esc_pos);
            }
        }
        _ => {
            // Other ESC sequences (SS2, SS3, …) are two bytes - complete as-is.
        }
    }
}

/// True when `pid` is still the leader of its own process group. This holds
/// both for the PTY session leader portable-pty spawns (`setsid()`) and for a
/// foreground job-control group leader. After a leader exits the kernel can
/// recycle `pid` onto an unrelated leader, so callers must also require the
/// pinned process start ([`crate::agents::parent_guard::may_signal_group`]).
#[cfg(all(unix, test))]
fn is_process_group_leader(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: getpgid is a pure query; it returns the pgid, or -1 (ESRCH) when
    // no such process exists - neither equals our positive `pid` unless `pid`
    // is genuinely its own group leader.
    unsafe { libc::getpgid(pid) == pid }
}

#[cfg(all(unix, test))]
fn may_signal_own_session(pid: i32, pinned_start: Option<u64>) -> bool {
    crate::agents::parent_guard::may_signal_group(
        pid,
        pinned_start,
        child_pid_start_time(pid as u32),
        is_process_group_leader(pid),
    )
}

/// Resolve a distinct PTY foreground job-control group while the duplicated
/// master fd is still open. `tcgetpgrp` alone is not ownership evidence: a
/// stale/recycled PGID must also still belong to the shell's terminal session
/// and have at least one live member whose process start we can pin. Pinning
/// members rather than the numeric leader keeps ordinary pipelines killable
/// after the process that originally supplied their PGID has exited.
#[cfg(all(target_os = "macos", test))]
fn foreground_process_group(
    pty_master_fd: i32,
    shell_pid: u32,
) -> Option<crate::agents::parent_guard::PinnedProcessGroup> {
    crate::agents::parent_guard::pin_foreground_process_group(pty_master_fd, shell_pid)
}

/// Send SIGTERM to the child's process group, guarded by leader + start-time
/// identity so a dead or recycled `pid` is a harmless no-op. Returns true if
/// SIGTERM was delivered. Factored out of `Drop` so the graceful-shutdown
/// step is unit-testable.
#[cfg(all(unix, test))]
fn terminate_process_group(pid: i32, pinned_start: Option<u64>) -> bool {
    if !may_signal_own_session(pid, pinned_start) {
        return false;
    }
    // SAFETY: kill(-pid, SIGTERM) signals every member of the group; FFI-safe
    // with the positive `pid` we just confirmed is our session leader with a
    // matching spawn-time pin.
    unsafe { libc::kill(-pid, libc::SIGTERM) == 0 }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        // 1. Resolve every live process group in the authenticated PTY session
        //    while the app-owned master dup is still open. Foreground, stopped,
        //    background and disowned jobs may all use PGIDs distinct from
        //    `child_pid`.
        #[cfg(target_os = "macos")]
        let terminal_session_groups = self.pty_master_fd.as_ref().map(|fd| {
            crate::agents::parent_guard::pin_terminal_session_process_groups(
                fd.as_raw_fd(),
                self.child_pid,
            )
        });

        let executor = self.background_executor.clone();

        #[cfg(target_os = "macos")]
        let groups = match terminal_session_groups {
            Some(Some(groups)) => groups,
            // The PTY existed but complete enumeration/authentication failed:
            // do not mistake a partial set for safe coverage. The runtime still
            // closes the master below, so the session gets SIGHUP.
            Some(None) => Vec::new(),
            // No PTY dup (display-only or spawn failure): the strict
            // original-leader fallback.
            None => crate::agents::parent_guard::pin_session_process_group(
                self.child_pid,
                self.child_proc_start,
            )
            .into_iter()
            .collect(),
        };

        if !groups.is_empty() {
            // 2. External watchers for every captured group BEFORE any signal, so
            //    orderly teardown survives immediate application exit.
            #[cfg(all(target_os = "macos", not(test)))]
            let teardown_guards = groups
                .iter()
                .cloned()
                .filter_map(crate::agents::parent_guard::spawn_process_group_guard)
                .collect::<Vec<_>>();

            // 3. SIGTERM every pinned group synchronously while the tty is still
            //    alive, so agents and shells run their TERM handlers (state
            //    checkpoint, HISTFILE flush) before the 100 ms SIGKILL below.
            for group in &groups {
                crate::agents::parent_guard::signal_pinned_process_group(group, libc::SIGTERM);
            }

            // 4. Close each external guard's control pipe while Drop is still
            //    running; the local timer below stays as the fallback.
            #[cfg(all(unix, not(test)))]
            drop(self.pty_guard.take());
            #[cfg(all(target_os = "macos", not(test)))]
            drop(teardown_guards);

            // 7. (scheduled) Re-check each target's PID/start/PGID/session
            //    identity at fire time; PGID reuse fails closed. The runtime
            //    thread never signals (see `ghostty_session::reap_child_bounded`).
            let kill = move || {
                for group in groups {
                    crate::agents::parent_guard::signal_pinned_process_group(&group, libc::SIGKILL);
                }
            };
            match executor {
                Some(bg) => {
                    bg.spawn(async move {
                        smol::Timer::after(std::time::Duration::from_millis(100)).await;
                        kill();
                    })
                    .detach();
                }
                None => {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        kill();
                    });
                }
            }
        } else {
            #[cfg(all(unix, not(test)))]
            drop(self.pty_guard.take());
        }

        // 5. Ask the runtime to shut down: it closes the PTY and reaps the
        //    child; it does not signal.
        self.ghostty.shutdown();
        self.child_pid = 0;

        // 6. Close the app-owned master dup exactly once.
        drop(self.pty_master_fd.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RUST_LOG=info` must show the resolved shell next to the configured
    /// one: both values, in one line, whether or not a shell was configured.
    #[test]
    fn shell_resolution_log_line_names_the_resolved_and_the_configured_shell() {
        assert_eq!(
            shell_resolution_log_line("/bin/zsh", Some("/opt/homebrew/bin/fish")),
            "Terminal shell resolved: \"/bin/zsh\" (default_shell=Some(\"/opt/homebrew/bin/fish\"))"
        );
        assert_eq!(
            shell_resolution_log_line("/bin/zsh", None),
            "Terminal shell resolved: \"/bin/zsh\" (default_shell=None)"
        );
        // The real call site feeds the config's `default_shell` through the
        // same trim-and-drop-empty filter and the same resolver.
        let resolved = resolve_default_shell(Some("/bin/sh"));
        let line = shell_resolution_log_line(&resolved, Some("/bin/sh"));
        assert!(line.contains("\"/bin/sh\""), "{line}");
        assert!(line.contains("default_shell=Some(\"/bin/sh\")"), "{line}");
    }
    use std::collections::HashMap;
    use std::io::Read;
    use std::path::{Path, PathBuf};

    fn platform_sep() -> char {
        ':'
    }

    #[test]
    fn new_pending_terminal_has_no_child_until_promoted() {
        // a pending terminal carries no child until the off-thread
        // start resolves and `promote_ghostty` swaps the runtime in.
        let (state, _pending) = TerminalState::new_pending(80, 24);
        assert_eq!(state.child_pid, 0);
    }

    #[test]
    fn spawn_failure_is_reported_once_without_sensitive_error_text() {
        const CANARY: &str =
            r#"C:\Users\synthetic-user\private\launch.ps1 --token super-secret-canary"#;
        let error = anyhow::Error::new(io::Error::from_raw_os_error(5)).context(CANARY);
        let os_error = raw_os_error_from_anyhow(&error);
        assert_eq!(os_error, Some(5));

        let failure = TerminalBackendFailureDiagnostics::new(
            TerminalBackendFailurePhase::OpenPty,
            TerminalBackendFailureDiagnostics::GHOSTTY_OPEN_PTY_FAILED,
            os_error,
        );
        let mut state = TerminalState::new_display_only(24, 80);
        state.report_spawn_failure(failure.clone(), "engine start failed");

        let diagnostics = state.backend_diagnostics();
        assert_eq!(diagnostics.failure, Some(failure));
        let formatted = diagnostics.to_string();
        assert_eq!(formatted.matches("reason_code=").count(), 1);
        assert!(formatted.contains("failure_phase=open_pty"));
        assert!(formatted.contains("reason_code=ghostty_open_pty_failed"));
        assert!(formatted.contains("os_error=5"));
        assert!(!formatted.contains(CANARY));
        assert!(!formatted.contains("private"));
        assert!(!formatted.contains("super-secret-canary"));
    }

    #[test]
    fn backend_failure_phases_and_reason_codes_are_stable() {
        assert_eq!(
            TerminalBackendFailurePhase::Initialization.as_str(),
            "initialization"
        );
        assert_eq!(TerminalBackendFailurePhase::OpenPty.as_str(), "open_pty");
        assert_eq!(TerminalBackendFailurePhase::Spawn.as_str(), "spawn");
        assert_eq!(
            TerminalBackendFailurePhase::PostSpawn.as_str(),
            "post_spawn"
        );
        assert_eq!(
            TerminalBackendFailureDiagnostics::GHOSTTY_POST_SPAWN_FAILED,
            "ghostty_post_spawn_failed"
        );
    }

    #[test]
    fn backend_diagnostics_expose_target_triple() {
        let diagnostics = TerminalState::new_display_only(24, 80).backend_diagnostics();
        assert_eq!(diagnostics.target_triple, env!("PANEFLOW_TARGET_TRIPLE"));
        // a macOS bug report must state the triple it came from.
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(diagnostics.target_triple, "aarch64-apple-darwin");
    }

    #[test]
    fn backend_diagnostics_expose_pinned_ghostty_build_identity() {
        let diagnostics = TerminalState::new_display_only(24, 80).backend_diagnostics();
        let ghostty = diagnostics.ghostty;
        let identity = paneflow_terminal_ghostty::build_identity();
        assert_eq!(
            ghostty.version,
            paneflow_terminal_ghostty::GHOSTTY_APP_VERSION
        );
        assert_eq!(ghostty.source_sha, identity.source_sha);
        assert_eq!(ghostty.api_version, identity.api_version);
        assert_eq!(ghostty.zig_version, identity.zig_version);
        assert_eq!(ghostty.optimization, identity.optimization);
        assert_eq!(ghostty.simd, identity.simd);
    }

    #[test]
    fn write_to_pty_buffers_input_while_display_only() {
        // US-012 regression: the launch pad's agent picker writes the
        // launch command the instant a terminal mounts - before the off-thread
        // fork promotes the PTY. The display-only notifier drops writes, so
        // without this queue the command (e.g. `claude`) is lost and the
        // terminal opens to a bare shell. `write_to_pty` must buffer instead.
        let (state, _events_tx) = TerminalState::new_pending(80, 24);
        state.write_to_pty(b"claude\r".to_vec());
        let queued = state.pending_input.lock().expect("pending_input lock");
        assert_eq!(queued.len(), 1);
        assert!(
            matches!(&queued[0], PendingTerminalInput::Raw(bytes) if bytes.as_ref() == b"claude\r")
        );
    }

    #[test]
    fn pending_input_is_bounded() {
        // A terminal that never promotes (spawn failure) must not accumulate
        // input without bound: writes past the cap are dropped, not queued.
        let (state, _events_tx) = TerminalState::new_pending(80, 24);
        let chunk = vec![b'x'; 8 * 1024];
        for _ in 0..(MAX_PENDING_INPUT_BYTES / chunk.len() + 2) {
            state.write_to_pty(chunk.clone());
        }
        let queued: usize = state
            .pending_input
            .lock()
            .expect("pending_input lock")
            .iter()
            .map(PendingTerminalInput::queued_bytes)
            .sum();
        assert!(
            queued <= MAX_PENDING_INPUT_BYTES,
            "buffered {queued} bytes exceeds the {MAX_PENDING_INPUT_BYTES} cap"
        );
    }

    fn test_key_input(
        action: paneflow_terminal_ghostty::KeyAction,
    ) -> paneflow_terminal_ghostty::KeyInput {
        paneflow_terminal_ghostty::KeyInput {
            key: paneflow_terminal_ghostty::Key::Function(5),
            action,
            modifiers: paneflow_terminal_ghostty::Modifiers::CONTROL,
            consumed_modifiers: paneflow_terminal_ghostty::Modifiers::empty(),
            text: String::new(),
            unshifted_codepoint: None,
            composing: false,
        }
    }

    fn test_mouse_input(
        action: paneflow_terminal_ghostty::MouseAction,
    ) -> paneflow_terminal_ghostty::MouseInput {
        paneflow_terminal_ghostty::MouseInput {
            action,
            button: Some(paneflow_terminal_ghostty::MouseButton::Left),
            modifiers: paneflow_terminal_ghostty::Modifiers::empty(),
            x: 8.0,
            y: 16.0,
            screen_width: 640,
            screen_height: 384,
            padding_top: 0,
            padding_bottom: 0,
            padding_left: 0,
            padding_right: 0,
            any_button_pressed: action != paneflow_terminal_ghostty::MouseAction::Release,
        }
    }

    #[test]
    fn control_releases_fit_after_general_input_saturates() {
        let general_limit = MAX_PENDING_INPUT_BYTES - INPUT_CONTROL_RESERVE_BYTES;
        let press =
            PendingTerminalInput::Key(test_key_input(paneflow_terminal_ghostty::KeyAction::Press));
        let key_release = PendingTerminalInput::Key(test_key_input(
            paneflow_terminal_ghostty::KeyAction::Release,
        ));
        let mouse_release = PendingTerminalInput::Mouse {
            input: test_mouse_input(paneflow_terminal_ghostty::MouseAction::Release),
            repeat: 1,
        };
        let focus = PendingTerminalInput::Focus(paneflow_terminal_ghostty::FocusEvent::Lost);

        assert!(!press.fits_after(general_limit));
        assert!(key_release.fits_after(general_limit));
        assert!(mouse_release.fits_after(general_limit));
        assert!(focus.fits_after(general_limit));
    }

    #[test]
    fn structured_input_is_queued_in_order_before_promotion() {
        // keys, mouse reports, focus events and pastes issued before
        // the off-thread start resolves are queued, in order, rather than lost.
        let (state, _pending) = TerminalState::new_pending(80, 24);

        assert_eq!(
            state.write_ghostty_key(test_key_input(paneflow_terminal_ghostty::KeyAction::Press)),
            BackendInputResult::Accepted
        );
        assert_eq!(
            state.write_ghostty_mouse(
                test_mouse_input(paneflow_terminal_ghostty::MouseAction::Press),
                2,
            ),
            BackendInputResult::Accepted
        );
        assert_eq!(
            state.write_ghostty_focus(paneflow_terminal_ghostty::FocusEvent::Gained),
            BackendInputResult::Accepted
        );
        assert_eq!(
            state.write_ghostty_paste("paste".to_string()),
            BackendInputResult::Accepted
        );

        let queued = state.pending_input.lock().expect("pending_input lock");
        assert_eq!(queued.len(), 4);
        assert!(matches!(queued[0], PendingTerminalInput::Key(_)));
        assert!(matches!(queued[1], PendingTerminalInput::Mouse { .. }));
        assert!(matches!(queued[2], PendingTerminalInput::Focus(_)));
        assert!(matches!(queued[3], PendingTerminalInput::Paste { .. }));
    }

    #[test]
    fn resolve_spawn_params_honors_initial_size() {
        // US-012: the cheap, render-thread-safe half of a spawn picks up the
        // requested grid size (and the 120x40 default when unspecified).
        let p = TerminalState::resolve_spawn_params(None, 1, 1, Some((100, 30)), None);
        assert_eq!((p.cols, p.rows), (100, 30));
        let d = TerminalState::resolve_spawn_params(None, 1, 1, None, None);
        assert_eq!((d.cols, d.rows), (120, 40));
    }

    #[test]
    fn resolve_spawn_params_uses_the_supplied_config_snapshot() {
        // Issue #298: the shell and the global `terminal.env` come from the
        // caller's in-memory snapshot, not from a re-read of `paneflow.json`.
        let config = paneflow_config::schema::PaneFlowConfig {
            default_shell: Some("/bin/sh".to_string()),
            terminal: Some(TerminalConfig {
                env: Some(HashMap::from([(
                    "PANEFLOW_TEST_SNAPSHOT_ENV".to_string(),
                    "from-memory".to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = TerminalState::resolve_spawn_params_with_profile(
            None,
            1,
            1,
            None,
            None,
            TerminalSurfaceProfile::Normal,
            &config,
        );
        assert_eq!(p.shell, "/bin/sh");
        assert_eq!(
            p.env.get("PANEFLOW_TEST_SNAPSHOT_ENV").map(String::as_str),
            Some("from-memory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_foreground_signal_mask_succeeds_on_unix() {
        // US-012: the foreground mask snapshot must succeed on the main thread
        // so the off-thread spawn can hand it to the child (Ctrl-C parity).
        assert!(capture_foreground_signal_mask().is_some());
    }

    #[test]
    fn prepend_puts_bin_dir_first_and_preserves_existing_entries() {
        let mut env: HashMap<String, String> = HashMap::new();
        let sep = platform_sep();
        env.insert("PATH".into(), format!("/usr/bin{sep}/usr/local/bin"));

        let bin_dir = PathBuf::from("/home/u/.cache/paneflow/bin/0.2.6");
        prepend_bin_dir_to_path(&mut env, &bin_dir);

        let joined = env.get("PATH").expect("PATH set by helper");
        let components: Vec<PathBuf> = std::env::split_paths(joined).collect();
        assert_eq!(
            components.first(),
            Some(&bin_dir),
            "US-009 AC: bin_dir must be first on PATH; got {components:?}"
        );
        assert!(
            components.iter().any(|p| p == Path::new("/usr/bin")),
            "US-009: original PATH entries must be preserved; got {components:?}"
        );
        assert!(
            components.iter().any(|p| p == Path::new("/usr/local/bin")),
            "US-009: original PATH entries must be preserved; got {components:?}"
        );
    }

    #[test]
    fn prepend_inserts_bin_dir_even_when_env_path_absent() {
        // AC: "If env map has no PATH, helper still sets PATH so the
        // child inherits the shim dir rather than silently no-op."
        let mut env: HashMap<String, String> = HashMap::new();
        let bin_dir = PathBuf::from("/tmp/paneflow-bins");
        prepend_bin_dir_to_path(&mut env, &bin_dir);

        let joined = env.get("PATH").expect("PATH set by helper");
        let components: Vec<PathBuf> = std::env::split_paths(joined).collect();
        assert_eq!(
            components.first(),
            Some(&bin_dir),
            "US-009: bin_dir must be first on PATH in the no-prior-PATH case"
        );
    }

    #[test]
    fn prepend_uses_platform_separator() {
        // Round-trip invariant: split_paths(join_paths(X)) == X. This
        // implicitly tests that the `:` separator is handled correctly - we
        // do not assert the raw bytes.
        let mut env: HashMap<String, String> = HashMap::new();
        let sep = platform_sep();
        env.insert("PATH".into(), format!("/a{sep}/b{sep}/c"));
        let bin_dir = PathBuf::from("/z");
        prepend_bin_dir_to_path(&mut env, &bin_dir);

        let joined = env.get("PATH").unwrap();
        let components: Vec<PathBuf> = std::env::split_paths(joined).collect();
        assert_eq!(
            components,
            vec![
                PathBuf::from("/z"),
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
            ],
            "US-009: split_paths(join_paths(...)) must round-trip on all platforms"
        );
    }

    #[test]
    fn prepend_treats_empty_path_as_absent() {
        // An empty `PATH` is not absent - `split_paths("")` on Unix
        // yields one `PathBuf::from("")` component that `execvp`
        // resolves as the CWD. We must NOT inherit that phantom entry
        // onto the child's PATH (shell-injection surface).
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("PATH".into(), String::new());
        let bin_dir = PathBuf::from("/z");
        prepend_bin_dir_to_path(&mut env, &bin_dir);

        let joined = env.get("PATH").expect("PATH set by helper");
        let components: Vec<PathBuf> = std::env::split_paths(joined).collect();
        assert!(
            !components.iter().any(|p| p.as_os_str().is_empty()),
            "US-009 hardening: empty PATH must not yield a phantom CWD entry; got {components:?}"
        );
        assert_eq!(
            components.first(),
            Some(&bin_dir),
            "US-009: bin_dir must still be first when empty PATH is treated as absent"
        );
    }

    // -----------------------------------------------------------------
    // US-003 - exit-status correctness (real code, first-write-wins).
    // -----------------------------------------------------------------

    #[test]
    fn child_exit_records_real_code_not_sentinel() {
        // US-003 AC: a real child exit code must round-trip into `exited`,
        // not the -1 fallback. The engine's runtime thread already decoded the
        // platform `ExitStatus` into this event, so the assertion is on the
        // state transition, not on the per-OS status encoding.
        let mut state = TerminalState::new_display_only(24, 80);
        assert!(state.exited.is_none(), "fresh terminal has no exit code");

        state.process_ghostty_event(GhosttyUiEvent::ChildExited {
            code: 42,
            signal: None,
        });
        assert_eq!(
            state.exited,
            Some(42),
            "the real exit code must be recorded, not -1"
        );
    }

    #[test]
    fn exit_fallback_does_not_clobber_real_child_exit_code() {
        // first-write-wins. A runtime failure reported after a
        // real child exit (the engine can publish both when the mailbox tears
        // down) must never overwrite the code already recorded.
        let mut state = TerminalState::new_display_only(24, 80);

        state.process_ghostty_event(GhosttyUiEvent::ChildExited {
            code: 1,
            signal: None,
        });
        state.process_ghostty_event(GhosttyUiEvent::RuntimeFailed(
            "engine mailbox closed".to_owned(),
        ));
        assert_eq!(
            state.exited,
            Some(1),
            "a later failure must not clobber the real exit code"
        );
    }

    // -----------------------------------------------------------------
    // US-002 - keep pane open on launch failure (keyboard_input_sent).
    // -----------------------------------------------------------------

    #[test]
    fn close_on_exit_discriminator_covers_both_branches() {
        // US-002 AC: clean exit (code 0) closes even with no input.
        let mut clean = TerminalState::new_display_only(24, 80);
        clean.exited = Some(0);
        assert!(
            clean.should_close_on_exit(),
            "US-002: a clean exit (code 0) must close the pane"
        );

        // Non-zero exit with NO user input = spawn/launch failure → stays open
        // so the exit overlay can render the code.
        let mut failed = TerminalState::new_display_only(24, 80);
        failed.exited = Some(127);
        assert!(
            !failed.should_close_on_exit(),
            "US-002: a non-zero exit with no input must keep the pane open"
        );

        // ...but once the user has interacted, ANY exit closes.
        failed.write_to_pty(b"x".as_slice());
        assert!(
            failed.should_close_on_exit(),
            "US-002: after user input, a non-zero exit must close the pane"
        );
    }

    #[test]
    fn write_to_pty_marks_user_input_but_fresh_state_does_not() {
        // US-002: a fresh terminal has not received user input; write_to_pty
        // flips the flag. (Automated writes use notifier.notify and are tested
        // implicitly by the discriminator staying false here.)
        let state = TerminalState::new_display_only(24, 80);
        assert!(
            !state
                .keyboard_input_sent
                .load(std::sync::atomic::Ordering::Relaxed),
            "fresh terminal must report no user input"
        );
        state.write_to_pty(b"a".as_slice());
        assert!(
            state
                .keyboard_input_sent
                .load(std::sync::atomic::Ordering::Relaxed),
            "write_to_pty must mark the session user-initiated"
        );
    }

    // -----------------------------------------------------------------
    // Env assembly contract. There is no mockable spawn seam: the engine
    // opens the PTY itself, so the env the child inherits is asserted directly
    // against the pure `assemble_pty_env`.
    // -----------------------------------------------------------------

    #[test]
    fn pty_spawn_injects_paneflow_bin_dir_and_prepends_path() {
        // Skip where the cache dir is unresolvable - the helper silent-fails
        // (correct behavior), but then there's nothing to assert on.
        if dirs::cache_dir().is_none() {
            eprintln!("skip: dirs::cache_dir() unresolvable in this environment");
            return;
        }

        let env = assemble_pty_env(HashMap::new(), 7, 3, None);

        let bin_dir = env
            .get("PANEFLOW_BIN_DIR")
            .expect("PANEFLOW_BIN_DIR must be set in the child env")
            .clone();
        assert!(!bin_dir.is_empty(), "PANEFLOW_BIN_DIR must not be empty");

        let path = env.get("PATH").expect("PATH must be set after injection");
        let first = std::env::split_paths(path)
            .next()
            .expect("PATH must have at least one component");
        assert_eq!(
            first,
            PathBuf::from(&bin_dir),
            "PANEFLOW_BIN_DIR must be first on PATH"
        );
    }

    #[test]
    fn detached_terminal_does_not_advertise_fake_workspace_id() {
        let env = assemble_pty_env(HashMap::new(), 0, 3, None);

        assert!(
            !env.contains_key("PANEFLOW_WORKSPACE_ID"),
            "workspace id 0 is a detached sentinel and must not reach child hooks"
        );
        assert_eq!(
            env.get("PANEFLOW_SURFACE_ID").map(String::as_str),
            Some("3")
        );
    }

    // Issue #369: POSIX resolves the codeset as LC_ALL > LC_CTYPE > LANG, so a
    // LANG-only insert lost to an inherited LC_ALL=C and the pane stayed in the
    // C locale. Whatever this test process inherited, the child env must either
    // carry both keys or neither, and never name an uninstalled locale.
    #[test]
    fn pty_env_locale_override_sets_lang_and_lc_all_together() {
        let env = assemble_pty_env(HashMap::new(), 1, 1, None);

        assert_eq!(
            env.get("LANG"),
            env.get("LC_ALL"),
            "a forced locale must set LANG and LC_ALL together"
        );
        if let Some(forced) = env.get("LANG") {
            assert!(
                is_utf8_locale(forced),
                "a forced locale must be UTF-8, got {forced}"
            );
            assert!(
                installed_locale_names().iter().any(|name| name == forced),
                "a forced locale must be installed, got {forced}"
            );
        }
    }

    fn locales(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn inherited_lc_all_c_is_overridden_with_a_utf8_locale() {
        let available = locales(&["C", "C.UTF-8", "en_US.UTF-8"]);

        let forced = utf8_locale_override(Some(""), Some("C"), None, &available)
            .expect("LC_ALL=C with no LANG must be overridden");

        assert!(
            is_utf8_locale(&forced),
            "forced locale must be UTF-8, got {forced}"
        );
    }

    #[test]
    fn utf8_locale_override_never_names_an_uninstalled_locale() {
        // No UTF-8 locale installed at all: nothing may be forced, least of all
        // the `en_US.UTF-8` the old code inserted unconditionally.
        let bare = locales(&["C", "POSIX", "fr_FR.ISO8859-1"]);
        assert_eq!(utf8_locale_override(None, Some("C"), None, &bare), None);

        assert_eq!(
            utf8_locale_override(
                Some("fr_FR.ISO8859-1"),
                None,
                None,
                &locales(&["fr_FR.UTF-8"])
            ),
            Some("fr_FR.UTF-8".to_string()),
            "the inherited language is kept when its UTF-8 spelling is installed"
        );
    }

    #[test]
    fn utf8_locale_override_leaves_a_utf8_block_alone() {
        let available = locales(&["C.UTF-8", "en_US.UTF-8", "de_DE.UTF-8"]);

        assert_eq!(
            utf8_locale_override(Some("de_DE.UTF-8"), None, None, &available),
            None
        );
        assert_eq!(
            utf8_locale_override(Some("C"), Some("de_DE.UTF-8"), None, &available),
            None,
            "a UTF-8 LC_ALL outranks everything and needs no override"
        );
        assert_eq!(
            utf8_locale_override(None, None, Some("de_DE.UTF-8"), &available),
            None,
            "LC_CTYPE outranks LANG for the codeset"
        );
        assert_eq!(
            utf8_locale_override(Some("de_DE.UTF-8"), None, Some("C"), &available),
            Some("de_DE.UTF-8".to_string()),
            "a non-UTF-8 LC_CTYPE outranks LANG and must be overridden"
        );
    }

    // user-supplied env vars are merged into the child PTY env.
    #[test]
    fn user_env_is_merged_into_pty_env() {
        let mut user = HashMap::new();
        user.insert("ANTHROPIC_API_KEY".to_string(), "sk-test-123".to_string());
        user.insert("MY_CUSTOM_VAR".to_string(), "hello".to_string());
        let env = assemble_pty_env(HashMap::new(), 1, 1, Some(user));

        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-test-123"),
            "US-014 AC: user env var must be present in the child env"
        );
        assert_eq!(
            env.get("MY_CUSTOM_VAR").map(String::as_str),
            Some("hello"),
            "US-014 AC: a second user env var must also be present"
        );
    }

    #[test]
    fn user_path_cannot_shadow_paneflow_bin_dir() {
        let mut user = HashMap::new();
        user.insert("PATH".to_string(), "/custom/bin".to_string());
        let env = assemble_pty_env(HashMap::new(), 1, 1, Some(user));
        let Some(bin_dir) = env.get("PANEFLOW_BIN_DIR") else {
            eprintln!("skip: PANEFLOW_BIN_DIR unavailable in this environment");
            return;
        };
        let path = env.get("PATH").expect("PATH must be present");
        let mut parts = std::env::split_paths(path);
        assert_eq!(
            parts.next().as_deref(),
            Some(std::path::Path::new(bin_dir)),
            "PANEFLOW_BIN_DIR must stay first even when user env sets PATH"
        );
        assert!(
            parts.any(|part| part == std::path::Path::new("/custom/bin")),
            "user PATH entries must still be preserved after the shim prepend"
        );
    }

    // US-014: TERM/COLORTERM are protected and cannot be overridden by user env.
    #[test]
    fn protected_keys_cannot_be_overridden_by_user_env() {
        let mut user = HashMap::new();
        user.insert("TERM".to_string(), "dumb".to_string());
        user.insert("COLORTERM".to_string(), "nope".to_string());
        user.insert("TERM_PROGRAM".to_string(), "spoofed".to_string());
        user.insert("TERM_PROGRAM_VERSION".to_string(), "0.0.0".to_string());
        user.insert("SHLVL".to_string(), "99".to_string());
        user.insert("KEEP_ME".to_string(), "yes".to_string());
        let env = assemble_pty_env(HashMap::new(), 1, 1, Some(user));

        assert_eq!(
            env.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "US-014 AC: TERM must stay Paneflow-owned even if the user sets it"
        );
        assert_eq!(
            env.get("COLORTERM").map(String::as_str),
            Some("truecolor"),
            "US-014 AC: COLORTERM must stay Paneflow-owned even if the user sets it"
        );
        assert_eq!(
            env.get("TERM_PROGRAM").map(String::as_str),
            Some("ghostty"),
            "TERM_PROGRAM must stay Paneflow-owned even if the user sets it"
        );
        assert_eq!(
            env.get("TERM_PROGRAM_VERSION").map(String::as_str),
            Some(paneflow_terminal_ghostty::GHOSTTY_APP_VERSION),
            "TERM_PROGRAM_VERSION must stay Paneflow-owned even if the user sets it"
        );
        assert_eq!(
            env.get("SHLVL").map(String::as_str),
            Some("0"),
            "SHLVL must stay reset so the child shell starts at level 1"
        );
        assert_eq!(
            env.get("KEEP_ME").map(String::as_str),
            Some("yes"),
            "US-014: a non-protected user var alongside protected ones still wins"
        );
    }

    // f010: dynamic-loader env vars from an untrusted source (imported
    // session.json surface env / global config env) must never reach the child
    // shell - letting LD_PRELOAD/LD_*/DYLD_* through is an RCE vector.
    #[test]
    fn loader_influencing_env_vars_are_dropped() {
        let mut user = HashMap::new();
        user.insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        user.insert("LD_LIBRARY_PATH".to_string(), "/tmp/evil".to_string());
        user.insert("LD_AUDIT".to_string(), "/tmp/audit.so".to_string());
        user.insert(
            "DYLD_INSERT_LIBRARIES".to_string(),
            "/tmp/e.dylib".to_string(),
        );
        user.insert("KEEP_ME".to_string(), "yes".to_string());
        let env = assemble_pty_env(HashMap::new(), 1, 1, Some(user));

        assert_eq!(
            env.get("LD_PRELOAD"),
            None,
            "f010: LD_PRELOAD from untrusted env must be dropped"
        );
        assert_eq!(
            env.get("LD_LIBRARY_PATH"),
            None,
            "f010: LD_LIBRARY_PATH from untrusted env must be dropped"
        );
        assert_eq!(
            env.get("LD_AUDIT"),
            None,
            "f010: LD_AUDIT from untrusted env must be dropped"
        );
        assert_eq!(
            env.get("DYLD_INSERT_LIBRARIES"),
            None,
            "f010: DYLD_* from untrusted env must be dropped"
        );
        assert_eq!(
            env.get("KEEP_ME").map(String::as_str),
            Some("yes"),
            "f010: a benign var alongside loader vars must still pass through"
        );
    }

    #[test]
    fn claudecode_env_is_dropped_from_child_env() {
        let mut base = HashMap::new();
        base.insert("CLAUDECODE".to_string(), "1".to_string());
        let mut user = HashMap::new();
        user.insert("CLAUDECODE".to_string(), "1".to_string());
        user.insert("KEEP_ME".to_string(), "yes".to_string());

        let env = assemble_pty_env(base, 1, 1, Some(user));

        assert_eq!(
            env.get("CLAUDECODE"),
            None,
            "CLAUDECODE must never reach agent child processes"
        );
        assert_eq!(env.get("KEEP_ME").map(String::as_str), Some("yes"));
    }

    #[test]
    fn inherited_agent_session_markers_are_dropped_from_child_env() {
        // Launching Paneflow from inside an agent session (or from an IDE
        // terminal that carries these) otherwise leaks the parent session's
        // identity into every pane. `CLAUDE_CODE_CHILD_SESSION` in particular
        // makes the agent disable transcript saving, so its conversation never
        // reaches `~/.claude/projects` and the thread cannot be resumed after a
        // restart.
        let mut base = HashMap::new();
        let mut user = HashMap::new();
        for key in INHERITED_AGENT_SESSION_ENV {
            base.insert((*key).to_string(), "inherited".to_string());
            // Also offered through `terminal.env`: the strip runs after the
            // merge, so a hand-written config cannot reinstate them either.
            user.insert((*key).to_string(), "from-config".to_string());
        }
        user.insert("KEEP_ME".to_string(), "yes".to_string());

        let env = assemble_pty_env(base, 1, 1, Some(user));

        for key in INHERITED_AGENT_SESSION_ENV {
            assert_eq!(
                env.get(*key),
                None,
                "{key} must never reach an agent spawned in a pane"
            );
        }
        assert_eq!(
            env.get("KEEP_ME").map(String::as_str),
            Some("yes"),
            "a benign var alongside the markers must still pass through"
        );
    }

    #[test]
    fn host_terminal_markers_are_recognized_whatever_their_casing() {
        // These arrive by inheritance in the host's own casing, so the match
        // cannot be the exact-name check the user-env merge relies on.
        for key in INHERITED_HOST_TERMINAL_ENV {
            assert!(
                is_inherited_host_terminal_env_key(key),
                "{key} is listed and must be recognized"
            );
            assert!(
                is_inherited_host_terminal_env_key(&key.to_lowercase()),
                "{key} must be recognized case-insensitively"
            );
        }
    }

    #[test]
    fn host_terminal_matcher_does_not_swallow_unrelated_names() {
        // A name that merely starts like a marker must survive, and so must
        // every variable PaneFlow sets for the pane.
        for key in [
            "conemu",
            "CONEMU",
            "TERM",
            "TERM_PROGRAM",
            "TMUXINATOR_CONFIG",
            "STYLE",
            "PATH",
            "KITTY_WINDOW_IDS",
            "PANEFLOW_SURFACE_ID",
        ] {
            assert!(
                !is_inherited_host_terminal_env_key(key),
                "{key} must survive - it is not a host-terminal identity marker"
            );
        }
    }

    #[test]
    fn the_strip_list_covers_both_families_it_claims_to() {
        // `inherited_env_keys_to_strip` reads the real process env, so what is
        // pinned here is its predicate: an agent-session marker must be
        // removed at the spawn boundary too, not only filtered out of the
        // assembled map - a `retain` cannot unset an INHERITED variable.
        for key in INHERITED_AGENT_SESSION_ENV {
            assert!(
                is_inherited_agent_session_env_key(key) || is_inherited_host_terminal_env_key(key),
                "{key} must be stripped from the inherited env, not just the map"
            );
        }
        assert!(
            !is_inherited_agent_session_env_key("PANEFLOW_SURFACE_ID")
                && !is_inherited_host_terminal_env_key("PANEFLOW_SURFACE_ID"),
            "the strip must not reach a variable Paneflow sets for the pane"
        );
    }

    #[test]
    fn host_terminal_markers_are_not_smuggled_through_the_assembled_env() {
        // The removal itself happens at the spawn boundary (`env_remove`),
        // because the assembled map only carries overrides. What this pins is
        // the other half of the contract: `assemble_pty_env` must not be the
        // thing that PUTS one back, and the keys Paneflow owns are untouched.
        let env = assemble_pty_env(HashMap::new(), 1, 1, None);
        for key in env.keys() {
            assert!(
                !is_inherited_host_terminal_env_key(key),
                "assemble_pty_env must never introduce the host marker {key}"
            );
        }
        assert_eq!(
            env.get("TERM_PROGRAM").map(String::as_str),
            Some("ghostty"),
            "the assembled env and the spawn override must agree (#184)"
        );
    }

    // foreground_command degrades gracefully (no panic, None) on a
    // display-only terminal (child_pid == 0, no real PTY) on every platform.
    #[test]
    fn foreground_command_none_for_display_only() {
        let state = TerminalState::new_display_only(24, 80);
        assert!(
            state.foreground_command().is_none(),
            "display-only terminal has no foreground process to resolve"
        );
    }

    // `scan_output` reads the tail the engine materializes from live PTY
    // output, which a display-only grid never produces. These two tests cover
    // the detection logic itself, so they hand the lines over directly.
    #[test]
    fn scan_output_uses_multiline_framework_context() {
        let mut state = TerminalState::new_display_only(24, 80);

        let services = state.detect_services_in_lines(&[
            "▲ Next.js 16.1.6".to_string(),
            "- Local: http://localhost:3000".to_string(),
        ]);

        assert_eq!(services.len(), 1);
        assert_eq!(services[0].port, 3000);
        assert_eq!(services[0].label.as_deref(), Some("Next.js"));
        assert!(services[0].is_frontend);
    }

    #[test]
    fn scan_output_dedups_until_port_leaves_live_set() {
        let mut state = TerminalState::new_display_only(24, 80);
        let lines = ["Vite ready at http://localhost:5173".to_string()];

        assert_eq!(state.detect_services_in_lines(&lines).len(), 1);
        assert!(state.detect_services_in_lines(&lines).is_empty());

        state.retain_reported_ports(&[]);
        let services = state.detect_services_in_lines(&lines);

        assert_eq!(services.len(), 1);
        assert_eq!(services[0].port, 5173);
    }

    #[test]
    fn announced_ports_are_deduped_and_bounded() {
        let mut state = TerminalState::new_display_only(24, 80);
        state.note_announced_port(3000);
        state.note_announced_port(3000);
        for port in 3001..3025 {
            state.note_announced_port(port);
        }

        assert_eq!(state.announced_ports.len(), 16);
        assert_eq!(state.announced_ports[0], 3000);
        assert_eq!(
            state.announced_ports.iter().filter(|&&p| p == 3000).count(),
            1
        );
    }

    #[test]
    fn search_scrollback_returns_unique_lines_and_preserves_cap() {
        let state = TerminalState::new_display_only(5, 80);
        state.write_output(b"first needle needle\nsecond needle\nthird needle\nwithout marker");

        let (limited, hit_cap) = state
            .scrollback_reader()
            .search_scrollback("needle", 2)
            .expect("scan completed");
        assert_eq!(limited.len(), 2);
        assert!(hit_cap);
        assert!(limited[0].1.contains("first needle needle"));
        assert!(limited[1].1.contains("second needle"));

        let (all, hit_cap) = state
            .scrollback_reader()
            .search_scrollback("needle", 8)
            .expect("scan completed");
        assert_eq!(all.len(), 3);
        assert!(!hit_cap);
        assert!(all[2].1.contains("third needle"));
    }

    /// Issue #362: a runtime that cannot answer is not a finished scan with
    /// zero hits. `surface.search` used to report `matches=[] truncated=true`
    /// for it, which a conductor reads as "raise max_matches" or "pattern
    /// absent"; the same unanswered runtime is an error on `surface.read`.
    #[test]
    fn search_scrollback_fails_when_the_runtime_does_not_answer() {
        let state = TerminalState::new_display_only(5, 80);
        state.write_output(b"first needle\nsecond needle\nwithout marker");
        let (found, hit_cap) = state
            .scrollback_reader()
            .search_scrollback("needle", 8)
            .expect("scan completed");
        assert_eq!(found.len(), 2);
        assert!(!hit_cap);

        state.ghostty.shutdown();

        assert!(
            state
                .scrollback_reader()
                .search_scrollback("needle", 8)
                .is_err(),
            "no answer must not read as an empty, capped scan"
        );
    }

    // A display-only terminal (child_pid == 0, no real PTY) must resolve no CWD
    // and, critically, must NOT reach the platform process-table FFI: on macOS
    // `proc_pidinfo(0, …)` targets the kernel swapper, fails with EPERM, and
    // would spam a misleading "shell may have exited" warning on every poll.
    #[test]
    fn cwd_now_none_for_display_only() {
        let state = TerminalState::new_display_only(24, 80);
        assert_eq!(state.child_pid, 0);
        assert!(
            state.cwd_now().is_none(),
            "display-only terminal has no shell CWD to resolve"
        );
    }

    #[test]
    fn pending_clipboard_ops_are_bounded() {
        let mut state = TerminalState::new_display_only(5, 20);

        for i in 0..(MAX_PENDING_CLIPBOARD_OPS + 2) {
            state.queue_clipboard_op(format!("op-{i}"));
        }

        assert_eq!(state.pending_clipboard_ops.len(), MAX_PENDING_CLIPBOARD_OPS);
        assert_eq!(state.pending_clipboard_ops[0], "op-2");
    }

    #[test]
    fn osc52_store_requires_focus_and_respects_the_shared_cap() {
        let mut state = TerminalState::new_display_only(5, 20);
        state.set_osc52_mode(Osc52Mode::CopyOnly);

        state.deliver_clipboard_text("unfocused".into());
        assert!(state.pending_clipboard_ops.is_empty());

        state.set_terminal_focused(true);
        state.deliver_clipboard_text("focused".into());
        assert_eq!(state.pending_clipboard_ops, vec!["focused".to_string()]);

        state.deliver_clipboard_text("x".repeat(MAX_OSC52_BYTES + 1));
        assert_eq!(state.pending_clipboard_ops.len(), 1);

        state.set_terminal_focused(false);
        state.deliver_clipboard_text("lost-focus".into());
        assert_eq!(state.pending_clipboard_ops.len(), 1);
    }

    /// Issue #315: `terminal.osc52_clipboard: "disabled"` must survive
    /// promotion. A focused pane whose program emits OSC 52 then writes
    /// nothing, at both the state gate and the engine-side gate.
    #[cfg(unix)]
    #[test]
    fn osc52_store_honors_the_disabled_terminal_setting() {
        let config = paneflow_config::schema::PaneFlowConfig {
            terminal: Some(TerminalConfig {
                osc52_clipboard: Some(Osc52ClipboardConfig::Disabled),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(Osc52Mode::from_config(&config) == Osc52Mode::Disabled);
        assert!(
            Osc52Mode::from_config(&paneflow_config::schema::PaneFlowConfig::default())
                == Osc52Mode::CopyOnly,
            "an absent key keeps the documented copy_only default"
        );

        let (mut state, pending) = TerminalState::new_pending(80, 24);
        state.set_spawn_osc52_mode(Osc52Mode::from_config(&config));
        let params = SpawnParams {
            shell: "/bin/sh".into(),
            shell_quoting: ShellQuoting::Posix,
            extra_args: Vec::new(),
            env: std::collections::HashMap::from([
                ("TERM".into(), "xterm-256color".into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ]),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            profile: TerminalSurfaceProfile::Normal,
        };
        let spawned = state
            .ghostty_session()
            .start(pending.ghostty, params, None, 1_000)
            .expect("spawn a PTY shell");
        state.promote_ghostty(spawned);

        assert!(
            state.osc52_mode == Osc52Mode::Disabled,
            "promotion must install the configured policy, not a hardcoded CopyOnly"
        );
        state.set_terminal_focused(true);
        assert!(!state.clipboard_gate.allows_store());
        state.deliver_clipboard_text("secret-token".into());
        assert!(state.pending_clipboard_ops.is_empty());
    }

    /// A tampered `session.json` line can carry live VT bytes: an OSC 8
    /// clickable-link injection, an OSC 0 title spoof, a raw CSI, an ESC
    /// introducer, a NUL, and a C1 control. The engine sanitizes on the way
    /// in, so none of them reach the grid as a live sequence.
    #[test]
    fn restore_scrollback_strips_escape_and_osc_injection() {
        let hostile = "\x1b]8;;https://evil.example/\x07click\x1b]8;;\x07\
                       \x1b]0;PWNED\x07\x1b[31mred\x00\u{9b}38m";
        let state = TerminalState::new_display_only(6, 80);
        state.restore_scrollback(hostile);
        state.restore_scrollback("a\tb");

        assert_eq!(state.title, "Terminal", "OSC 0 must not retitle the pane");

        let backend = state.session_backend();
        let restored = (0..6)
            .filter_map(|row| backend.line_text_at(Point::new(row, 0)))
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        for marker in ["https://evil.example/", "click", "PWNED", "red", "38m"] {
            assert!(
                restored.contains(marker),
                "plain glyphs must survive; {marker:?} missing from {restored:?}"
            );
        }
        assert!(
            !restored.contains('\x1b') && !restored.contains('\x07'),
            "no VT introducer may reach the grid; got {restored:?}"
        );
    }

    /// The undo replay (#195) feeds a capture of the closed pane into the new
    /// PTY verbatim, escapes included, so the capture is where the policy
    /// lives (`TerminalExtra::replay`): a program that set the clipboard
    /// (OSC 52), the working directory (OSC 7), or the title (OSC 0/2) in the
    /// closed pane must not replay any of them into the new one, while the
    /// styling that `restore_scrollback` strips has to survive the trip.
    #[test]
    fn restore_replay_keeps_styling_but_carries_no_clipboard_pwd_or_title() {
        use crate::terminal::types::{CellFlags, Color, NamedColor};

        let source = TerminalState::new_display_only(6, 80);
        source.write_output(
            b"\x1b]52;c;UFdORUQ=\x07\x1b]7;file://evil.example/pwned\x07\
              \x1b]0;PWNED\x07\x1b]2;PWNED\x07above-the-fold\n\
              \x1b[1;31mred\x1b[0m plain",
        );
        let replay = source.capture_replay().expect("a styled capture");
        let bytes = String::from_utf8_lossy(&replay);
        assert!(bytes.contains("\x1b["), "SGR must survive; got {bytes:?}");
        for forbidden in [
            "\x1b]52",
            "\x1b]7;",
            "\x1b]0;",
            "\x1b]2;",
            "PWNED",
            "evil.example",
        ] {
            assert!(
                !bytes.contains(forbidden),
                "{forbidden:?} must not be captured; got {bytes:?}"
            );
        }

        let mut target = TerminalState::new_display_only(6, 80);
        target.restore_replay(&replay);
        target.sync();
        assert_eq!(target.title, "Terminal", "no title may replay");
        assert_eq!(target.current_cwd, None, "no OSC 7 may replay");
        assert!(
            target.pending_clipboard_ops.is_empty(),
            "no OSC 52 may replay"
        );

        let backend = target.session_backend();
        let restored = (0..6)
            .filter_map(|row| backend.line_text_at(Point::new(row, 0)))
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        for marker in ["above-the-fold", "red", "plain"] {
            assert!(
                restored.contains(marker),
                "glyphs must survive; {marker:?} missing from {restored:?}"
            );
        }
        assert!(
            !restored.contains('\x1b') && !restored.contains('\x07'),
            "no VT introducer may land in the grid as text; got {restored:?}"
        );
        let (content, _) =
            backend.render_content(TerminalWindowSize::new(80, 6, 8, 16), 0, 5, false);
        let red = content
            .cells
            .iter()
            .find(|cell| cell.c == 'r')
            .expect("the styled glyph must be on screen; no other row carries an r");
        assert_eq!(
            red.fg,
            Color::Named(NamedColor::Red),
            "foreground must survive"
        );
        assert!(red.flags.contains(CellFlags::BOLD), "bold must survive");
    }

    /// Extraction drains terminal history while excluding the active viewport
    /// rows, so a restore cannot replay the previous visible frame ahead of
    /// fresh shell output.
    #[test]
    fn extract_scrollback_drains_history_only() {
        let state = TerminalState::new_display_only(3, 80);
        state.restore_scrollback("history-alpha\nhistory-bravo\nvisible-charlie\nvisible-delta");

        let drained = state
            .extract_scrollback()
            .expect("seeded scrollback should not be empty");

        for marker in ["history-alpha", "history-bravo"] {
            assert!(
                drained.contains(marker),
                "drained scrollback must contain {marker:?}; got:\n{drained}"
            );
        }
        for marker in ["visible-charlie", "visible-delta"] {
            assert!(
                !drained.contains(marker),
                "active viewport must exclude {marker:?}; got:\n{drained}"
            );
        }
    }

    #[test]
    fn output_generation_advances_on_pty_output() {
        // `workspace.up` polls `output_generation` as its prefill
        // readiness signal. A fresh terminal has produced nothing (0); the
        // counter must advance once the shell emits output (Wakeup events
        // drained by `sync`), proving the signal tracks real PTY activity.
        let mut state = TerminalState::new(None, 1, 1, Some((80, 24)), None, None)
            .expect("spawn a PTY-backed terminal");
        assert_eq!(
            state.output_generation, 0,
            "a fresh terminal has produced no output"
        );

        std::thread::sleep(std::time::Duration::from_millis(250));
        state.write_to_pty_silent(b"echo PANEFLOW_GEN_OK\n".to_vec());

        // Up to 12s. A login shell can cold-start slowly on a loaded CI runner
        // before emitting its prompt - output_generation only advances once the PTY
        // produces any output. The loop breaks immediately on success, so the
        // larger budget only costs wall-time when the signal never arrives.
        let mut advanced = false;
        for _ in 0..240 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            state.sync();
            if state.output_generation > 0 {
                advanced = true;
                break;
            }
        }
        assert!(
            advanced,
            "output_generation must advance once the PTY emits output"
        );
    }

    // --- process-group survival tests (fork; engine-neutral) ---

    // -----------------------------------------------------------------

    #[cfg(target_os = "macos")]
    #[test]
    fn leaderless_foreground_group_is_discovered_and_force_killed() {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::time::{Duration, Instant};

        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: openpty initializes both fd outputs; null termios/winsize ask
        // the OS for defaults. The owned fds are closed below.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty"
        );

        // Build a real controlling-terminal session with a foreground process
        // group whose numeric leader exits. This models a normal pipeline
        // after its first command finishes while another command remains.
        // Every child-side operation after fork is an async-signal-safe libc
        // call; failures use `_exit` rather than touching Rust runtime state.
        let shell_pid = unsafe { libc::fork() };
        if shell_pid == 0 {
            // SAFETY: this is the freshly forked child. Only raw syscalls run.
            unsafe {
                libc::close(master_fd);
                if libc::setsid() < 0 || libc::ioctl(slave_fd, libc::TIOCSCTTY.into(), 0) < 0 {
                    libc::_exit(120);
                }
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);

                let group_leader = libc::fork();
                if group_leader < 0 {
                    libc::_exit(121);
                }
                if group_leader == 0 {
                    if libc::setpgid(0, 0) < 0 {
                        libc::_exit(122);
                    }
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    if libc::tcsetpgrp(slave_fd, libc::getpid()) < 0 {
                        libc::_exit(123);
                    }
                    let worker = libc::fork();
                    if worker < 0 {
                        libc::_exit(124);
                    }
                    if worker == 0 {
                        libc::signal(libc::SIGHUP, libc::SIG_IGN);
                        libc::signal(libc::SIGTERM, libc::SIG_IGN);
                        const READY: &[u8] = b"__PANEFLOW_FG_READY__\n";
                        let _ = libc::write(slave_fd, READY.as_ptr().cast(), READY.len());
                        loop {
                            libc::pause();
                        }
                    }
                    libc::_exit(0);
                }

                // Reap the numeric process-group leader, then keep the shell
                // session alive while its TERM/HUP-resistant foreground
                // worker remains in the now-leaderless group.
                let mut status = 0;
                let _ = libc::waitpid(group_leader, &mut status, 0);
                loop {
                    libc::pause();
                }
            }
        }
        assert!(shell_pid > 1, "fork session leader");
        // SAFETY: parent owns these fds; it keeps only the master.
        unsafe {
            libc::close(slave_fd);
        }
        // SAFETY: master_fd is the one owned result from openpty.
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

        struct JobControlCleanup {
            shell_pid: i32,
            master_fd: i32,
        }
        impl Drop for JobControlCleanup {
            fn drop(&mut self) {
                // Test-only best-effort cleanup for any assertion path.
                // SAFETY: all targets were created by this fixture.
                unsafe {
                    let foreground = libc::tcgetpgrp(self.master_fd);
                    if foreground > 1 && foreground != self.shell_pid {
                        libc::kill(-foreground, libc::SIGKILL);
                    }
                    libc::kill(-self.shell_pid, libc::SIGKILL);
                    let mut status = 0;
                    let _ = libc::waitpid(self.shell_pid, &mut status, 0);
                }
            }
        }
        let cleanup = JobControlCleanup {
            shell_pid,
            master_fd: master.as_raw_fd(),
        };

        // Nonblocking reads keep a broken job-control setup from hanging CI.
        // SAFETY: fcntl only changes flags on our owned PTY master.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert!(flags >= 0, "get master flags");
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                0,
                "set master nonblocking"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let mut buffer = [0u8; 1024];
        while !output
            .windows(b"__PANEFLOW_FG_READY__".len())
            .any(|window| window == b"__PANEFLOW_FG_READY__")
        {
            match master.read(&mut buffer) {
                Ok(0) => panic!("job-control PTY closed before readiness"),
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "job-control readiness timed out: {}",
                        String::from_utf8_lossy(&output)
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("read job-control PTY: {error}"),
            }
        }

        // The readiness write happens in the worker before its parent exits;
        // wait until the session leader has reaped that numeric group leader.
        let foreground_pgid = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
        assert!(foreground_pgid > 1, "foreground PGID");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: read-only query. ESRCH proves no process occupies the
            // numeric leader PID while the worker keeps -PGID alive.
            if unsafe { libc::getpgid(foreground_pgid) } < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "foreground process-group leader did not exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: signal 0 proves the leaderless group still contains worker.
        assert_eq!(unsafe { libc::kill(-foreground_pgid, 0) }, 0);

        let foreground = foreground_process_group(master.as_raw_fd(), shell_pid as u32)
            .expect("distinct owned foreground group");
        assert_eq!(foreground.pgid, foreground_pgid as u32);
        assert_ne!(foreground.pgid, shell_pid as u32);
        assert!(
            foreground
                .members
                .iter()
                .all(|(pid, _)| *pid != foreground.pgid),
            "authorization must not depend on the exited numeric leader"
        );
        assert!(
            crate::agents::parent_guard::shutdown_pinned_process_group(&foreground),
            "guarded TERM/KILL ladder must accept the pinned foreground group"
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: signal 0 is a read-only existence probe.
            let alive = unsafe { libc::kill(-(foreground.pgid as i32), 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "TERM/HUP-resistant foreground group survived SIGKILL"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(cleanup);
    }

    #[cfg(unix)]
    #[test]
    fn terminate_process_group_delivers_sigterm_and_is_honored() {
        // the process group receives SIGTERM (not a hard SIGKILL).
        // The child is its own session/group leader (setsid) and traps SIGTERM
        // to exit 42; a SIGKILL would instead show signal 9 with no exit code.
        // Proving the trap ran proves SIGTERM was delivered to the group - and
        // by construction `Drop` sends it synchronously *before* scheduling the
        // 100ms-grace SIGKILL.
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        // `sleep 30 &` + `wait` (not a foreground sleep): POSIX requires the
        // `wait` builtin to be interrupted by a trapped signal, so the trap
        // runs promptly even if the group SIGTERM races `sleep`'s fork→exec
        // window (where an inherited blocked mask can leave it alive - a
        // foreground sleep then pins the shell for its full 30s before the
        // trap fires, which is exactly the aarch64-CI hang this replaces).
        // `echo ready` is the readiness handshake: once the parent reads it,
        // setsid + trap + background spawn are all done - no blind warmup.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "trap 'exit 42' TERM; sleep 30 & echo ready; wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid() runs in the forked child before exec; it detaches
        // the child into its own session/group so kill(-pid, ...) targets
        // exactly this group, with no shared-state hazard.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let mut child = cmd.spawn().expect("spawn test child");
        let pid = child.id() as i32;

        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut ready)
            .expect("read readiness line");
        assert_eq!(ready.trim_end(), "ready", "handshake line");

        let pinned_start = child_pid_start_time(pid as u32);
        assert!(
            terminate_process_group(pid, pinned_start),
            "SIGTERM must be delivered to the live process group"
        );

        // The trap exits 42 well within the 100ms grace window; poll for exit
        // with a generous ceiling - the suite runs fully parallel on 4-core CI
        // runners and a 5s deadline has flaked under that load (same class as
        // the v0.3.9 stdout_cap deflake). A regression still fails, just slower.
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait child") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("child did not exit after SIGTERM within 30s");
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        assert_eq!(
            status.code(),
            Some(42),
            "child must exit via its SIGTERM handler (42), not be SIGKILLed (signal={:?})",
            status.signal()
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminate_process_group_is_noop_for_dead_or_invalid_group() {
        // AC (unhappy path): an empty/invalid group must be a harmless
        // no-op guarded by the `getpgid(pid) == pid` identity check - no panic,
        // returns false.
        assert!(
            !terminate_process_group(0, None),
            "pid 0 must be rejected (would signal the caller's own group)"
        );
        assert!(
            !terminate_process_group(-5, None),
            "negative pid must be rejected"
        );
        // A very high pid is almost certainly not its own live group leader;
        // getpgid returns ESRCH (≠ pid) so SIGTERM is never sent.
        assert!(
            !terminate_process_group(0x7FFF_FFF0, Some(1)),
            "non-existent group must be a no-op, not a panic"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminate_process_group_is_noop_for_mismatched_start_pin() {
        // A live setsid leader with a non-matching spawn pin must not be
        // signaled: that is the recycled-session-leader window.
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo ready; exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid() runs in the forked child before exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn test child");
        let pid = child.id() as i32;

        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut ready)
            .expect("read readiness line");
        assert_eq!(ready.trim_end(), "ready", "handshake line");

        let live_start = child_pid_start_time(pid as u32);
        let bogus_pin = Some(live_start.map(|s| s.wrapping_add(1)).unwrap_or(1));
        assert!(
            !terminate_process_group(pid, bogus_pin),
            "mismatched start pin must not SIGTERM a live session leader"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "child must still be running after the mismatched-pin no-op"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn hostile_zdotdir_cannot_override_shell_integration() {
        let mut base = HashMap::new();
        base.insert(
            "ZDOTDIR".to_string(),
            "/app/shell-integration/zsh".to_string(),
        );
        base.insert(
            "PANEFLOW_ORIG_ZDOTDIR".to_string(),
            "/real/zdot".to_string(),
        );
        let mut user = HashMap::new();
        user.insert("ZDOTDIR".to_string(), "/tmp/evil-zdot".to_string());
        user.insert(
            "PANEFLOW_ORIG_ZDOTDIR".to_string(),
            "/tmp/evil-orig".to_string(),
        );
        user.insert("KEEP_ME".to_string(), "yes".to_string());
        user.insert(
            "NODE_OPTIONS".to_string(),
            "--max-old-space-size=4096".to_string(),
        );

        let env = assemble_pty_env(base, 1, 1, Some(user));

        assert_eq!(
            env.get("ZDOTDIR").map(String::as_str),
            Some("/app/shell-integration/zsh"),
            "hostile ZDOTDIR must not replace the value shell integration set"
        );
        assert_eq!(
            env.get("PANEFLOW_ORIG_ZDOTDIR").map(String::as_str),
            Some("/real/zdot"),
            "hostile PANEFLOW_ORIG_ZDOTDIR must not replace the integration original"
        );
        assert_eq!(env.get("KEEP_ME").map(String::as_str), Some("yes"));
        assert_eq!(
            env.get("NODE_OPTIONS").map(String::as_str),
            Some("--max-old-space-size=4096"),
            "NODE_OPTIONS remains mergeable ; not part of the zsh-startup denylist"
        );
    }

    #[test]
    fn hostile_zdotdir_is_dropped_when_integration_did_not_set_it() {
        let mut user = HashMap::new();
        user.insert("ZDOTDIR".to_string(), "/tmp/evil-zdot".to_string());
        user.insert(
            "PANEFLOW_ORIG_ZDOTDIR".to_string(),
            "/tmp/evil-orig".to_string(),
        );

        let env = assemble_pty_env(HashMap::new(), 1, 1, Some(user));

        assert_ne!(
            env.get("ZDOTDIR").map(String::as_str),
            Some("/tmp/evil-zdot"),
            "assembling pty env with hostile ZDOTDIR must not keep the hostile value"
        );
        assert_eq!(
            env.get("ZDOTDIR"),
            None,
            "untrusted ZDOTDIR must not be injected when integration did not set it"
        );
        assert_eq!(
            env.get("PANEFLOW_ORIG_ZDOTDIR"),
            None,
            "untrusted PANEFLOW_ORIG_ZDOTDIR must not be injected when integration did not set it"
        );
    }

    fn seed_numbered_history(rows: usize, cols: usize, n: usize) -> TerminalState {
        let state = TerminalState::new_display_only(rows, cols);
        let mut body = String::new();
        for i in 0..n {
            body.push_str(&format!("line-{i:04}\n"));
        }
        state.restore_scrollback(&body);
        state
    }

    /// The string-level spec `extract_scrollback_window` is checked against:
    /// retained history followed by the screen (#184 Phase 3.6).
    fn transcript(state: &TerminalState) -> String {
        let history = state.extract_scrollback().unwrap_or_default();
        match state.screen_text() {
            Some(screen) if history.is_empty() => screen,
            Some(screen) => format!("{history}\n{screen}"),
            None => history,
        }
    }

    /// Windowed extract returns the last N history lines and a row-span
    /// `total` matching `extract_scrollback` + `paginate_scrollback`, without
    /// joining the full 4000-line persistence string first.
    #[test]
    fn extract_scrollback_window_returns_last_n_without_full_buffer() {
        let state = seed_numbered_history(4, 40, 80);
        let full = transcript(&state);
        let (expected, returned, total, eof) =
            crate::app::ipc_handler::paginate_scrollback(&full, 5, 0);
        assert!(
            total > 5,
            "fixture must have more history than the window; total={total}"
        );
        assert!(!eof, "a 5-line tail of a larger buffer is not eof");

        let (text, got_returned, got_total, got_eof) = state
            .extract_scrollback_window(5, 0)
            .expect("the runtime answers");
        assert_eq!(text, expected);
        assert_eq!(got_returned, returned);
        assert_eq!(got_total, total);
        assert_eq!(got_eof, eof);
        assert!(
            text.starts_with("line-") && text.contains('\n'),
            "window should be joined history lines, got {text:?}"
        );
        let last = full.rsplit('\n').next().unwrap();
        assert!(
            text.ends_with(last),
            "offset 0 must end on the newest row (the bottom of the screen) {last:?}; got {text:?}"
        );
    }

    /// Issue #83: the undo-close record bounds its per-leaf extract rather
    /// than walking the full 4000-row history on the GPUI thread and handing
    /// most of it straight to the record budget to release.
    #[test]
    fn extract_scrollback_capped_returns_only_the_newest_lines() {
        let state = seed_numbered_history(4, 40, 500);
        // History followed by the screen: the newest rows are the ones on
        // screen when the pane closed (#184 Phase 3.6).
        let full = transcript(&state);
        let capped = state.extract_scrollback_capped(100).expect("history");

        assert!(
            full.lines().count() > 100,
            "the fixture must have more history than the cap"
        );
        assert_eq!(
            capped.lines().count(),
            100,
            "the cap bounds how many rows are read and joined, not just the result"
        );
        assert!(
            full.ends_with(&capped),
            "undo replays the tail, so the rows kept must be the NEWEST ones"
        );
        // A cap above the available history is the full extract.
        assert_eq!(
            state.extract_scrollback_capped(crate::limits::MAX_SCROLLBACK_EXTRACT_LINES),
            Some(full)
        );
    }

    /// #184 Phase 3.6: a full-screen TUI has no history at all, so a read of
    /// the pane is its retained history followed by the screen the program is
    /// painting right now.
    #[test]
    fn extract_scrollback_window_appends_the_live_screen_after_history() {
        let state = seed_numbered_history(4, 40, 80);
        let history = state.extract_scrollback().expect("history");
        let screen = state
            .screen_text()
            .expect("the newest numbered rows are on the screen");
        assert!(
            !history.ends_with(&screen),
            "fixture: the history extract stops at the viewport"
        );

        let (text, returned, total, eof) = state
            .extract_scrollback_window(crate::limits::MAX_SCROLLBACK_EXTRACT_LINES, 0)
            .expect("the runtime answers");
        let expected = format!("{history}\n{screen}");
        assert_eq!(text, expected);
        assert_eq!(returned, expected.lines().count());
        assert_eq!(total, returned);
        assert!(eof);

        // A one-row window is the bottom screen row, not the newest history row.
        let (tail, ..) = state
            .extract_scrollback_window(1, 0)
            .expect("the runtime answers");
        assert_eq!(tail, screen.rsplit('\n').next().unwrap());
    }

    #[test]
    fn extract_scrollback_window_offset_matches_paginate() {
        let state = seed_numbered_history(4, 40, 80);
        let full = transcript(&state);
        for &(lines, offset) in &[(5, 0), (5, 5), (10, 20), (3, 40), (200, 0)] {
            let expected = crate::app::ipc_handler::paginate_scrollback(&full, lines, offset);
            let got = state.extract_scrollback_window(lines, offset);
            assert_eq!(got, Some(expected), "lines={lines} offset={offset}");
        }
    }

    #[test]
    fn extract_scrollback_window_empty_terminal_is_eof() {
        let state = TerminalState::new_display_only(24, 80);
        assert_eq!(
            state.extract_scrollback_window(200, 0),
            Some((String::new(), 0, 0, true))
        );
    }

    /// A runtime that cannot answer is not a blank pane: `surface.read`
    /// used to report `{"text":"","total_lines":0,"eof":true}` for it, and
    /// `paneflow wait` / `flow` settled on that as "the agent went quiet".
    #[test]
    fn extract_scrollback_window_is_none_when_the_runtime_does_not_answer() {
        let state = seed_numbered_history(4, 40, 10);
        assert!(state.extract_scrollback_window(200, 0).is_some());

        state.ghostty.shutdown();

        assert_eq!(
            state.extract_scrollback_window(200, 0),
            None,
            "no answer must not read as an empty transcript at eof"
        );
        assert_eq!(
            state.extract_scrollback_capped(100),
            None,
            "the undo capture maps the same failure to nothing, not to an empty record"
        );
    }

    /// Issue #83 hands the cap the NEWEST rows; the cap must keep them. A
    /// fixture wide enough that its rows cross `MAX_CHARS` is the only way
    /// into the cap branch, so this one is 1200 multibyte rows.
    #[test]
    fn extract_scrollback_capped_keeps_the_newest_rows_when_the_char_cap_engages() {
        let state = TerminalState::new_display_only(4, 240);
        let row = |i: usize| format!("{i:05}{}", "中".repeat(115));
        let mut chunk = String::new();
        for i in 0..1200 {
            chunk.push_str(&row(i));
            chunk.push('\n');
            if (i + 1) % 100 == 0 {
                state.write_output(chunk.as_bytes());
                chunk.clear();
            }
        }

        let (full, returned, _, _) = state
            .extract_scrollback_window(1200, 0)
            .expect("the runtime answers");
        assert_eq!(returned, 1200, "fixture: every row is retained");
        assert!(
            full.len() > crate::limits::MAX_CHARS,
            "fixture must cross MAX_CHARS; len={}",
            full.len()
        );
        let screen = state
            .screen_text()
            .expect("the newest rows are on the screen");

        let capped = state.extract_scrollback_capped(1200).expect("history");

        assert!(capped.len() <= crate::limits::MAX_CHARS);
        assert!(
            full.ends_with(&capped),
            "undo replays the tail, so the rows kept must be the NEWEST ones"
        );
        assert!(
            capped.ends_with(&screen),
            "the screen when the pane closed is the newest text and must survive the cap"
        );
        assert!(
            capped.starts_with(&row(1200 - capped.lines().count())),
            "the kept text starts on a complete row"
        );
    }

    /// Shift+Cmd+R resets the emulator. It used to type `ESC c` at the
    /// child, which interrupts a running agent instead of resetting the
    /// screen; now the grid resets and nothing is written towards the PTY.
    #[test]
    fn reset_terminal_resets_the_emulator_without_writing_to_the_pty() {
        let state = seed_numbered_history(4, 40, 10);
        state.write_output(b"\x1b[?2004h");
        let (before, ..) = state
            .extract_scrollback_window(200, 0)
            .expect("the runtime answers");
        assert!(before.contains("line-0009"));
        assert!(
            state
                .session_backend()
                .modes()
                .contains(Modes::BRACKETED_PASTE),
            "fixture: a negotiated mode"
        );

        state.reset_terminal();

        assert_eq!(
            state.extract_scrollback_window(200, 0),
            Some((String::new(), 0, 0, true)),
            "RIS drops the history and the screen"
        );
        assert!(
            !state
                .session_backend()
                .modes()
                .contains(Modes::BRACKETED_PASTE),
            "RIS drops negotiated modes"
        );
        assert!(
            state
                .pending_input
                .lock()
                .expect("pending_input lock")
                .is_empty(),
            "the reset is not queued as input for the child"
        );
        assert_eq!(state.ghostty.queued_input_bytes(), 0);
        assert!(
            !state
                .keyboard_input_sent
                .load(std::sync::atomic::Ordering::Relaxed),
            "the reset is not user input"
        );
    }

    /// U-001: a multibyte codepoint straddling the byte cap must not panic
    /// the cut; it lands on a char boundary and then on a complete line.
    #[test]
    fn cap_scrollback_truncates_on_char_boundary() {
        const MAX: usize = 100;
        // 99 ASCII bytes, then a 4-byte '🦀' occupying byte indices 99..103, so
        // the cut point `len - MAX` (3) is a boundary but the crab straddles
        // the old head-side cut at byte 100. A single line with no newline
        // cannot be kept as a complete line, so the cap yields nothing,
        // exactly like the engine's `bounded_recent_text`.
        let mut s = "a".repeat(MAX - 1);
        s.push('🦀');
        assert!(s.len() > MAX, "fixture must exceed the cap");
        cap_scrollback_at_char_boundary(&mut s, MAX);
        assert_eq!(
            s, "",
            "one oversized line with no newline is not kept partially"
        );

        // The tail-side cut inside a codepoint: `len - cap` lands in the
        // middle of the second crab, the partial first line is dropped, and
        // the complete newest lines survive.
        let mut s = String::from("🦀🦀\nab\ncd");
        cap_scrollback_at_char_boundary(&mut s, 9);
        assert_eq!(s, "ab\ncd");

        // A cut that lands exactly after a newline keeps the line after it.
        let mut s = String::from("old\nnew");
        cap_scrollback_at_char_boundary(&mut s, 3);
        assert_eq!(s, "new");
    }

    /// The cap keeps the NEWEST rows: `extract_scrollback_capped` hands it the
    /// tail of the transcript, and undo replays that tail, so cutting the
    /// head (what it used to do) dropped the screen and replayed the oldest
    /// rows instead. Multibyte rows, and a fixture that really crosses
    /// `MAX_CHARS`, so the cap branch is the one under test.
    #[test]
    fn cap_scrollback_keeps_the_newest_complete_rows() {
        let row = |i: usize| format!("{i:05}{}", "中".repeat(120));
        let rows: Vec<String> = (0..1200).map(row).collect();
        let full = rows.join("\n");
        assert!(
            full.len() > crate::limits::MAX_CHARS,
            "fixture must exceed MAX_CHARS; len={}",
            full.len()
        );

        let mut capped = full.clone();
        cap_scrollback_at_char_boundary(&mut capped, crate::limits::MAX_CHARS);

        assert!(capped.len() <= crate::limits::MAX_CHARS);
        assert!(
            full.ends_with(&capped),
            "the kept text must be the newest rows, not the oldest"
        );
        assert!(
            capped.starts_with(&rows[1200 - capped.lines().count()]),
            "the kept text starts on a complete row"
        );
        assert!(
            capped.ends_with(&rows[1199]),
            "the newest row (the bottom of the screen) survives the cap"
        );
        assert!(
            !capped.starts_with(&rows[0]),
            "the oldest rows are the ones the cap drops"
        );
    }

    /// Already-aligned cap is a no-op beyond the existing line trim.
    #[test]
    fn cap_scrollback_noop_under_cap() {
        let mut s = "short line".to_string();
        let before = s.clone();
        cap_scrollback_at_char_boundary(&mut s, 100);
        assert_eq!(s, before);
    }

    #[test]
    fn try_write_to_pty_queues_while_display_only() {
        // A pane that is still spawning must queue, not error: `promote_ghostty`
        // flushes the queue, so `sent: true` over IPC is honest here.
        let (state, _pending) = TerminalState::new_pending(80, 24);
        assert_eq!(state.try_write_to_pty(b"claude\r".to_vec()), Ok(()));
        let queued = state.pending_input.lock().expect("pending_input lock");
        assert_eq!(queued.len(), 1);
        assert!(
            matches!(&queued[0], PendingTerminalInput::Raw(bytes) if bytes.as_ref() == b"claude\r")
        );
    }

    #[test]
    fn try_write_to_pty_is_rejected_after_a_spawn_failure() {
        // A failed spawn never promotes, so whatever was queued for the child
        // has no child to reach and a later write must be refused, not queued
        // forever behind a `sent: true` (the pane is a static error pane).
        let (mut state, _pending) = TerminalState::new_pending(80, 24);
        assert_eq!(state.try_write_to_pty(b"claude\r".to_vec()), Ok(()));
        state.report_spawn_failure(
            TerminalBackendFailureDiagnostics::new(
                TerminalBackendFailurePhase::Spawn,
                TerminalBackendFailureDiagnostics::GHOSTTY_OPEN_PTY_FAILED,
                Some(5),
            ),
            "spawn failed",
        );

        assert_eq!(
            state.try_write_to_pty(b"x".to_vec()),
            Err(PtyWriteError::Rejected),
            "a spawn-failed pane must report the write it cannot deliver"
        );
        assert!(
            state
                .pending_input
                .lock()
                .expect("pending_input lock")
                .is_empty(),
            "input queued for the child that never came is dropped with it"
        );
        assert_eq!(
            state.write_ghostty_key(test_key_input(paneflow_terminal_ghostty::KeyAction::Press)),
            BackendInputResult::Rejected,
            "structured input is refused the same way"
        );
    }

    #[test]
    fn try_write_to_pty_errors_on_pending_overflow() {
        let (state, _pending) = TerminalState::new_pending(80, 24);
        let chunk = vec![b'x'; MAX_PENDING_INPUT_BYTES - INPUT_CONTROL_RESERVE_BYTES];
        assert_eq!(state.try_write_to_pty(chunk), Ok(()));
        assert_eq!(
            state.try_write_to_pty(b"y".as_slice()),
            Err(PtyWriteError::Rejected),
            "a full pending buffer must be reported, never silently dropped"
        );
    }

    #[test]
    fn osc_title_is_scrubbed_and_bounded_at_ingestion() {
        let mut state = TerminalState::new_display_only(24, 80);
        let raw = format!("\u{202E}safe\n\u{0007}{}", "界".repeat(20_000));

        state.ingest_title(raw);

        assert!(state.title.starts_with("safe "), "{:?}", state.title);
        assert!(state.title.chars().count() <= 241);
        assert!(!state.title.contains('\u{202E}'));
        assert!(!state.title.contains('\n'));
        assert!(!state.title.contains('\u{0007}'));
        assert!(state.title.ends_with('…'));
    }

    /// The fork's teardown contract on the Ghostty host (#184 decision 3):
    /// dropping the state SIGTERMs every pinned process group in the PTY
    /// session - a background job and a stopped job both live in groups
    /// distinct from the shell's - and escalates to SIGKILL 100 ms later.
    /// The runtime thread reaps only; if it signalled, this would still pass,
    /// so the assertion is on the *outcome* every path must guarantee.
    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_the_state_kills_background_and_stopped_jobs_in_the_pty_session() {
        use std::time::{Duration, Instant};

        fn sleep_children_of(shell_pid: u32) -> Vec<i32> {
            let out = std::process::Command::new("pgrep")
                .args(["-P", &shell_pid.to_string(), "-x", "sleep"])
                .output()
                .expect("pgrep");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        }
        fn alive(pid: i32) -> bool {
            // SAFETY: signal 0 is an existence probe.
            unsafe { libc::kill(pid, 0) == 0 }
        }

        let (mut state, pending) = TerminalState::new_pending(80, 24);
        let params = SpawnParams {
            shell: "/bin/sh".into(),
            shell_quoting: ShellQuoting::Posix,
            extra_args: Vec::new(),
            env: std::collections::HashMap::from([
                ("TERM".into(), "xterm-256color".into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ]),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            profile: TerminalSurfaceProfile::Normal,
        };
        let spawned = state
            .ghostty_session()
            .start(pending.ghostty, params, None, 1_000)
            .expect("spawn a PTY shell");
        let shell_pid = spawned.child_pid;
        assert!(shell_pid > 0);
        state.promote_ghostty(spawned);
        assert!(
            state.pty_master_fd.is_some(),
            "promotion keeps the app-owned master dup"
        );

        // One background job, one stopped foreground job: two process groups
        // that are not the shell's own.
        state.write_to_pty(b"sleep 3600 &\n".to_vec());
        std::thread::sleep(Duration::from_millis(300));
        state.write_to_pty(b"sleep 3600\n".to_vec());
        std::thread::sleep(Duration::from_millis(300));
        state.write_to_pty(b"\x1a".to_vec());

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut sleeps = Vec::new();
        while Instant::now() < deadline {
            sleeps = sleep_children_of(shell_pid);
            if sleeps.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            sleeps.len() >= 2,
            "expected two sleep children, got {sleeps:?}"
        );

        drop(state);

        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline && sleeps.iter().any(|pid| alive(*pid)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survivors: Vec<i32> = sleeps.iter().copied().filter(|pid| alive(*pid)).collect();
        for pid in &survivors {
            // SAFETY: test-only cleanup of a process this fixture created.
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        assert!(
            survivors.is_empty(),
            "jobs {survivors:?} survived TerminalState::drop"
        );
    }
}
