//! Terminal-agent launcher: the CLI coding agents Paneflow starts in a
//! terminal pane (Claude Code, Codex, OpenCode, Grok, Cursor, Antigravity,
//! Copilot, and Muse Code). Both the tab-bar launcher buttons
//! (`pane.rs`) and the new-pane picker iterate this single
//! source of truth so the per-agent visibility gate and the "respect
//! bypass" contract can never drift between them.
//!
//! Each variant maps to a display name, an icon, an accent tint, a
//! Settings → AI Agent visibility flag (`*_button_visible`), a stable
//! persistence tag, and a launch command. The launch command honors
//! `claude_code_bypass_permissions` exactly as the tab bar does.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use paneflow_config::schema::PaneFlowConfig;

/// One of the CLI coding agents Paneflow can launch in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalAgent {
    ClaudeCode,
    Codex,
    OpenCode,
    Grok,
    Cursor,
    Antigravity,
    Copilot,
    Muse,
}

impl TerminalAgent {
    /// Every variant, in display order (matches the tab-bar button row).
    /// The relative order of retained agents stays stable across upgrades.
    pub const ALL: [TerminalAgent; 8] = [
        TerminalAgent::ClaudeCode,
        TerminalAgent::Codex,
        TerminalAgent::OpenCode,
        TerminalAgent::Grok,
        TerminalAgent::Cursor,
        TerminalAgent::Antigravity,
        TerminalAgent::Copilot,
        TerminalAgent::Muse,
    ];

