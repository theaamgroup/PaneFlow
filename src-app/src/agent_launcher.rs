//! Terminal-agent launcher: the CLI coding agents Paneflow starts in a
//! terminal pane (Claude Code, Codex, OpenCode, Pi, Hermes, plus the
//! cmux-derived set: Grok, Amp, Cursor, Gemini, Kiro, Antigravity,
//! Copilot, CodeBuddy, Factory, Qoder, plus Openclaw). Both the tab-bar
//! launcher buttons
//! (`pane.rs`) and the launch pad iterate this single
//! source of truth so the per-agent visibility gate and the "respect
//! bypass" contract can never drift between them.
//!
//! Each variant maps to a display name, an icon, an accent tint, a
//! Settings → AI Agent visibility flag (`*_button_visible`), a stable
//! persistence tag, and a launch command. The launch command honors
//! `claude_code_bypass_permissions` exactly as the tab bar does.

use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use paneflow_config::schema::PaneFlowConfig;

/// One of the CLI coding agents Paneflow can launch in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalAgent {
    ClaudeCode,
    Codex,
    OpenCode,
    Pi,
    Hermes,
    Grok,
    Amp,
    Cursor,
    Gemini,
    Kiro,
    Antigravity,
    Copilot,
    CodeBuddy,
    Factory,
    Qoder,
    Openclaw,
}

