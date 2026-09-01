//! Codex writer (EP-003 US-008).
//!
//! Preferred path: shell out to `codex mcp add paneflow -- <bridge>` when
//! the `codex` CLI is on PATH. Do **not** `mcp remove` first: a crash
//! between remove and add used to drop the paneflow entry while the rest
//! of the file stayed intact, and holding [`crate::io::ConfigLock`] across
//! the CLI does not serialize `codex`. After a successful add, the on-disk
//! file is verified; a mismatch or a failed add falls back to a locked,
//! format-preserving `toml_edit` upsert of `[mcp_servers.paneflow]` in
//! `~/.codex/config.toml`, keeping comments, sibling tables, and unknown
//! keys intact. Fallback also runs when `codex` is absent.
//!
//! **Volatility:** Codex's config schema and `codex mcp` subcommand flags
//! move fast (verified 2026: `[mcp_servers.<name>]` with `command`/`args`,
//! `codex mcp add` exists but its flags are under-documented). Re-verify
//! against `codex mcp --help` if registration regresses; the TOML fallback
//! is the stable path.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::agents::{support, AgentConfigWriter, InstallOutcome, StatusOutcome, UninstallOutcome};
use crate::detect::{self, Presence};
use crate::{io, merge};

const CLI: &str = "codex";

#[cfg(test)]
type CliHook = Box<dyn Fn(&[&str]) -> Result<()>>;

pub struct Codex {
    config_path: Option<PathBuf>,
    allow_cli: bool,
    /// Test-only stand-in for `codex`. When set, install/uninstall never
    /// consult PATH or spawn a real process.
    #[cfg(test)]
    cli: Option<CliHook>,
}

