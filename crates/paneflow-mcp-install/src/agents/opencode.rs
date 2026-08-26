//! opencode writer (EP-003 US-010).
//!
//! opencode's schema diverges from every other agent:
//! - the container key is **`mcp`**, not `mcpServers`;
//! - the entry is `{type: "local", command: [<path>], enabled: true}` -
//!   `command` is an **array**, with the binary path as its first element.
//!
//! Config lives in opencode's global config path, preferring JSONC when an
//! existing `opencode.jsonc` is present. No CLI mutates server config, so this
//! is always a direct merge - preserving `$schema`, sibling `mcp.*` entries,
//! and JSONC comments / trailing commas when the chosen path is `.jsonc`.
//!
//! **Volatility:** opencode's config schema is young; re-verify the `mcp`
//! key, `type: "local"`, and array `command` if registration regresses.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::json;

use crate::agents::{support, AgentConfigWriter, InstallOutcome, StatusOutcome, UninstallOutcome};
use crate::detect::{self, Presence};
use crate::{io, merge};

const CLI: &str = "opencode";
const CONTAINER: &str = "mcp";

pub struct OpenCode {
    config_paths: Vec<PathBuf>,
}

impl OpenCode {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config_paths: support::opencode_configs(),
        }
    }

    fn path(&self) -> Result<&Path> {
        self.config_paths
            .iter()
            .find(|p| p.exists())
            .or_else(|| self.config_paths.first())
            .map(PathBuf::as_path)
            .ok_or_else(|| anyhow!("cannot resolve opencode config path"))
    }

    fn entry(bridge: &str) -> serde_json::Value {
        // `command` is an ARRAY for opencode; `type: "local"` marks a stdio
        // child process; `enabled: true` activates it.
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

fn install_jsonc(path: &Path, entry: serde_json::Value) -> Result<InstallOutcome> {
    io::with_config_lock(path, || match std::fs::read(path) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).with_context(|| {
                    format!(
                        "{} is not valid UTF-8 JSONC - refusing to overwrite it; fix or remove it, then re-run",
                        path.display()
                    )
                })?;
            let root = merge::parse_jsonc_text(text).with_context(|| {
                    format!(
                        "{} is not valid JSONC - refusing to overwrite it; fix or remove it, then re-run",
                        path.display()
                    )
                })?;
            let had_prior = root
                .get(CONTAINER)
                .and_then(|c| c.get(support::ENTRY))
                .is_some();
            match merge::upsert_jsonc_entry(text, CONTAINER, support::ENTRY, &entry)
                .with_context(|| format!("update {} failed", path.display()))?
            {
                None => Ok(InstallOutcome::AlreadyCurrent),
                Some(new_text) => {
                    io::write_if_changed_unlocked(path, new_text.as_bytes())?;
                    Ok(if had_prior {
                        InstallOutcome::Updated
                    } else {
                        InstallOutcome::Installed
                    })
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut root = serde_json::Value::Object(serde_json::Map::new());
            merge::merge_json_entry(&mut root, CONTAINER, support::ENTRY, entry)?;
            io::write_if_changed_unlocked(path, &merge::json_to_bytes(&root)?)?;
            Ok(InstallOutcome::Installed)
        }
        Err(e) => Err(e).with_context(|| format!("read {} failed", path.display())),
    })
}

