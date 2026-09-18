//! Launch Pad (EP-002 US-005, CLI Cockpit).
//!
//! A modal (custom-buttons modal scaffold) that compresses the
//! worktree-per-agent ritual into one gesture: pick an agent, name a NEW
//! branch, optionally write a prompt - confirm runs the existing
//! orchestration-v2 worktree engine OFF the render thread
//! (`smol::unblock` + `worktree::add_worktree`, 120 s deadline, sibling
//! `<repo>.worktrees/<slug>` or hashed collision fallback, then
//! `copy_env_files` no-clobber), and only
//! on success splits the focused pane at the worktree path, launches the
//! agent CLI (`TerminalAgent::launch_command`, honors the Claude bypass
//! setting) and pre-fills the prompt through the existing settle-poll -
//! never submitted (FR-01).
//!
//! Atomicity (US-005 AC4): a worktree failure surfaces git's error verbatim
//! in the modal and creates NO pane. Branch names are NOT validated locally
//! git is the single authority (AC7). The created worktree is registered
//! as a [`ManagedWorktree`] so teardown parity with `paneflow up` holds
//! (AC5 - no second worktree population).

use gpui::{
    AnyElement, ClickEvent, Context, Entity, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, SharedString, Styled, WeakEntity, Window, deferred, div,
    prelude::*, px, svg,
};
use paneflow_config::schema::TerminalSurfaceProfile;

use crate::PaneFlowApp;
use crate::agent_launcher::TerminalAgent;
use crate::layout::{LayoutTree, MAX_PANES, SplitDirection};
use crate::pane::Pane;
use crate::terminal::TerminalView;
use crate::ui_primitives::{AnimatedHoverExt, lerp_color};
use crate::widgets::text_area::TextArea;
use crate::widgets::text_input::TextInput;
use crate::workspace::worktree::{self, ManagedWorktree};

/// Shown in place of a "not installed" verdict while the first PATH walk
/// for agent CLIs is still running (issue #518, upstream df375ba5). The
/// launch pad and the pane palette share it.
pub(crate) const AGENT_SCAN_PENDING_COPY: &str = "Looking for agent CLIs on this machine.";

/// Live Launch Pad modal state, owned by `PaneFlowApp`.
/// The request a confirm was validated with when the first PATH walk was
/// still pending (issue #518); replayed verbatim once the walk lands.
#[derive(Debug, Clone)]
pub(crate) struct QueuedConfirm {
    pub(crate) agent_idx: usize,
    pub(crate) branch: String,
    pub(crate) prompt: String,
}

pub(crate) struct LaunchPadState {
    /// Workspace the launch targets, by stable id (survives reorders and
    /// closes - re-resolved when the background work returns).
    pub(crate) ws_id: u64,
    /// Pane to split next to; weak so a close while the modal is open (or
    /// while git runs) degrades to splitting the first leaf.
    pub(crate) target: WeakEntity<Pane>,
    /// Index into [`TerminalAgent::ALL`].
    pub(crate) agent_idx: usize,
    /// `true` while `agent_idx` is the row 0 fallback chosen because the
    /// first PATH walk had not published when the pad opened (issue #518),
    /// and the user has not picked a row since. The boot warm's completion
    /// settles it onto the first installed agent through
    /// [`settle_default_agent`].
    pub(crate) agent_default_pending: bool,
    /// A confirm pressed while the first PATH walk was still pending
    /// (issue #518): never waited for on the GPUI thread, replayed by the
    /// boot warm's completion through `launch_pad_resume_queued_confirm`.
    /// The validated request is snapshotted, so an edit made while the
    /// looking copy shows is not what gets launched.
    pub(crate) confirm_queued: Option<QueuedConfirm>,
    pub(crate) branch_input: Entity<TextInput>,
    pub(crate) prompt_input: Entity<TextArea>,
    pub(crate) issue_input: Entity<TextInput>,
    pub(crate) issue_loading: bool,
    /// `true` while the worktree creation runs - disables re-submission
    /// (US-005 AC8: no double worktree) and Escape.
    pub(crate) running: bool,
    /// Last failure, shown verbatim in the modal (git stderr included).
    pub(crate) error: Option<String>,
}

/// Everything the background-completion handler needs to build the pane.
struct LaunchPlan {
    ws_id: u64,
    repo_root: std::path::PathBuf,
    worktree_path: std::path::PathBuf,
    branch: String,
    agent: TerminalAgent,
    prompt: String,
}

struct LaunchPadCreationFailure {
    message: String,
    checkout_may_remain: bool,
}

fn worktree_checkout_may_exist(repo_root: &std::path::Path, path: &std::path::Path) -> bool {
    match path.try_exists() {
        Ok(true) | Err(_) => return true,
        Ok(false) => {}
    }
    match worktree::list_worktrees(repo_root) {
        Ok(entries) => entries.into_iter().any(|entry| entry.path == path),
        // Losing lifecycle evidence is worse than retaining a reservation
        // when git cannot prove the failed creation left nothing behind.
        Err(_) => true,
    }
}

fn launch_pad_worktree_plan(
    repo_root: &std::path::Path,
    branch: &str,
) -> Result<(std::path::PathBuf, bool), String> {
    let legacy_path = worktree::worktree_dir(repo_root, branch);
    let hashed_path = worktree::worktree_dir_hashed(repo_root, branch);
    let entries = worktree::list_worktrees(repo_root)?;
    let mut path = legacy_path.clone();
    for entry in &entries {
        if entry.branch.as_deref() == Some(branch) {
            return Err(format!(
                "branch '{branch}' is already checked out at {}",
                entry.path.display()
            ));
        }
        if entry.path == legacy_path {
            path = hashed_path.clone();
        }
    }
    for entry in &entries {
        if entry.path == path {
            return Err(format!(
                "{} exists but holds another branch ({})",
                path.display(),
                entry.branch.as_deref().unwrap_or("detached")
            ));
        }
    }
    if path.exists() {
        return Err(format!(
            "{} exists but is not a registered worktree; remove it first",
            path.display()
        ));
    }
    Ok((path, !worktree::branch_exists(repo_root, branch)))
}

