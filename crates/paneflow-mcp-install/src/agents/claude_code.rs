//! Claude Code writer (EP-003 US-007).
//!
//! Preferred path: shell out to `claude mcp add -s user --transport stdio
//! paneflow -- <bridge>` when the `claude` CLI is on PATH - it owns the
//! schema and writes user-scope servers to `~/.claude.json`. Do **not**
//! `mcp remove` first: a crash between remove and add used to drop the
//! paneflow entry while the rest of the file stayed intact, and holding
//! [`crate::io::ConfigLock`] across the CLI does not serialize `claude`.
//! After a successful add, the on-disk file is verified; a mismatch or a
//! failed add falls back to a locked merge into `~/.claude.json` under
//! `mcpServers.paneflow`. Fallback also runs when `claude` is absent.
//!
//! The entry carries **no `env` block** (PRD D5): the bridge inherits
//! `PANEFLOW_SOCKET_PATH` from the pane it runs in. Per 2026 verification
//! the entry also carries `type: "stdio"`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::agents::{support, AgentConfigWriter, InstallOutcome, StatusOutcome, UninstallOutcome};
use crate::detect::{self, Presence};
use crate::{io, merge};

const CLI: &str = "claude";
const CONTAINER: &str = "mcpServers";

#[cfg(test)]
type CliHook = Box<dyn Fn(&[&str]) -> Result<()>>;

pub struct ClaudeCode {
    config_path: Option<PathBuf>,
    /// Whether shell-out to the `claude` CLI is permitted. Always true in
    /// production; forced false in unit tests so they never mutate the
    /// developer's real `~/.claude.json` via a real `claude` on PATH.
    allow_cli: bool,
    /// Test-only stand-in for `claude`. When set, install/uninstall never
    /// consult PATH or spawn a real process.
    #[cfg(test)]
    cli: Option<CliHook>,
}

impl ClaudeCode {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config_path: support::claude_config(),
            allow_cli: true,
            #[cfg(test)]
            cli: None,
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

    fn cli_available(&self) -> bool {
        #[cfg(test)]
        if self.cli.is_some() {
            return true;
        }
        self.allow_cli && support::cli_on_path(CLI)
    }

    fn invoke_cli(&self, args: &[&str]) -> Result<()> {
        #[cfg(test)]
        if let Some(cli) = &self.cli {
            return cli(args);
        }
        support::shell_out(CLI, args)
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
        let cli = if self.allow_cli { Some(CLI) } else { None };
        let paths: Vec<PathBuf> = self.config_path.clone().into_iter().collect();
        detect::detect(cli, &paths)
    }