    /// Stable display rank - index in [`Self::ALL`]. Used by the sidebar to
    /// order multi-tool status rows deterministically instead of letting
    /// `HashMap` iteration order leak into the UI.
    pub fn display_rank(self) -> usize {
        Self::ALL
            .iter()
            .position(|a| *a == self)
            .unwrap_or(usize::MAX)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "Claude Code",
            TerminalAgent::Codex => "Codex",
            TerminalAgent::OpenCode => "OpenCode",
            TerminalAgent::Grok => "Grok",
            TerminalAgent::Cursor => "Cursor",
            TerminalAgent::Antigravity => "Antigravity",
            TerminalAgent::Copilot => "Copilot",
            TerminalAgent::Muse => "Muse Code",
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "icons/claude-color.svg",
            TerminalAgent::Codex => "icons/codex.svg",
            TerminalAgent::OpenCode => "icons/opencode-color.svg",
            TerminalAgent::Grok => "agents/grok.svg",
            TerminalAgent::Cursor => "agents/cursor.svg",
            TerminalAgent::Antigravity => "agents/antigravity-color.svg",
            TerminalAgent::Copilot => "agents/githubcopilot.svg",
            TerminalAgent::Muse => "agents/muse-color.svg",
        }
    }

    /// Brand accent for the icon tint, as a packed `0xRRGGBB`. `None`
    /// means "use the theme's primary text color" -- the OpenCode logo
    /// is a monochrome `currentColor` SVG (so is Codex, which
    /// carries the OpenAI blossom mark).
    pub fn accent(self) -> Option<u32> {
        match self {
            TerminalAgent::ClaudeCode => Some(0xd97757),
            // Single-color brand logos: `svg()` renders a monochrome alpha
            // mask, so the silhouette is painted in this brand color.
            TerminalAgent::Muse => Some(0x0081FB),
            // The rest are either monochrome `currentColor` logos (tinted
            // with the theme's primary text color so they stay readable on
            // every theme) or multi-color logos rendered in their native
            // palette via `img()` (see `icon_multicolor`), where `accent`
            // is unused.
            TerminalAgent::Codex
            | TerminalAgent::OpenCode
            | TerminalAgent::Grok
            | TerminalAgent::Cursor
            | TerminalAgent::Antigravity
            | TerminalAgent::Copilot => None,
        }
    }

    /// Whether the icon must be rendered in its native colors via `img()`
    /// (multi-color logos: gradients or several distinct fills) instead of
    /// a `text_color`-tinted monochrome `svg()` mask. GPUI's `svg()`
    /// flattens every path to one tint, which would destroy these palettes;
    /// `img()` rasterizes the SVG (resvg) and preserves every fill. A
    /// single-color brand logo stays monochrome and uses `accent()`.
    pub fn icon_multicolor(self) -> bool {
        matches!(self, TerminalAgent::Antigravity)
    }

    /// Stable persistence tag for the session.json `terminal_agent`
    /// field. Kept distinct from the binary name so a future rename of
    /// the CLI does not invalidate persisted threads.
    pub fn tag(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "claude_code",
            TerminalAgent::Codex => "codex",
            TerminalAgent::OpenCode => "opencode",
            TerminalAgent::Grok => "grok",
            TerminalAgent::Cursor => "cursor",
            TerminalAgent::Antigravity => "antigravity",
            TerminalAgent::Copilot => "copilot",
            TerminalAgent::Muse => "muse",
        }
    }

    /// EP-005 US-013: map a detected process basename back to its agent
    /// (reverse of [`Self::binary`]). Exact match only - the per-pane scan
    /// matches `/proc/<pid>/comm` verbatim, so a wrapper script or a
    /// suffixed binary never produces a pill.
    pub fn from_binary(name: &str) -> Option<TerminalAgent> {
        TerminalAgent::ALL
            .iter()
            .copied()
            .find(|a| a.binary() == name)
    }

    /// Declared identity of a launch command: the first pipeline segment
    /// whose leading token names a known agent binary.
    ///
    /// This is the cmux model - the agent an entry point is *about to run* is
    /// known before any process exists, so the surface can carry its identity
    /// from frame zero instead of waiting for the process scan. The input is
    /// always a command Paneflow itself composed or a local IPC client sent;
    /// it is NEVER terminal output, so this cannot be spoofed by a remote
    /// shell the way an OSC title can. The per-pane scan stays the
    /// PID-authoritative belt that confirms or corrects the declaration.
    ///
    /// Segmenting on the shell operators is what makes [`Self::launch_command`]
    /// resolve at all: every agent command is prefixed with a clear
    /// (`clear && claude`, `Clear-Host; claude`), so a naive first-token
    /// read would only ever see the clear. Within a segment the leading token
    /// must BE the binary - `npm run claude` names npm, not Claude, and
    /// correctly declares nothing. Leading `KEY=value` env assignments are
    /// skipped and a path prefix is stripped.
    pub fn from_launch_command(command: &str) -> Option<TerminalAgent> {
        command.split(['&', '|', ';', '\n']).find_map(|segment| {
            let token = segment
                .split_whitespace()
                .find(|token| !is_env_assignment(token))?;
            let base = token.rsplit('/').next().unwrap_or(token);
            TerminalAgent::from_binary(base)
        })
    }

    pub fn from_tag(tag: &str) -> Option<TerminalAgent> {
        match tag {
            "claude_code" => Some(TerminalAgent::ClaudeCode),
            "codex" => Some(TerminalAgent::Codex),
            "opencode" => Some(TerminalAgent::OpenCode),
            "grok" => Some(TerminalAgent::Grok),
            "cursor" => Some(TerminalAgent::Cursor),
            "antigravity" => Some(TerminalAgent::Antigravity),
            "copilot" => Some(TerminalAgent::Copilot),
            "muse" => Some(TerminalAgent::Muse),
            _ => None,
        }
    }

    /// Whether this launcher is shown in the tab bar and the new-pane picker.
    ///
    /// Tri-state on the `*_button_visible` config key:
    /// - `Some(true)`  - user explicitly enabled it: always shown.
    /// - `Some(false)` - user explicitly disabled it: always hidden.
    /// - `None` (key absent, the default) - Claude Code, Codex, and Grok are
    ///   shown when their CLI binary is installed; every other agent stays
    ///   hidden. The user can still force-show any uninstalled or non-default
    ///   agent by toggling it on.
    pub fn is_visible(self, config: &PaneFlowConfig) -> bool {
        self.is_visible_with(config, TerminalAgent::is_installed)
    }

    /// Visibility resolution with install detection injected so the allowlist
    /// contract is testable without depending on the test host's `PATH`.
    fn is_visible_with(
        self,
        config: &PaneFlowConfig,
        is_installed: impl FnOnce(TerminalAgent) -> bool,
    ) -> bool {
        let explicit: Option<bool> = match self {
            TerminalAgent::ClaudeCode => config.claude_code_button_visible,
            TerminalAgent::Codex => config.codex_button_visible,
            TerminalAgent::OpenCode => config.opencode_button_visible,
            TerminalAgent::Grok => config.grok_button_visible,
            TerminalAgent::Cursor => config.cursor_button_visible,
            TerminalAgent::Antigravity => config.antigravity_button_visible,
            TerminalAgent::Copilot => config.copilot_button_visible,
            TerminalAgent::Muse => config.muse_button_visible,
        };
        explicit.unwrap_or_else(|| self.is_default_enabled() && is_installed(self))
    }

    /// Fresh-config allowlist: the launchers shown when the config carries no
    /// explicit visibility value for them.
    fn is_default_enabled(self) -> bool {
        matches!(
            self,
            TerminalAgent::ClaudeCode | TerminalAgent::Codex | TerminalAgent::Grok
        )
    }

    /// Raw JSON key used by Settings persistence.
    pub(crate) fn button_visibility_key(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "claude_code_button_visible",
            TerminalAgent::Codex => "codex_button_visible",
            TerminalAgent::OpenCode => "opencode_button_visible",
            TerminalAgent::Grok => "grok_button_visible",
            TerminalAgent::Cursor => "cursor_button_visible",
            TerminalAgent::Antigravity => "antigravity_button_visible",
            TerminalAgent::Copilot => "copilot_button_visible",
            TerminalAgent::Muse => "muse_button_visible",
        }
    }

    /// The CLI executable looked up on `PATH` to decide default visibility;
    /// also the leading token of [`Self::launch_command`].
    pub fn binary(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "claude",
            TerminalAgent::Codex => "codex",
            TerminalAgent::OpenCode => "opencode",
            TerminalAgent::Grok => "grok",
            TerminalAgent::Cursor => "cursor-agent",
            TerminalAgent::Antigravity => "agy",
            TerminalAgent::Copilot => "copilot",
            TerminalAgent::Muse => "muse",
        }
    }

    /// Whether this agent's CLI binary is found on `PATH`. Drives the
    /// default visibility in [`Self::is_visible`].
    ///
    /// **Never blocks.** `which` walks `PATH` off-thread. Render (and every
    /// other caller with a snapshot already in hand) reads that snapshot and
    /// never waits on the walk: a TTL miss schedules `paneflow-agent-which`
    /// and returns the last answer, and a cold cache (issue #518) schedules
    /// the first walk and answers `false` at once. Read
    /// [`installed_binary_scan_pending`] to tell that `false` apart from a
    /// finished scan that found nothing. The cache mutex is never held
    /// across `which`.
    pub fn is_installed(self) -> bool {
        installed_binaries_contains(self.binary())
    }

    /// Supported interactive agents start with their bare executable.
    fn command_args(self) -> &'static [&'static str] {
        &[]
    }

    fn launch_spec(self, config: &PaneFlowConfig) -> AgentCommandSpec {
        let mut spec = AgentCommandSpec::new(self.binary());
        spec.extend_args(self.command_args().iter().copied());
        if self == TerminalAgent::ClaudeCode
            && config.claude_code_bypass_permissions.unwrap_or(false)
        {
            spec.push_arg("--permission-mode");
            spec.push_arg("bypassPermissions");
        }
        spec
    }

    /// Bare command that starts the agent. Honors
    /// `claude_code_bypass_permissions` for Claude Code.
    fn command(self, config: &PaneFlowConfig) -> String {
        self.launch_spec(config).render_shell_command()
    }

    /// Map this launcher to the session reader PaneFlow can safely use.
    /// `None` means the CLI does not expose a documented local list+resume
    /// contract suitable for the sidebar yet.
    pub fn session_agent(self) -> Option<crate::agent_sessions::SessionAgent> {
        use crate::agent_sessions::SessionAgent;
        match self {
            TerminalAgent::ClaudeCode => Some(SessionAgent::Claude),
            TerminalAgent::Codex => Some(SessionAgent::Codex),
            TerminalAgent::OpenCode => Some(SessionAgent::OpenCode),
            TerminalAgent::Grok => Some(SessionAgent::Grok),
            TerminalAgent::Cursor => Some(SessionAgent::Cursor),
            _ => None,
        }
    }

    /// Shell-aware launch command. The clear prefix is selected for the
    /// configured shell (`clear`, `cls`, or `Clear-Host`) so the agent TUI owns
    /// the viewport from the first frame on every platform.
    pub fn launch_command(self, config: &PaneFlowConfig) -> String {
        // US-042: trim + drop-empty exactly like the PTY session does when it
        // resolves the shell (`pty_session.rs:442`). A config such as
        // `"default_shell": "  pwsh  "` otherwise reaches `clear_then`
        // untrimmed, fails the `which::which` probe, and emits the wrong
        // clear arm for a POSIX command.
        let shell = config
            .default_shell
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        crate::terminal::shell::clear_then(&self.command(config), shell)
    }

    /// Visible variants for the given config, in display order. Drives
    /// both the new-pane picker and (via the same gates) the tab bar.
    pub fn visible(config: &PaneFlowConfig) -> Vec<TerminalAgent> {
        TerminalAgent::ALL
            .into_iter()
            .filter(|a| a.is_visible(config))
            .collect()
    }

    /// [`Self::visible`] with the installed answer supplied by the caller.
    /// The pane palette passes "installed" for every agent while the first
    /// PATH walk is pending (issue #518) so the default-enabled agents get a
    /// row that reads `looking` instead of vanishing from the catalogue.
    pub(crate) fn visible_with(
        config: &PaneFlowConfig,
        is_installed: impl Fn(TerminalAgent) -> bool,
    ) -> Vec<TerminalAgent> {
        TerminalAgent::ALL
            .into_iter()
            .filter(|a| a.is_visible_with(config, &is_installed))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentCommandSpec {
    program: &'static str,
    args: Vec<String>,
}

impl AgentCommandSpec {
    pub(crate) fn new(program: &'static str) -> Self {
        Self {
            program,
            args: Vec::new(),
        }
    }

    pub(crate) fn push_arg(&mut self, arg: impl Into<String>) {
        self.args.push(arg.into());
    }

    fn extend_args(&mut self, args: impl IntoIterator<Item = &'static str>) {
        self.args.extend(args.into_iter().map(str::to_string));
    }

    pub(crate) fn render_shell_command(&self) -> String {
        debug_assert!(is_plain_shell_token(self.program));
        let mut command = self.program.to_string();
        for arg in &self.args {
            debug_assert!(is_plain_shell_token(arg));
            command.push(' ');
            command.push_str(arg);
        }
        command
    }
}

pub(crate) fn is_plain_shell_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
}

