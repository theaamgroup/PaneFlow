//! Claude Code writer (EP-003 US-007).
//!
//! Direct merge into the user-scope `~/.claude.json` (or
//! `$CLAUDE_CONFIG_DIR/.claude.json`) at `mcpServers.paneflow`, holding
//! [`crate::io::ConfigLock`], as the Gemini writer does. Every other key and
//! sibling server stays, and a write that changes the file copies the old
//! bytes to `.claude.json.bak` first. No `claude` process is spawned: the
//! entry `claude mcp add` writes carries an `"env": {}` block that `status`
//! reports as needing repair, so the direct edit wrote the final bytes on
//! every install anyway (issue #847).
//!
//! The entry carries **no `env` block** (PRD D5): the bridge inherits
//! `PANEFLOW_SOCKET_PATH` from the pane it runs in. Per 2026 verification
//! the entry also carries `type: "stdio"`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::agents::{support, AgentConfigWriter, InstallOutcome, StatusOutcome, UninstallOutcome};
use crate::detect::{self, Presence};

const CLI: &str = "claude";
const CONTAINER: &str = "mcpServers";

pub struct ClaudeCode {
    config_path: Option<PathBuf>,
}

impl ClaudeCode {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config_path: support::claude_config(),
        }
    }

    fn path(&self) -> Result<&Path> {
        self.config_path
            .as_deref()
            .ok_or_else(|| anyhow!("cannot resolve home dir for ~/.claude.json"))
    }

    fn entry(bridge: &str) -> serde_json::Value {
        // No `env` (D5). `type: "stdio"` matches what `claude mcp add` writes.
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

impl Default for ClaudeCode {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentConfigWriter for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn presence(&self) -> Presence {
        let paths: Vec<PathBuf> = self.config_path.clone().into_iter().collect();
        detect::detect(Some(CLI), &paths)
    }

    fn install(&self, bridge: &Path) -> Result<InstallOutcome> {
        let bridge_s = bridge.to_string_lossy().into_owned();
        support::json_install(self.path()?, CONTAINER, Self::entry(&bridge_s))
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        // A present-but-unparseable `~/.claude.json` is an error (US-021),
        // never "nothing to remove": `json_uninstall` parses it under the
        // lock and refuses to touch it.
        support::json_uninstall(self.path()?, CONTAINER)
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        support::json_status(self.path()?, CONTAINER, bridge, Self::validate_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_writer(path: PathBuf) -> ClaudeCode {
        ClaudeCode {
            config_path: Some(path),
        }
    }

    #[test]
    fn install_writes_stdio_entry_without_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        let w = test_writer(p.clone());

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let entry = &v["mcpServers"]["paneflow"];
        assert_eq!(entry["type"], json!("stdio"));
        assert_eq!(entry["command"], json!("/data/paneflow-mcp"));
        assert_eq!(entry["args"], json!([]));
        assert!(
            entry.get("env").is_none(),
            "D5: entry must carry no env block"
        );
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let w = test_writer(dir.path().join(".claude.json"));
        w.install(Path::new("/data/paneflow-mcp")).unwrap();
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
    }

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
        let w = test_writer(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn install_preserves_unrelated_claude_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "numStartups": 42,
                "mcpServers": { "github": { "command": "gh-mcp" } }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = test_writer(p.clone());
        w.install(Path::new("/data/paneflow-mcp")).unwrap();

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(42));
        assert_eq!(v["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert_eq!(
            v["mcpServers"]["paneflow"]["command"],
            json!("/data/paneflow-mcp")
        );
    }

    #[test]
    fn uninstall_malformed_config_is_error() {
        // US-021: a present-but-unparseable config is corruption, not
        // "nothing to remove" - surface a loud error so the user fixes it
        // rather than silently believing the entry was already gone.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        std::fs::write(&p, b"{ broken").unwrap();
        let w = test_writer(p.clone());
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
        let w = test_writer(dir.path().join("missing.json"));
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
        let w = test_writer(p.clone());

        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::Removed);
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
    fn install_updates_stale_entry_in_place() {
        // A stale entry is rewritten in place by the locked merge, never
        // removed first. Unrelated Claude state and sibling servers stay,
        // and `.bak` holds the bytes from before the write.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        let before = serde_json::to_vec(&json!({
            "numStartups": 42,
            "mcpServers": {
                "github": { "command": "gh-mcp" },
                "paneflow": {
                    "type": "stdio",
                    "command": "/old/paneflow-mcp",
                    "args": []
                }
            }
        }))
        .unwrap();
        std::fs::write(&p, &before).unwrap();
        let w = test_writer(p.clone());

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Updated
        );
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(42));
        assert_eq!(v["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert_eq!(
            v["mcpServers"]["paneflow"],
            ClaudeCode::entry("/data/paneflow-mcp")
        );
        assert_eq!(
            std::fs::read(dir.path().join(".claude.json.bak")).unwrap(),
            before
        );
    }

    #[test]
    fn install_repairs_empty_env_block_from_claude_mcp_add() {
        // `claude mcp add -s user --transport stdio` (2.1.283) writes the
        // entry with `"env": {}`. Status flags it, and install replaces it
        // with the managed entry (issue #847).
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "numStartups": 42,
                "mcpServers": {
                    "github": { "command": "gh-mcp" },
                    "paneflow": {
                        "type": "stdio",
                        "command": "/data/paneflow-mcp",
                        "args": [],
                        "env": {}
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = test_writer(p.clone());
        let bridge = Path::new("/data/paneflow-mcp");

        assert!(matches!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
        assert_eq!(w.install(bridge).unwrap(), InstallOutcome::Updated);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["paneflow"],
            ClaudeCode::entry("/data/paneflow-mcp"),
            "install must replace the entry `claude mcp add` wrote"
        );
        assert_eq!(v["numStartups"], json!(42));
        assert_eq!(v["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert_eq!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::Installed {
                path: "/data/paneflow-mcp".into()
            }
        );
    }

    #[test]
    fn uninstall_then_status_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        let w = test_writer(p);
        w.install(Path::new("/data/paneflow-mcp")).unwrap();
        assert_eq!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed {
                path: "/data/paneflow-mcp".into()
            }
        );
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::Removed);
        assert_eq!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NotInstalled
        );
    }
}
