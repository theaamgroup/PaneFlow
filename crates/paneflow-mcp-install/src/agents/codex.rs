//! Codex writer.
//!
//! PaneFlow registered the bridge as `[mcp_servers.paneflow]` in
//! `$CODEX_HOME/config.toml` (or `~/.codex/config.toml`). Removal is a
//! format-preserving `toml_edit` edit under [`crate::io`]'s lock: comments,
//! sibling tables, and unknown keys stay intact, and the old bytes land in
//! `config.toml.bak` first. No `codex` process is spawned: `codex mcp
//! remove` drops inline comments from sibling tables (issue #847).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::agents::{support, AgentConfigWriter, StatusOutcome, UninstallOutcome};

pub struct Codex {
    config_path: Option<PathBuf>,
}

impl Codex {
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
            .ok_or_else(|| anyhow!("cannot resolve Codex config path"))
    }
}

impl AgentConfigWriter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn label(&self) -> &'static str {
        "Codex"
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

    const MANAGED: &str = "[mcp_servers.paneflow]\n\
         command = \"/data/paneflow-mcp\"\n\
         args = []\n\
         env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\n";

    #[test]
    fn status_reads_the_managed_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, MANAGED).unwrap();
        let w = Codex::at(p);

        assert_eq!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed {
                path: "/data/paneflow-mcp".into()
            }
        );
    }

    #[test]
    fn uninstall_then_nothing_to_remove() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, MANAGED).unwrap();
        let w = Codex::at(p);
        assert!(matches!(
            w.uninstall().unwrap(),
            UninstallOutcome::Removed { .. }
        ));
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
        let w = Codex::at(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn status_needs_repair_when_env_overrides_paneflow_identity() {
        // Issue #648: a static `env` value that pins or widens PaneFlow's
        // identity is not the shape PaneFlow wrote. An unrelated env key is.
        let bridge = Path::new("/data/paneflow-mcp");
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let w = Codex::at(p.clone());
        for key in [
            "PANEFLOW_MCP_SCOPE",
            "PANEFLOW_SOCKET_PATH",
            "PANEFLOW_WORKSPACE_ID",
            "PANEFLOW_SURFACE_ID",
        ] {
            std::fs::write(
                &p,
                format!("{MANAGED}\n[mcp_servers.paneflow.env]\n{key} = \"x\"\n"),
            )
            .unwrap();
            let status = w.status(Some(bridge)).unwrap();
            assert!(
                matches!(
                    status,
                    StatusOutcome::NeedsRepair { ref reason, .. } if reason.contains(key)
                ),
                "expected NeedsRepair naming {key}, got {status:?}"
            );
        }

        std::fs::write(
            &p,
            format!("{MANAGED}\n[mcp_servers.paneflow.env]\nCUSTOM = \"keep\"\n"),
        )
        .unwrap();
        assert!(matches!(
            w.status(Some(bridge)).unwrap(),
            StatusOutcome::Installed { .. }
        ));
    }

    #[test]
    fn status_needs_repair_when_env_forwarding_is_incomplete() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\nenv_vars = [\"CUSTOM_VAR\"]\n",
        )
        .unwrap();
        let w = Codex::at(p);

        assert!(matches!(
            w.status(Some(Path::new("/data/paneflow-mcp"))).unwrap(),
            StatusOutcome::NeedsRepair { .. }
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
        let w = Codex::at(p.clone());
        assert!(
            w.uninstall().is_err(),
            "uninstall on a malformed present config must error, not return NothingToRemove"
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"this = = broken");
    }

    #[test]
    fn uninstall_absent_config_is_nothing_to_remove() {
        let dir = tempfile::TempDir::new().unwrap();
        let w = Codex::at(dir.path().join("missing.toml"));
        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::NothingToRemove);
    }

    #[test]
    fn uninstall_inline_mcp_servers_table_is_error() {
        // An inline `mcp_servers` parent can hold the entry, so uninstall
        // must refuse it loudly rather than report NothingToRemove while
        // the entry stays in place.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let src = "mcp_servers = { paneflow = { command = \"/x/paneflow-mcp\", args = [] } }\n";
        std::fs::write(&p, src).unwrap();
        let w = Codex::at(p.clone());
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
        let before = format!(
            "# codex config\nmodel = \"gpt-5\"\n\n\
             [mcp_servers.github]\ncommand = \"gh-mcp\" # inline comment\n\n{MANAGED}"
        );
        std::fs::write(&p, &before).unwrap();
        let w = Codex::at(p.clone());

        assert_eq!(
            w.uninstall().unwrap(),
            UninstallOutcome::Removed {
                file: p.clone(),
                backup: dir.path().join("config.toml.bak"),
            }
        );
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
}