const INSTALLED_BINARIES_TTL: Duration = Duration::from_secs(2);

/// Longest one PATH directory may hold the installed-agent scan.
/// A local `stat` answers in microseconds; a dead network mount does not.
/// Same 250 ms window as the `.git` directory probe: one stalled directory
/// cannot pin boot or a queued launch. The helper keeps running, so a later
/// refresh (the in-flight flag is cleared when this scan returns) reads the
/// answer when the mount recovers instead of treating the miss as final.
const INSTALLED_AGENT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

type ProbeFn = Arc<dyn Fn() -> HashSet<&'static str> + Send + Sync>;

struct InstalledBinaryCache {
    checked_at: Option<Instant>,
    found: HashSet<&'static str>,
    refresh_in_flight: bool,
}

impl InstalledBinaryCache {
    fn is_stale(&self) -> bool {
        self.checked_at
            .is_none_or(|checked_at| checked_at.elapsed() >= INSTALLED_BINARIES_TTL)
    }
}

struct InstalledBinaryInner {
    cache: Mutex<InstalledBinaryCache>,
    initial_ready: Mutex<bool>,
    initial_cvar: Condvar,
    probe: ProbeFn,
}

/// Dropped without `published` when the probe panics or the spawn callback
/// unwinds: clears `refresh_in_flight` and unblocks cold waiters so a failed
/// walk cannot stick the cache.
struct RefreshGuard {
    inner: Arc<InstalledBinaryInner>,
    published: bool,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if !self.published {
            self.inner.abandon_refresh();
        }
    }
}

impl InstalledBinaryInner {
    fn lock_cache(&self) -> std::sync::MutexGuard<'_, InstalledBinaryCache> {
        match self.cache.lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                tracing::warn!(
                    target: "paneflow_app::agent_launcher",
                    "installed binary cache mutex poisoned; recovering"
                );
                poisoned.into_inner()
            }
        }
    }

    fn lock_initial_ready(&self) -> std::sync::MutexGuard<'_, bool> {
        match self.initial_ready.lock() {
            Ok(ready) => ready,
            Err(poisoned) => {
                tracing::warn!(
                    target: "paneflow_app::agent_launcher",
                    "installed binary ready mutex poisoned; recovering"
                );
                poisoned.into_inner()
            }
        }
    }

    fn mark_initial_ready(&self) {
        let mut ready = self.lock_initial_ready();
        *ready = true;
        self.initial_cvar.notify_all();
    }

    fn wait_for_initial(&self) {
        let mut ready = self.lock_initial_ready();
        while !*ready {
            ready = match self.initial_cvar.wait(ready) {
                Ok(ready) => ready,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn publish(&self, found: HashSet<&'static str>) {
        {
            let mut cache = self.lock_cache();
            cache.found = found;
            cache.checked_at = Some(Instant::now());
            cache.refresh_in_flight = false;
        }
        self.mark_initial_ready();
    }

    fn abandon_refresh(&self) {
        {
            let mut cache = self.lock_cache();
            cache.refresh_in_flight = false;
            if cache.checked_at.is_none() {
                // Unblock cold waiters with the empty snapshot rather than
                // deadlock if the probe thread never publishes.
                cache.checked_at = Some(Instant::now());
            }
        }
        self.mark_initial_ready();
    }

    fn run_refresh(self: &Arc<Self>) {
        let mut guard = RefreshGuard {
            inner: Arc::clone(self),
            published: false,
        };
        let found = (self.probe)();
        self.publish(found);
        guard.published = true;
    }
}

struct InstalledBinaries {
    inner: Arc<InstalledBinaryInner>,
}

impl InstalledBinaries {
    fn new() -> Self {
        Self::with_probe(Arc::new(probe_installed_binaries))
    }

    fn with_probe(probe: ProbeFn) -> Self {
        Self {
            inner: Arc::new(InstalledBinaryInner {
                cache: Mutex::new(InstalledBinaryCache {
                    checked_at: None,
                    found: HashSet::new(),
                    refresh_in_flight: false,
                }),
                initial_ready: Mutex::new(false),
                initial_cvar: Condvar::new(),
                probe,
            }),
        }
    }

    /// Snapshot read that never waits on `which`. A stale or cold cache
    /// schedules one refresh (if none is in flight) and answers from the
    /// current snapshot, which is empty until the first walk publishes.
    fn contains(&self, binary: &'static str) -> bool {
        let (hit, spawn) = {
            let mut cache = self.inner.lock_cache();
            let spawn = cache.is_stale() && !cache.refresh_in_flight;
            if spawn {
                cache.refresh_in_flight = true;
            }
            (cache.found.contains(binary), spawn)
        };

        if spawn {
            self.spawn_refresh();
        }

        hit
    }

    /// Blocking read: like [`Self::contains`], but a cold cache waits for
    /// the first walk to publish instead of answering from the empty
    /// snapshot. Only the cache tests need that blocking answer.
    #[cfg(test)]
    fn contains_now(&self, binary: &'static str) -> bool {
        // `contains` only schedules; wait for the first publish (immediate
        // once ready, and an abandoned spawn marks it ready with the empty
        // snapshot) and re-read.
        let _ = self.contains(binary);
        self.inner.wait_for_initial();
        self.inner.lock_cache().found.contains(binary)
    }

    /// `true` until the first walk has published (or been abandoned).
    fn scan_pending(&self) -> bool {
        self.inner.lock_cache().checked_at.is_none()
    }

    /// Run the walk on the caller's thread when the cache is stale and no
    /// refresh is in flight; otherwise wait for an in-flight cold walk so
    /// the caller returns with a published snapshot either way. Boot calls
    /// this from `smol::unblock`, so the GPUI thread never walks `PATH`.
    ///
    /// Each PATH directory is bounded by [`INSTALLED_AGENT_PROBE_TIMEOUT`].
    /// A directory that misses the deadline is skipped for this snapshot;
    /// the walk still returns, `refresh_in_flight` clears, and boot can
    /// replay a launch that was queued while the scan was pending.
    fn warm(&self) {
        let run_here = {
            let mut cache = self.inner.lock_cache();
            let run_here = cache.is_stale() && !cache.refresh_in_flight;
            if run_here {
                cache.refresh_in_flight = true;
            }
            run_here
        };
        if run_here {
            self.inner.run_refresh();
        } else {
            self.inner.wait_for_initial();
        }
    }

    fn spawn_refresh(&self) {
        let inner = Arc::clone(&self.inner);
        match std::thread::Builder::new()
            .name("paneflow-agent-which".into())
            .spawn(move || inner.run_refresh())
        {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    target: "paneflow_app::agent_launcher",
                    error = %err,
                    "failed to spawn installed-binary probe thread; abandoning this refresh"
                );
                // Never walk PATH on the caller: `contains` is read from
                // render frames (issue #518), so a thread-exhausted process
                // would otherwise run every `which` on the GPUI thread.
                // Abandoning publishes the empty snapshot to cold waiters;
                // the boot warm / the next stale read schedules another walk.
                self.inner.abandon_refresh();
            }
        }
    }

    #[cfg(test)]
    fn seed(&self, found: HashSet<&'static str>, checked_at: Instant) {
        {
            let mut cache = self.inner.lock_cache();
            cache.found = found;
            cache.checked_at = Some(checked_at);
            cache.refresh_in_flight = false;
        }
        self.inner.mark_initial_ready();
    }

    #[cfg(test)]
    fn cache_mutex_is_free(&self) -> bool {
        self.inner.cache.try_lock().is_ok()
    }

    #[cfg(test)]
    fn refresh_in_flight(&self) -> bool {
        self.inner.lock_cache().refresh_in_flight
    }
}