impl PaneFlowApp {
    pub(crate) fn handle_open_launch_pad(
        &mut self,
        _: &crate::OpenLaunchPad,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.mode, paneflow_config::schema::AppMode::Cli) {
            return;
        }
        if self.launch_pad.is_some() {
            // Toggle semantics, but never abandon a run in flight.
            if !self.launch_pad.as_ref().is_some_and(|lp| lp.running) {
                self.launch_pad = None;
                self.overlay_origin_pane = None;
                cx.notify();
            }
            return;
        }
        let Some(ws) = self.active_workspace() else {
            return;
        };
        let ws_id = ws.id;
        let target = self
            .focused_or_first_pane(window, cx)
            .map(|p| p.downgrade())
            .unwrap_or_else(WeakEntity::new_invalid);

        let weak_app = cx.entity().downgrade();
        let issue_input = cx.new(|cx| TextInput::new("", "GitHub issue # or URL (optional)", cx));
        let branch_input = cx.new(|cx| TextInput::new("", "new-branch-name", cx));
        let prompt_input =
            cx.new(|cx| TextArea::new("Prompt (optional) - pre-filled, never submitted", cx));
        prompt_input.update(cx, |ta, _| {
            // The prompt is OPTIONAL: Enter in the empty field must still
            // confirm the form (review R2 - TaSubmit would otherwise
            // swallow the key without bubbling to the modal handler).
            ta.set_submit_on_empty(true);
            // Same re-entrancy discipline as the Composer: defer + weak.
            let w = weak_app.clone();
            ta.on_submit(move |_text, _window, cx| {
                let w = w.clone();
                cx.defer(move |cx| {
                    let _ = w.update(cx, |app, cx| app.launch_pad_confirm(cx));
                });
            });
            let w = weak_app.clone();
            ta.on_escape(move |_window, cx| {
                let w = w.clone();
                cx.defer(move |cx| {
                    let _ = w.update(cx, |app, cx| app.launch_pad_cancel(cx));
                });
            });
        });
        let branch_focus = branch_input.read(cx).focus_handle.clone();

        // Default to the first installed agent so confirm works out of the
        // box; fall back to 0 (the row renders grayed, confirm rejects).
        // Issue #518: while the first PATH walk is pending every row reads
        // as not installed, so remember that the fallback was provisional
        // and let the boot warm's completion pick the real default.
        // The two reads lock the cache separately, so the pending flag is
        // read first: a walk that publishes between them then leaves the
        // flag `true` and the settle re-picks from the full snapshot, while
        // the other order could pair a row-0 fallback from the empty
        // snapshot with a cleared flag that nothing settles.
        let agent_default_pending = crate::agent_launcher::installed_binary_scan_pending();
        let agent_idx = default_agent_idx(|a| a.is_installed());

