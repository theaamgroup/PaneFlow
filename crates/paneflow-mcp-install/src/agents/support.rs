//! Shared plumbing for the per-agent writers.
//!
//! - Config-path resolution from a home dir and one environment's overrides
//!   ([`crate::agents::AgentConfigEnv`]).
//! - Format-generic uninstall / status built on the tested [`crate::merge`]
//!   and [`crate::io`] primitives. Every removal edits the agent's config
//!   file directly under the PaneFlow config lock; no agent CLI is spawned. A
//!   write first copies the parsed bytes to a backup that never replaces an
//!   existing file, and is refused when the file changed since the parse.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::agents::{StatusOutcome, UninstallOutcome};
use crate::{io, merge};

/// The entry name PaneFlow registered under each agent's container key.
pub(crate) const ENTRY: &str = "paneflow";

/// File name of the retired bridge binary. Only an entry whose command has
/// this file name is PaneFlow's; any other `paneflow` entry is the user's.
const BRIDGE_BINARY: &str = "paneflow-mcp";

/// Does `command` run a `paneflow-mcp` binary, at any path? Covers entries
/// left at an older data-dir path as well as the current one.
pub(crate) fn names_bridge_binary(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .is_some_and(|name| name == BRIDGE_BINARY)
}

// ---------------------------------------------------------------------------
// Config paths (resolved against the real home / XDG dirs)
// ---------------------------------------------------------------------------

/// User-scope MCP file. Official default is `$HOME/.claude.json` (NOT
/// `~/.claude/.claude.json`). When `CLAUDE_CONFIG_DIR` is set and non-empty,
/// Claude Code reads `.claude.json` from inside that directory instead.
pub(crate) fn claude_config_from(
    home: Option<PathBuf>,
    claude_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    // `$CLAUDE_CONFIG_DIR/.claude.json` when set, else `$HOME/.claude.json`
    // (NOT `~/.claude/.claude.json` — that is the settings dir).
    claude_config_dir
        .clone()
        .filter(|p| !p.as_os_str().is_empty())
        .and_then(|dir| {
            paneflow_agent_config::claude_config_dir_from(home.clone(), Some(dir))
                .map(|d| d.join(".claude.json"))
        })
        .or_else(|| home.map(|h| h.join(".claude.json")))
}

/// `$CODEX_HOME/config.toml`, falling back to `~/.codex/config.toml`.
pub(crate) fn codex_config_from(
    home: Option<PathBuf>,
    codex_home: Option<OsString>,
) -> Option<PathBuf> {
    codex_home
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| h.join(".codex")))
        .map(|h| h.join("config.toml"))
}

/// `~/.gemini/settings.json`.
pub(crate) fn gemini_config_from(home: Option<PathBuf>) -> Option<PathBuf> {
    home.map(|h| h.join(".gemini").join("settings.json"))
}

/// opencode global config candidates: the one file `OPENCODE_CONFIG` names,
/// else `opencode.jsonc` and `opencode.json` in the config directory. The
/// cleanup probes every candidate.
pub(crate) fn opencode_configs_from(
    home: Option<PathBuf>,
    xdg_config_home: Option<OsString>,
    opencode_config: Option<OsString>,
    opencode_config_dir: Option<OsString>,
) -> Vec<PathBuf> {
    if let Some(config) = opencode_config
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        return vec![config];
    }

    let mut out = Vec::new();
    if let Some(dir) = opencode_config_dir
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        // `OPENCODE_CONFIG_DIR` *is* the config directory (the one holding
        // `opencode.json`), not its parent - the same reading the shim's
        // `opencode_config_dir_from` uses (issue #233).
        push_opencode_names_in(&mut out, dir);
        return out;
    }

    {
        if let Some(dir) = xdg_config_home
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| home.map(|h| h.join(".config")))
        {
            push_opencode_names(&mut out, dir);
        }
    }

    out
}

fn push_opencode_names(out: &mut Vec<PathBuf>, config_base: PathBuf) {
    push_opencode_names_in(out, config_base.join("opencode"));
}