fn probe_installed_binaries() -> HashSet<&'static str> {
    let dirs = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    probe_agents_on_path(&dirs)
}

/// The PATH scan with its lookup injected. The candidate list is
/// `TerminalAgent::ALL`, so only known agent binaries can come back; the
/// lookup decides which of them are present. Split out so a fixture can
/// drive it without depending on what the host has installed.
#[cfg(test)]
fn probe_installed_binaries_with(is_installed: impl Fn(&str) -> bool) -> HashSet<&'static str> {
    TerminalAgent::ALL
        .into_iter()
        .map(TerminalAgent::binary)
        .filter(|bin| is_installed(bin))
        .collect()
}

/// One in-flight lookup for a single PATH entry, shared so a dead mount
/// does not spawn another `stat` on every refresh.
struct DirLookup {
    /// `None` while the helper is still in `stat`. `Some` once it has
    /// answered, including an empty set when the directory has no agent.
    hits: Mutex<Option<HashSet<&'static str>>>,
    cond: Condvar,
    started: Instant,
}

fn dir_lookups() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Arc<DirLookup>>> {
    static LOOKUPS: OnceLock<Mutex<HashMap<PathBuf, Arc<DirLookup>>>> = OnceLock::new();
    LOOKUPS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
type PathDirProbeHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Test stand-in for `stat` on one PATH entry. Production scans pass through.
#[cfg(test)]
static PATH_DIR_PROBE_HOOK: Mutex<Option<PathDirProbeHook>> = Mutex::new(None);

#[cfg(test)]
fn install_path_dir_probe_hook(hook: Option<PathDirProbeHook>) {
    *PATH_DIR_PROBE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(test)]
fn note_path_dir_probe(dir: &Path) {
    let hook = PATH_DIR_PROBE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook(dir);
    }
}

/// Publishes on drop, including when the scan panics, so a helper cannot
/// leave its directory in flight forever.
struct DirLookupPublish {
    lookup: Arc<DirLookup>,
    hits: Option<HashSet<&'static str>>,
}

impl Drop for DirLookupPublish {
    fn drop(&mut self) {
        publish_dir_lookup(&self.lookup, self.hits.take().unwrap_or_default());
    }
}

fn publish_dir_lookup(lookup: &DirLookup, hits: HashSet<&'static str>) {
    {
        let mut slot = lookup
            .hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(hits);
    }
    lookup.cond.notify_all();
}

fn start_dir_lookup(dir: &Path) -> Arc<DirLookup> {
    let (lookup, spawn) = {
        let mut map = dir_lookups();
        if let Some(existing) = map.get(dir).cloned() {
            (existing, false)
        } else {
            let lookup = Arc::new(DirLookup {
                hits: Mutex::new(None),
                cond: Condvar::new(),
                started: Instant::now(),
            });
            map.insert(dir.to_path_buf(), Arc::clone(&lookup));
            (lookup, true)
        }
    };
    if spawn {
        spawn_dir_lookup(dir, &lookup);
    }
    lookup
}

fn spawn_dir_lookup(dir: &Path, lookup: &Arc<DirLookup>) {
    let dir_for_thread = dir.to_path_buf();
    let lookup_for_thread = Arc::clone(lookup);
    let spawned = std::thread::Builder::new()
        .name("paneflow-agent-which".into())
        .spawn(move || {
            let mut publish = DirLookupPublish {
                lookup: lookup_for_thread,
                hits: None,
            };
            publish.hits = Some(scan_dir_blocking(&dir_for_thread));
        });
    if let Err(err) = spawned {
        tracing::warn!(
            target: "paneflow_app::agent_launcher",
            error = %err,
            directory = %dir.display(),
            "failed to spawn installed-agent PATH probe; skipping that directory"
        );
        publish_dir_lookup(lookup, HashSet::new());
    }
}

fn scan_dir_blocking(dir: &Path) -> HashSet<&'static str> {
    // Before any `stat`: a dead mount blocks inside `which`, and the test
    // hook does the same for one directory. The caller waits on a deadline
    // and does not join this thread.
    #[cfg(test)]
    note_path_dir_probe(dir);
    let mut hits = HashSet::new();
    for bin in TerminalAgent::ALL.into_iter().map(TerminalAgent::binary) {
        if which::which_in(bin, Some(dir.as_os_str()), Path::new(".")).is_ok() {
            hits.insert(bin);
        }
    }
    hits
}

fn wait_dir_lookup(dir: &Path, lookup: &DirLookup) -> Option<HashSet<&'static str>> {
    let remaining = INSTALLED_AGENT_PROBE_TIMEOUT.saturating_sub(lookup.started.elapsed());
    let slot = lookup
        .hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        return slot.clone();
    }
    if remaining.is_zero() {
        // Already past this directory's deadline (or a previous scan is
        // still blocked in it). Don't spend another budget here.
        return None;
    }
    let (slot, status) = lookup
        .cond
        .wait_timeout_while(slot, remaining, |slot| slot.is_none())
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hits = slot.clone();
    let timed_out = status.timed_out();
    drop(slot);
    if hits.is_none() && timed_out {
        tracing::warn!(
            target: "paneflow_app::agent_launcher",
            directory = %dir.display(),
            "installed-agent PATH probe did not answer within {remaining:?}; skipping that directory"
        );
    }
    hits
}

fn forget_dir_lookup(dir: &Path, lookup: &Arc<DirLookup>) {
    let mut map = dir_lookups();
    if map
        .get(dir)
        .is_some_and(|current| Arc::ptr_eq(current, lookup))
    {
        map.remove(dir);
    }
}

/// PATH scan whose blocking `stat`s cannot outlive
/// [`INSTALLED_AGENT_PROBE_TIMEOUT`] per directory. Lookups start together,
/// so one stalled entry does not push the rest past that window: their
/// clocks are already running, and a directory that has answered is kept.
/// A miss is not final — the helper's later answer stays in the map for
/// the next refresh, which can run because the caller clears
/// `refresh_in_flight` when this returns.
fn probe_agents_on_path(dirs: &[PathBuf]) -> HashSet<&'static str> {
    let lookups: Vec<(PathBuf, Arc<DirLookup>)> = dirs
        .iter()
        .map(|dir| (dir.clone(), start_dir_lookup(dir)))
        .collect();
    let mut found = HashSet::new();
    for (dir, lookup) in &lookups {
        if let Some(hits) = wait_dir_lookup(dir, lookup) {
            found.extend(hits);
            forget_dir_lookup(dir, lookup);
        }
    }
    found
}

