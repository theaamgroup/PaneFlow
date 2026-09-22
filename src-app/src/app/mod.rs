//! App-layer modules extracted from `main.rs`.
//!
//! See `tasks/prd-src-app-refactor.md` for the ongoing decomposition plan.

pub mod about_dialog;
pub mod actions;
pub(crate) mod agent_context;
pub(crate) mod agent_status;
pub mod bootstrap;
pub mod broadcast;
pub mod cli_diff_dock;
pub mod close_confirm;
pub mod close_guard;
pub mod command_palette;
pub mod composer;
pub mod constants;
pub mod custom_buttons_modal;
pub mod diff_dock;
pub mod diff_sidebar;
pub mod drag;
pub mod event_handlers;
pub mod ipc_handler;
pub mod notifications;
pub(crate) mod overlay_origin;
pub mod pane_overview;
pub mod pane_palette;
pub mod review;
pub mod session;
pub mod sessions_context_menu;
pub mod sessions_handoff;
pub mod sessions_sidebar;
pub mod settings;
pub mod sidebar;
pub mod sidebar_actions_menu;
pub mod system_info_dialog;
pub mod tab_worktree;
pub mod workspace_ops;

pub(crate) mod work_review;