fn push_opencode_names_in(out: &mut Vec<PathBuf>, dir: PathBuf) {
    out.push(dir.join("opencode.jsonc"));
    out.push(dir.join("opencode.json"));
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The config text at `path`, or `None` when there is nothing to probe: the
/// file does not exist, or its raw bytes never mention a `paneflow-mcp`
/// binary. The byte scan runs before any parse, so a config that is too large
/// for [`io::read_config`], or does not parse, but never named the bridge
/// does not hold the binary back.
fn read_if_it_may_name_bridge(path: &Path) -> Result<Option<String>> {
    if !path.exists() || !io::may_contain(path, BRIDGE_BINARY.as_bytes()) {
        return Ok(None);
    }
    io::read_config(path)
}

// ---------------------------------------------------------------------------
// JSON / JSONC uninstall / status (Claude Code, Gemini, opencode)
// ---------------------------------------------------------------------------

/// Remove `root[container][paneflow]` at `path` when `command_of` says the
/// entry runs a `paneflow-mcp` binary. No-op when the file or entry is
/// absent; a `paneflow` entry with any other command is left alone
/// ([`UninstallOutcome::KeptUserEntry`]). Every file is parsed as JSONC and
/// edited by splicing, so the rest of it stays byte for byte. A
/// present-but-invalid file, or a file that changes between the parse and
/// the rename, is never written.
pub(crate) fn json_uninstall(
    path: &Path,
    container: &str,
    command_of: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<UninstallOutcome> {
    if !path.exists() {
        return Ok(UninstallOutcome::NothingToRemove);
    }
    io::with_config_lock(path, || {
        let Some(source) = io::read_config(path)? else {
            return Ok(UninstallOutcome::NothingToRemove);
        };
        let root = merge::parse_json_family(path, &source)?;
        let Some(entry) = json_entry(&root, container)? else {
            return Ok(UninstallOutcome::NothingToRemove);
        };
        let command = command_of(entry);
        if !command.as_deref().is_some_and(names_bridge_binary) {
            return Ok(UninstallOutcome::KeptUserEntry { command });
        }
        let updated = merge::remove_json_family_entry(path, &source, container, ENTRY)?
            .with_context(|| format!("{} lost its `{ENTRY}` entry mid-edit", path.display()))?;
        let backup = io::replace_unchanged_unlocked(path, &source, &updated)?;
        Ok(UninstallOutcome::Removed {
            file: path.to_path_buf(),
            backup,
        })
    })
}

/// Read-only state of the `paneflow` entry in the JSON or JSONC file at
/// `path`. `validate` classifies the entry (string command for most agents,
/// first array element for opencode). `expected` is the bridge path used to
/// flag staleness when it is available. A file that never mentions a
/// `paneflow-mcp` binary reads as `NotInstalled` without being parsed.
pub(crate) fn json_status(
    path: &Path,
    container: &str,
    expected: Option<&Path>,
    validate: impl Fn(&serde_json::Value, Option<&Path>) -> StatusOutcome,
) -> Result<StatusOutcome> {
    let Some(source) = read_if_it_may_name_bridge(path)? else {
        return Ok(StatusOutcome::NotInstalled);
    };
    let root = merge::parse_json_family(path, &source)?;
    let Some(entry) = json_entry(&root, container)? else {
        return Ok(StatusOutcome::NotInstalled);
    };
    Ok(validate(entry, expected))
}

/// `root[container][paneflow]`, `None` when either level is absent. A
/// non-object container is an error: it cannot be classified or edited.
fn json_entry<'a>(
    root: &'a serde_json::Value,
    container: &str,
) -> Result<Option<&'a serde_json::Value>> {
    let Some(container_value) = root.get(container) else {
        return Ok(None);
    };
    let Some(container_object) = container_value.as_object() else {
        bail!("config key `{container}` is not an object - refusing to classify it");
    };
    Ok(container_object.get(ENTRY))
}

// ---------------------------------------------------------------------------
// TOML uninstall / status (Codex)
// ---------------------------------------------------------------------------