fn uninstall_jsonc(path: &Path) -> Result<UninstallOutcome> {
    if !path.exists() {
        return Ok(UninstallOutcome::NothingToRemove);
    }
    io::with_config_lock(path, || {
        if !path.exists() {
            return Ok(UninstallOutcome::NothingToRemove);
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("read {} failed", path.display()))?;
        let text = std::str::from_utf8(&bytes).with_context(|| {
            format!(
                "{} is not valid UTF-8 JSONC - refusing to overwrite it; fix or remove it, then re-run",
                path.display()
            )
        })?;
        match merge::remove_jsonc_entry(text, CONTAINER, support::ENTRY)
            .with_context(|| format!("update {} failed", path.display()))?
        {
            None => Ok(UninstallOutcome::NothingToRemove),
            Some(new_text) => {
                io::write_if_changed_unlocked(path, new_text.as_bytes())?;
                Ok(UninstallOutcome::Removed)
            }
        }
    })
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentConfigWriter for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn label(&self) -> &'static str {
        "opencode"
    }

    fn presence(&self) -> Presence {
        detect::detect(Some(CLI), &self.config_paths)
    }

    fn install(&self, bridge: &Path) -> Result<InstallOutcome> {
        let bridge_s = bridge.to_string_lossy().into_owned();
        let path = self.path()?;
        let entry = Self::entry(&bridge_s);
        if merge::is_jsonc_path(path) {
            install_jsonc(path, entry)
        } else {
            support::json_install(path, CONTAINER, entry)
        }
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        let path = self.path()?;
        if merge::is_jsonc_path(path) {
            uninstall_jsonc(path)
        } else {
            support::json_uninstall(path, CONTAINER)
        }
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        // opencode stores `command` as an array → use the array extractor.
        support::json_status(self.path()?, CONTAINER, bridge, Self::validate_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_writer(path: PathBuf) -> OpenCode {
        OpenCode {
            config_paths: vec![path],
        }
    }

    #[test]
    fn install_writes_local_array_entry_under_mcp() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        let w = test_writer(p.clone());
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let entry = &v["mcp"]["paneflow"];
        assert_eq!(entry["type"], json!("local"));
        assert_eq!(
            entry["command"],
            json!(["/data/paneflow-mcp"]),
            "command is an array"
        );
        assert_eq!(entry["enabled"], json!(true));
        // Must NOT land under mcpServers.
        assert!(v.get("mcpServers").is_none());
    }

    #[test]
    fn install_preserves_schema_and_sibling_mcp_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": { "weather": { "type": "local", "command": ["weather-mcp"], "enabled": true } }
            }))
            .unwrap(),
        )
        .unwrap();
        let w = test_writer(p.clone());
        w.install(Path::new("/data/paneflow-mcp")).unwrap();

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["$schema"], json!("https://opencode.ai/config.json"));
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
        assert_eq!(
            v["mcp"]["paneflow"]["command"],
            json!(["/data/paneflow-mcp"])
        );
    }

    #[test]
    fn status_reads_array_command_and_flags_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        let w = test_writer(p);
        w.install(Path::new("/old/paneflow-mcp")).unwrap();
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
        let w = test_writer(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn install_updates_existing_jsonc_candidate() {
        let dir = tempfile::TempDir::new().unwrap();
        let jsonc = dir.path().join("opencode.jsonc");
        let json = dir.path().join("opencode.json");
        std::fs::write(
            &jsonc,
            br#"
{
  // keep this file selected
  "mcp": {
    "weather": { "type": "local", "command": ["weather-mcp"], "enabled": true },
  },
}
"#,
        )
        .unwrap();
        let w = OpenCode {
            config_paths: vec![jsonc.clone(), json.clone()],
        };

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        assert!(jsonc.exists());
        assert!(!json.exists());
        let raw = std::fs::read_to_string(&jsonc).unwrap();
        assert!(
            raw.contains("// keep this file selected"),
            "JSONC comments must survive install:\n{raw}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&raw).is_err(),
            "must remain JSONC, not rewritten as JSON"
        );
        let v = merge::parse_jsonc_text(&raw).unwrap();
        assert_eq!(
            v["mcp"]["paneflow"]["command"],
            json!(["/data/paneflow-mcp"])
        );
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
        assert_eq!(std::fs::read_to_string(&jsonc).unwrap(), raw);
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
        let w = test_writer(jsonc.clone());
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::Removed);
        let raw = std::fs::read_to_string(&jsonc).unwrap();
        assert!(
            raw.contains("// keep this file selected"),
            "JSONC comments must survive uninstall:\n{raw}"
        );
        let v = merge::parse_jsonc_text(&raw).unwrap();
        assert!(v["mcp"].get("paneflow").is_none());
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
    }

    #[test]
    fn uninstall_malformed_config_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(&p, b"{ broken").unwrap();
        let w = test_writer(p.clone());

        assert!(w.uninstall().is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"{ broken");
    }
}
