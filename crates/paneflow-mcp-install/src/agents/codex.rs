//! Codex writer (EP-003 US-008).
//!
//! Direct, format-preserving `toml_edit` upsert of `[mcp_servers.paneflow]`
//! in `$CODEX_HOME/config.toml` (or `~/.codex/config.toml`), holding
//! [`crate::io::ConfigLock`]. Comments, sibling tables, and unknown keys stay
//! intact, and a write that changes the file copies the old bytes to
//! `config.toml.bak` first. No `codex` process is spawned: `codex mcp add`
//! writes neither `args` nor `env_vars`, so the direct edit wrote the final
//! bytes on every install anyway, and `codex mcp remove` drops inline
//! comments from sibling tables (issue #847).
//!
//! **Volatility:** Codex's config schema moves fast (verified 2026:
//! `[mcp_servers.<name>]` with `command`/`args`/`env_vars`). Re-verify the
//! table shape against Codex's config docs if registration regresses.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::agents::{support, AgentConfigWriter, InstallOutcome, StatusOutcome, UninstallOutcome};
use crate::detect::{self, Presence};

const CLI: &str = "codex";

pub struct Codex {
    config_path: Option<PathBuf>,
}

impl Codex {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config_path: support::codex_config(),
        }
    }

    fn path(&self) -> Result<&Path> {
        self.config_path
            .as_deref()
            .ok_or_else(|| anyhow!("cannot resolve Codex config path"))
    }
}