        self.launch_pad = Some(LaunchPadState {
            ws_id,
            target,
            agent_idx,
            agent_default_pending,
            confirm_queued: None,
            branch_input,
            prompt_input,
            issue_input,
            issue_loading: false,
            running: false,
            error: None,
        });
        // Issue #523: remember the pane for a command palette that folds us,
        // resolved while the pane still owns the focus.
        self.overlay_origin_pane = self.pane_owning_focus(window, cx).map(|p| p.downgrade());
        window.focus(&branch_focus, cx);
        cx.notify();
    }

    /// Issue #518: the boot warm has published the installed-agent
    /// snapshot. A pad that opened during the walk defaulted to row 0
    /// provisionally; move it to the first installed agent unless the user
    /// picked a row meanwhile. Returns `true` when the selection moved.
    pub(crate) fn launch_pad_settle_default_agent(&mut self) -> bool {
        let Some(lp) = self.launch_pad.as_mut() else {
            return false;
        };
        settle_default_agent(&mut lp.agent_idx, &mut lp.agent_default_pending, |a| {
            a.is_installed()
        })
    }

    /// Issue #518: the boot warm has published. A confirm queued while the
    /// walk was pending runs now against the real installed answer.
    pub(crate) fn launch_pad_resume_queued_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(lp) = self.launch_pad.as_mut() else {
            return;
        };
        let Some(queued) = lp.confirm_queued.take() else {
            return;
        };
        lp.error = None;
        self.launch_pad_submit(queued.agent_idx, queued.branch, queued.prompt, cx);
    }

    /// Escape path - only honored before confirmation (US-005 AC8: the
    /// in-flight run keeps the modal up with its "Creating…" state).
    pub(crate) fn launch_pad_cancel(&mut self, cx: &mut Context<Self>) {
        if self.launch_pad.as_ref().is_some_and(|lp| lp.running) {
            return;
        }
        self.launch_pad = None;
        self.overlay_origin_pane = None;
        cx.notify();
    }

    fn launch_pad_set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(lp) = self.launch_pad.as_mut() {
            lp.running = false;
            lp.error = Some(message.into());
            cx.notify();
        }
    }

    /// Validate the form and run the worktree engine off-thread. Nothing is
    /// executed when a guard fails (US-005 AC6) - the error shows in the
    /// modal and the form stays editable.
    pub(crate) fn launch_pad_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(lp) = self.launch_pad.as_ref() else {
            return;
        };
        if lp.running || lp.issue_loading {
            // AC8: a click/Enter during the run never double-creates.
            return;
        }
        let agent_idx = lp.agent_idx;
        let branch = lp.branch_input.read(cx).value().trim().to_string();
        // Same delivery profile as the Composer (security review): LF-only,
        // trailing newlines trimmed, 64 KiB cap before the PTY write.
        let (prompt, _truncated) =
            crate::app::composer::normalize_composer_text(&lp.prompt_input.read(cx).value());
        self.launch_pad_submit(agent_idx, branch, prompt, cx);
    }

    /// The confirm proper, on values already read from the form: the live
    /// Enter passes what the inputs hold, a replay after the cold PATH walk
    /// passes the snapshot it was queued with (issue #518).
    fn launch_pad_submit(
        &mut self,
        agent_idx: usize,
        branch: String,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let Some(lp) = self.launch_pad.as_ref() else {
            return;
        };
        if lp.running || lp.issue_loading {
            return;
        }
        let ws_id = lp.ws_id;

        let Some(agent) = TerminalAgent::ALL.get(agent_idx).copied() else {
            self.launch_pad_set_error("No agent selected", cx);
            return;
        };
        if branch.is_empty() {
            self.launch_pad_set_error("Branch name is empty", cx);
            return;
        }
        let Some(ws) = self.workspaces.iter().find(|w| w.id == ws_id) else {
            self.launch_pad_set_error("Workspace was closed", cx);
            return;
        };
        // AC6: cwd without a git repo → explicit error, nothing executed.
        let Some(repo_root) = ws.repo_root.clone() else {
            self.launch_pad_set_error("No git repository for this workspace", cx);
            return;
        };
        if ws.active_tab().is_zoomed() {
            self.launch_pad_set_error("Unzoom before splitting panes", cx);
            return;
        }
        if !ws.active_tab().can_add_pane() {
            self.launch_pad_set_error(format!("Maximum pane count reached ({MAX_PANES})"), cx);
            return;
        }
        // Every guard that does not need the PATH walk runs first, so only a
        // form that would launch right now can be queued: an Enter on an
        // empty branch is refused here and never replayed after an edit.
        // The pending flag is read before the snapshot: a walk that publishes
        // between the two reads then shows up as installed and proceeds,
        // while the other order could refuse an installed agent.
        let scan_pending = crate::agent_launcher::installed_binary_scan_pending();
        if !agent.is_installed() {
            // Issue #518: the first PATH walk has not published. Never wait
            // for it here (this is the GPUI thread; a slow PATH entry would
            // freeze the window): queue the confirm, say so, and let the
            // boot warm's completion replay it with the real answer. The
            // confirm commits the row the user sees, so the provisional
            // default is no longer pending: the settle must not move it
            // before the replay.
            if scan_pending {
                if let Some(lp) = self.launch_pad.as_mut() {
                    lp.confirm_queued = Some(QueuedConfirm {
                        agent_idx,
                        branch: branch.clone(),
                        prompt: prompt.clone(),
                    });
                    lp.agent_default_pending = false;
                }
                self.launch_pad_set_error(AGENT_SCAN_PENDING_COPY, cx);
                return;
            }
            self.launch_pad_set_error(format!("{} is not installed", agent.display_name()), cx);
            return;
        }

        if let Some(lp) = self.launch_pad.as_mut() {
            lp.running = true;
            lp.error = None;
        }
        cx.notify();

        let plan = LaunchPlan {
            ws_id,
            repo_root: repo_root.clone(),
            worktree_path: worktree::worktree_dir(&repo_root, &branch),
            branch: branch.clone(),
            agent,
            prompt,
        };
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                // Planning runs off-thread because it calls git, but creation
                // starts only after the exact collision-resolved path is
                // reserved back on the serialized GPUI thread.
                let result =
                    smol::unblock(move || launch_pad_worktree_plan(&repo_root, &branch)).await;
                cx.update(|cx| {
                    let _ = this.update(cx, |app, cx| {
                        app.launch_pad_begin_creation(result, plan, cx);
                    });
                });
            },
        )
        .detach();
    }

    /// Reserve the exact path selected by the off-thread planner before any
    /// filesystem creation. The durable retirement journal doubles as the
    /// in-flight ownership reservation: IPC and other launch paths already
    /// gate on it, and a crash after `git worktree add` can safely replay it.
    fn launch_pad_begin_creation(
        &mut self,
        result: Result<(std::path::PathBuf, bool), String>,
        mut plan: LaunchPlan,
        cx: &mut Context<Self>,
    ) {
        let (worktree_path, create_branch) = match result {
            Ok(result) => result,
            Err(error) => {
                self.launch_pad_set_error(error, cx);
                return;
            }
        };
        plan.worktree_path = worktree_path.clone();
        if self.managed_worktree_conflicts(&worktree_path, None, cx) {
            self.launch_pad_set_error("Worktree is already owned or being retired", cx);
            return;
        }

        let reservation = ManagedWorktree {
            path: worktree_path.clone(),
            repo_root: plan.repo_root.clone(),
            branch: plan.branch.clone(),
            teardown: Default::default(),
            identity: None,
        };
        self.pending_worktree_teardowns.push(reservation);
        self.pending_worktree_teardowns = worktree::merge_managed_worktree_records(std::mem::take(
            &mut self.pending_worktree_teardowns,
        ));
        self.publish_pending_worktree_teardowns();
        if !self.save_session_blocking(cx) {
            self.pending_worktree_teardowns
                .retain(|worktree| worktree.path != worktree_path);
            self.publish_pending_worktree_teardowns();
            self.launch_pad_set_error(
                "Could not persist the worktree ownership reservation; nothing was created",
                cx,
            );
            return;
        }

        let repo_root = plan.repo_root.clone();
        let branch = plan.branch.clone();
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let result = smol::unblock(move || {
                    if let Err(message) =
                        worktree::add_worktree(&repo_root, &worktree_path, &branch, create_branch)
                    {
                        let checkout_may_remain =
                            worktree_checkout_may_exist(&repo_root, &worktree_path);
                        return Err(LaunchPadCreationFailure {
                            message,
                            checkout_may_remain,
                        });
                    }
                    // Best-effort by design (US-007 orchestration-v2): a
                    // partial copy is not a failure.
                    let _ = worktree::copy_env_files(&repo_root, &worktree_path);
                    Ok::<std::path::PathBuf, LaunchPadCreationFailure>(worktree_path)
                })
                .await;
                cx.update(|cx| {
                    let _ = this.update(cx, |app, cx| {
                        app.launch_pad_finish(result, plan, cx);
                    });
                });
            },
        )
        .detach();
    }

    /// Main-thread completion: error → verbatim in the modal, zero panes
    /// (AC4); success → split + launch + prefill + ManagedWorktree
    /// registration, then close.
    fn launch_pad_finish(
        &mut self,
        result: Result<std::path::PathBuf, LaunchPadCreationFailure>,
        mut plan: LaunchPlan,
        cx: &mut Context<Self>,
    ) {
        let worktree_path = match result {
            Ok(path) => path,
            Err(failure) => {
                if failure.checkout_may_remain {
                    self.launch_pad_set_error(
                        format!(
                            "{}; ownership reservation retained because the checkout may remain at {}",
                            failure.message,
                            plan.worktree_path.display()
                        ),
                        cx,
                    );
                    return;
                }
                let reservation = self
                    .pending_worktree_teardowns
                    .iter()
                    .position(|worktree| worktree.path == plan.worktree_path)
                    .map(|index| self.pending_worktree_teardowns.remove(index));
                self.publish_pending_worktree_teardowns();
                let mut reservation_retained = false;
                if !self.save_session_blocking(cx)
                    && let Some(reservation) = reservation
                {
                    reservation_retained = true;
                    self.pending_worktree_teardowns.push(reservation);
                    self.pending_worktree_teardowns = worktree::merge_managed_worktree_records(
                        std::mem::take(&mut self.pending_worktree_teardowns),
                    );
                    self.publish_pending_worktree_teardowns();
                }
                let message = if reservation_retained {
                    format!(
                        "{}; ownership reservation retained because its durable record could not be cleared",
                        failure.message
                    )
                } else {
                    failure.message
                };
                if self.launch_pad.is_some() {
                    self.launch_pad_set_error(message, cx);
                } else {
                    self.show_toast(format!("Launch Pad: {message}"), cx);
                }
                return;
            }
        };
        plan.worktree_path = worktree_path;

        let identity = match worktree::worktree_identity(&plan.worktree_path) {
            Ok(identity) => identity,
            Err(error) => {
                self.launch_pad_set_error(
                    format!(
                        "Could not authenticate created worktree {}; ownership reservation retained: {error}",
                        plan.worktree_path.display()
                    ),
                    cx,
                );
                return;
            }
        };
        let Some(reservation_idx) = self
            .pending_worktree_teardowns
            .iter()
            .position(|worktree| worktree.path == plan.worktree_path)
        else {
            self.launch_pad_set_error(
                "Worktree ownership reservation disappeared during creation",
                cx,
            );
            return;
        };
        self.pending_worktree_teardowns[reservation_idx].identity = Some(identity);

        let Some(ws_idx) = self.workspaces.iter().position(|w| w.id == plan.ws_id) else {
            // The durable reservation already owns this checkout. If its
            // target workspace vanished, retire it through the same guarded
            // path as every other orphaned managed worktree.
            let reserved: Vec<_> = self
                .pending_worktree_teardowns
                .iter()
                .filter(|worktree| worktree.path == plan.worktree_path)
                .cloned()
                .collect();
            log::warn!(
                "launch pad: workspace closed during worktree creation; retiring {}",
                plan.worktree_path.display()
            );
            self.show_toast(
                format!(
                    "Workspace closed - cleaning up worktree at {}",
                    plan.worktree_path.display()
                ),
                cx,
            );
            self.launch_pad = None;
            self.overlay_origin_pane = None;
            if self.save_session_blocking(cx) {
                self.spawn_persisted_worktree_teardown(reserved, cx);
            } else {
                self.show_toast(
                    "Workspace closed - worktree cleanup deferred until its ownership record can be saved",
                    cx,
                );
            }
            cx.notify();
            return;
        };

        let reservation = self.pending_worktree_teardowns.remove(reservation_idx);
        self.publish_pending_worktree_teardowns();
        // Transfer ownership from the durable reservation to the workspace in
        // one main-thread turn, then make that transfer durable before a pane
        // can start using the checkout.
        self.workspaces[ws_idx]
            .managed_worktrees
            .push(reservation.clone());
        if !self.save_session_blocking(cx) {
            self.workspaces[ws_idx]
                .managed_worktrees
                .retain(|worktree| worktree.path != plan.worktree_path);
            self.pending_worktree_teardowns.push(reservation.clone());
            self.pending_worktree_teardowns = worktree::merge_managed_worktree_records(
                std::mem::take(&mut self.pending_worktree_teardowns),
            );
            self.publish_pending_worktree_teardowns();
            self.launch_pad_set_error(
                format!(
                    "Could not persist worktree ownership; cleaning up {}",
                    plan.worktree_path.display()
                ),
                cx,
            );
            self.spawn_persisted_worktree_teardown(vec![reservation], cx);
            return;
        }

        // The active tab can change or become zoomed while the worktree is
        // created off-thread. Refuse before spawning a PTY into the temporary
        // zoom tree, but make the already-created checkout explicit.
        if self.workspaces[ws_idx].active_tab().is_zoomed() {
            self.launch_pad_set_error(
                format!(
                    "Unzoom before splitting panes - worktree created at {}",
                    plan.worktree_path.display()
                ),
                cx,
            );
            return;
        }

        // Re-check the pane budget - it may have filled during the run.
        if !self.workspaces[ws_idx].active_tab().can_add_pane() {
            self.launch_pad_set_error(
                format!(
                    "Maximum pane count reached ({MAX_PANES}) - worktree created at {}",
                    plan.worktree_path.display()
                ),
                cx,
            );
            return;
        }

        let target = self
            .launch_pad
            .as_ref()
            .and_then(|lp| lp.target.upgrade())
            .filter(|t| {
                self.workspaces[ws_idx]
                    .active_tab()
                    .root
                    .as_ref()
                    .is_some_and(|r| r.contains_leaf(t))
            });
        let new_terminal = cx.new(|cx| {
            TerminalView::with_cwd_env_and_profile(
                plan.ws_id,
                Some(plan.worktree_path.clone()),
                None,
                None,
                TerminalSurfaceProfile::Agent,
                cx,
            )
        });
        let new_pane = self.create_pane(new_terminal.clone(), plan.ws_id, cx);
        let tab = self.workspaces[ws_idx].active_tab_mut();
        if tab.root.is_none() && tab.saved_layout.is_none() {
            tab.root = Some(LayoutTree::Leaf(new_pane.clone()));
        } else {
            let Some(root) = tab.root.as_mut() else {
                self.launch_pad_set_error("Workspace has no layout root", cx);
                return;
            };
            // PRD: split in the active preset's direction, fallback Vertical.
            // No active preset is tracked anywhere (LayoutPreset is a one-shot
            // `workspace.up` input), so the documented fallback IS the default:
            // Vertical = side-by-side, the natural cockpit arrangement.
            match target {
                Some(t) => {
                    if !root.split_at_pane(&t, SplitDirection::Vertical, new_pane.clone()) {
                        root.split_first_leaf(SplitDirection::Vertical, new_pane.clone());
                    }
                }
                None => root.split_first_leaf(SplitDirection::Vertical, new_pane.clone()),
            }
        }

        new_terminal
            .read(cx)
            .send_command(&plan.agent.launch_command(&self.cached_config));
        // The plan already names the agent - declare it so the pane carries its
        // logo immediately instead of waiting for the per-pane scan.
        new_terminal.update(cx, |view, _cx| view.declare_agent(plan.agent));
        if !plan.prompt.trim().is_empty() {
            // Existing settle-poll: waits for the CLI to go quiet, then
            // pre-fills WITHOUT a carriage return - human-in-loop (FR-01).
            Self::schedule_prompt_prefill(&new_terminal, plan.prompt, usize::MAX, cx);
        }

        self.launch_pad = None;

        self.overlay_origin_pane = None;
        self.pending_pane_focus = Some(new_pane);
        self.activate_workspace_without_window(ws_idx, cx);
    }

    pub(crate) fn handle_launch_pad_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let Some(lp) = self.launch_pad.as_ref() else {
            return;
        };
        match key {
            "escape" => self.launch_pad_cancel(cx),
            "enter"
                if lp.issue_input.read(cx).focus_handle.is_focused(window)
                    && !lp.issue_input.read(cx).value().trim().is_empty() =>
            {
                self.load_launch_pad_issue(cx)
            }
            "enter" => self.launch_pad_confirm(cx),
            "tab" => {
                // Cycle focus through the three text fields (custom-buttons
                // modal convention). The agent list is mouse-driven.
                let branch_focused = lp.branch_input.read(cx).focus_handle.is_focused(window);
                let issue_focused = lp.issue_input.read(cx).focus_handle.is_focused(window);
                let next = if issue_focused {
                    lp.branch_input.read(cx).focus_handle.clone()
                } else if branch_focused {
                    lp.prompt_input.read(cx).focus_handle.clone()
                } else {
                    lp.issue_input.read(cx).focus_handle.clone()
                };
                window.focus(&next, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    pub(crate) fn render_launch_pad(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(lp) = self.launch_pad.as_ref() else {
            return div().into_any_element();
        };
        let ui = crate::theme::ui_colors();
        let running = lp.running || lp.issue_loading;

        // Agent picker: every entry of TerminalAgent::ALL, the missing ones
        // grayed out and inert (US-005 AC1).
        let mut agent_list = div()
            .id("launch-pad-agents")
            .flex()
            .flex_col()
            .max_h(px(180.))
            .overflow_y_scroll()
            .border_1()
            .border_color(ui.border)
            .rounded(px(6.));
        // Issue #518: while the first PATH walk is still running every row
        // reads as not installed; mark them as pending instead, and the boot
        // warm's `cx.notify()` repaints them once the walk publishes.
        let scan_pending = crate::agent_launcher::installed_binary_scan_pending();
        for (idx, agent) in TerminalAgent::ALL.iter().enumerate() {
            let installed = agent.is_installed();
            let is_selected = idx == lp.agent_idx;
            let resting_background = if is_selected {
                ui.subtle
            } else {
                ui.subtle.opacity(0.0)
            };
            let row = div()
                .id(SharedString::from(format!(
                    "launch-pad-agent-{}",
                    agent.tag()
                )))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(5.))
                .text_size(px(12.))
                .bg(resting_background)
                // Multi-color logos render via `img()` (resvg keeps every
                // native fill); monochrome logos stay a tinted `svg()` mask
                // - same split as the sidebar launcher.
                .child(if agent.icon_multicolor() {
                    gpui::img(agent.icon_path())
                        .size(px(13.))
                        .flex_none()
                        .when(!installed, |d| d.opacity(0.5))
                        .into_any_element()
                } else {
                    svg()
                        .size(px(13.))
                        .flex_none()
                        .path(agent.icon_path())
                        .text_color(if installed { ui.text } else { ui.muted })
                        .into_any_element()
                })
                .child(
                    div()
                        .flex_1()
                        .text_color(if installed { ui.text } else { ui.muted })
                        .when(!installed, |d| d.opacity(0.5))
                        .child(agent.display_name()),
                );
            if installed {
                agent_list = agent_list.child(
                    row.cursor_pointer()
                        .animated_hover(move |style, delta| {
                            style.bg(lerp_color(resting_background, ui.subtle, delta));
                        })
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                            if let Some(lp) = this.launch_pad.as_mut()
                                && !lp.running
                            {
                                lp.agent_idx = idx;
                                lp.agent_default_pending = false;
                                cx.notify();
                            }
                            cx.stop_propagation();
                        })),
                );
            } else {
                agent_list = agent_list.child(
                    row.child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(ui.muted)
                            .child(if scan_pending {
                                "looking"
                            } else {
                                "not installed"
                            }),
                    ),
                );
            }
        }
        let field_label =
            |label: &'static str| div().text_size(px(11.)).text_color(ui.muted).child(label);

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(16.))
            .py(px(10.))
            .child(field_label("Agent"))
            .child(agent_list)
            // Issue #518: a sibling of the scrolling list, not its last row,
            // so the pending copy is visible without scrolling 17 rows.
            .children(scan_pending.then(|| {
                div()
                    .text_size(px(11.))
                    .text_color(ui.muted)
                    .child(AGENT_SCAN_PENDING_COPY)
            }))
            .child(field_label("Start from GitHub issue"))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .border_1()
                            .border_color(ui.border)
                            .rounded(px(6.))
                            .px(px(8.))
                            .py(px(4.))
                            .child(lp.issue_input.clone()),
                    )
                    .child(
                        div()
                            .id("launch-issue-load")
                            .cursor_pointer()
                            .child(if lp.issue_loading {
                                "Loading…"
                            } else {
                                "Load issue"
                            })
                            .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
                                app.load_launch_pad_issue(cx)
                            })),
                    ),
            )
            .child(field_label("New branch"))
            .child(
                div()
                    .border_1()
                    .border_color(ui.border)
                    .rounded(px(6.))
                    .px(px(8.))
                    .py(px(4.))
                    .child(lp.branch_input.clone()),
            )
            .child(field_label("Prompt"))
            .child(
                div()
                    .border_1()
                    .border_color(ui.border)
                    .rounded(px(6.))
                    .px(px(8.))
                    .py(px(4.))
                    .max_h(px(140.))
                    .child(lp.prompt_input.clone()),
            );

        if let Some(err) = &lp.error {
            // AC4/AC7: git's failure verbatim - inert text, never parsed.
            body = body.child(
                div()
                    .text_size(px(11.))
                    .text_color(ui.vc_deleted)
                    .child(err.clone()),
            );
        }

        let confirm_label: SharedString = if lp.issue_loading {
            "Loading issue…".into()
        } else if running {
            "Creating…".into()
        } else {
            "Create worktree + launch".into()
        };
        let confirm_background = if running {
            ui.subtle
        } else {
            ui.accent.opacity(0.15)
        };
        let confirm_text = if running { ui.muted } else { ui.accent };
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(16.))
            .py(px(10.))
            .border_t_1()
            .border_color(ui.border)
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child("Enter: load or create · Tab: fields · Esc: cancel"),
            )
            .child(
                div()
                    .id("launch-pad-confirm")
                    .px(px(12.))
                    .py(px(5.))
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .bg(confirm_background)
                    .text_color(confirm_text)
                    .when(!running, |d| d.cursor_pointer())
                    .animated_hover(move |style, delta| {
                        let hovered_opacity = if running { 1.0 } else { 0.8 };
                        style.opacity(1.0 + (hovered_opacity - 1.0) * delta);
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                        this.launch_pad_confirm(cx);
                        cx.stop_propagation();
                    }))
                    .child(confirm_label),
            );

        let card = div()
            .id("launch-pad")
            .occlude()
            .track_focus(&self.launch_pad_focus)
            .on_key_down(cx.listener(Self::handle_launch_pad_key_down))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.launch_pad_cancel(cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .w(px(520.))
            .flex()
            .flex_col()
            .bg(ui.overlay)
            .border_1()
            .border_color(ui.border)
            .rounded(px(10.))
            .overflow_hidden()
            .child(
                div()
                    .px(px(16.))
                    .pt(px(14.))
                    .pb(px(6.))
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(ui.text)
                    .child("Launch Pad"),
            )
            .child(body)
            .child(footer);

        deferred(
            div()
                .id("launch-pad-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(72.))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(card),
        )
        .with_priority(8)
        .into_any_element()
    }
}