/// Codex's parent table for MCP servers.
pub(crate) const CODEX_TABLE: &str = "mcp_servers";

/// PaneFlow pane identity that Codex was told to forward to the bridge.
/// Part of the shape PaneFlow wrote, so `status` can tell its own entry
/// from a hand-edited one.
pub(crate) const CODEX_ENV_VARS: &[&str] = &[
    "PANEFLOW_SOCKET_PATH",
    "PANEFLOW_WORKSPACE_ID",
    "PANEFLOW_SURFACE_ID",
];

/// Static Codex `env` keys that override the identity the pane is supposed
/// to forward through [`CODEX_ENV_VARS`].
const CODEX_FORBIDDEN_ENV_KEYS: &[&str] = &[
    "PANEFLOW_MCP_SCOPE",
    "PANEFLOW_SOCKET_PATH",
    "PANEFLOW_WORKSPACE_ID",
    "PANEFLOW_SURFACE_ID",
];

const CODEX_ENV_OVERRIDE_REASON: &str = "Codex MCP env must not set PANEFLOW_MCP_SCOPE, PANEFLOW_SOCKET_PATH, PANEFLOW_WORKSPACE_ID, or PANEFLOW_SURFACE_ID";

fn codex_env_vars_ok(entry: &toml_edit::Item) -> bool {
    entry
        .get("env_vars")
        .and_then(toml_edit::Item::as_array)
        .is_some_and(|array| {
            CODEX_ENV_VARS
                .iter()
                .all(|required| array.iter().any(|item| item.as_str() == Some(*required)))
        })
}

fn codex_env_has_forbidden_override(entry: &toml_edit::Item) -> bool {
    entry
        .get("env")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|env| {
            CODEX_FORBIDDEN_ENV_KEYS
                .iter()
                .any(|key| env.contains_key(key))
        })
}

/// `command` of a Codex entry, when it is a string.
fn toml_command(entry: &toml_edit::Item) -> Option<String> {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::to_string)
}

/// Remove `[mcp_servers.paneflow]` at `path` when its command runs a
/// `paneflow-mcp` binary. Same contract as [`json_uninstall`]. An inline
/// `mcp_servers = { ... }` parent is refused: it can still hold the entry,
/// so it is an error, never "nothing to remove".
pub(crate) fn toml_uninstall(path: &Path) -> Result<UninstallOutcome> {
    if !path.exists() {
        return Ok(UninstallOutcome::NothingToRemove);
    }
    io::with_config_lock(path, || {
        let Some(source) = io::read_config(path)? else {
            return Ok(UninstallOutcome::NothingToRemove);
        };
        let mut doc = merge::parse_toml(path, &source)?;
        if doc.get(CODEX_TABLE).is_some_and(|item| !item.is_table()) {
            bail!("`{CODEX_TABLE}` is not a TOML table - refusing to overwrite");
        }
        let Some(entry) = doc.get(CODEX_TABLE).and_then(|t| t.get(ENTRY)) else {
            return Ok(UninstallOutcome::NothingToRemove);
        };
        let command = toml_command(entry);
        if !command.as_deref().is_some_and(names_bridge_binary) {
            return Ok(UninstallOutcome::KeptUserEntry { command });
        }
        merge::remove_toml_entry(&mut doc, CODEX_TABLE, ENTRY);
        let backup = io::replace_unchanged_unlocked(path, &source, &doc.to_string())?;
        Ok(UninstallOutcome::Removed {
            file: path.to_path_buf(),
            backup,
        })
    })
}