    fn install(&self, bridge: &Path) -> Result<InstallOutcome> {
        let path = self.path()?;
        let bridge_s = bridge.to_string_lossy().into_owned();

        // Idempotency + update detection via the same file the CLI writes.
        // This validates the whole managed entry, not just the command path.
        let status = support::json_status(path, CONTAINER, Some(bridge), Self::validate_entry)?;
        if matches!(status, StatusOutcome::Installed { .. }) {
            return Ok(InstallOutcome::AlreadyCurrent);
        }
        let had_prior = support::json_entry_present(path, CONTAINER)?;

        if self.cli_available() {
            io::backup(path)?;
            // Skip `mcp remove`. A crash between remove and add used to
            // drop `mcpServers.paneflow` while the rest of the file stayed
            // intact. `add` is attempted as-is; a non-idempotent CLI (entry
            // already present) fails here and the locked merge repairs it.
            match self.invoke_cli(&[
                "mcp",
                "add",
                "-s",
                "user",
                "--transport",
                "stdio",
                "paneflow",
                "--",
                &bridge_s,
            ]) {
                Ok(()) => match self.status(Some(bridge))? {
                    StatusOutcome::Installed { .. } => {
                        return Ok(if had_prior {
                            InstallOutcome::Updated
                        } else {
                            InstallOutcome::Installed
                        });
                    }
                    _ => {
                        log::warn!(
                            "paneflow mcp: `claude mcp add` exited 0 but ~/.claude.json does not match the managed entry; falling back to direct merge"
                        );
                    }
                },
                Err(e) => {
                    log::warn!(
                        "paneflow mcp: `claude mcp add` failed ({e:#}); falling back to direct ~/.claude.json merge"
                    );
                }
            }
        }

        support::json_install(path, CONTAINER, Self::entry(&bridge_s))
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        let path = self.path()?;
        // US-021: a present-but-unparseable `~/.claude.json` must surface a
        // loud error, not be silently mistaken for "nothing to remove". The
        // tolerant `current_json_command` below swallows parse failures
        // (`.ok()?` → None), so probe parseability first - `read_json_or_default`
        // is `Err` on a present malformed file and `Ok` (skeleton) when absent.
        if path.exists() {
            merge::read_json_or_default(path)?;
        }
        if !support::json_entry_present(path, CONTAINER)? {
            return Ok(UninstallOutcome::NothingToRemove);
        }
        if self.cli_available() {
            io::backup(path)?;
            if let Ok(()) = self.invoke_cli(&["mcp", "remove", "paneflow"]) {
                return Ok(UninstallOutcome::Removed);
            }
        }
        support::json_uninstall(path, CONTAINER)
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        support::json_status(self.path()?, CONTAINER, bridge, Self::validate_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn test_writer(path: PathBuf) -> ClaudeCode {
        ClaudeCode {
            config_path: Some(path),
            allow_cli: false, // never shell out to a real `claude` in tests
            cli: None,
        }
    }

    fn record_args(calls: &Rc<RefCell<Vec<Vec<String>>>>, args: &[&str]) {
        calls
            .borrow_mut()
            .push(args.iter().map(|s| (*s).to_string()).collect());
    }

    fn assert_add_without_remove(calls: &[Vec<String>]) {
        assert!(
            calls.iter().all(|args| !args.iter().any(|a| a == "remove")),
            "install must not call mcp remove: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|args| args.windows(2).any(|w| w == ["mcp", "add"])),
            "expected mcp add: {calls:?}"
        );
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
    fn install_cli_add_skips_remove_and_verifies_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        let dest = p.clone();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = ClaudeCode {
            config_path: Some(p.clone()),
            allow_cli: true,
            cli: Some(Box::new(move |args| {
                record_args(&calls_h, args);
                let v = json!({
                    "mcpServers": {
                        "paneflow": {
                            "type": "stdio",
                            "command": "/data/paneflow-mcp",
                            "args": []
                        }
                    }
                });
                std::fs::write(&dest, serde_json::to_vec(&v).unwrap()).unwrap();
                Ok(())
            })),
        };

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        assert_add_without_remove(&calls.borrow());
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["paneflow"]["command"],
            json!("/data/paneflow-mcp")
        );
    }

    #[test]
    fn install_cli_success_falls_back_when_file_does_not_match() {
        // CLI exited 0 but did not write our watched file (e.g. it edited
        // `$CLAUDE_CONFIG_DIR/.claude.json` while we track `$HOME`).
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join(".claude.json");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = ClaudeCode {
            config_path: Some(p.clone()),
            allow_cli: true,
            cli: Some(Box::new(move |args| {
                record_args(&calls_h, args);
                Ok(())
            })),
        };

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        assert_add_without_remove(&calls.borrow());
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["paneflow"]["command"],
            json!("/data/paneflow-mcp")
        );
        assert_eq!(v["mcpServers"]["paneflow"]["type"], json!("stdio"));
    }

    #[test]
    fn install_cli_failure_falls_back_without_remove() {
        // Non-idempotent `mcp add` (stale entry already present) must not
        // be preceded by `mcp remove`; the locked merge updates in place.
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
                        "command": "/old/paneflow-mcp",
                        "args": []
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = ClaudeCode {
            config_path: Some(p.clone()),
            allow_cli: true,
            cli: Some(Box::new(move |args| {
                record_args(&calls_h, args);
                Err(anyhow!("mcp add: already exists"))
            })),
        };

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Updated
        );
        assert_add_without_remove(&calls.borrow());
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(42));
        assert_eq!(v["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert_eq!(
            v["mcpServers"]["paneflow"]["command"],
            json!("/data/paneflow-mcp")
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