fn installed_binaries() -> &'static InstalledBinaries {
    static CACHE: OnceLock<InstalledBinaries> = OnceLock::new();
    CACHE.get_or_init(InstalledBinaries::new)
}

/// Agent binaries found on `PATH`. The cache is short-lived rather than
/// process-lifetime so agents installed while Paneflow is open can appear
/// without a restart. Render reads a snapshot; `which` runs on
/// `paneflow-agent-which` and never under the cache mutex.
fn installed_binaries_contains(binary: &'static str) -> bool {
    installed_binaries().contains(binary)
}

/// Shown in place of a "not installed" verdict while the first PATH walk
/// for agent CLIs is still running (issue #518). The new-pane picker shows it.
pub(crate) const AGENT_SCAN_PENDING_COPY: &str = "Looking for agent CLIs on this machine.";

/// `true` while no PATH walk has published yet, so the UI can say it is
/// still looking instead of claiming nothing is installed (issue #518).
pub(crate) fn installed_binary_scan_pending() -> bool {
    installed_binaries().scan_pending()
}

/// Blocking warm of the installed-agent cache for the boot task: walks
/// `PATH` on the caller's thread (call it from `smol::unblock`), or waits
/// for the walk already in flight. Returns once a snapshot is published,
/// including when a PATH directory misses [`INSTALLED_AGENT_PROBE_TIMEOUT`].
pub(crate) fn refresh_installed_binaries() {
    installed_binaries().warm();
}

