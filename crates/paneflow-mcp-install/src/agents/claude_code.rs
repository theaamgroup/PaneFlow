//! Claude Code writer.
//!
//! PaneFlow registered the bridge in the user-scope `~/.claude.json` (or
//! `$CLAUDE_CONFIG_DIR/.claude.json`) at `mcpServers.paneflow`, as
//! `{type: "stdio", command, args: []}` with no `env` block. Removal is a
//! direct edit under [`crate::io`]'s lock: every other key and sibling server
//! stays, and the old bytes land in `.claude.json.bak` first. Claude Code
//! rewrites this file itself without PaneFlow's lock, so the file is re-read
//! right before the rename and the write is refused when it no longer holds
//! the parsed bytes. That narrows the race with Claude Code's own writes but
//! does not close it (see [`crate::io`]).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::agents::{support, AgentConfigWriter, StatusOutcome, UninstallOutcome};

const CONTAINER: &str = "mcpServers";

pub struct ClaudeCode {
    config_path: Option<PathBuf>,
}

impl ClaudeCode {
    /// A writer for the config at `config_path`; `None` when the home
    /// directory could not be resolved.
    #[must_use]
    pub fn new(config_path: Option<PathBuf>) -> Self {
        Self { config_path }
    }

    /// A writer for the config at `path`, for tests.
    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            config_path: Some(path),
        }
    }

    fn path(&self) -> Result<&Path> {
        self.config_path
            .as_deref()
            .ok_or_else(|| anyhow!("cannot resolve home dir for ~/.claude.json"))
    }

    /// The entry PaneFlow wrote, so `status` can tell it from a hand edit.
    fn entry(bridge: &str) -> serde_json::Value {
        json!({ "type": "stdio", "command": bridge, "args": [] })
    }

    fn validate_entry(entry: &serde_json::Value, expected: Option<&Path>) -> StatusOutcome {
        let found = support::string_command(entry);
        let shape_ok = found
            .as_deref()
            .is_some_and(|path| *entry == Self::entry(path));
        support::classify_entry(
            found,
            expected,
            shape_ok,
            "Claude Code MCP entry must be stdio, have empty args, and no env block",
        )
    }
}

impl AgentConfigWriter for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        // A present-but-unparseable `~/.claude.json` is an error (US-021),
        // never "nothing to remove": `json_uninstall` parses it under the
        // lock and refuses to touch it.
        support::json_uninstall(self.path()?, CONTAINER, support::string_command)
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        support::json_status(self.path()?, CONTAINER, bridge, Self::validate_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_needs_repair_when_shape_differs() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "mcpServers": {
                    "paneflow": {
                        "type": "stdio",
                        "command": "/data/paneflow-mcp",
                        "args": [],
                        "env": { "SHOULD_NOT_BE_HERE": "1" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = ClaudeCode::at(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn uninstall_malformed_config_is_error() {
        // US-021: a present-but-unparseable config is corruption, not
        // "nothing to remove" - surface a loud error so the user fixes it
        // rather than silently believing the entry was already gone.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        std::fs::write(&p, b"{ broken").unwrap();
        let w = ClaudeCode::at(p.clone());
        assert!(
            w.uninstall().is_err(),
            "uninstall on a malformed present config must error, not return NothingToRemove"
        );
        // The invalid file was NOT overwritten.
        assert_eq!(std::fs::read(&p).unwrap(), b"{ broken");
    }

    #[test]
    fn uninstall_absent_config_is_nothing_to_remove() {
        // Counterpart to the malformed case: a genuinely absent file is a
        // clean NothingToRemove, not an error.
        let dir = tempfile::TempDir::new().unwrap();
        let w = ClaudeCode::at(dir.path().join("missing.json"));
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::NothingToRemove);
    }

    #[test]
    fn uninstall_preserves_top_level_keys_and_siblings() {
        // The direct removal drops only `mcpServers.paneflow`; unrelated
        // Claude state and sibling servers stay, and `.bak` holds the exact
        // bytes from before the uninstall.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        let before = serde_json::to_vec(&json!({
            "numStartups": 42,
            "mcpServers": {
                "github": { "command": "gh-mcp" },
                "paneflow": ClaudeCode::entry("/data/paneflow-mcp")
            }
        }))
        .unwrap();
        std::fs::write(&p, &before).unwrap();
        let w = ClaudeCode::at(p.clone());

        assert_eq!(
            w.uninstall().unwrap(),
            UninstallOutcome::Removed {
                file: p.clone(),
                backup: dir.path().join(".claude.json.bak"),
            }
        );
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(42));
        assert_eq!(v["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert!(
            v["mcpServers"].get("paneflow").is_none(),
            "paneflow entry must be gone: {v}"
        );
        assert_eq!(
            std::fs::read(dir.path().join(".claude.json.bak")).unwrap(),
            before
        );
    }

    #[test]
    fn uninstall_then_status_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "mcpServers": { "paneflow": ClaudeCode::entry("/data/paneflow-mcp") }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = ClaudeCode::at(p);
        assert_eq!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed {
                path: "/data/paneflow-mcp".into()
            }
        );
        assert!(matches!(
            w.uninstall().unwrap(),
            UninstallOutcome::Removed { .. }
        ));
        assert_eq!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NotInstalled
        );
    }
}