/// Read-only state of the Codex entry. Same byte-scan shortcut as
/// [`json_status`].
pub(crate) fn toml_status(path: &Path, expected: Option<&Path>) -> Result<StatusOutcome> {
    let Some(source) = read_if_it_may_name_bridge(path)? else {
        return Ok(StatusOutcome::NotInstalled);
    };
    let doc = merge::parse_toml(path, &source)?;
    let Some(entry) = doc.get(CODEX_TABLE).and_then(|t| t.get(ENTRY)) else {
        return Ok(StatusOutcome::NotInstalled);
    };
    let found = toml_command(entry);
    let args_ok = entry
        .get("args")
        .and_then(|a| a.as_array())
        .is_some_and(|args| args.is_empty());
    let enabled_ok = entry
        .get("enabled")
        .and_then(|e| e.as_bool())
        .unwrap_or(true);
    let forbidden_env = codex_env_has_forbidden_override(entry);
    let shape_ok = args_ok && enabled_ok && codex_env_vars_ok(entry) && !forbidden_env;
    let reason = if forbidden_env {
        CODEX_ENV_OVERRIDE_REASON
    } else {
        "Codex MCP entry must have empty args, forward PaneFlow's socket/workspace variables, and must not be disabled"
    };
    Ok(classify_entry(found, expected, shape_ok, reason))
}

// ---------------------------------------------------------------------------
// Command-path extractors
// ---------------------------------------------------------------------------

/// `command` as a plain string (Claude Code, Gemini).
pub(crate) fn string_command(entry: &serde_json::Value) -> Option<String> {
    entry.get("command")?.as_str().map(str::to_string)
}