impl Codex {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config_path: support::codex_config(),
            allow_cli: true,
            #[cfg(test)]
            cli: None,
        }
    }

    fn path(&self) -> Result<&Path> {
        self.config_path
            .as_deref()
            .ok_or_else(|| anyhow!("cannot resolve Codex config path"))
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
        let cli = if self.allow_cli { Some(CLI) } else { None };
        // Detect via the config dir too: `~/.codex/` existing is a strong
        // signal even before `config.toml` is created.
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Some(cfg) = &self.config_path {
            paths.push(cfg.clone());
            if let Some(parent) = cfg.parent() {
                paths.push(parent.to_path_buf());
            }
        }
        detect::detect(cli, &paths)
    }

    fn install(&self, bridge: &Path) -> Result<InstallOutcome> {
        let path = self.path()?;
        let bridge_s = bridge.to_string_lossy().into_owned();

        let status = support::toml_status(path, Some(bridge))?;
        if matches!(status, StatusOutcome::Installed { .. }) {
            return Ok(InstallOutcome::AlreadyCurrent);
        }
        let had_prior = support::toml_entry_present(path)?;

        if self.cli_available() {
            io::backup(path)?;
            // Skip `mcp remove`. A crash between remove and add used to
            // drop `[mcp_servers.paneflow]` while the rest of the file
            // stayed intact. `add` is attempted as-is; a non-idempotent
            // CLI (entry already present) fails here and the locked merge
            // repairs it without wiping unknown keys.
            match self.invoke_cli(&["mcp", "add", "paneflow", "--", &bridge_s]) {
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
                            "paneflow mcp: `codex mcp add` exited 0 but ~/.codex/config.toml does not match the managed entry; falling back to direct edit"
                        );
                    }
                },
                Err(e) => {
                    log::warn!(
                        "paneflow mcp: `codex mcp add` failed ({e:#}); falling back to direct ~/.codex/config.toml edit"
                    );
                }
            }
        }

        let fallback = support::toml_install(path, &bridge_s)?;
        Ok(match fallback {
            InstallOutcome::AlreadyCurrent => InstallOutcome::AlreadyCurrent,
            InstallOutcome::Installed | InstallOutcome::Updated if had_prior => {
                InstallOutcome::Updated
            }
            InstallOutcome::Installed | InstallOutcome::Updated => InstallOutcome::Installed,
        })
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        let path = self.path()?;
        // US-021: a present-but-unparseable `~/.codex/config.toml` must
        // surface a loud error, not be silently mistaken for "nothing to
        // remove". The tolerant `current_toml_command` below swallows parse
        // failures (`.ok()?` → None), so probe parseability first -
        // `read_toml_or_default` is `Err` on a present malformed file and
        // `Ok` (empty doc) when absent.
        if path.exists() {
            merge::read_toml_or_default(path)?;
        }
        if !support::toml_entry_present(path)? {
            return Ok(UninstallOutcome::NothingToRemove);
        }
        if self.cli_available() {
            io::backup(path)?;
            // Like install, trust the on-disk postcondition, not the CLI's
            // exit status (issue #215): `codex mcp remove` can exit 0 while
            // the watched file still carries the entry.
            match self.invoke_cli(&["mcp", "remove", "paneflow"]) {
                Ok(()) => match self.status(None)? {
                    StatusOutcome::NotInstalled => return Ok(UninstallOutcome::Removed),
                    _ => {
                        log::warn!(
                            "paneflow mcp: `codex mcp remove` exited 0 but ~/.codex/config.toml still carries the paneflow entry; falling back to direct edit"
                        );
                    }
                },
                Err(e) => {
                    log::warn!(
                        "paneflow mcp: `codex mcp remove` failed ({e:#}); falling back to direct ~/.codex/config.toml edit"
                    );
                }
            }
        }
        support::toml_uninstall(path)
    }

    fn status(&self, bridge: Option<&Path>) -> Result<StatusOutcome> {
        support::toml_status(self.path()?, bridge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn test_writer(path: PathBuf) -> Codex {
        Codex {
            config_path: Some(path),
            allow_cli: false,
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
            "[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\nenv_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\"]\nenabled = false\n",
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
    fn install_cli_add_skips_remove_and_verifies_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let dest = p.clone();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = Codex {
            config_path: Some(p.clone()),
            allow_cli: true,
            cli: Some(Box::new(move |args| {
                record_args(&calls_h, args);
                std::fs::write(
                    &dest,
                    "[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\n",
                )
                .unwrap();
                Ok(())
            })),
        };

        assert_eq!(
            w.install(Path::new("/data/paneflow-mcp")).unwrap(),
            InstallOutcome::Installed
        );
        assert_add_without_remove(&calls.borrow());
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["command"].as_str(),
            Some("/data/paneflow-mcp")
        );
        assert!(doc["mcp_servers"]["paneflow"]["env_vars"]
            .as_array()
            .is_some_and(|env_vars| env_vars.len() == 2));
    }

    #[test]
    fn install_cli_success_falls_back_when_file_does_not_match() {
        // CLI exited 0 but did not write our watched file.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = Codex {
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
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["command"].as_str(),
            Some("/data/paneflow-mcp")
        );
    }

    #[test]
    fn install_cli_failure_falls_back_without_remove() {
        // Non-idempotent `mcp add` must not be preceded by `mcp remove`.
        // The locked in-place upsert (#42) updates command/args and keeps
        // unknown keys.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "# codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.github]\ncommand = \"gh-mcp\"\n\n[mcp_servers.paneflow]\ncommand = \"/old/paneflow-mcp\"\nargs = []\nstartup_timeout_sec = 10\n",
        )
        .unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = Codex {
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
        let txt = std::fs::read_to_string(&p).unwrap();
        assert!(txt.contains("# codex config"));
        assert!(txt.contains("model = \"gpt-5\""));
        assert!(txt.contains("gh-mcp"), "sibling server preserved");
        assert!(
            txt.contains("startup_timeout_sec = 10"),
            "unknown keys must survive the locked merge fallback"
        );
        let doc = txt.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            doc["mcp_servers"]["paneflow"]["command"].as_str(),
            Some("/data/paneflow-mcp")
        );
    }

    #[test]
    fn uninstall_cli_success_falls_back_when_entry_still_present() {
        // Issue #215: `codex mcp remove` can exit 0 without removing the
        // entry from the watched file (e.g. it edited a different
        // `$CODEX_HOME`). Uninstall must verify the postcondition and fall
        // back to the locked direct edit instead of reporting Removed.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "# codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.github]\ncommand = \"gh-mcp\"\n\n[mcp_servers.paneflow]\ncommand = \"/data/paneflow-mcp\"\nargs = []\n",
        )
        .unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_h = Rc::clone(&calls);
        let w = Codex {
            config_path: Some(p.clone()),
            allow_cli: true,
            cli: Some(Box::new(move |args| {
                record_args(&calls_h, args);
                Ok(()) // exit 0 without touching the file
            })),
        };

        assert_eq!(w.uninstall().unwrap(), UninstallOutcome::Removed);
        assert!(
            calls
                .borrow()
                .iter()
                .any(|args| args.windows(2).any(|pair| pair == ["mcp", "remove"])),
            "expected mcp remove: {:?}",
            calls.borrow()
        );
        let txt = std::fs::read_to_string(&p).unwrap();
        assert!(txt.contains("# codex config"));
        assert!(txt.contains("gh-mcp"), "sibling server preserved");
        let doc = txt.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(
            doc.get("mcp_servers")
                .and_then(|t| t.get("paneflow"))
                .is_none(),
            "fallback must remove the entry the CLI left behind: {txt}"
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