impl Default for Codex {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentConfigWriter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn label(&self) -> &'static str {
        "Codex"
    }

    fn presence(&self) -> Presence {
        // Detect via the config dir too: `~/.codex/` existing is a strong
        // signal even before `config.toml` is created.
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Some(cfg) = &self.config_path {
            paths.push(cfg.clone());
            if let Some(parent) = cfg.parent() {
                paths.push(parent.to_path_buf());
            }
        }
        detect::detect(Some(CLI), &paths)
    }

    fn install(&self, bridge: &Path) -> Result<InstallOutcome> {
        let path = self.path()?;
        // An entry status already accepts is left byte-for-byte alone. The
        // upsert is not a no-op on every such file: it refuses an inline
        // `mcp_servers = { ... }` parent that status reads fine.
        let status = support::toml_status(path, Some(bridge))?;
        if matches!(status, StatusOutcome::Installed { .. }) {
            return Ok(InstallOutcome::AlreadyCurrent);
        }
        support::toml_install(path, &bridge.to_string_lossy())
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        // A present-but-unparseable `config.toml` is an error (US-021),
        // never "nothing to remove": `toml_uninstall` parses it under the
        // lock and refuses to touch it.
        support::toml_uninstall(self.path()?)
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        support::toml_status(self.path()?, bridge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_writer(path: PathBuf) -> Codex {
        Codex {
            config_path: Some(path),
        }
    }

    #[test]
    fn install_writes_mcp_servers_table() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let w = test_writer(p.clone());
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        let txt = std::fs::read_to_string(&p).unwrap();
        assert!(txt.contains("paneflow"));
        assert!(txt.contains("/data/paneflow-mcp"));
        // Re-parse to confirm the table path.
        let doc = txt.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["command"].as_str(),
            Some("/data/paneflow-mcp")
        );
        let env_vars = doc["mcp_servers"]["paneflow"]["env_vars"]
            .as_array()
            .unwrap();
        assert!(env_vars
            .iter()
            .any(|value| value.as_str() == Some("PANEFLOW_SOCKET_PATH")));
        assert!(env_vars
            .iter()
            .any(|value| value.as_str() == Some("PANEFLOW_WORKSPACE_ID")));
        assert!(env_vars
            .iter()
            .any(|value| value.as_str() == Some("PANEFLOW_SURFACE_ID")));
    }

    #[test]
    fn install_repairs_legacy_env_forwarding_for_agent_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "# keep my settings\n[mcp_servers.other]\ncommand = \"other-mcp\"\n\n\
             [mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\n\
             env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"CUSTOM_VAR\"]\n\
             startup_timeout_sec = 20\n",
        )
        .unwrap();
        let w = test_writer(p.clone());
        let bridge = Path::new("/data/paneflow-mcp");
        assert!(matches!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
        assert_eq!(w.install(bridge).unwrap(), InstallOutcome::Updated);
        let repaired = std::fs::read_to_string(&p).unwrap();
        let doc = repaired.parse::<toml_edit::DocumentMut>().unwrap();
        let forwarded: Vec<_> = doc["mcp_servers"]["paneflow"]["env_vars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            forwarded,
            [
                "PANEFLOW_SOCKET_PATH",
                "PANEFLOW_WORKSPACE_ID",
                "CUSTOM_VAR",
                "PANEFLOW_SURFACE_ID"
            ]
        );
        assert!(repaired.contains("# keep my settings"));
        assert_eq!(
            doc["mcp_servers"]["other"]["command"].as_str(),
            Some("other-mcp")
        );
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["startup_timeout_sec"].as_integer(),
            Some(20)
        );
        assert!(matches!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::Installed { .. }
        ));
        assert_eq!(w.install(bridge).unwrap(), InstallOutcome::AlreadyCurrent);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), repaired);
    }

    #[test]
    fn install_preserves_existing_config_and_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "# codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.github]\ncommand = \"gh-mcp\"\n",
        )
        .unwrap();
        let w = test_writer(p.clone());
        w.install(Path::new("/data/paneflow-mcp")).unwrap();

        let txt = std::fs::read_to_string(&p).unwrap();
        assert!(txt.contains("# codex config"));
        assert!(txt.contains("model = \"gpt-5\""));
        assert!(txt.contains("gh-mcp"), "sibling server preserved");
        assert!(txt.contains("/data/paneflow-mcp"));
    }

    #[test]
    fn install_idempotent_and_uninstall() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let w = test_writer(p);
        w.install(Path::new("/data/paneflow-mcp")).unwrap();
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::Removed);
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::NothingToRemove);
    }

    #[test]
    fn status_needs_repair_when_disabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\nenabled = false\n",
        )
        .unwrap();
        let w = test_writer(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn install_enables_disabled_entry_with_current_command() {
        // Issue #214: a disabled entry whose command already matched used
        // to report AlreadyCurrent while status kept saying NeedsRepair.
        // The advertised repair must proceed and enable the managed entry.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\nenv_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\nenabled = false\n",
        )
        .unwrap();
        let w = test_writer(p.clone());

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Updated
        );
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["enabled"].as_bool(),
            Some(true)
        );
        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed { .. }
        ));
    }

    #[test]
    fn status_needs_repair_when_env_overrides_paneflow_scope() {
        // Issue #648: a managed command with the required env_vars still
        // widens the bridge when `env.PANEFLOW_MCP_SCOPE` is `all`. Status
        // must say NeedsRepair, and install must delete the key before the
        // already-current short-circuit. The same contract covers the three
        // identity variables Codex is supposed to forward, not pin.
        let bridge = Path::new("/data/paneflow-mcp");
        let header = "[mcp_servers.paneflow]\n\
             command = \"/data/paneflow-mcp\"\n\
             args = []\n\
             env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\n";

        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            format!("{header}\n[mcp_servers.paneflow.env]\nPANEFLOW_MCP_SCOPE = \"all\"\n"),
        )
        .unwrap();
        assert_scope_override_repaired(&p, bridge, "PANEFLOW_MCP_SCOPE", None);

        for (key, value) in [
            ("PANEFLOW_SOCKET_PATH", "/tmp/pinned.sock"),
            ("PANEFLOW_WORKSPACE_ID", "7"),
            ("PANEFLOW_SURFACE_ID", "9"),
        ] {
            std::fs::write(
                &p,
                format!("{header}\n[mcp_servers.paneflow.env]\n{key} = \"{value}\"\nCUSTOM = \"keep\"\n"),
            )
            .unwrap();
            assert_scope_override_repaired(&p, bridge, key, Some("keep"));
        }

        // An unrelated env entry is not a PaneFlow override.
        std::fs::write(
            &p,
            format!("{header}\n[mcp_servers.paneflow.env]\nCUSTOM = \"keep\"\n"),
        )
        .unwrap();
        let w = test_writer(p.clone());
        assert!(matches!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::Installed { .. }
        ));
        assert_eq!(w.install(bridge).unwrap(), InstallOutcome::AlreadyCurrent);
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["env"]["CUSTOM"].as_str(),
            Some("keep")
        );

        // Inline `env` tables are the same override.
        std::fs::write(
            &p,
            "[mcp_servers]\npaneflow = { command = \"/data/paneflow-mcp\", args = [], env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"], env = { PANEFLOW_MCP_SCOPE = \"all\", CUSTOM = \"keep\" } }\n",
        )
        .unwrap();
        assert_scope_override_repaired(&p, bridge, "PANEFLOW_MCP_SCOPE", Some("keep"));
    }

    fn assert_scope_override_repaired(
        path: &Path,
        bridge: &Path,
        forbidden: &str,
        kept: Option<&str>,
    ) {
        let w = test_writer(path.to_path_buf());
        let status = w.status(Some(bridge)).unwrap();
        assert!(
            matches!(
                status,
                StatusOutcome::NeedsRepair { ref reason, .. } if reason.contains(forbidden)
            ),
            "expected NeedsRepair naming {forbidden}, got {status:?}"
        );
        assert_eq!(w.install(bridge).unwrap(), InstallOutcome::Updated);
        let doc = std::fs::read_to_string(path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let entry = &doc["mcp_servers"]["paneflow"];
        assert!(
            entry
                .get("env")
                .and_then(|env| env.get(forbidden))
                .is_none(),
            "install must remove {forbidden}: {}",
            std::fs::read_to_string(path).unwrap()
        );
        if let Some(kept) = kept {
            assert_eq!(entry["env"]["CUSTOM"].as_str(), Some(kept));
        }
        let forwarded = entry["env_vars"].as_array().unwrap();
        for required in [
            "PANEFLOW_SOCKET_PATH",
            "PANEFLOW_WORKSPACE_ID",
            "PANEFLOW_SURFACE_ID",
        ] {
            assert!(
                forwarded.iter().any(|item| item.as_str() == Some(required)),
                "env_vars must still forward {required}"
            );
        }
        assert!(matches!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::Installed { .. }
        ));
        assert_eq!(w.install(bridge).unwrap(), InstallOutcome::AlreadyCurrent);
    }

    #[test]
    fn install_repairs_missing_env_forwards_and_preserves_custom_ones() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\nenv_vars = [\"CUSTOM_VAR\"]\n",
        )
        .unwrap();
        let w = test_writer(p.clone());

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Updated
        );

        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let env_vars = doc["mcp_servers"]["paneflow"]["env_vars"]
            .as_array()
            .unwrap();
        for expected in [
            "CUSTOM_VAR",
            "PANEFLOW_SOCKET_PATH",
            "PANEFLOW_WORKSPACE_ID",
            "PANEFLOW_SURFACE_ID",
        ] {
            assert!(
                env_vars
                    .iter()
                    .any(|value| value.as_str() == Some(expected)),
                "missing {expected} in {env_vars:?}"
            );
        }
        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed { .. }
        ));
    }

    #[test]
    fn uninstall_malformed_config_is_error() {
        // US-021: symmetric with the Claude Code writer - a present-but-
        // unparseable config is corruption, surfaced loudly, not swallowed
        // as NothingToRemove.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, b"this = = broken").unwrap();
        let w = test_writer(p.clone());
        assert!(
            w.uninstall().is_err(),
            "uninstall on a malformed present config must error, not return NothingToRemove"
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"this = = broken");
    }

    #[test]
    fn uninstall_absent_config_is_nothing_to_remove() {
        let dir = tempfile::TempDir::new().unwrap();
        let w = test_writer(dir.path().join("missing.toml"));
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::NothingToRemove);
    }

    #[test]
    fn uninstall_inline_mcp_servers_table_is_error() {
        // An inline `mcp_servers` parent can hold the entry, so uninstall
        // must refuse it loudly rather than report NothingToRemove while
        // the entry stays in place.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let src = "mcp_servers = { paneflow = { command = \"/x\", args = [] } }\n";
        std::fs::write(&p, src).unwrap();
        let w = test_writer(p.clone());
        let err = w.uninstall().expect_err(
            "uninstall on an inline mcp_servers table must error, not return NothingToRemove",
        );
        assert!(
            format!("{err:#}").contains("is not a TOML table"),
            "unexpected error: {err:#}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), src);
    }

    #[test]
    fn uninstall_preserves_comments_and_siblings() {
        // The direct `toml_edit` removal keeps the header comment and a
        // sibling's inline comment byte-for-byte, and `.bak` holds the
        // exact bytes from before the uninstall.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let before = "# codex config\nmodel = \"gpt-5\"\n\n\
                      [mcp_servers.github]\ncommand = \"gh-mcp\" # inline comment\n\n\
                      [mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\n\
                      env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\n";
        std::fs::write(&p, before).unwrap();
        let w = test_writer(p.clone());

        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::Removed);
        let txt = std::fs::read_to_string(&p).unwrap();
        assert!(
            txt.starts_with("# codex config\n"),
            "header comment lost: {txt}"
        );
        assert!(
            txt.lines()
                .any(|line| line == "command = \"gh-mcp\" # inline comment"),
            "sibling inline comment lost: {txt}"
        );
        let doc = txt.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(
            doc["mcp_servers"].get("paneflow").is_none(),
            "paneflow entry must be gone: {txt}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            before
        );
    }

    #[test]
    fn install_refuses_invalid_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, b"this = = broken").unwrap();
        let w = test_writer(p.clone());
        assert!(w.install(Path::new("/data/paneflow-mcp")).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"this = = broken");
    }
}
