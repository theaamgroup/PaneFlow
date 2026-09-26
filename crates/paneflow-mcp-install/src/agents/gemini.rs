//! Gemini CLI writer.
//!
//! PaneFlow registered the bridge in `~/.gemini/settings.json` under
//! `mcpServers.paneflow` = `{command, args: [], trust: true}`, with no `env`
//! block. Removal is a direct edit under [`crate::io`]'s lock that keeps
//! every other setting and server.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::agents::{support, AgentConfigWriter, StatusOutcome, UninstallOutcome};

const CONTAINER: &str = "mcpServers";

pub struct Gemini {
    config_path: Option<PathBuf>,
}

impl Gemini {
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
            .ok_or_else(|| anyhow!("cannot resolve home dir for ~/.gemini/settings.json"))
    }

    /// The entry PaneFlow wrote, so `status` can tell it from a hand edit.
    fn entry(bridge: &str) -> serde_json::Value {
        json!({ "command": bridge, "args": [], "trust": true })
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
            "Gemini MCP entry must have empty args and trust=true",
        )
    }
}

impl AgentConfigWriter for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn label(&self) -> &'static str {
        "Gemini CLI"
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
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
    fn status_then_uninstall_preserves_other_settings_and_servers() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "theme": "GitHub",
                "mcpServers": {
                    "context7": { "command": "c7" },
                    "paneflow": Gemini::entry("/data/paneflow-mcp")
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = Gemini::at(p.clone());

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
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["theme"], json!("GitHub"));
        assert_eq!(v["mcpServers"]["context7"]["command"], json!("c7"));
        assert!(v["mcpServers"].get("paneflow").is_none());
    }

    #[test]
    fn status_needs_repair_when_not_trusted() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "mcpServers": {
                    "paneflow": {
                        "command": "/data/paneflow-mcp",
                        "args": [],
                        "trust": false
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = Gemini::at(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn uninstall_malformed_config_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"{ broken").unwrap();
        let w = Gemini::at(p.clone());

        assert!(w.uninstall().is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"{ broken");
    }
}