/// `KEY=value` shell prefix in front of a command (`RUST_LOG=info codex`).
/// Conservative: the key must be a non-empty identifier, so `--flag=x` and a
/// bare `=foo` are not mistaken for assignments.
fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((key, _)) => {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !key.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_agents_have_no_native_identity() {
        for (tag, binary) in [
            ("pi", "pi"),
            ("hermes", "hermes"),
            ("amp", "amp"),
            ("kiro", "kiro-cli"),
            ("codebuddy", "codebuddy"),
            ("factory", "droid"),
            ("qoder", "qodercli"),
            ("openclaw", "openclaw"),
            ("deepseek_harness", "dsh"),
            ("gemini", "gemini"),
        ] {
            assert_eq!(TerminalAgent::from_tag(tag), None, "retired tag {tag}");
            assert_eq!(
                TerminalAgent::from_binary(binary),
                None,
                "retired binary {binary}"
            );
            assert_eq!(
                TerminalAgent::from_launch_command(&format!("clear && {binary}")),
                None
            );
        }
    }

    #[test]
    fn retired_visibility_keys_load_without_restoring_launchers() {
        let config: PaneFlowConfig = serde_json::from_value(serde_json::json!({
            "pi_button_visible": true, "hermes_agent_button_visible": true,
            "amp_button_visible": true, "kiro_button_visible": true,
            "codebuddy_button_visible": true, "factory_button_visible": true,
            "qoder_button_visible": true, "openclaw_button_visible": true,
            "deepseek_harness_button_visible": true, "gemini_button_visible": true,
            "codex_button_visible": true,
        }))
        .unwrap();
        let visible: Vec<_> = TerminalAgent::ALL
            .into_iter()
            .filter(|agent| agent.is_visible_with(&config, |_| false))
            .map(TerminalAgent::tag)
            .collect();
        assert_eq!(visible, ["codex"]);
    }

    /// Issue #1132 removed Gemini CLI; Antigravity keeps its launcher,
    /// identity, multicolor mark, opt-in visibility, and bare `agy` command.
    #[test]
    fn antigravity_launcher_stays_supported_after_gemini_removal() {
        let agent = TerminalAgent::Antigravity;
        assert!(TerminalAgent::ALL.contains(&agent));
        assert_eq!(agent.display_name(), "Antigravity");
        assert_eq!(agent.icon_path(), "agents/antigravity-color.svg");
        assert!(agent.icon_multicolor());
        assert_eq!(agent.accent(), None);
        assert_eq!(TerminalAgent::from_tag("antigravity"), Some(agent));
        assert_eq!(TerminalAgent::from_binary("agy"), Some(agent));
        assert_eq!(
            TerminalAgent::from_launch_command("clear && agy"),
            Some(agent)
        );
        assert_eq!(agent.button_visibility_key(), "antigravity_button_visible");
        assert_eq!(agent.command(&PaneFlowConfig::default()), "agy");
        assert_eq!(agent.session_agent(), None);

        let config = PaneFlowConfig::default();
        assert!(
            !agent.is_visible_with(&config, |_| true),
            "opt-in by default"
        );
        let shown = PaneFlowConfig {
            antigravity_button_visible: Some(true),
            ..Default::default()
        };
        assert!(TerminalAgent::visible(&shown).contains(&agent));
    }

    /// Issue #518: `contains` is read from render frames, so a failed probe
    /// thread spawn must abandon the refresh, never run the PATH walk on
    /// the caller.
    #[test]
    fn a_failed_probe_thread_spawn_abandons_the_refresh_instead_of_probing_on_the_caller() {
        let src = include_str!("agent_launcher.rs");
        let body = src
            .split("fn spawn_refresh(&self) {")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("spawn_refresh exists");
        let err_arm = body
            .split("Err(err) => {")
            .nth(1)
            .expect("the spawn error arm");
        assert!(
            err_arm.contains("self.inner.abandon_refresh();"),
            "the error arm abandons the refresh: {err_arm}"
        );
        assert!(
            !err_arm.contains("run_refresh()"),
            "the error arm must not walk PATH on the caller: {err_arm}"
        );
    }

    // Every agent's own launch command must declare that agent - otherwise a
    // pane launched from the palette shows no logo until the process scan
    // lands, which is exactly the latency this declaration removes.
    #[test]
    fn launch_command_declares_its_own_agent() {
        let config = PaneFlowConfig::default();
        for agent in TerminalAgent::ALL {
            assert_eq!(
                TerminalAgent::from_launch_command(&agent.launch_command(&config)),
                Some(agent),
                "{} launch command must declare itself",
                agent.display_name()
            );
        }
    }

    #[test]
    fn from_launch_command_handles_paths_and_env_prefixes() {
        assert_eq!(
            TerminalAgent::from_launch_command("/usr/local/bin/claude --resume abc"),
            Some(TerminalAgent::ClaudeCode)
        );
        assert_eq!(
            TerminalAgent::from_launch_command("RUST_LOG=info NO_COLOR=1 codex"),
            Some(TerminalAgent::Codex)
        );
        // A wrapper is not the agent: the declaration must stay silent and let
        // the PID-authoritative scan speak.
        assert_eq!(TerminalAgent::from_launch_command("npm run claude"), None);
        assert_eq!(TerminalAgent::from_launch_command("claude-wrapper"), None);
        assert_eq!(TerminalAgent::from_launch_command(""), None);
        assert_eq!(TerminalAgent::from_launch_command("   "), None);
        // A flag that looks like an assignment must not be skipped as env.
        assert_eq!(TerminalAgent::from_launch_command("--model=x codex"), None);
    }

    #[test]
    fn tag_roundtrip() {
        for agent in TerminalAgent::ALL {
            assert_eq!(TerminalAgent::from_tag(agent.tag()), Some(agent));
        }
        assert_eq!(TerminalAgent::from_tag("unknown"), None);
    }

    // EP-005 US-013: `from_tag` is the session.json ingress whitelist for
    // the persisted `agent` field - hostile or malformed values (oversized,
    // control chars, near-misses) must all map to None so no pill renders.
    #[test]
    fn from_tag_rejects_hostile_session_values() {
        assert_eq!(TerminalAgent::from_tag(""), None);
        assert_eq!(
            TerminalAgent::from_tag("Claude_Code"),
            None,
            "case-sensitive"
        );
        assert_eq!(TerminalAgent::from_tag("claude_code "), None, "no trim");
        assert_eq!(TerminalAgent::from_tag("claude_code\u{202e}"), None);
        assert_eq!(TerminalAgent::from_tag("codex\n"), None);
        assert_eq!(TerminalAgent::from_tag(&"x".repeat(10_000)), None);
    }

    #[test]
    fn binary_roundtrip_via_from_binary() {
        // EP-005 US-013: the scan's comm match resolves back to the agent.
        for agent in TerminalAgent::ALL {
            assert_eq!(TerminalAgent::from_binary(agent.binary()), Some(agent));
        }
        assert_eq!(TerminalAgent::from_binary("bash"), None);
        assert_eq!(TerminalAgent::from_binary("claude-code-cli"), None);
    }

    #[test]
    fn binary_is_launch_command_leading_token() {
        // The PATH probe (`binary`) must match the actual executable the
        // launcher runs, or default visibility detects the wrong binary.
        let cfg = PaneFlowConfig::default();
        for agent in TerminalAgent::ALL {
            let command = agent.command(&cfg);
            let leading = command.split_whitespace().next().unwrap_or_default();
            assert_eq!(
                leading,
                agent.binary(),
                "{} binary must match its launch command's leading token",
                agent.display_name()
            );
        }
    }

    #[test]
    fn explicit_visibility_overrides_install_detection() {
        // `Some(true)`/`Some(false)` win over PATH detection, so the result
        // is deterministic on any machine (and never touches the filesystem
        // here - the `unwrap_or_else` install probe is short-circuited).
        let shown = PaneFlowConfig {
            antigravity_button_visible: Some(true),
            ..Default::default()
        };
        assert!(TerminalAgent::Antigravity.is_visible(&shown));

        let hidden = PaneFlowConfig {
            antigravity_button_visible: Some(false),
            ..Default::default()
        };
        assert!(!TerminalAgent::Antigravity.is_visible(&hidden));
    }

    #[test]
    fn absent_visibility_uses_default_allowlist_and_install_detection() {
        let config = PaneFlowConfig::default();
        for agent in TerminalAgent::ALL {
            let allowlisted = matches!(
                agent,
                TerminalAgent::ClaudeCode | TerminalAgent::Codex | TerminalAgent::Grok
            );
            assert_eq!(
                agent.is_visible_with(&config, |_| true),
                allowlisted,
                "{} must follow the fresh-config allowlist when installed",
                agent.display_name()
            );
            assert!(
                !agent.is_visible_with(&config, |_| false),
                "{} must stay hidden when it is not installed",
                agent.display_name()
            );
        }
    }

    #[test]
    fn explicit_visibility_does_not_consult_install_detection() {
        let shown = PaneFlowConfig {
            antigravity_button_visible: Some(true),
            ..Default::default()
        };
        assert!(TerminalAgent::Antigravity.is_visible_with(&shown, |_| {
            unreachable!("explicit true must short-circuit install detection")
        }));

        let hidden = PaneFlowConfig {
            claude_code_button_visible: Some(false),
            ..Default::default()
        };
        assert!(!TerminalAgent::ClaudeCode.is_visible_with(&hidden, |_| {
            unreachable!("explicit false must short-circuit install detection")
        }));
    }

    #[test]
    fn icon_paths_are_embedded_assets() {
        // Every icon must live under an embedded asset root (`icons/` or
        // `agents/`) or the tab-bar `svg()` silently renders nothing.
        for agent in TerminalAgent::ALL {
            let p = agent.icon_path();
            assert!(
                p.starts_with("icons/") || p.starts_with("agents/"),
                "{} icon path `{p}` is not under an embedded asset root",
                agent.display_name()
            );
            assert!(
                crate::assets::Assets::get(p).is_some(),
                "{} icon `{p}` is not an embedded asset",
                agent.display_name()
            );
        }
    }

    #[test]
    fn claude_bypass_flag_toggles_command() {
        let off = PaneFlowConfig {
            claude_code_bypass_permissions: Some(false),
            ..Default::default()
        };
        assert_eq!(TerminalAgent::ClaudeCode.command(&off), "claude");
        let on = PaneFlowConfig {
            claude_code_bypass_permissions: Some(true),
            ..Default::default()
        };
        assert_eq!(
            TerminalAgent::ClaudeCode.command(&on),
            "claude --permission-mode bypassPermissions"
        );
    }

    #[test]
    fn non_claude_agents_ignore_bypass() {
        let config = PaneFlowConfig {
            claude_code_bypass_permissions: Some(true),
            ..Default::default()
        };
        assert_eq!(TerminalAgent::Codex.command(&config), "codex");
    }

    #[test]
    fn launch_spec_keeps_program_and_args_structured_until_render() {
        let cfg = PaneFlowConfig {
            claude_code_bypass_permissions: Some(true),
            ..Default::default()
        };

        let spec = TerminalAgent::ClaudeCode.launch_spec(&cfg);

        assert_eq!(spec.program, "claude");
        assert_eq!(spec.args, vec!["--permission-mode", "bypassPermissions"]);
        assert_eq!(
            spec.render_shell_command(),
            "claude --permission-mode bypassPermissions"
        );
    }

    #[test]
    fn launch_spec_plain_token_guard_matches_agent_command_surface() {
        for agent in TerminalAgent::ALL {
            assert!(
                is_plain_shell_token(agent.binary()),
                "{} binary must stay a plain shell token",
                agent.display_name()
            );
            for arg in agent.command_args() {
                assert!(
                    is_plain_shell_token(arg),
                    "{} arg `{arg}` must stay a plain shell token",
                    agent.display_name()
                );
            }
        }
        assert!(is_plain_shell_token(SAMPLE_UUID));
        assert!(!is_plain_shell_token("two words"));
        assert!(!is_plain_shell_token("$(reboot)"));
    }

    const SAMPLE_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn session_agent_maps_only_readable_stores() {
        use crate::agent_sessions::SessionAgent;
        assert_eq!(
            TerminalAgent::ClaudeCode.session_agent(),
            Some(SessionAgent::Claude)
        );
        assert_eq!(
            TerminalAgent::Codex.session_agent(),
            Some(SessionAgent::Codex)
        );
        assert_eq!(
            TerminalAgent::OpenCode.session_agent(),
            Some(SessionAgent::OpenCode)
        );
        assert_eq!(
            TerminalAgent::Grok.session_agent(),
            Some(SessionAgent::Grok)
        );
        assert_eq!(
            TerminalAgent::Cursor.session_agent(),
            Some(SessionAgent::Cursor)
        );
        assert_eq!(TerminalAgent::Antigravity.session_agent(), None);
        assert_eq!(TerminalAgent::Copilot.session_agent(), None);
        assert_eq!(TerminalAgent::Muse.session_agent(), None);
    }

    #[test]
    fn muse_command_is_bare() {
        let cfg = PaneFlowConfig::default();
        assert_eq!(TerminalAgent::Muse.command(&cfg), "muse");
    }

    #[test]
    fn probe_filters_lookup_to_known_agent_binaries() {
        use std::cell::RefCell;

        // Fixture PATH: one real agent binary plus names that are on many
        // hosts but are not agents. Only the agent may come back.
        let installed: HashSet<&str> = ["claude", "node", "python3", "not-an-agent"]
            .into_iter()
            .collect();
        let asked = RefCell::new(Vec::new());
        let found = probe_installed_binaries_with(|bin| {
            asked.borrow_mut().push(bin.to_owned());
            installed.contains(bin)
        });

        let expected: HashSet<&'static str> =
            [TerminalAgent::ClaudeCode.binary()].into_iter().collect();
        assert_eq!(
            found, expected,
            "probe must report exactly the installed agent binaries"
        );
        for unknown in ["node", "python3", "not-an-agent"] {
            assert!(
                !found.contains(unknown),
                "probe leaked non-agent binary {unknown}"
            );
        }
        let mut asked = asked.into_inner();
        asked.sort_unstable();
        let mut candidates: Vec<String> = TerminalAgent::ALL
            .iter()
            .map(|a| a.binary().to_owned())
            .collect();
        candidates.sort_unstable();
        assert_eq!(
            asked, candidates,
            "probe must consult the lookup for every known agent binary"
        );
    }

    #[test]
    fn probe_only_reports_known_agent_binaries() {
        // Live PATH scan: host-dependent, so this can only check the subset
        // property. Say so when there is nothing to check rather than
        // passing silently; the fixture test above covers the filter.
        let found = probe_installed_binaries();
        if found.is_empty() {
            eprintln!(
                "probe_only_reports_known_agent_binaries: no agent binaries on PATH, \
                 subset check is vacuous on this host"
            );
        }
        for bin in found {
            assert!(
                TerminalAgent::ALL.iter().any(|a| a.binary() == bin),
                "probe returned unknown binary {bin}"
            );
        }
    }

    #[test]
    fn fresh_snapshot_is_not_reprobed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let probe_calls = Arc::new(AtomicUsize::new(0));
        let binaries = InstalledBinaries::with_probe(Arc::new({
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                HashSet::from(["claude"])
            }
        }));
        binaries.seed(HashSet::from(["codex"]), Instant::now());

        assert!(binaries.contains("codex"));
        assert!(!binaries.contains("claude"));
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
    }

    /// Issue #518: the first `is_installed` in a process schedules the walk
    /// and answers at once; only the blocking `contains_now` waits for it. The probe
    /// blocks on a channel, so a blocking cold read would hang the test
    /// rather than merely slow it.
    #[test]
    fn cold_contains_does_not_block_while_contains_now_waits_for_the_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let probe_calls = Arc::new(AtomicUsize::new(0));
        let binaries = Arc::new(InstalledBinaries::with_probe(Arc::new({
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                release_rx
                    .lock()
                    .expect("release_rx")
                    .recv_timeout(Duration::from_secs(5))
                    .expect("probe released");
                HashSet::from(["claude"])
            }
        })));

        assert!(binaries.scan_pending(), "a fresh cache is pending");
        let start = Instant::now();
        assert!(
            !binaries.contains("claude"),
            "a cold read answers from the empty snapshot"
        );
        assert!(
            !binaries.contains("claude"),
            "a second cold read still does not wait"
        );
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "cold reads must not wait for the probe"
        );
        assert!(
            binaries.scan_pending(),
            "still pending until the walk publishes"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while probe_calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            1,
            "one walk is scheduled, not one per read"
        );

        let waiter = std::thread::spawn({
            let binaries = Arc::clone(&binaries);
            move || binaries.contains_now("claude")
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !waiter.is_finished(),
            "contains_now must wait for the probe"
        );

        release_tx.send(()).expect("release");
        assert!(waiter.join().expect("waiter thread"));
        assert!(!binaries.scan_pending());
        assert!(binaries.contains("claude"));
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            1,
            "a fresh snapshot must not schedule another walk"
        );
    }

    /// The boot warm (`refresh_installed_binaries`) runs the walk on the
    /// caller's thread - `smol::unblock`, never GPUI - and a cold read on
    /// any other thread never walks itself: the probe only ever runs on
    /// `paneflow-agent-which` or the warming thread.
    #[test]
    fn cold_reads_never_run_the_probe_on_the_caller_thread() {
        let probe_threads = Arc::new(Mutex::new(Vec::<std::thread::ThreadId>::new()));
        let binaries = Arc::new(InstalledBinaries::with_probe(Arc::new({
            let probe_threads = Arc::clone(&probe_threads);
            move || {
                probe_threads
                    .lock()
                    .expect("probe_threads")
                    .push(std::thread::current().id());
                std::thread::sleep(Duration::from_millis(10));
                HashSet::from(["claude"])
            }
        })));

        let caller = std::thread::current().id();
        assert!(!binaries.contains("claude"));
        assert!(binaries.contains_now("claude"));
        let threads = probe_threads.lock().expect("probe_threads").clone();
        assert_eq!(threads.len(), 1, "one walk for the cold read");
        assert_ne!(threads[0], caller, "the walk must not run on the reader");

        // A warm from another thread while the snapshot is fresh reads it
        // and walks nothing; once stale it walks on that thread only.
        let warm_on_thread = |binaries: &Arc<InstalledBinaries>| {
            let binaries = Arc::clone(binaries);
            std::thread::spawn(move || {
                binaries.warm();
                std::thread::current().id()
            })
            .join()
            .expect("warmer")
        };
        warm_on_thread(&binaries);
        assert_eq!(probe_threads.lock().expect("probe_threads").len(), 1);

        binaries.seed(
            HashSet::from(["claude"]),
            Instant::now() - INSTALLED_BINARIES_TTL - Duration::from_millis(1),
        );
        let warmer_id = warm_on_thread(&binaries);
        let threads = probe_threads.lock().expect("probe_threads").clone();
        assert_eq!(threads.len(), 2, "a stale warm walks once");
        assert_eq!(
            threads[1], warmer_id,
            "the warm walks on the warming thread"
        );
        assert_ne!(threads[1], caller);
    }

    /// Upstream df375ba5: once the snapshot is published, reads are a hash
    /// lookup and never touch PATH.
    #[test]
    fn installed_binary_reads_never_scan_on_the_caller_thread_once_warm() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let probe_calls = Arc::new(AtomicUsize::new(0));
        let binaries = InstalledBinaries::with_probe(Arc::new({
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(200));
                HashSet::new()
            }
        }));
        binaries.seed(HashSet::from(["claude"]), Instant::now());
        assert!(!binaries.scan_pending());

        let started = Instant::now();
        for _ in 0..1_000 {
            assert!(binaries.contains("claude"));
            assert!(!binaries.contains("paneflow-no-such-agent-binary"));
            assert!(binaries.contains_now("claude"));
        }
        // The probe sleeps 200 ms, so anything under that proves no walk ran
        // on this thread; the generous bound survives a loaded test run.
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "warm reads must not walk PATH"
        );
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_cold_lookups_share_one_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let probe_calls = Arc::new(AtomicUsize::new(0));
        let binaries = Arc::new(InstalledBinaries::with_probe(Arc::new({
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(40));
                HashSet::from(["claude"])
            }
        })));

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let binaries = Arc::clone(&binaries);
                std::thread::spawn(move || binaries.contains_now("claude"))
            })
            .collect();
        for thread in threads {
            assert!(thread.join().expect("lookup thread"));
        }
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
    }

    /// A warm that finds a cold walk already in flight waits for it instead
    /// of starting a second one, so boot and an early render share one walk.
    #[test]
    fn warm_joins_an_in_flight_cold_walk() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let probe_calls = Arc::new(AtomicUsize::new(0));
        let binaries = Arc::new(InstalledBinaries::with_probe(Arc::new({
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(40));
                HashSet::from(["claude"])
            }
        })));

        assert!(!binaries.contains("claude"));
        binaries.warm();
        assert!(!binaries.scan_pending());
        assert!(binaries.contains("claude"));
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_refresh_does_not_block_contains_or_hold_the_cache_mutex() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let in_probe = Arc::new((Mutex::new(false), Condvar::new()));
        let probe_calls = Arc::new(AtomicUsize::new(0));

        let binaries = InstalledBinaries::with_probe(Arc::new({
            let release = Arc::clone(&release);
            let in_probe = Arc::clone(&in_probe);
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                {
                    let mut entered = in_probe.0.lock().expect("in_probe");
                    *entered = true;
                    in_probe.1.notify_all();
                }
                let mut released = release.0.lock().expect("release");
                while !*released {
                    released = release.1.wait(released).expect("release wait");
                }
                HashSet::from(["codex"])
            }
        }));

        binaries.seed(
            HashSet::from(["claude"]),
            Instant::now() - INSTALLED_BINARIES_TTL - Duration::from_millis(1),
        );

        let start = Instant::now();
        assert!(
            binaries.contains("claude"),
            "stale lookup must serve the snapshot"
        );
        assert!(
            !binaries.contains("codex"),
            "stale lookup must not wait for the in-flight probe"
        );
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "render-path contains must not block on which"
        );

        {
            let mut entered = in_probe.0.lock().expect("in_probe");
            let deadline = Duration::from_secs(2);
            let start_wait = Instant::now();
            while !*entered {
                let remaining = deadline.saturating_sub(start_wait.elapsed());
                assert!(
                    !remaining.is_zero(),
                    "probe thread never entered which-equivalent work"
                );
                let (guard, result) = in_probe
                    .1
                    .wait_timeout(entered, remaining)
                    .expect("in_probe wait");
                entered = guard;
                assert!(
                    !result.timed_out() || *entered,
                    "probe thread never entered which-equivalent work"
                );
            }
        }
        assert!(
            binaries.cache_mutex_is_free(),
            "which must not run under the cache mutex"
        );

        {
            let mut released = release.0.lock().expect("release");
            *released = true;
            release.1.notify_all();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if binaries.contains("codex") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            binaries.contains("codex"),
            "snapshot must publish once the off-thread probe finishes"
        );
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn is_installed_reads_without_panicking() {
        let _ = TerminalAgent::ClaudeCode.is_installed();
        let _ = TerminalAgent::Codex.is_installed();
        let _ = installed_binary_scan_pending();
        // The process-wide cache: the warm must return with the first
        // snapshot published.
        refresh_installed_binaries();
        assert!(!installed_binary_scan_pending());
    }

    /// Issue #905: one PATH directory on a dead mount must not hold the
    /// scan, the boot warm, or a launch queued during that scan. The hook
    /// blocks in one directory; the warm returns inside the deadline with
    /// the in-flight flag clear, and the queued launch is replayed while
    /// that directory is still blocked.
    #[test]
    fn installed_agent_probe_finishes_when_a_path_dir_stalls() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        const PATH_DIR_STALL: Duration = Duration::from_secs(3);
        let bound = Duration::from_secs(1);
        assert!(
            INSTALLED_AGENT_PROBE_TIMEOUT < bound,
            "the deadline must sit well under the stalled directory"
        );

        let healthy = tempfile::tempdir().expect("temp dir");
        let claude = healthy.path().join("claude");
        std::fs::write(&claude, "#!/bin/sh\n").expect("write claude");
        let mut perms = std::fs::metadata(&claude).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&claude, perms).expect("chmod");
        let stalled = healthy.path().join("stalled-mount");

        let stall_finished = Arc::new(AtomicBool::new(false));
        let hook_finished = Arc::clone(&stall_finished);
        let stalled_hook = stalled.clone();

        struct ClearPathDirProbeHook;
        impl Drop for ClearPathDirProbeHook {
            fn drop(&mut self) {
                install_path_dir_probe_hook(None);
            }
        }
        let _clear_hook = ClearPathDirProbeHook;
        install_path_dir_probe_hook(Some(Arc::new(move |dir| {
            if dir == stalled_hook.as_path() {
                std::thread::sleep(PATH_DIR_STALL);
                hook_finished.store(true, Ordering::SeqCst);
            }
        })));

        // Stalled entry first, the installed binary after it: a walk that
        // blocks inside the first directory never sees `claude`.
        let dirs = vec![stalled, healthy.path().to_path_buf()];
        let binaries = InstalledBinaries::with_probe(Arc::new(move || probe_agents_on_path(&dirs)));

        assert!(
            binaries.scan_pending(),
            "a launch confirmed now is waiting on the first scan"
        );
        // Boot takes the queued launch only after `refresh_installed_binaries`
        // returns (`pane_palette_resume_queued_launch`).
        let mut queued = Some("claude");

        let started = Instant::now();
        binaries.warm();
        let replayed = queued.take();
        let elapsed = started.elapsed();

        assert!(
            elapsed < bound,
            "stalled PATH directory held the scan for {elapsed:?}; bound is {bound:?}"
        );
        assert!(
            !binaries.refresh_in_flight(),
            "the deadline must clear refresh_in_flight so a later refresh can run"
        );
        assert!(
            !binaries.scan_pending(),
            "the scan must publish on the deadline instead of staying on 'looking'"
        );
        assert_eq!(
            replayed,
            Some("claude"),
            "a launch queued during the scan must be replayed when the warm returns"
        );
        assert!(
            !stall_finished.load(Ordering::SeqCst),
            "the warm returned while the PATH directory was still stalled"
        );
        assert!(
            binaries.contains("claude"),
            "a directory that answered is kept when another directory stalls"
        );

        binaries.seed(
            HashSet::from(["claude"]),
            Instant::now() - INSTALLED_BINARIES_TTL - Duration::from_millis(1),
        );
        let again = Instant::now();
        binaries.warm();
        assert!(
            again.elapsed() < bound,
            "a later refresh waited on the stalled directory for {:?}",
            again.elapsed()
        );
        assert!(
            !binaries.refresh_in_flight(),
            "the later refresh must clear refresh_in_flight too"
        );
        assert!(
            binaries.contains("claude"),
            "the later refresh still sees a binary the stalled directory did not hide"
        );
        assert!(
            !stall_finished.load(Ordering::SeqCst),
            "the later refresh must not wait out the stall"
        );
    }
}