/// `command` as an array whose first element is the binary path (opencode).
pub(crate) fn array_command(entry: &serde_json::Value) -> Option<String> {
    entry
        .get("command")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Compare a found command path and entry shape against the expected bridge
/// path, when that path is available.
pub(crate) fn classify_entry(
    found: Option<String>,
    expected: Option<&Path>,
    shape_ok: bool,
    repair_reason: &str,
) -> StatusOutcome {
    let Some(found) = found.filter(|p| !p.is_empty()) else {
        return StatusOutcome::NeedsRepair {
            path: None,
            reason: "MCP entry is missing a command path".to_string(),
        };
    };

    if let Some(expected) = expected {
        let expected = expected.to_string_lossy();
        if found != expected {
            return StatusOutcome::StalePath {
                found,
                expected: expected.into_owned(),
            };
        }
    }

    if !shape_ok {
        return StatusOutcome::NeedsRepair {
            path: Some(found),
            reason: repair_reason.to_string(),
        };
    }

    StatusOutcome::Installed { path: found }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validate_string_entry(entry: &serde_json::Value, expected: Option<&Path>) -> StatusOutcome {
        classify_entry(string_command(entry), expected, true, "shape mismatch")
    }

    #[test]
    fn names_bridge_binary_matches_the_file_name_at_any_path() {
        assert!(names_bridge_binary(
            "/Users/a/Library/Application Support/paneflow/bin/paneflow-mcp"
        ));
        assert!(names_bridge_binary("/old/paneflow-dev/bin/paneflow-mcp"));
        assert!(names_bridge_binary("paneflow-mcp"));
        assert!(!names_bridge_binary("/usr/local/bin/paneflow-mcp-proxy"));
        assert!(!names_bridge_binary("/opt/paneflow-mcp/server"));
        assert!(!names_bridge_binary("npx"));
        assert!(!names_bridge_binary(""));
    }

    #[test]
    fn claude_config_default_is_home_dot_claude_json() {
        assert_eq!(
            claude_config_from(Some(PathBuf::from("/home/alice")), None).unwrap(),
            PathBuf::from("/home/alice/.claude.json")
        );
        assert_eq!(
            claude_config_from(Some(PathBuf::from("/home/alice")), Some(OsString::from("")))
                .unwrap(),
            PathBuf::from("/home/alice/.claude.json")
        );
    }

    #[test]
    fn claude_config_honors_claude_config_dir() {
        assert_eq!(
            claude_config_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("/tmp/claude-cfg"))
            )
            .unwrap(),
            PathBuf::from("/tmp/claude-cfg").join(".claude.json")
        );
    }

    #[test]
    fn codex_config_honors_codex_home() {
        assert_eq!(
            codex_config_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("/tmp/codex-home"))
            )
            .unwrap(),
            PathBuf::from("/tmp/codex-home").join("config.toml")
        );
    }

    #[test]
    fn opencode_config_candidates_prefer_custom_path() {
        assert_eq!(
            opencode_configs_from(
                Some(PathBuf::from("/home/alice")),
                None,
                Some(OsString::from("/tmp/opencode.jsonc")),
                None,
            ),
            vec![PathBuf::from("/tmp/opencode.jsonc")]
        );
    }

    #[test]
    fn opencode_config_candidates_prefer_jsonc_in_custom_dir() {
        assert_eq!(
            opencode_configs_from(
                Some(PathBuf::from("/home/alice")),
                None,
                None,
                Some(OsString::from("/tmp/opencode-config")),
            ),
            vec![
                PathBuf::from("/tmp/opencode-config").join("opencode.jsonc"),
                PathBuf::from("/tmp/opencode-config").join("opencode.json"),
            ]
        );
    }

    /// Issue #233: the shim's `opencode_config_dir_from`
    /// (`crates/paneflow-shim/src/hooks/opencode.rs`) treats
    /// `OPENCODE_CONFIG_DIR` as the config directory itself and writes
    /// `$DIR/opencode.json`; the candidates this crate edits must resolve to
    /// the same file for the same env, or the cleanup looks for the bridge
    /// entry where OpenCode never loaded it.
    #[test]
    fn opencode_config_candidates_agree_with_shim_config_dir() {
        let home = Some(PathBuf::from("/Users/alice"));
        let cases = [
            (
                Some("/Users/alice/.config"),
                None,
                None,
                "/Users/alice/.config/opencode",
            ),
            (
                None,
                Some("/tmp/custom/opencode.json"),
                Some("/tmp/ignored"),
                "/tmp/custom",
            ),
            (None, None, Some("/tmp/opencode"), "/tmp/opencode"),
            (None, None, None, "/Users/alice/.config/opencode"),
        ];
        for (xdg, config, config_dir, shim_dir) in cases {
            let candidates = opencode_configs_from(
                home.clone(),
                xdg.map(OsString::from),
                config.map(OsString::from),
                config_dir.map(OsString::from),
            );
            let json = PathBuf::from(shim_dir).join("opencode.json");
            assert!(
                candidates.contains(&json),
                "env xdg={xdg:?} config={config:?} dir={config_dir:?}: {candidates:?} lacks {json:?}"
            );
            for candidate in &candidates {
                assert_eq!(
                    candidate.parent(),
                    Some(Path::new(shim_dir)),
                    "candidate {candidate:?} not in the shim's directory"
                );
            }
        }
    }

    #[test]
    fn json_uninstall_removes_only_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "mcpServers": {
                    "paneflow": { "command": "/data/paneflow-mcp" },
                    "other": { "command": "x" }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            json_uninstall(&p, "mcpServers", string_command).unwrap(),
            UninstallOutcome::Removed { .. }
        ));
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(after["mcpServers"].get("paneflow").is_none());
        assert_eq!(after["mcpServers"]["other"]["command"], json!("x"));
        // Second uninstall → nothing to remove.
        assert_eq!(
            json_uninstall(&p, "mcpServers", string_command).unwrap(),
            UninstallOutcome::NothingToRemove
        );
    }

    #[test]
    fn json_uninstall_keeps_a_paneflow_entry_with_another_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        let before = serde_json::to_vec(&json!({
            "mcpServers": { "paneflow": { "command": "/usr/local/bin/my-server" } }
        }))
        .unwrap();
        std::fs::write(&p, &before).unwrap();

        assert_eq!(
            json_uninstall(&p, "mcpServers", string_command).unwrap(),
            UninstallOutcome::KeptUserEntry {
                command: Some("/usr/local/bin/my-server".into())
            }
        );
        assert_eq!(std::fs::read(&p).unwrap(), before);
        assert!(!dir.path().join("settings.json.bak").exists());
    }

    #[test]
    fn json_uninstall_absent_file_does_not_create_parent_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("missing-parent").join("settings.json");

        assert_eq!(
            json_uninstall(&p, "mcpServers", string_command).unwrap(),
            UninstallOutcome::NothingToRemove
        );
        assert!(!p.parent().unwrap().exists());
    }

    #[test]
    fn json_status_reports_installed_and_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            serde_json::to_vec(
                &json!({ "mcpServers": { "paneflow": { "command": "/cur/paneflow-mcp" } } }),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            json_status(
                &p,
                "mcpServers",
                Some(Path::new("/cur/paneflow-mcp")),
                validate_string_entry,
            )
            .unwrap(),
            StatusOutcome::Installed {
                path: "/cur/paneflow-mcp".into()
            }
        );
        assert_eq!(
            json_status(
                &p,
                "mcpServers",
                Some(Path::new("/new/paneflow-mcp")),
                validate_string_entry,
            )
            .unwrap(),
            StatusOutcome::StalePath {
                found: "/cur/paneflow-mcp".into(),
                expected: "/new/paneflow-mcp".into()
            }
        );
    }

    #[test]
    fn json_status_not_installed_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("missing.json");
        assert_eq!(
            json_status(
                &p,
                "mcpServers",
                Some(Path::new("/x")),
                validate_string_entry,
            )
            .unwrap(),
            StatusOutcome::NotInstalled
        );
    }

    #[test]
    fn json_status_without_expected_path_requires_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            serde_json::to_vec(
                &json!({ "mcpServers": { "paneflow": { "args": ["paneflow-mcp"] } } }),
            )
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            json_status(&p, "mcpServers", None, validate_string_entry).unwrap(),
            StatusOutcome::NeedsRepair { .. }
        ));
    }

    #[test]
    fn toml_uninstall_and_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[mcp_servers.paneflow]\n\
             command = \"/cur/paneflow-mcp\"\n\
             args = []\n\
             env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\n",
        )
        .unwrap();

        assert_eq!(
            toml_status(&p, Some(Path::new("/cur/paneflow-mcp"))).unwrap(),
            StatusOutcome::Installed {
                path: "/cur/paneflow-mcp".into()
            }
        );
        assert_eq!(
            toml_status(&p, Some(Path::new("/new/paneflow-mcp"))).unwrap(),
            StatusOutcome::StalePath {
                found: "/cur/paneflow-mcp".into(),
                expected: "/new/paneflow-mcp".into()
            }
        );
        assert!(matches!(
            toml_uninstall(&p).unwrap(),
            UninstallOutcome::Removed { .. }
        ));
        assert_eq!(
            toml_uninstall(&p).unwrap(),
            UninstallOutcome::NothingToRemove
        );
        assert_eq!(
            toml_status(&p, Some(Path::new("/cur/paneflow-mcp"))).unwrap(),
            StatusOutcome::NotInstalled
        );
    }

    #[test]
    fn toml_status_needs_repair_when_disabled_in_either_table_form() {
        for src in [
            "[mcp_servers.paneflow]\n\
             command = \"/cur/paneflow-mcp\"\n\
             args = []\n\
             env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\n\
             enabled = false\n",
            "[mcp_servers]\npaneflow = { command = \"/cur/paneflow-mcp\", args = [], env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"], enabled = false }\n",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let p = dir.path().join("config.toml");
            std::fs::write(&p, src).unwrap();
            assert_eq!(
                toml_status(&p, Some(Path::new("/cur/paneflow-mcp"))).unwrap(),
                StatusOutcome::NeedsRepair {
                    path: Some("/cur/paneflow-mcp".into()),
                    reason: "Codex MCP entry must have empty args, forward PaneFlow's socket/workspace variables, and must not be disabled".into(),
                },
                "{src}"
            );
        }
    }

    #[test]
    fn toml_uninstall_absent_file_does_not_create_parent_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("missing-parent").join("config.toml");

        assert_eq!(
            toml_uninstall(&p).unwrap(),
            UninstallOutcome::NothingToRemove
        );
        assert!(!p.parent().unwrap().exists());
    }

    #[test]
    fn array_command_extracts_first_element() {
        let entry = json!({ "type": "local", "command": ["/bin/paneflow-mcp"], "enabled": true });
        assert_eq!(array_command(&entry), Some("/bin/paneflow-mcp".to_string()));
    }
}
