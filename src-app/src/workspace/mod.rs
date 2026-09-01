//! Workspace - a named collection of terminal panes with a split layout.
//!
//! Module layout (US-030 of the src-app refactor PRD):
//! - [`git`] - git metadata probing (branch, diff stats, `.git` dir lookup)
//! - [`ports`] - cross-platform TCP listening-port detection
//!
//! The [`Workspace`] struct and its constructors live in this `mod.rs`; git
//! and port helpers are re-exported so external callers keep the flat
//! `crate::workspace::*` API.

mod git;
pub mod pid_resolve;
mod ports;
pub mod surface_naming;
mod tab;
pub mod worktree;

static RETIRING_WORKTREE_PATHS: std::sync::LazyLock<std::sync::RwLock<Vec<std::path::PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(Vec::new()));

/// Publish the durable retirement journal to terminal-producing UI that does
/// not own a `PaneFlowApp` reference (notably embedded Diff Review views).
pub(crate) fn set_retiring_worktree_paths(paths: Vec<std::path::PathBuf>) {
    match RETIRING_WORKTREE_PATHS.write() {
        Ok(mut retiring) => *retiring = paths,
        Err(poisoned) => *poisoned.into_inner() = paths,
    }
}

/// Read-only ingress gate for off-tree terminal producers.
pub(crate) fn path_is_in_retiring_worktree(path: &std::path::Path) -> bool {
    let retiring = RETIRING_WORKTREE_PATHS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retiring.iter().any(|root| {
        path.starts_with(root)
            || path
                .canonicalize()
                .ok()
                .is_some_and(|resolved| resolved.starts_with(root))
    })
}

pub use git::{GitDiffStats, detect_branch, find_git_dir, resolve_repo_root};
#[cfg(test)]
pub(crate) use ports::PortEntry;
pub use ports::{PaneScan, scan_panes};
pub use tab::Tab;
pub(crate) use tab::apply_pane_rename_to_tab;

/// Hard cap on open workspaces (US-054: single source for the bound previously
/// re-declared as a local `const` at every create/IPC site).
pub(crate) const MAX_WORKSPACES: usize = 20;

/// Hard cap on tabs inside a single workspace (US-001, prd-cli-tab-hierarchy).
/// Declared next to [`MAX_WORKSPACES`] and exported by the same path so every
/// create site shares one value, as `MAX_PANES` already does for leaves.
// Enforced by `Workspace::open_tab`; the UI create sites land with US-010.
pub(crate) const MAX_TABS_PER_WORKSPACE: usize = 32;

use gpui::{App, Entity, Window};
use paneflow_config::schema::{ButtonCommand, LayoutNode, TabSession};

use crate::ai_types::AgentSession;
use crate::launch_cwd;
use crate::layout::LayoutTree;
use crate::pane::Pane;

use self::git::parse_head;