/// First installed row of [`TerminalAgent::ALL`], or 0 when none is (the
/// row renders grayed and confirm rejects).
fn default_agent_idx(installed: impl Fn(TerminalAgent) -> bool) -> usize {
    TerminalAgent::ALL
        .iter()
        .position(|a| installed(*a))
        .unwrap_or(0)
}

/// Settle a provisional default once the first PATH walk has published
/// (issue #518). Only a pad still flagged `pending` moves, and only when a
/// different installed agent exists; the flag clears either way, so a
/// later user click is never overridden. Returns `true` when `agent_idx`
/// changed.
fn settle_default_agent(
    agent_idx: &mut usize,
    pending: &mut bool,
    installed: impl Fn(TerminalAgent) -> bool,
) -> bool {
    if !*pending {
        return false;
    }
    *pending = false;
    let settled = default_agent_idx(installed);
    if settled == *agent_idx {
        return false;
    }
    *agent_idx = settled;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #518: neither confirm path may wait for the first PATH walk on
    /// the GPUI thread (a slow PATH entry would freeze the window), and
    /// neither may drop the Enter: a confirm during the walk is queued and
    /// the boot warm's completion replays it with the real answer.
    #[test]
    fn confirm_paths_never_block_on_the_cold_walk_and_are_replayed_when_it_lands() {
        let pad = include_str!("launch_pad.rs");
        let confirm = pad
            .split("fn launch_pad_submit(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("launch_pad_submit exists");
        assert!(
            !confirm.contains("is_installed_now()"),
            "launch_pad_submit must not wait on the GPUI thread: {confirm}"
        );
        assert!(
            confirm.contains("lp.confirm_queued = Some(QueuedConfirm {"),
            "launch_pad_submit queues a confirm made during the walk: {confirm}"
        );
        let resume = pad
            .split("pub(crate) fn launch_pad_resume_queued_confirm(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("launch_pad_resume_queued_confirm exists");
        assert!(
            resume.contains(
                "self.launch_pad_submit(queued.agent_idx, queued.branch, queued.prompt, cx)"
            ) && !resume.contains("launch_pad_confirm("),
            "the replay submits the snapshot, never a re-read of the live form: {resume}"
        );
        assert!(
            confirm.contains("lp.agent_default_pending = false;"),
            "a queued confirm commits the selected row, so the settle must not move it: {confirm}"
        );
        let queue_at = confirm
            .find("lp.confirm_queued = Some(QueuedConfirm {")
            .expect("confirm queues");
        for guard in [
            "branch.is_empty()",
            "\"Workspace was closed\"",
            "ws.repo_root.clone()",
            "is_zoomed()",
            "can_add_pane()",
        ] {
            let at = confirm
                .find(guard)
                .unwrap_or_else(|| panic!("confirm keeps the `{guard}` guard"));
            assert!(
                at < queue_at,
                "`{guard}` must be checked before a confirm can be queued, or an invalid \
                 form edited during the walk is replayed without another Enter: {confirm}"
            );
        }
        let pending_at = confirm
            .find("installed_binary_scan_pending()")
            .expect("confirm reads the pending flag");
        let snapshot_at = confirm
            .find("agent.is_installed()")
            .expect("confirm reads the snapshot");
        assert!(
            pending_at < snapshot_at,
            "the pending flag is read before the snapshot: {confirm}"
        );

        let settings = include_str!("../settings/tabs/workspaces.rs");
        assert!(
            !settings.contains("visible_now("),
            "Settings click handlers read the snapshot, never the blocking lookup"
        );
        for site in [
            "fn add_workspace_template_pane(",
            "fn set_workspace_template_pane_kind(",
            "PaneKind::Agent => {\n                pane.command = None;\n                pane.prompt = (!prompt.is_empty())",
        ] {
            let body = settings
                .split(site)
                .nth(1)
                .and_then(|rest| rest.split("\n    }\n").next())
                .unwrap_or_else(|| panic!("`{site}` exists"));
            assert!(
                body.contains("installed_binary_scan_pending()")
                    && body.contains("AGENT_SCAN_PENDING_COPY"),
                "`{site}` must refuse with the looking copy while the walk is pending: {body}"
            );
            let pending_at = body
                .find("installed_binary_scan_pending()")
                .expect("checked above");
            let snapshot_at = body
                .find("TerminalAgent::visible(")
                .unwrap_or_else(|| panic!("`{site}` reads the snapshot"));
            assert!(
                pending_at < snapshot_at,
                "`{site}` reads the pending flag before the snapshot, or a publish between \
                 the two locks bypasses the guard: {body}"
            );
        }

        let palette = include_str!("pane_palette.rs");
        let launchable = palette
            .split("fn ensure_launchable(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("ensure_launchable exists");
        assert!(
            !launchable.contains("is_installed_now()"),
            "ensure_launchable must not wait on the GPUI thread: {launchable}"
        );
        let launch = palette
            .split("pub(crate) fn pane_palette_launch(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("pane_palette_launch exists");
        assert!(
            launch.contains("palette.launch_queued = Some(preset.clone());"),
            "pane_palette_launch queues a launch made during the walk: {launch}"
        );

        let boot = include_str!("bootstrap.rs");
        let warm = boot
            .split("smol::unblock(crate::agent_launcher::refresh_installed_binaries).await;")
            .nth(1)
            .and_then(|rest| rest.split(".detach();").next())
            .expect("the boot warm awaits the first walk");
        for replay in [
            "app.launch_pad_resume_queued_confirm(cx);",
            "app.pane_palette_resume_queued_launch(cx);",
        ] {
            assert!(
                warm.contains(replay),
                "the boot warm's completion must replay queued confirms: missing `{replay}` in {warm}"
            );
        }
    }

    /// Issue #518: a pad opened during the first PATH walk defaults to row
    /// 0 provisionally; the boot warm's completion moves it to the first
    /// installed agent, and a user pick made in the meantime is kept.
    #[test]
    fn pending_default_agent_settles_on_first_installed_once_the_walk_publishes() {
        let installed = |a: TerminalAgent| a == TerminalAgent::Codex;
        let codex = TerminalAgent::ALL
            .iter()
            .position(|a| *a == TerminalAgent::Codex)
            .expect("codex row");
        assert_ne!(
            codex, 0,
            "the fixture must not coincide with the fallback row"
        );

        // Cold open: nothing installed yet, row 0 is provisional.
        let mut idx = default_agent_idx(|_| false);
        let mut pending = true;
        assert_eq!(idx, 0);
        assert!(settle_default_agent(&mut idx, &mut pending, installed));
        assert_eq!(idx, codex);
        assert!(!pending);

        // A second settle is a no-op: the flag is spent.
        assert!(!settle_default_agent(&mut idx, &mut pending, |_| false));
        assert_eq!(idx, codex);

        // The user clicked a row while the walk ran: keep it.
        let (mut idx, mut pending) = (3, false);
        assert!(!settle_default_agent(&mut idx, &mut pending, installed));
        assert_eq!(idx, 3);

        // The walk found nothing: row 0 stays, the flag still clears.
        let (mut idx, mut pending) = (0, true);
        assert!(!settle_default_agent(&mut idx, &mut pending, |_| false));
        assert_eq!(idx, 0);
        assert!(!pending);

        // A warm open never flags: the default is already the real one.
        assert_eq!(default_agent_idx(installed), codex);
    }

    #[test]
    fn launch_pad_refuses_split_when_zoomed() {
        let src = include_str!("launch_pad.rs");
        let confirm = src
            .split("pub(crate) fn launch_pad_confirm(")
            .nth(1)
            .and_then(|rest| rest.split("fn launch_pad_begin_creation(").next())
            .expect("launch_pad_confirm body");
        let zoom_guard = confirm
            .find("ws.active_tab().is_zoomed()")
            .expect("Launch Pad must refuse zoom before starting worktree creation");
        let start = confirm
            .find("lp.running = true")
            .expect("Launch Pad running transition");
        assert!(
            zoom_guard < start,
            "zoom refusal must happen before worktree creation starts: {confirm}"
        );
        assert!(
            confirm[zoom_guard..start].contains("Unzoom before splitting panes"),
            "the early refusal must use the standard split message"
        );

        let finish = src
            .split("fn launch_pad_finish(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn handle_launch_pad_key_down")
                    .next()
            })
            .expect("launch_pad_finish body");
        let late_guard = finish
            .find("active_tab().is_zoomed()")
            .expect("completion must re-check zoom after asynchronous worktree creation");
        let terminal = finish
            .find("let new_terminal")
            .expect("terminal creation site");
        assert!(
            late_guard < terminal,
            "a tab may become zoomed during creation, so re-check before spawning its PTY"
        );
        let refusal = &finish[late_guard..terminal];
        assert!(
            refusal.contains("Unzoom before splitting panes")
                && refusal.contains("worktree created at"),
            "a late refusal must explain both the required action and the checkout left behind"
        );
    }

    #[test]
    fn launch_pad_plan_uses_hashed_path_when_slug_path_is_claimed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir(&repo_root).expect("repo dir");
        assert!(test_git(&repo_root, &["init"]), "git fixture required");
        let repo_root = std::fs::canonicalize(&repo_root).expect("canonicalize repo");
        assert!(test_git(&repo_root, &["config", "core.autocrlf", "false"]));
        std::fs::write(repo_root.join("README.md"), "init\n").expect("readme");
        assert!(test_git(&repo_root, &["add", "README.md"]));
        assert!(test_git(
            &repo_root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "-m",
                "init",
            ],
        ));

        // `feat/a b` / `feat/a-b` slug-collide, but git rejects the space in a
        // ref name; `feat-a-b` is the valid occupier of that same slug path.
        let branch_a = "feat-a-b";
        let branch_b = "feat/a-b";
        let legacy = worktree::worktree_dir(&repo_root, branch_a);
        std::fs::create_dir_all(legacy.parent().expect("worktree parent")).expect("parent dir");
        assert!(
            test_git(
                &repo_root,
                &[
                    "worktree",
                    "add",
                    legacy.to_str().expect("utf8 path"),
                    "-b",
                    branch_a,
                ],
            ),
            "git fixture required"
        );

        let (path, create_branch) = launch_pad_worktree_plan(&repo_root, branch_b).expect("plan");
        assert_eq!(path, worktree::worktree_dir_hashed(&repo_root, branch_b));
        assert!(create_branch);
    }

    #[test]
    fn exact_planned_path_is_reserved_before_creation_and_never_disarmed() {
        let src = include_str!("launch_pad.rs");
        let begin = src
            .split("fn launch_pad_begin_creation(")
            .nth(1)
            .and_then(|rest| rest.split("/// Main-thread completion").next())
            .expect("main-thread reservation stage");
        let exact_path_at = begin
            .find("plan.worktree_path = worktree_path.clone()")
            .expect("exact collision-resolved path");
        let reserve_at = begin
            .find("self.pending_worktree_teardowns.push(reservation)")
            .expect("in-flight reservation");
        let persist_at = begin
            .find("self.save_session_blocking(cx)")
            .expect("durable reservation");
        let create_at = begin.find("cx.spawn(").expect("creation worker");
        assert!(
            exact_path_at < reserve_at && reserve_at < persist_at && persist_at < create_at,
            "the exact path must be durably reserved before worktree creation: {begin}"
        );

        let finish = src
            .split("fn launch_pad_finish(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn handle_launch_pad_key_down")
                    .next()
            })
            .expect("completion stage");
        assert!(
            !finish.contains("remove_file") && !finish.contains("owner_marker_path"),
            "completion must never unlink a marker that another owner may have claimed: {finish}"
        );
    }

    #[test]
    fn failed_add_with_registered_markerless_checkout_keeps_reservation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir(&repo_root).expect("repo dir");
        assert!(test_git(&repo_root, &["init"]), "git fixture required");
        std::fs::write(repo_root.join("README.md"), "init\n").expect("readme");
        assert!(test_git(&repo_root, &["add", "README.md"]));
        assert!(test_git(
            &repo_root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "-m",
                "init",
            ],
        ));
        let branch = "feat/failed-marker-rollback";
        let path = worktree::worktree_dir(&repo_root, branch);
        std::fs::create_dir_all(path.parent().expect("worktree parent")).expect("parent dir");
        assert!(test_git(
            &repo_root,
            &[
                "worktree",
                "add",
                path.to_str().expect("utf8 path"),
                "-b",
                branch,
            ],
        ));
        assert!(
            worktree_checkout_may_exist(&repo_root, &path),
            "a failed rollback's registered markerless checkout must retain lifecycle evidence"
        );

        let src = include_str!("launch_pad.rs");
        let finish = src
            .split("fn launch_pad_finish(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn handle_launch_pad_key_down")
                    .next()
            })
            .expect("completion stage");
        let retain_at = finish
            .find("if failure.checkout_may_remain")
            .expect("may-remain failure arm");
        let remove_at = finish
            .find("self.pending_worktree_teardowns.remove(index)")
            .expect("proven-absent release arm");
        let retain_arm = &finish[retain_at..remove_at];
        assert!(
            retain_arm.contains("ownership reservation retained") && retain_arm.contains("return;"),
            "a possibly registered checkout must return before clearing its reservation: {retain_arm}"
        );
    }

    #[test]
    fn worktree_checkout_may_exist_is_true_when_list_worktrees_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("not-a-repo");
        std::fs::create_dir(&repo_root).expect("non-repo dir");
        let missing = tmp.path().join("missing-checkout");
        assert!(
            !missing.exists(),
            "path must be absent so try_exists does not short-circuit the git list"
        );
        assert!(
            worktree::list_worktrees(&repo_root).is_err(),
            "non-repo root must make list_worktrees error so the fail-closed arm runs"
        );
        assert!(
            worktree_checkout_may_exist(&repo_root, &missing),
            "a git list failure must fail-closed and keep the reservation"
        );
    }

    #[test]
    fn worktree_checkout_may_exist_is_false_for_missing_path_in_real_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir(&repo_root).expect("repo dir");
        assert!(test_git(&repo_root, &["init"]), "git fixture required");
        let missing = tmp.path().join("missing-checkout");
        assert!(
            !missing.exists(),
            "path must be absent so try_exists does not short-circuit the git list"
        );
        let entries = worktree::list_worktrees(&repo_root).expect("list worktrees");
        assert!(
            entries.iter().all(|entry| entry.path != missing),
            "fixture must not register the missing path"
        );
        assert!(
            !worktree_checkout_may_exist(&repo_root, &missing),
            "a missing path with a successful empty match must not keep the reservation"
        );
    }

    fn test_git(cwd: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }
}
