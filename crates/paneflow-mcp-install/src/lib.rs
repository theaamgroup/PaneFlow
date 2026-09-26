//! `paneflow-mcp-install` - the one-time cleanup of the retired MCP bridge.
//!
//! Issue #857 removed the `paneflow-mcp` bridge. This crate exists only so
//! PaneFlow can uninstall it from users' machines: on GUI launch,
//! [`remove_legacy_bridge`] removes the `paneflow` MCP entry PaneFlow wrote
//! into Claude Code, Codex, Gemini CLI and opencode, then deletes the
//! extracted binary. It ships for at least two releases so Sparkle's updates
//! reach nearly everyone, and then issue #868
//! (<https://github.com/theaamgroup/paneflow/issues/868>) deletes this crate,
//! `paneflow-agent-config`'s JSONC editor, the `toml_edit` dependency and
//! `runtime_paths::bridge_binary_path`.
//!
//! Layering:
//! - [`cleanup`] - [`remove_legacy_bridge`]: probe each agent, remove only
//!   entries whose command runs a `paneflow-mcp` binary, delete the binary
//!   once no entry points at one.
//! - `agents` - one writer per agent config file: config-path resolution
//!   ([`AgentConfigEnv`]), a read-only `status` probe and an `uninstall` that
//!   removes the entry.
//! - `merge` - safe JSON / JSONC / TOML parse and entry removal.
//! - `io` - bounded reads, a raw byte scan, the config lock, a backup that
//!   never replaces a file, and an atomic write that re-reads the file right
//!   before the rename and refuses when its bytes changed.
//!
//! The whole crate is panic-free in non-test paths (workspace lints:
//! `panic = deny`, `unwrap_used`/`expect_used` = warn). Errors flow through
//! `anyhow::Result` and are reported per agent, so one failing agent never
//! aborts the others.

// Integration tests are compiled as a separate crate, so the `clippy.toml`
// `allow-*-in-tests` keys do not reach them (clippy #13981). Belt-and-suspenders.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod agents;
pub mod cleanup;
mod io;
mod merge;

pub use agents::AgentConfigEnv;
pub use cleanup::{remove_legacy_bridge, CleanupReport};
