//! opencode writer.
//!
//! opencode's schema diverges from every other agent:
//! - the container key is **`mcp`**, not `mcpServers`;
//! - PaneFlow's entry was `{type: "local", command: [<path>], enabled: true}` -
//!   `command` is an **array**, with the binary path as its first element.
//!
//! opencode reads a global `opencode.jsonc` and `opencode.json` (or the one
//! file `OPENCODE_CONFIG` names). The cleanup makes one writer per candidate
//! file and probes each, so an entry is found whichever file holds it; a
//! candidate that does not exist is simply nothing to remove. Removal is a
//! surgical splice, so comments and trailing commas survive byte for byte.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use crate::agents::{support, AgentConfigWriter, StatusOutcome, UninstallOutcome};

const CONTAINER: &str = "mcp";

pub struct OpenCode {
    config_path: PathBuf,
}

impl OpenCode {
    /// A writer for the one candidate config at `config_path`.
    #[must_use]
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    /// The entry PaneFlow wrote, so `status` can tell it from a hand edit.
    fn entry(bridge: &str) -> serde_json::Value {
        json!({ "type": "local", "command": [bridge], "enabled": true })
    }

    fn validate_entry(entry: &serde_json::Value, expected: Option<&Path>) -> StatusOutcome {
        let found = support::array_command(entry);
        let shape_ok = found
            .as_deref()
            .is_some_and(|path| *entry == Self::entry(path));
        support::classify_entry(
            found,
            expected,
            shape_ok,
            "opencode MCP entry must be local, enabled, and use command array form",
        )
    }
}

impl AgentConfigWriter for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn label(&self) -> &'static str {
        "opencode"
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        // opencode stores `command` as an array → use the array extractor.
        support::json_uninstall(&self.config_path, CONTAINER, support::array_command)
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        support::json_status(&self.config_path, CONTAINER, bridge, Self::validate_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_array_command_and_flags_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "mcp": { "paneflow": OpenCode::entry("/old/paneflow-mcp") }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = OpenCode::new(p);
        assert_eq!(
            w.status(Some(Path::new("/old/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed {
                path: "/old/paneflow-mcp".into()
            }
        );
        assert_eq!(
            w.status(Some(Path::new("/new/paneflow-mcp"))).unwrap(),
            StatusOutcome::StalePath {
                found: "/old/paneflow-mcp".into(),
                expected: "/new/paneflow-mcp".into()
            }
        );
    }

    #[test]
    fn status_needs_repair_when_disabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "mcp": {
                    "paneflow": {
                        "type": "local",
                        "command": ["/data/paneflow-mcp"],
                        "enabled": false
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = OpenCode::new(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn a_missing_candidate_is_nothing_to_remove_and_is_not_created() {
        // Covers `OPENCODE_CONFIG` naming a file that does not exist.
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("custom").join("my-opencode.jsonc");
        let w = OpenCode::new(missing.clone());

        assert_eq!(w.status(None).unwrap(), StatusOutcome::NotInstalled);
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::NothingToRemove);
        assert!(!missing.exists() && !missing.parent().unwrap().exists());
    }

    #[test]
    fn uninstall_jsonc_preserves_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let jsonc = dir.path().join("opencode.jsonc");
        std::fs::write(
            &jsonc,
            br#"
{
  // keep this file selected
  "mcp": {
    "weather": { "type": "local", "command": ["weather-mcp"], "enabled": true },
    "paneflow": { "type": "local", "command": ["/data/paneflow-mcp"], "enabled": true }
  }
}
"#,
        )
        .unwrap();
        let w = OpenCode::new(jsonc.clone());
        assert!(matches!(
            w.uninstall().unwrap(),
            UninstallOutcome::Removed { .. }
        ));
        let raw = std::fs::read_to_string(&jsonc).unwrap();
        assert!(
            raw.contains("// keep this file selected"),
            "JSONC comments must survive uninstall:\n{raw}"
        );
        let v = crate::merge::parse_json_family(&jsonc, &raw).unwrap();
        assert!(v["mcp"].get("paneflow").is_none());
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
    }

    #[test]
    fn uninstall_malformed_config_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(&p, b"{ broken").unwrap();
        let w = OpenCode::new(p.clone());

        assert!(w.uninstall().is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"{ broken");
    }
}