/// Monotonic workspace ID counter. Each workspace gets a unique ID at construction.
static NEXT_WORKSPACE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_workspace_id() -> u64 {
    NEXT_WORKSPACE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Runtime-only notification state for a completed agent turn.
///
/// A natural `ai.stop` marks the completion unread only while this workspace is
/// not visible in the active Paneflow window. It survives the transient
/// `AgentState::Finished` session auto-clear until the user interacts with the
/// workspace card or its pane area.
#[derive(Debug, Default)]
pub(crate) struct AgentCompletionNotification {
    unread: bool,
}

impl AgentCompletionNotification {
    pub(crate) fn record_finished(&mut self, workspace_visible: bool) {
        self.unread = !workspace_visible;
    }

    pub(crate) fn acknowledge(&mut self) {
        self.unread = false;
    }

    pub(crate) fn is_unread(&self) -> bool {
        self.unread
    }
}

pub struct Workspace {
    /// Unique workspace identifier, assigned at construction.
    pub id: u64,
    pub title: String,
    /// Working directory at creation time. Does not update when the shell `cd`s.
    pub cwd: String,
    /// Working compositions of this workspace. Each tab owns the layout tree
    /// (and the zoom `saved_layout`) the workspace used to own directly.
    ///
    /// Invariant (FR-01): never empty. The field is private so no caller can
    /// drain it; [`Workspace::close_tab`] substitutes an empty tab rather than
    /// leaving zero.
    tabs: Vec<Tab>,
    /// Index into [`Workspace::tabs`] of the visible tab. Kept in range by
    /// every mutation; readers go through [`Workspace::active_tab`].
    active_tab_idx: usize,
    /// Cached git diff stats, refreshed by a background poller.
    pub git_stats: GitDiffStats,
    /// Current git branch name. Empty string when not a git repo or branch unknown.
    pub git_branch: String,
    /// Whether this workspace's CWD is inside a git repository.
    pub is_git_repo: bool,
    /// Resolved `.git` directory path (for file watcher). `None` if not a git repo.
    pub git_dir: Option<std::path::PathBuf>,
    /// Working directory of the shared repository (parent of the *main* `.git`),
    /// canonicalized. Sibling worktrees of one repo share an identical value -
    /// the invariant the sidebar uses to group them. `None` when not a git repo.
    pub repo_root: Option<std::path::PathBuf>,
    /// Whether this workspace's CWD is a *linked* git worktree (as opposed to
    /// the repo's main checkout). Linked worktrees carry a `commondir` file.
    // Read by EP-002 (US-005) to target git operations at the worktree root and
    // by EP-004 column labeling; stored at construction in EP-001 (US-001).
    #[allow(dead_code)]
    pub is_worktree: bool,
    /// Concrete worktree checkout root resolved at workspace construction.
    /// Review UI reads this directly so rebuilding columns stays in-memory.
    pub worktree_root: std::path::PathBuf,
    /// Active TCP listening ports from workspace terminal processes.
    pub active_ports: Vec<u16>,
    /// Generation counter for event-driven port scans - the cancellation
    /// belt for workspace close/reuse (superseded scans check it to abort).
    pub port_scan_generation: u64,
    /// True while a scan ladder (debounce + retries) is in flight for this
    /// workspace - ActivityBursts arriving meanwhile are absorbed instead of
    /// superseding the pending scan (under sustained output, the old
    /// generation-bump-per-burst starved the 500ms debounce indefinitely).
    pub port_scan_pending: bool,
    /// Service metadata for `active_ports` chips, fed from BOTH sides:
    /// OS-side argv classification (authoritative for `is_frontend`, with a
    /// synthesized localhost URL) and PTY-output detection (enrichment -
    /// exact URL with path, backend labels). Keyed by port number; pruned
    /// when ports are removed from `active_ports`.
    pub service_labels: std::collections::HashMap<u16, crate::terminal::ServiceInfo>,
    /// Registered AI agent sessions for this workspace, keyed by PID. A
    /// workspace can hold many concurrent sessions (e.g., two Claude
    /// Codes + one Codex) - the sidebar aggregates them per tool with
    /// `ai_types::aggregate_by_tool`. Cleaned up by the stale-PID sweep
    /// in `event_handlers::sweep_stale_pids`.
    pub agent_sessions: std::collections::HashMap<u32, AgentSession>,
    /// Persistent-in-session completion notification shown as a blue dot in
    /// the Workspaces sidebar until the user interacts with this workspace.
    pub(crate) agent_completion_notification: AgentCompletionNotification,
    /// AI agent process basenames detected by walking the workspace's
    /// PTY descendants (Linux `/proc/<pid>/comm`, macOS `libproc::name`).
    /// Independent of the optional IPC hook handshake -- this is what
    /// the sidebar pastille reads so the "session active" signal works
    /// even when Claude Code is launched without the Paneflow shim.
    /// Refreshed by the per-pane `scan_panes` walk (EP-005 US-012) - the
    /// union of every pane's detected agents; the recognition vocabulary
    /// is `TerminalAgent::ALL` binaries (16), unified from the historical
    /// 3-name `AI_PROCESS_NAMES` list.
    pub detected_agents: std::collections::HashSet<String>,
    /// User-defined New pane palette buttons for this workspace.
    /// Rendered after the 2 built-in defaults (Claude / Codex).
    pub custom_buttons: Vec<ButtonCommand>,
    /// Absolute directory paths expanded in the Files tree sidebar, held
    /// per-workspace so reopening the sidebar (within a session or after a
    /// restart) restores the same expansion (PRD files-tree US-007). Excludes
    /// the implicit root. Persisted as workspace-relative paths in
    /// `session.json`; the sidebar's visibility itself is never persisted.
    pub files_expanded: Vec<std::path::PathBuf>,
    /// Git worktrees Paneflow created for this workspace's panes via
    /// `paneflow up` (`worktree = "branch"`, EP-002 orchestration-v2). Torn
    /// down - clean ones only, branch never deleted - when the workspace
    /// closes; persisted in `session.json` so a crash keeps the ownership
    /// record. Empty for every workspace not built by `up` with worktrees.
    pub managed_worktrees: Vec<worktree::ManagedWorktree>,
    /// US-008: whether the sidebar folder row for this workspace shows its
    /// tab children. Session-only, exactly like the Files sidebar expansion
    /// state - it is never written to `session.json`, so a restart starts
    /// every workspace expanded.
    pub sidebar_expanded: bool,
    /// Issue #107: whether the user pinned this workspace to the top of the
    /// sidebar. Unlike [`Workspace::sidebar_expanded`] this IS persisted (as
    /// `WorkspaceSession::pinned`) - a pin is a deliberate choice about a
    /// project, not a transient view state, so it has to outlive a quit.
    ///
    /// Only the Auto ordering reads it; under Manual ordering the rail keeps
    /// storage order and the pin is inert (the star still renders, so the
    /// state is never invisible).
    pub pinned: bool,
}

/// The pure half of [`Workspace::is_idle`]: a workspace is idle when every
/// terminal's foreground command is unknown (`None`, the scanner has not
/// answered) or an interactive shell sitting at a prompt. An empty slice - a
/// workspace with no terminal - is idle.
///
/// Split out because [`Workspace`]'s constructors resolve git metadata off
/// disk, so the truth table cannot be exercised through a real workspace.
pub(crate) fn commands_are_idle(commands: &[Option<String>]) -> bool {
    commands.iter().all(|command| match command {
        None => true,
        Some(command) => surface_naming::is_shell_command(command),
    })
}

impl Workspace {
    /// US-013: shared private factory for the three public constructors (kills
    /// the verbatim triplication). Resolves the *cheap* git metadata - `.git`
    /// dir, branch (`parse_head`), repo root - synchronously, since those are
    /// direct `.git/HEAD` file reads, not subprocesses. `git_stats` is left at
    /// its `default()` (0/0): the `git diff --shortstat` subprocess is the
    /// blocking call, deferred off the render thread by
    /// [`crate::PaneFlowApp::spawn_initial_git_stats`] right after creation.
    fn build(id: u64, title: String, cwd: String, root: LayoutTree) -> Self {
        Self::build_with_tab(id, title, cwd, Tab::new(String::new(), Some(root)))
    }

    /// Same factory, one level lower: it takes the workspace's single starting
    /// tab instead of a layout tree, so a workspace can also be born empty
    /// (`Tab::empty()`) without duplicating the git-metadata resolution.
    fn build_with_tab(id: u64, title: String, cwd: String, tab: Tab) -> Self {
        let git_dir = find_git_dir(&cwd);
        let (git_branch, is_git_repo) = match &git_dir {
            Some(dir) => parse_head(dir),
            None => (String::new(), false),
        };
        let (repo_root, is_worktree) = match &git_dir {
            Some(dir) => resolve_repo_root(dir),
            None => (None, false),
        };
        let worktree_root =
            git::resolve_worktree_root(&cwd, git_dir.as_deref(), repo_root.as_deref(), is_worktree);
        Self {
            id,
            title,
            cwd,
            tabs: vec![tab],
            active_tab_idx: 0,
            git_stats: GitDiffStats::default(),
            git_branch,
            is_git_repo,
            git_dir,
            repo_root,
            is_worktree,
            worktree_root,
            active_ports: vec![],
            port_scan_generation: 0,
            port_scan_pending: false,
            service_labels: std::collections::HashMap::new(),
            agent_sessions: std::collections::HashMap::new(),
            agent_completion_notification: AgentCompletionNotification::default(),
            detected_agents: std::collections::HashSet::new(),
            custom_buttons: Vec::new(),
            files_expanded: Vec::new(),
            managed_worktrees: Vec::new(),
            sidebar_expanded: true,
            pinned: false,
        }
    }

    /// Create a workspace with a pre-allocated ID (use `next_workspace_id()` to obtain one).
    pub fn with_id(id: u64, title: impl Into<String>, pane: Entity<Pane>) -> Self {
        let cwd = launch_cwd::implicit_launch_cwd().display().to_string();
        Self::build(id, title.into(), cwd, LayoutTree::Leaf(pane))
    }

    /// Create a workspace with a pre-allocated ID and explicit CWD.
    pub fn with_cwd_and_id(
        id: u64,
        title: impl Into<String>,
        cwd: std::path::PathBuf,
        pane: Entity<Pane>,
    ) -> Self {
        Self::build(
            id,
            title.into(),
            cwd.display().to_string(),
            LayoutTree::Leaf(pane),
        )
    }

    /// Create an empty workspace with a pre-allocated ID and explicit CWD: a
    /// folder holding a single empty tab, therefore no pane and no PTY.
    ///
    /// This is what "new workspace" means since EP-003: opening a project must
    /// not spawn a shell the user did not ask for. The workspace stays in the
    /// state the model already had a name for - the one a workspace falls back
    /// to when its last pane is closed (FR-01) - so nothing downstream needs a
    /// new special case.
    pub fn empty_with_cwd_and_id(
        id: u64,
        title: impl Into<String>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self::build_with_tab(id, title.into(), cwd.display().to_string(), Tab::empty())
    }

    /// Create a workspace with a pre-allocated ID and layout tree.
    pub fn with_layout_and_id(
        id: u64,
        title: impl Into<String>,
        cwd: std::path::PathBuf,
        root: LayoutTree,
    ) -> Self {
        Self::build(id, title.into(), cwd.display().to_string(), root)
    }

    /// US-018: rebuild a workspace from a restored session, tabs included.
    ///
    /// The session boundary is the only caller that legitimately supplies more
    /// than one starting tab. `tabs` is capped at [`MAX_TABS_PER_WORKSPACE`]
    /// (the read-cap half of the pairing `open_tab` enforces on the write
    /// side) and an empty list degrades to a single [`Tab::empty`], so the
    /// FR-01 invariant holds whatever the file said.
    pub fn restored_with_id(
        id: u64,
        title: impl Into<String>,
        cwd: std::path::PathBuf,
        mut tabs: Vec<Tab>,
        active_tab: usize,
    ) -> Self {
        if tabs.len() > MAX_TABS_PER_WORKSPACE {
            log::warn!(
                "session restore: workspace holds {} tabs, keeping the first {}",
                tabs.len(),
                MAX_TABS_PER_WORKSPACE
            );
            tabs.truncate(MAX_TABS_PER_WORKSPACE);
        }
        let first = if tabs.is_empty() {
            Tab::empty()
        } else {
            tabs.remove(0)
        };
        let mut ws = Self::build_with_tab(id, title.into(), cwd.display().to_string(), first);
        ws.tabs.append(&mut tabs);
        ws.active_tab_idx = active_tab.min(ws.tabs.len().saturating_sub(1));
        ws
    }

    // --- Tab access (US-001) -------------------------------------------
    //
    // `tabs` is private: callers reach a tab through these accessors, so the
    // "at least one tab" invariant cannot be observed broken.

    /// Every tab of this workspace, in display order. Never empty.
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Number of tabs. Always >= 1.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Index of the visible tab, clamped into range.
    pub fn active_tab_idx(&self) -> usize {
        self.active_tab_idx.min(self.tabs.len().saturating_sub(1))
    }

    /// The visible tab: the one every layout operation applies to.
    pub fn active_tab(&self) -> &Tab {
        let idx = self.active_tab_idx();
        debug_assert!(!self.tabs.is_empty(), "workspace must keep one tab");
        &self.tabs[idx]
    }

    /// Mutable access to the visible tab.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        let idx = self.active_tab_idx();
        debug_assert!(!self.tabs.is_empty(), "workspace must keep one tab");
        &mut self.tabs[idx]
    }

    /// Mutable access to an arbitrary tab, for operations targeting a tab that
    /// is not the visible one (a `surface_id` resolved into a background tab).
    pub fn tab_mut(&mut self, idx: usize) -> Option<&mut Tab> {
        self.tabs.get_mut(idx)
    }

    /// Make `idx` the visible tab. Out-of-range indices are ignored.
    pub fn set_active_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab_idx = idx;
        }
    }

    /// Index of the tab owning `pane`, zoom-saved trees included.
    pub fn tab_index_containing_pane(&self, pane: &Entity<Pane>) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.contains_pane(pane))
    }

    /// Whether this workspace is an empty folder: one unnamed tab holding no
    /// pane at all. True for a freshly created workspace and for one whose
    /// last pane was closed, and false as soon as a tab is opened, named, or
    /// filled.
    ///
    /// The placeholder tab exists only to honour FR-01 ("a workspace always
    /// keeps one tab"); it is not a tab the user asked for, so the sidebar
    /// renders no child row for it and [`Workspace::open_tab`] fills it in
    /// place instead of leaving it behind.
    pub fn is_empty_shell(&self) -> bool {
        match self.tabs.as_slice() {
            [tab] => tab.title.is_empty() && tab.root.is_none() && tab.saved_layout.is_none(),
            _ => false,
        }
    }

    /// Make `tab` the active tab. It replaces the placeholder of an empty
    /// workspace and is appended otherwise. Returns `false` - without mutating
    /// anything - when the workspace already holds [`MAX_TABS_PER_WORKSPACE`]
    /// tabs.
    pub fn open_tab(&mut self, tab: Tab) -> bool {
        if self.is_empty_shell() {
            // Filling the placeholder rather than pushing past it: otherwise
            // the first tab of a new workspace would land at index 1, behind a
            // permanent empty sibling nobody created.
            self.tabs[0] = tab;
            self.active_tab_idx = 0;
            return true;
        }
        if self.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            log::warn!(
                "workspace {}: tab limit reached ({MAX_TABS_PER_WORKSPACE}), refusing to open a new tab",
                self.id
            );
            return false;
        }
        self.tabs.push(tab);
        self.active_tab_idx = self.tabs.len() - 1;
        true
    }

    /// Whether this workspace can take one more tab. Checked *before* a tab
    /// is detached from its source workspace (US-011) so a refused move
    /// leaves the dragged tab - and its live terminals - exactly where it was.
    pub fn can_open_tab(&self) -> bool {
        self.tabs.len() < MAX_TABS_PER_WORKSPACE
    }

    /// Move the tab at `from` so it ends up at `to`, keeping the same tab
    /// visible across the reorder (US-011). Out-of-range indices are ignored.
    pub fn reorder_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to > self.tabs.len() || from == to {
            return;
        }
        let active_id = self.tabs[self.active_tab_idx()].id;
        let tab = self.tabs.remove(from);
        let insert_at = to.min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
        if let Some(idx) = self.tabs.iter().position(|tab| tab.id == active_id) {
            self.active_tab_idx = idx;
        }
    }

    /// Remove the tab at `idx` and return it. Closing the last tab leaves an
    /// empty tab behind instead of an empty workspace (FR-01).
    pub fn close_tab(&mut self, idx: usize) -> Option<Tab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::empty());
            self.active_tab_idx = 0;
        } else if self.active_tab_idx >= self.tabs.len() {
            self.active_tab_idx = self.tabs.len() - 1;
        } else if self.active_tab_idx > idx {
            self.active_tab_idx -= 1;
        }
        Some(removed)
    }

    // --- Layout, delegated to the active tab (US-002 / US-003) ----------

    pub fn is_zoomed(&self) -> bool {
        self.active_tab().is_zoomed()
    }

    pub fn exit_zoom(&mut self, cx: &mut App) -> Option<Entity<Pane>> {
        self.active_tab_mut().exit_zoom(cx)
    }

    /// Total leaf panes across every tab of this workspace. Per-tab caps read
    /// [`Tab::pane_count`] instead (`MAX_PANES` bounds a tab, not a workspace).
    pub fn pane_count(&self) -> usize {
        self.tabs.iter().map(Tab::pane_count).sum()
    }

    pub fn any_pane(&self, mut f: impl FnMut(&Entity<Pane>) -> bool) -> bool {
        self.tabs.iter().any(|tab| tab.any_pane(&mut f))
    }

    pub fn collect_panes(&self) -> Vec<Entity<Pane>> {
        let mut panes = Vec::new();
        for tab in &self.tabs {
            for pane in tab.collect_panes() {
                if !panes.contains(&pane) {
                    panes.push(pane);
                }
            }
        }
        panes
    }

    /// Issue #107: whether nothing is running in this workspace - the signal
    /// the sidebar's Auto ordering sorts "active" above "inactive" on.
    ///
    /// True when the workspace holds no terminal at all, or when every
    /// terminal's foreground command is unknown or an interactive shell. Reads
    /// only `TerminalState::cached_foreground_command`, which the off-thread
    /// pane scanner keeps warm, so this does no I/O and is safe to call once
    /// per frame from the render path.
    ///
    /// This is the *signal* only. Issue #76 owns whatever an idle row looks
    /// like; nothing here changes a color.
    pub fn is_idle(&self, cx: &App) -> bool {
        // Tab-by-tab, like `ipc_handler::workspace_surface_entries`, rather than
        // through `collect_panes`: that one dedupes with a linear `contains` per
        // pane, and this runs once per workspace per frame.
        let mut commands = Vec::new();
        for tab in &self.tabs {
            for pane in tab.collect_panes() {
                for terminal in pane.read(cx).terminals() {
                    commands.push(terminal.read(cx).terminal.foreground_command());
                }
            }
        }
        commands_are_idle(&commands)
    }

    /// Focus the first pane of the *visible* tab. Deliberately not a
    /// whole-workspace walk: focus can only land on a rendered pane, so
    /// background tabs are out of reach by construction.
    ///
    /// Returns `true` when focus landed on a pane. `false` means the visible
    /// tab is empty (issue #108: the substitute tab a last-tab close leaves
    /// behind), so the caller must park focus itself or the window ends up
    /// with no focused element at all.
    ///
    /// `#[must_use]` so a discarded report is a compile error, not a silent
    /// regression of issue #108.
    #[must_use]
    pub fn focus_first(&self, window: &mut Window, cx: &mut App) -> bool {
        self.active_tab().focus_first(window, cx)
    }

    /// Serialize the visible tab's layout to a `LayoutNode`, including per-pane
    /// scrollback. Per-tab serialization is [`Tab::serialize`]; the session
    /// writer uses [`Self::serialize_tabs_without_scrollback`] with the v2
    /// schema (US-018).
    ///
    /// IPC `workspace.current` uses [`Self::serialize_layout_without_scrollback`]
    /// so the GPUI tick does not extract 4000 lines per pane (issue #29).
    #[allow(dead_code)]
    pub fn serialize_layout(&self, cx: &App) -> Option<LayoutNode> {
        self.active_tab().serialize(cx)
    }

    /// Serialize the visible tab's layout WITHOUT per-pane scrollback.
    /// IPC `workspace.current` uses this so the GPUI tick does not extract
    /// 4000 lines per pane (issue #29).
    pub fn serialize_layout_without_scrollback(&self, cx: &App) -> Option<LayoutNode> {
        self.active_tab().serialize_without_scrollback(cx)
    }

    /// US-018: serialize every tab for session persistence, without terminal
    /// output - that must remain local to the process that produced it.
    ///
    /// This is what `session.json` v2 stores: the whole tab list, not just the
    /// tab that happened to be visible at save time.
    pub fn serialize_tabs_without_scrollback(&self, cx: &App) -> Vec<TabSession> {
        self.tabs
            .iter()
            .map(|tab| TabSession {
                title: tab.title.clone(),
                layout: tab.serialize_without_scrollback(cx),
            })
            .collect()
    }
}