impl TerminalAgent {
    /// Every variant, in display order (matches the tab-bar button row).
    /// The original five lead; the cmux-derived launchers follow so the
    /// button order is stable for users who upgraded from a 5-agent build.
    pub const ALL: [TerminalAgent; 16] = [
        TerminalAgent::ClaudeCode,
        TerminalAgent::Codex,
        TerminalAgent::OpenCode,
        TerminalAgent::Pi,
        TerminalAgent::Hermes,
        TerminalAgent::Grok,
        TerminalAgent::Amp,
        TerminalAgent::Cursor,
        TerminalAgent::Gemini,
        TerminalAgent::Kiro,
        TerminalAgent::Antigravity,
        TerminalAgent::Copilot,
        TerminalAgent::CodeBuddy,
        TerminalAgent::Factory,
        TerminalAgent::Qoder,
        TerminalAgent::Openclaw,
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
            TerminalAgent::Pi => "Pi",
            TerminalAgent::Hermes => "Hermes Agent",
            TerminalAgent::Grok => "Grok",
            TerminalAgent::Amp => "Amp",
            TerminalAgent::Cursor => "Cursor",
            TerminalAgent::Gemini => "Gemini",
            TerminalAgent::Kiro => "Kiro",
            TerminalAgent::Antigravity => "Antigravity",
            TerminalAgent::Copilot => "Copilot",
            TerminalAgent::CodeBuddy => "CodeBuddy",
            TerminalAgent::Factory => "Factory",
            TerminalAgent::Qoder => "Qoder",
            TerminalAgent::Openclaw => "Openclaw",
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "icons/claude-color.svg",
            TerminalAgent::Codex => "icons/codex.svg",
            TerminalAgent::OpenCode => "icons/opencode-color.svg",
            TerminalAgent::Pi => "icons/pi-coding-agent.svg",
            TerminalAgent::Hermes => "icons/hermesagent.svg",
            TerminalAgent::Grok => "agents/grok.svg",
            TerminalAgent::Amp => "agents/amp-color.svg",
            TerminalAgent::Cursor => "agents/cursor.svg",
            TerminalAgent::Gemini => "agents/gemini-color.svg",
            TerminalAgent::Kiro => "agents/kiro-color.svg",
            TerminalAgent::Antigravity => "agents/antigravity-color.svg",
            TerminalAgent::Copilot => "agents/githubcopilot.svg",
            TerminalAgent::CodeBuddy => "agents/codebuddy-color.svg",
            TerminalAgent::Factory => "agents/factory.svg",
            TerminalAgent::Qoder => "agents/qoder-color.svg",
            TerminalAgent::Openclaw => "agents/openclaw-color.svg",
        }
    }

    /// Brand accent for the icon tint, as a packed `0xRRGGBB`. `None`
    /// means "use the theme's primary text color" -- the OpenCode / Pi /
    /// Hermes logos are monochrome `currentColor` SVGs (so is Codex, which
    /// carries the OpenAI blossom mark).
    pub fn accent(self) -> Option<u32> {
        match self {
            TerminalAgent::ClaudeCode => Some(0xd97757),
            // Single-color brand logos: `svg()` renders a monochrome alpha
            // mask, so the silhouette is painted in this brand color.
            TerminalAgent::Amp => Some(0xF34E3F),
            TerminalAgent::Qoder => Some(0x2ADB5C),
            // The rest are either monochrome `currentColor` logos (tinted
            // with the theme's primary text color so they stay readable on
            // every theme) or multi-color logos rendered in their native
            // palette via `img()` (see `icon_multicolor`), where `accent`
            // is unused.
            TerminalAgent::Codex
            | TerminalAgent::OpenCode
            | TerminalAgent::Pi
            | TerminalAgent::Hermes
            | TerminalAgent::Grok
            | TerminalAgent::Cursor
            | TerminalAgent::Gemini
            | TerminalAgent::Kiro
            | TerminalAgent::Antigravity
            | TerminalAgent::Copilot
            | TerminalAgent::CodeBuddy
            | TerminalAgent::Factory
            | TerminalAgent::Openclaw => None,
        }
    }

    /// Whether the icon must be rendered in its native colors via `img()`
    /// (multi-color logos: gradients or several distinct fills) instead of
    /// a `text_color`-tinted monochrome `svg()` mask. GPUI's `svg()`
    /// flattens every path to one tint, which would destroy these palettes;
    /// `img()` rasterizes the SVG (resvg) and preserves every fill. A
    /// single-color brand logo stays monochrome and uses `accent()`.
    pub fn icon_multicolor(self) -> bool {
        matches!(
            self,
            TerminalAgent::Antigravity
                | TerminalAgent::CodeBuddy
                | TerminalAgent::Gemini
                | TerminalAgent::Kiro
                | TerminalAgent::Openclaw
        )
    }

    /// Stable persistence tag for the session.json `terminal_agent`
    /// field. Kept distinct from the binary name so a future rename of
    /// the CLI does not invalidate persisted threads.
    pub fn tag(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "claude_code",
            TerminalAgent::Codex => "codex",
            TerminalAgent::OpenCode => "opencode",
            TerminalAgent::Pi => "pi",
            TerminalAgent::Hermes => "hermes",
            TerminalAgent::Grok => "grok",
            TerminalAgent::Amp => "amp",
            TerminalAgent::Cursor => "cursor",
            TerminalAgent::Gemini => "gemini",
            TerminalAgent::Kiro => "kiro",
            TerminalAgent::Antigravity => "antigravity",
            TerminalAgent::Copilot => "copilot",
            TerminalAgent::CodeBuddy => "codebuddy",
            TerminalAgent::Factory => "factory",
            TerminalAgent::Qoder => "qoder",
            TerminalAgent::Openclaw => "openclaw",
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
            "pi" => Some(TerminalAgent::Pi),
            "hermes" => Some(TerminalAgent::Hermes),
            "grok" => Some(TerminalAgent::Grok),
            "amp" => Some(TerminalAgent::Amp),
            "cursor" => Some(TerminalAgent::Cursor),
            "gemini" => Some(TerminalAgent::Gemini),
            "kiro" => Some(TerminalAgent::Kiro),
            "antigravity" => Some(TerminalAgent::Antigravity),
            "copilot" => Some(TerminalAgent::Copilot),
            "codebuddy" => Some(TerminalAgent::CodeBuddy),
            "factory" => Some(TerminalAgent::Factory),
            "qoder" => Some(TerminalAgent::Qoder),
            "openclaw" => Some(TerminalAgent::Openclaw),
            _ => None,
        }
    }

    /// Whether this launcher is shown in the tab bar / launch pad.
    ///
    /// Tri-state on the `*_button_visible` config key:
    /// - `Some(true)`  - user explicitly enabled it: always shown.
    /// - `Some(false)` - user explicitly disabled it: always hidden.
    /// - `None` (key absent, the default) - shown only if the agent's CLI
    ///   binary is installed ([`Self::is_installed`]), so a fresh config
    ///   surfaces exactly the agents present on the machine. The user can
    ///   still force-show an uninstalled agent by toggling it on.
    pub fn is_visible(self, config: &PaneFlowConfig) -> bool {
        let explicit: Option<bool> = match self {
            TerminalAgent::ClaudeCode => config.claude_code_button_visible,
            TerminalAgent::Codex => config.codex_button_visible,
            TerminalAgent::OpenCode => config.opencode_button_visible,
            TerminalAgent::Pi => config.pi_button_visible,
            TerminalAgent::Hermes => config.hermes_agent_button_visible,
            TerminalAgent::Grok => config.grok_button_visible,
            TerminalAgent::Amp => config.amp_button_visible,
            TerminalAgent::Cursor => config.cursor_button_visible,
            TerminalAgent::Gemini => config.gemini_button_visible,
            TerminalAgent::Kiro => config.kiro_button_visible,
            TerminalAgent::Antigravity => config.antigravity_button_visible,
            TerminalAgent::Copilot => config.copilot_button_visible,
            TerminalAgent::CodeBuddy => config.codebuddy_button_visible,
            TerminalAgent::Factory => config.factory_button_visible,
            TerminalAgent::Qoder => config.qoder_button_visible,
            TerminalAgent::Openclaw => config.openclaw_button_visible,
        };
        explicit.unwrap_or_else(|| self.is_installed())
    }

    /// The CLI executable looked up on `PATH` to decide default visibility;
    /// also the leading token of [`Self::launch_command`].
    pub fn binary(self) -> &'static str {
        match self {
            TerminalAgent::ClaudeCode => "claude",
            TerminalAgent::Codex => "codex",
            TerminalAgent::OpenCode => "opencode",
            TerminalAgent::Pi => "pi",
            TerminalAgent::Hermes => "hermes",
            TerminalAgent::Grok => "grok",
            TerminalAgent::Amp => "amp",
            TerminalAgent::Cursor => "cursor-agent",
            TerminalAgent::Gemini => "gemini",
            TerminalAgent::Kiro => "kiro-cli",
            TerminalAgent::Antigravity => "agy",
            TerminalAgent::Copilot => "copilot",
            TerminalAgent::CodeBuddy => "codebuddy",
            TerminalAgent::Factory => "droid",
            TerminalAgent::Qoder => "qodercli",
            TerminalAgent::Openclaw => "openclaw",
        }
    }

    /// Whether this agent's CLI binary is found on `PATH`. Drives the
    /// default visibility in [`Self::is_visible`].
    ///
    /// `which` walks `PATH` off-thread. Render (and every other caller with a
    /// snapshot already in hand) reads that snapshot and never waits on the
    /// walk: a TTL miss schedules `paneflow-agent-which` and returns the last
    /// answer. The first lookup in a process waits for that thread so CLI
    /// PATH checks (`paneflow up`) cannot race an empty cache. The cache
    /// mutex is never held across `which`.
    pub fn is_installed(self) -> bool {
        installed_binaries_contains(self.binary())
    }

    /// Static arguments appended after [`Self::binary`] for interactive agents
    /// whose CLI entry point is a subcommand rather than the bare executable.
    fn command_args(self) -> &'static [&'static str] {
        match self {
            TerminalAgent::Kiro => &["chat"],
            TerminalAgent::Openclaw => &["tui"],
            _ => &[],
        }
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
            TerminalAgent::Pi => Some(SessionAgent::Pi),
            TerminalAgent::Hermes => Some(SessionAgent::Hermes),
            TerminalAgent::Grok => Some(SessionAgent::Grok),
            TerminalAgent::Cursor => Some(SessionAgent::Cursor),
            TerminalAgent::Gemini => Some(SessionAgent::Gemini),
            TerminalAgent::Kiro => Some(SessionAgent::Kiro),
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
    /// both the launch pad and (via the same gates) the tab bar.
    pub fn visible(config: &PaneFlowConfig) -> Vec<TerminalAgent> {
        TerminalAgent::ALL
            .into_iter()
            .filter(|a| a.is_visible(config))
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

    fn contains(&self, binary: &'static str) -> bool {
        let (snapshot, spawn, wait_for_initial) = {
            let mut cache = self.inner.lock_cache();
            let spawn = cache.is_stale() && !cache.refresh_in_flight;
            if spawn {
                cache.refresh_in_flight = true;
            }
            let wait_for_initial = cache.checked_at.is_none();
            (cache.found.clone(), spawn, wait_for_initial)
        };

        if spawn {
            self.spawn_refresh();
        }

        if wait_for_initial {
            self.inner.wait_for_initial();
            return self.inner.lock_cache().found.contains(binary);
        }

        snapshot.contains(binary)
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
                    "failed to spawn installed-binary probe thread; probing on caller"
                );
                // Last resort: still never hold the cache mutex across which.
                self.inner.run_refresh();
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
}

fn probe_installed_binaries() -> HashSet<&'static str> {
    TerminalAgent::ALL
        .into_iter()
        .map(TerminalAgent::binary)
        .filter(|bin| which::which(bin).is_ok())
        .collect()
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
            gemini_button_visible: Some(true),
            ..Default::default()
        };
        assert!(TerminalAgent::Gemini.is_visible(&shown));

        let hidden = PaneFlowConfig {
            gemini_button_visible: Some(false),
            ..Default::default()
        };
        assert!(!TerminalAgent::Gemini.is_visible(&hidden));
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
        assert_eq!(TerminalAgent::Pi.command(&config), "pi");
        assert_eq!(TerminalAgent::Hermes.command(&config), "hermes");
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
        assert_eq!(TerminalAgent::Pi.session_agent(), Some(SessionAgent::Pi));
        assert_eq!(
            TerminalAgent::Hermes.session_agent(),
            Some(SessionAgent::Hermes)
        );
        assert_eq!(
            TerminalAgent::Grok.session_agent(),
            Some(SessionAgent::Grok)
        );
        assert_eq!(
            TerminalAgent::Cursor.session_agent(),
            Some(SessionAgent::Cursor)
        );
        assert_eq!(
            TerminalAgent::Gemini.session_agent(),
            Some(SessionAgent::Gemini)
        );
        assert_eq!(
            TerminalAgent::Kiro.session_agent(),
            Some(SessionAgent::Kiro)
        );
        assert_eq!(TerminalAgent::Amp.session_agent(), None);
        assert_eq!(TerminalAgent::Antigravity.session_agent(), None);
        assert_eq!(TerminalAgent::Copilot.session_agent(), None);
        assert_eq!(TerminalAgent::CodeBuddy.session_agent(), None);
        assert_eq!(TerminalAgent::Factory.session_agent(), None);
        assert_eq!(TerminalAgent::Qoder.session_agent(), None);
        assert_eq!(TerminalAgent::Openclaw.session_agent(), None);
    }

    #[test]
    fn bare_commands_preserve_multi_token_agent_commands() {
        let cfg = PaneFlowConfig::default();
        assert_eq!(TerminalAgent::Kiro.command(&cfg), "kiro-cli chat");
        assert_eq!(TerminalAgent::Openclaw.command(&cfg), "openclaw tui");
    }

    #[test]
    fn probe_only_reports_known_agent_binaries() {
        for bin in probe_installed_binaries() {
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

    #[test]
    fn first_contains_waits_for_initial_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let probe_calls = Arc::new(AtomicUsize::new(0));
        let binaries = InstalledBinaries::with_probe(Arc::new({
            let probe_calls = Arc::clone(&probe_calls);
            move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                HashSet::from(["claude"])
            }
        }));

        let start = Instant::now();
        assert!(binaries.contains("claude"));
        assert!(
            start.elapsed() >= Duration::from_millis(30),
            "cold lookup must wait for the off-thread probe"
        );
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
        assert!(binaries.contains("claude"));
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            1,
            "a fresh snapshot must not schedule another walk"
        );
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
                std::thread::spawn(move || binaries.contains("claude"))
            })
            .collect();
        for thread in threads {
            assert!(thread.join().expect("lookup thread"));
        }
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
    }
}