impl Workspace {
    /// US-015: push a refreshed [`PaneFlowConfig`] to every `Pane` in the
    /// workspace's layout so the tab bar re-renders against the new config
    /// without a per-frame `load_config()`. Called from
    /// `PaneFlowApp::process_config_changes` on every ConfigWatcher reload.
    pub fn propagate_config(&self, config: &paneflow_config::schema::PaneFlowConfig, cx: &mut App) {
        for tab in &self.tabs {
            if let Some(root) = &tab.root {
                walk_and_push_config(root, config, cx);
            }
            if let Some(saved) = &tab.saved_layout {
                walk_and_push_config(saved, config, cx);
            }
        }
    }
}

fn walk_and_push_config(
    node: &LayoutTree,
    config: &paneflow_config::schema::PaneFlowConfig,
    cx: &mut App,
) {
    match node {
        LayoutTree::Leaf(pane) => {
            pane.update(cx, |p, cx| {
                p.apply_config(config, cx);
            });
        }
        LayoutTree::Container { children, .. } => {
            for child in children {
                walk_and_push_config(&child.node, config, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, Focusable, TestAppContext};

    use super::{
        AgentCompletionNotification, MAX_TABS_PER_WORKSPACE, Tab, Workspace, commands_are_idle,
    };
    use crate::layout::LayoutTree;
    use crate::terminal::TerminalView;

    fn test_workspace(cx: &mut impl AppContext) -> Workspace {
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx));
        Workspace::build(1, "ws".to_string(), String::new(), LayoutTree::Leaf(pane))
    }

    /// A single-terminal pane whose cached foreground command is seeded to
    /// `foreground_command` - the field the off-thread pane scanner writes and
    /// the only thing [`Workspace::is_idle`] reads.
    fn terminal_pane(
        cx: &mut impl AppContext,
        foreground_command: Option<&str>,
    ) -> gpui::Entity<crate::pane::Pane> {
        let command = foreground_command.map(str::to_string);
        let terminal = cx.new(|cx| {
            let mut view = TerminalView::display_only_for_test(1, cx);
            view.terminal.cached_foreground_command = command;
            view
        });
        cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx))
    }

    /// A workspace holding one terminal per entry of `commands`, each in its
    /// own tab (a pane is mono-surface). One tab per terminal on purpose:
    /// `is_idle` walks the whole tab list, not just the visible tab, so the
    /// fixture has to make that walk observable.
    fn workspace_with_foreground_commands(
        cx: &mut impl AppContext,
        commands: &[Option<&str>],
    ) -> Workspace {
        let (first, rest) = commands.split_first().expect("at least one terminal");
        let mut ws = Workspace::build(
            1,
            "ws".to_string(),
            String::new(),
            LayoutTree::Leaf(terminal_pane(cx, *first)),
        );
        for command in rest {
            let pane = terminal_pane(cx, *command);
            assert!(
                ws.open_tab(Tab::new(String::new(), Some(LayoutTree::Leaf(pane)))),
                "the fixture must be under MAX_TABS_PER_WORKSPACE"
            );
        }
        ws
    }

    #[gpui::test]
    fn workspace_keeps_one_tab_when_the_last_one_is_closed(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        assert_eq!(ws.tab_count(), 1);

        let removed = ws.close_tab(0);

        assert!(removed.is_some(), "closing the only tab must yield it");
        assert_eq!(ws.tab_count(), 1, "workspace must never hold zero tabs");
        assert_eq!(ws.active_tab_idx(), 0);
        assert!(
            ws.active_tab().root.is_none(),
            "the substitute tab is empty"
        );
        assert_eq!(ws.pane_count(), 0);
    }

    /// Issue #83: undoing the close of a workspace's LAST tab has to leave ONE
    /// tab, not the restored tab sitting beside the `Tab::empty()` placeholder
    /// `close_tab` left behind. `open_tab` already fills that placeholder in
    /// place; the undo-close-tab restore leans on it, so pin it here.
    #[gpui::test]
    fn open_tab_fills_the_placeholder_left_by_closing_the_last_tab(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        ws.close_tab(0);
        assert_eq!(ws.tab_count(), 1);
        assert!(ws.is_empty_shell(), "close_tab leaves a placeholder");

        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx));
        let opened = ws.open_tab(Tab::new("Restored", Some(LayoutTree::Leaf(pane))));

        assert!(opened);
        assert_eq!(
            ws.tab_count(),
            1,
            "the placeholder is replaced, not pushed past"
        );
        assert_eq!(ws.active_tab().title, "Restored");
        assert_eq!(ws.active_tab_idx(), 0);
    }

    /// Issue #108: a workspace left with the substitute empty tab has nothing
    /// to focus, and the caller has to know that so it can park focus inside
    /// `app_content` instead (otherwise every global keybinding goes dead).
    #[gpui::test]
    fn focus_first_reports_false_for_a_zero_pane_workspace(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        ws.close_tab(0);
        assert_eq!(ws.pane_count(), 0, "the substitute tab holds no panes");

        let focused = cx.update(|window, cx| ws.focus_first(window, cx));

        assert!(!focused, "no pane exists, so focus cannot have landed");
    }

    /// #184 Phase 4: wanting the Files rail belongs to the tab that asked for
    /// it. Opening it in one tab must not put it in front of a sibling, and a
    /// tab switch must find its own flag - the app-level mirror is rebuilt from
    /// `active_tab().files_sidebar_open` every frame.
    #[gpui::test]
    fn files_sidebar_open_is_scoped_to_the_tab_that_opened_it(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        assert!(ws.open_tab(Tab::new("Second", None)));
        assert_eq!(ws.active_tab_idx(), 1);

        ws.active_tab_mut().files_sidebar_open = true;
        assert!(ws.active_tab().files_sidebar_open);
        assert!(
            !ws.tabs()[0].files_sidebar_open,
            "opening the rail in one tab must not open it for a sibling"
        );

        ws.set_active_tab(0);
        assert!(
            !ws.active_tab().files_sidebar_open,
            "the sibling tab starts closed and stays closed"
        );
        ws.set_active_tab(1);
        assert!(
            ws.active_tab().files_sidebar_open,
            "the tab that opened the rail still wants it after a round trip"
        );

        // The flag travels with the tab through a reorder and dies with it on
        // close: it is tab state, not a slot in the workspace.
        ws.reorder_tab(1, 0);
        assert!(ws.tabs()[0].files_sidebar_open);
        assert!(!ws.tabs()[1].files_sidebar_open);
        let closed = ws.close_tab(0).expect("the opening tab is removed");
        assert!(closed.files_sidebar_open);
        assert!(!ws.active_tab().files_sidebar_open);
    }

    #[gpui::test]
    fn focus_first_reports_true_and_focuses_when_a_pane_exists(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let ws = test_workspace(cx);
        let pane = ws.active_tab().root.as_ref().unwrap().first_leaf().unwrap();

        cx.update(|window, cx| {
            assert!(
                ws.focus_first(window, cx),
                "a pane exists, so focus must land on it"
            );
            assert!(
                pane.read(cx).focus_handle(cx).is_focused(window),
                "the reported focus must be the pane's"
            );
        });
    }

    /// Contract pin, not a behaviour claim. `Tab::focus_first` reports `true`
    /// for any `Some(root)` - it does not check that a leaf was actually
    /// focused - and `LayoutTree::focus_first` silently no-ops on a container
    /// with no children, so a childless root reports success with focus
    /// unmoved. Unreachable today (`validate_layout` pads every split to >= 2
    /// children); this documents the hole so a future change to either side
    /// has to notice it.
    #[gpui::test]
    fn focus_first_reports_true_for_a_childless_root_though_focus_never_moves(
        cx: &mut TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let tab = Tab::new("t", Some(LayoutTree::empty()));

        let reported = cx.update(|window, cx| tab.focus_first(window, cx));

        assert!(
            reported,
            "the contract is root.is_some(), not 'a leaf was focused'"
        );
    }

    #[gpui::test]
    fn closing_a_tab_left_of_the_active_one_keeps_the_same_tab_visible(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        assert!(ws.open_tab(Tab::new("second", None)));
        let active_id = ws.active_tab().id;
        assert_eq!(ws.active_tab_idx(), 1);

        ws.close_tab(0);

        assert_eq!(ws.tab_count(), 1);
        assert_eq!(ws.active_tab().id, active_id);
        assert_eq!(ws.active_tab_idx(), 0);
    }

    #[gpui::test]
    fn opening_beyond_the_tab_cap_is_refused_without_mutation(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        for i in 1..MAX_TABS_PER_WORKSPACE {
            assert!(ws.open_tab(Tab::new(format!("t{i}"), None)), "tab {i}");
        }
        assert_eq!(ws.tab_count(), MAX_TABS_PER_WORKSPACE);
        let active_before = ws.active_tab().id;

        let accepted = ws.open_tab(Tab::new("overflow", None));

        assert!(!accepted, "the cap must refuse the extra tab");
        assert_eq!(ws.tab_count(), MAX_TABS_PER_WORKSPACE);
        assert_eq!(
            ws.active_tab().id,
            active_before,
            "a refused open must not move the active tab"
        );
    }

    #[gpui::test]
    fn zoom_is_per_tab(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        let pane = ws.active_tab().root.as_ref().unwrap().first_leaf().unwrap();
        let first_tab = ws.active_tab_idx();

        // Zoom the first tab: root keeps the zoomed leaf, saved_layout the tree.
        let full = ws.active_tab_mut().root.take().unwrap();
        ws.active_tab_mut().saved_layout = Some(full);
        ws.active_tab_mut().root = Some(LayoutTree::Leaf(pane));
        assert!(ws.is_zoomed());

        assert!(ws.open_tab(Tab::new("second", None)));
        assert!(!ws.is_zoomed(), "a fresh tab is not zoomed");

        ws.set_active_tab(first_tab);
        assert!(ws.is_zoomed(), "returning to the tab restores its zoom");
    }

    #[gpui::test]
    fn reorder_tab_keeps_the_same_tab_visible(cx: &mut TestAppContext) {
        // US-011: reordering is a view operation - the tab you were looking at
        // stays the one you look at, wherever it lands.
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        assert!(ws.open_tab(Tab::new("second", None)));
        assert!(ws.open_tab(Tab::new("third", None)));
        let ids: Vec<u64> = ws.tabs().iter().map(|tab| tab.id).collect();
        ws.set_active_tab(0);

        ws.reorder_tab(0, 2);

        assert_eq!(
            ws.tabs().iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![ids[1], ids[2], ids[0]]
        );
        assert_eq!(ws.active_tab().id, ids[0]);
        assert_eq!(ws.active_tab_idx(), 2);

        // Out-of-range and no-op moves mutate nothing.
        ws.reorder_tab(2, 2);
        ws.reorder_tab(9, 0);
        assert_eq!(
            ws.tabs().iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![ids[1], ids[2], ids[0]]
        );
        assert_eq!(ws.active_tab().id, ids[0]);
    }

    #[gpui::test]
    fn can_open_tab_reports_the_cap_before_a_move_detaches_anything(cx: &mut TestAppContext) {
        // US-011: a cross-workspace move asks this *before* removing the tab
        // from its source, so a refused move never kills a live terminal.
        let cx = cx.add_empty_window();
        let mut ws = test_workspace(cx);
        assert!(ws.can_open_tab());
        for i in 1..MAX_TABS_PER_WORKSPACE {
            assert!(ws.open_tab(Tab::new(format!("t{i}"), None)), "tab {i}");
        }
        assert!(!ws.can_open_tab());
        assert!(!ws.open_tab(Tab::new("overflow", None)));
    }

    #[gpui::test]
    fn a_new_workspace_is_an_empty_folder(cx: &mut TestAppContext) {
        let _ = cx.add_empty_window();
        let ws = Workspace::empty_with_cwd_and_id(7, "project", std::path::PathBuf::from("/tmp"));

        assert!(ws.is_empty_shell(), "an opened folder starts empty");
        assert_eq!(ws.tab_count(), 1, "FR-01: one tab always exists");
        assert_eq!(ws.pane_count(), 0, "no pane means no PTY was spawned");
        assert!(ws.active_tab().root.is_none());
    }

    #[gpui::test]
    fn the_first_tab_of_an_empty_workspace_replaces_the_placeholder(cx: &mut TestAppContext) {
        let _ = cx.add_empty_window();
        let mut ws =
            Workspace::empty_with_cwd_and_id(7, "project", std::path::PathBuf::from("/tmp"));

        assert!(ws.open_tab(Tab::new("first", None)));

        assert_eq!(
            ws.tab_count(),
            1,
            "the placeholder is filled, not pushed past"
        );
        assert_eq!(ws.active_tab_idx(), 0);
        assert_eq!(ws.active_tab().title, "first");
        assert!(!ws.is_empty_shell());

        assert!(ws.open_tab(Tab::new("second", None)));
        assert_eq!(ws.tab_count(), 2, "later tabs append as usual");
        assert_eq!(ws.active_tab_idx(), 1);
    }

    #[gpui::test]
    fn a_workspace_holding_a_pane_is_not_an_empty_shell(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let ws = test_workspace(cx);

        assert!(!ws.is_empty_shell());
    }

    #[test]
    fn agent_completion_is_unread_only_while_workspace_is_not_visible() {
        let mut notification = AgentCompletionNotification::default();
        assert!(!notification.is_unread());

        notification.record_finished(false);
        assert!(notification.is_unread());

        notification.record_finished(true);
        assert!(!notification.is_unread());

        notification.record_finished(false);
        notification.acknowledge();
        assert!(!notification.is_unread());
    }

    /// Issue #107: the idle signal the sidebar's Auto ordering sorts on. The
    /// truth table lives on the pure helper because a `Workspace` cannot be
    /// built in a plain unit test (its constructors read `.git` off disk).
    #[test]
    fn commands_are_idle_truth_table() {
        // No terminals at all: nothing is running.
        assert!(commands_are_idle(&[]));
        // Every terminal parked at a prompt.
        assert!(commands_are_idle(&[
            Some("zsh".into()),
            Some("bash".into())
        ]));
        // The scanner has not answered yet - absence of a command is not
        // evidence of work.
        assert!(commands_are_idle(&[None, None]));
        assert!(commands_are_idle(&[None, Some("zsh".into())]));
        // A shell resolved to its absolute path, and one carrying arguments.
        assert!(commands_are_idle(&[Some("/bin/zsh".into())]));
        assert!(commands_are_idle(&[Some("zsh -l".into())]));
        // One busy terminal makes the whole workspace active.
        assert!(!commands_are_idle(&[
            Some("zsh".into()),
            Some("cargo run".into())
        ]));
        assert!(!commands_are_idle(&[Some("vim".into())]));
    }

    /// [`Workspace::is_idle`] over a real workspace, asserted in BOTH
    /// directions off the same fixture. The positive half alone would pass
    /// against `fn is_idle(&self, _) -> bool { true }`; changing one terminal's
    /// cached command and requiring the answer to follow is what proves the
    /// walk actually reaches the terminals and reads them.
    #[gpui::test]
    fn workspace_idleness_is_read_off_the_terminals_it_walks(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let ws = workspace_with_foreground_commands(cx, &[None, Some("zsh"), Some("/bin/bash -l")]);

        cx.update(|_window, cx| {
            assert_eq!(ws.tab_count(), 3, "one terminal per tab");
            assert!(
                ws.is_idle(cx),
                "an unknown command and two shells are all idle"
            );

            // Same workspace, one field changed. This is the LAST tab, which
            // `open_tab` also makes the ACTIVE one - so it does not by itself
            // prove the walk reaches backgrounded tabs. That property is
            // pinned by `one_busy_terminal_beside_a_shell_makes_the_workspace_active`,
            // whose `busy_first` case leaves the busy tab behind the active one.
            // What this half proves is the direction: the answer follows the
            // terminals' cached command rather than being a constant.
            let last_pane = ws
                .collect_panes()
                .last()
                .expect("the fixture has panes")
                .clone();
            let terminal = last_pane
                .read(cx)
                .terminals()
                .next()
                .expect("the pane holds a terminal")
                .clone();
            terminal.update(cx, |view, _cx| {
                view.terminal.cached_foreground_command = Some("cargo run".into());
            });

            assert!(
                !ws.is_idle(cx),
                "is_idle must follow the terminals' cached foreground command"
            );
        });
    }

    #[gpui::test]
    fn a_workspace_running_a_command_is_not_idle(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let ws = workspace_with_foreground_commands(cx, &[Some("cargo run")]);
        cx.update(|_window, cx| {
            assert!(
                !ws.is_idle(cx),
                "a single busy terminal is the whole workspace's answer"
            );
        });
    }

    /// The "every terminal" half of the definition: idle is an `all`, so one
    /// busy pane beside a prompt is enough to make the workspace active - in
    /// either position, which also pins that the walk visits every tab rather
    /// than short-circuiting on the first.
    #[gpui::test]
    fn one_busy_terminal_beside_a_shell_makes_the_workspace_active(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let shell_first = workspace_with_foreground_commands(cx, &[Some("zsh"), Some("cargo run")]);
        let busy_first = workspace_with_foreground_commands(cx, &[Some("cargo run"), Some("zsh")]);

        cx.update(|_window, cx| {
            assert_eq!(shell_first.tab_count(), 2);
            assert!(
                !shell_first.is_idle(cx),
                "the busy terminal is in the second tab and still counts"
            );
            assert!(
                !busy_first.is_idle(cx),
                "the trailing shell must not talk the workspace back into idle"
            );
        });
    }
}
