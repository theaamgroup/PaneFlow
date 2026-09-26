//! First-launch removal of the retired MCP bridge (issue #857).
//!
//! Older PaneFlow builds extracted `paneflow-mcp` on every launch and could
//! register it with Claude Code, Codex, Gemini CLI and opencode. This pass
//! undoes both:
//!
//! 1. Nothing happens unless the extracted binary still exists. Once it is
//!    gone, later launches cost one `stat`.
//! 2. Each agent's config is probed read-only: the default files under the
//!    home directory, every file the process environment names, and every
//!    file the login shell's environment names when the app captured it
//!    (opencode merges its global file with an `OPENCODE_CONFIG` one, so an
//!    override never hides the default). A file whose raw bytes never mention
//!    `paneflow-mcp` is not parsed. Only a `paneflow` entry whose command
//!    runs a `paneflow-mcp` binary (at any path) is PaneFlow's; any other
//!    `paneflow` entry is the user's own and stays.
//! 3. PaneFlow's entry is removed under the config lock by splicing it out,
//!    so every other byte of the file stays. The old bytes go to a backup
//!    that never replaces an existing file. Nothing is written when the file
//!    fails to parse, is a Codex inline `mcp_servers` table, or no longer
//!    holds the parsed bytes when it is re-read right before the rename. A
//!    writer that skips PaneFlow's lock (Claude Code) can still lose a write
//!    that lands between that re-read and the rename; see the `io` module.
//! 4. The binary is deleted in a second phase: only by a pass that finds no
//!    bridge entry at all - nothing removed, nothing kept, no config it
//!    could not read. A pass that removed entries keeps it, because an agent
//!    still running with its config in memory can write the entry back after
//!    our rename; the next launch re-probes and deletes the binary once
//!    everything is clean. A kept entry keeps the binary too, so it keeps
//!    working instead of failing on every agent start.
//!
//! One `info` line per config changed, one `warn` per entry kept, and one
//! `info` line saying whether the binary went. There is no UI.

use std::path::Path;

use crate::agents::{
    self, names_bridge_binary, AgentConfigEnv, AgentConfigWriter, StatusOutcome, UninstallOutcome,
};

/// What one pass did. Production only logs it; tests assert on it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// Ids of the agents whose `paneflow` entry was removed.
    pub removed: Vec<&'static str>,
    /// Ids of the agents whose bridge entry stayed, with the reason.
    pub kept: Vec<(&'static str, String)>,
    /// Whether any probe found a bridge entry, or could not tell. The
    /// binary is deleted only by a pass where this stays `false`.
    pub found_entry: bool,
    /// Whether the extracted bridge binary was deleted.
    pub binary_deleted: bool,
}

/// Remove PaneFlow's `paneflow` MCP entry from every supported agent, and
/// delete the extracted bridge `binary` on a later pass that finds none.
///
/// `edits_refused` is `Some(reason)` when this process must not edit agent
/// configs (a debug build without `PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1`, or a
/// run with `PANEFLOW_HOME` set): the pass then only reads, and keeps the
/// binary while an entry still points at one.
///
/// The config files probed are the defaults under the home directory, the
/// ones this process's environment names, and, when `login_shell` is given,
/// the ones the login shell's profile names (`CLAUDE_CONFIG_DIR`,
/// `CODEX_HOME`, … exported only there are invisible to a GUI launch).
///
/// Blocking: it reads and writes agent config files and waits (bounded) on
/// the config lock, so callers run it off the UI thread.
pub fn remove_legacy_bridge(
    binary: &Path,
    edits_refused: Option<&str>,
    login_shell: Option<&AgentConfigEnv>,
) -> CleanupReport {
    let envs = probed_envs(AgentConfigEnv::from_process(), login_shell);
    let writers = agents::writers_for(dirs::home_dir().as_deref(), &envs);
    let writers: Vec<&dyn AgentConfigWriter> = writers.iter().map(AsRef::as_ref).collect();
    remove_legacy_bridge_with(binary, &writers, edits_refused)
}

/// The environments whose config files a pass probes, in order: this
/// process's, the no-override defaults, then the login shell's when it set
/// anything. `writers_for` drops the files two of them share.
fn probed_envs(
    process: AgentConfigEnv,
    login_shell: Option<&AgentConfigEnv>,
) -> Vec<AgentConfigEnv> {
    let mut envs = vec![process, AgentConfigEnv::default()];
    envs.extend(login_shell.filter(|env| !env.is_empty()).cloned());
    envs
}

pub(crate) fn remove_legacy_bridge_with(
    binary: &Path,
    writers: &[&dyn AgentConfigWriter],
    edits_refused: Option<&str>,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    // Nothing was ever extracted here, or an earlier pass finished: read no
    // config at all.
    if std::fs::symlink_metadata(binary).is_err() {
        return report;
    }

    for writer in writers {
        let agent = writer.label();
        let status = match writer.status(Some(binary)) {
            Ok(status) => status,
            Err(error) => {
                report.found_entry = true;
                keep(&mut report, *writer, format!("{error:#}"));
                continue;
            }
        };
        // `NotInstalled`, a command-less entry, or a `paneflow` entry that
        // runs something else: not PaneFlow's, and it does not keep the
        // binary alive.
        if !StatusOutcome::command(&status).is_some_and(names_bridge_binary) {
            continue;
        }
        report.found_entry = true;
        if let Some(reason) = edits_refused {
            keep(&mut report, *writer, reason.to_string());
            continue;
        }
        match writer.uninstall() {
            Ok(UninstallOutcome::Removed { file, backup }) => {
                log::info!(
                    "mcp bridge cleanup: removed the paneflow entry from {agent} ({}; backup {})",
                    file.display(),
                    backup.display()
                );
                report.removed.push(writer.id());
            }
            // The entry vanished, or became the user's own, between the
            // probe and the locked re-parse. None of ours is left, but the
            // file is moving under us: let the next pass confirm it.
            Ok(UninstallOutcome::NothingToRemove | UninstallOutcome::KeptUserEntry { .. }) => {}
            Err(error) => keep(&mut report, *writer, format!("{error:#}")),
        }
    }

    if !report.kept.is_empty() {
        log::info!(
            "mcp bridge cleanup: kept {} because an entry may still point at it",
            binary.display()
        );
    } else if report.found_entry {
        log::info!(
            "mcp bridge cleanup: kept {} until a later launch finds no entry",
            binary.display()
        );
    } else {
        match std::fs::remove_file(binary) {
            Ok(()) => {
                log::info!("mcp bridge cleanup: deleted {}", binary.display());
                report.binary_deleted = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!(
                "mcp bridge cleanup: could not delete {} ({error}); the next launch retries",
                binary.display()
            ),
        }
    }
    report
}

/// Record and log an entry that stays, which keeps the binary too.
fn keep(report: &mut CleanupReport, writer: &dyn AgentConfigWriter, reason: String) {
    log::warn!(
        "mcp bridge cleanup: kept the {} entry: {reason}",
        writer.label()
    );
    report.kept.push((writer.id(), reason));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::claude_code::ClaudeCode;
    use crate::agents::codex::Codex;
    use crate::agents::gemini::Gemini;
    use crate::agents::opencode::OpenCode;
    use crate::agents::testutil::Mock;
    use std::path::PathBuf;

    const GATE: &str = "debug builds do not edit agent configs";

    /// A temp dir holding an extracted-looking bridge binary.
    fn bridge() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let binary = dir.path().join("bin").join("paneflow-mcp");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"\xcf\xfa\xed\xfe bridge").unwrap();
        (dir, binary)
    }

    fn run(binary: &Path, writers: &[&dyn AgentConfigWriter]) -> CleanupReport {
        remove_legacy_bridge_with(binary, writers, None)
    }

    fn bak(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".bak");
        PathBuf::from(name)
    }

    /// Phase two of the binary's deletion: the pass that removed an entry
    /// kept the binary, and the next pass over the same files finds nothing
    /// of ours and deletes it.
    fn assert_deleted_on_the_next_pass(
        binary: &Path,
        first: &CleanupReport,
        writers: &[&dyn AgentConfigWriter],
    ) {
        assert!(
            first.found_entry && !first.binary_deleted && binary.exists(),
            "{first:?}"
        );
        let second = run(binary, writers);
        assert!(
            second.removed.is_empty() && second.kept.is_empty() && !second.found_entry,
            "{second:?}"
        );
        assert!(second.binary_deleted && !binary.exists(), "{second:?}");
    }

    // -- per agent, real files --------------------------------------------

    #[test]
    fn claude_code_entry_is_removed_and_everything_else_stays() {
        let (dir, binary) = bridge();
        let config = dir.path().join(".claude.json");
        let before = serde_json::to_string_pretty(&serde_json::json!({
            "numStartups": 42,
            "projects": { "/repo": { "allowedTools": [] } },
            "mcpServers": {
                "github": { "command": "gh-mcp" },
                "paneflow": { "type": "stdio", "command": binary, "args": [] }
            }
        }))
        .unwrap()
            + "\n";
        std::fs::write(&config, &before).unwrap();

        let writer = ClaudeCode::at(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["claude-code"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        let expected = serde_json::to_string_pretty(&serde_json::json!({
            "numStartups": 42,
            "projects": { "/repo": { "allowedTools": [] } },
            "mcpServers": { "github": { "command": "gh-mcp" } }
        }))
        .unwrap()
            + "\n";
        assert_eq!(std::fs::read_to_string(&config).unwrap(), expected);
        assert_eq!(std::fs::read_to_string(bak(&config)).unwrap(), before);
    }

    #[test]
    fn codex_entry_is_removed_and_other_tables_and_comments_stay_byte_for_byte() {
        let (dir, binary) = bridge();
        let config = dir.path().join("config.toml");
        let head = "# my codex config\nmodel = \"gpt-5\" # pinned\n\n\
                    [mcp_servers.github]\ncommand = \"gh-mcp\" # inline comment\n";
        let tail = "\n# trailing tables stay too\n[profiles.fast]\nmodel = \"gpt-5-mini\"\n";
        let entry = format!(
            "\n[mcp_servers.paneflow]\ncommand = \"{}\"\nargs = []\n\
             env_vars = [\"PANEFLOW_SOCKET_PATH\", \"PANEFLOW_WORKSPACE_ID\", \"PANEFLOW_SURFACE_ID\"]\n",
            binary.display()
        );
        let before = format!("{head}{entry}{tail}");
        std::fs::write(&config, &before).unwrap();

        let writer = Codex::at(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["codex"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            format!("{head}{tail}")
        );
        assert_eq!(std::fs::read_to_string(bak(&config)).unwrap(), before);
    }

    #[test]
    fn gemini_entry_is_removed_and_everything_else_stays() {
        let (dir, binary) = bridge();
        let config = dir.path().join("settings.json");
        let before = serde_json::to_string_pretty(&serde_json::json!({
            "theme": "GitHub",
            "mcpServers": {
                "atlas": { "command": "atlas-mcp" },
                "paneflow": { "command": binary, "args": [], "trust": true },
                "context7": { "command": "c7" },
                "zeta": { "command": "zeta-mcp" }
            },
            "general": { "vimMode": true }
        }))
        .unwrap()
            + "\n";
        std::fs::write(&config, &before).unwrap();

        let writer = Gemini::at(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["gemini"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        // Siblings keep their order: the last one is not swapped forward.
        let expected = serde_json::to_string_pretty(&serde_json::json!({
            "theme": "GitHub",
            "mcpServers": {
                "atlas": { "command": "atlas-mcp" },
                "context7": { "command": "c7" },
                "zeta": { "command": "zeta-mcp" }
            },
            "general": { "vimMode": true }
        }))
        .unwrap()
            + "\n";
        assert_eq!(std::fs::read_to_string(&config).unwrap(), expected);
        assert_eq!(std::fs::read_to_string(bak(&config)).unwrap(), before);
    }

    #[test]
    fn opencode_jsonc_entry_is_removed_and_comments_stay_byte_for_byte() {
        let (dir, binary) = bridge();
        let config = dir.path().join("opencode.jsonc");
        let head =
            "{\n  // my opencode config\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \
                    \"mcp\": {\n    /* weather first */\n    \
                    \"weather\": { \"type\": \"local\", \"command\": [\"weather-mcp\"] }";
        let entry = format!(
            ",\n    \"paneflow\": {{ \"type\": \"local\", \"command\": [\"{}\"], \"enabled\": true }}",
            binary.display()
        );
        let tail = "\n  },\n  \"theme\": \"dark\", // trailing comma kept\n}\n";
        let before = format!("{head}{entry}{tail}");
        std::fs::write(&config, &before).unwrap();

        let writer = OpenCode::new(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["opencode"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            format!("{head}{tail}")
        );
        assert_eq!(std::fs::read_to_string(bak(&config)).unwrap(), before);
    }

    #[test]
    fn an_entry_at_an_older_data_dir_path_is_removed() {
        // StalePath: the entry names a `paneflow-mcp` left by an older build
        // at a different data dir, not the binary this pass found.
        let (dir, binary) = bridge();
        let config = dir.path().join(".claude.json");
        let old = "/Users/someone/Library/Application Support/paneflow-old/bin/paneflow-mcp";
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "mcpServers": { "paneflow": { "type": "stdio", "command": old, "args": [] } }
            }))
            .unwrap(),
        )
        .unwrap();
        let writer = ClaudeCode::at(config.clone());
        assert!(matches!(
            writer.status(Some(&binary)).unwrap(),
            StatusOutcome::StalePath { .. }
        ));

        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["claude-code"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert!(after["mcpServers"].get("paneflow").is_none(), "{after}");
    }

    #[test]
    fn a_hand_edited_entry_that_still_runs_the_bridge_is_removed() {
        // NeedsRepair with a path: a disabled opencode entry is still ours.
        let (dir, binary) = bridge();
        let config = dir.path().join("opencode.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "mcp": { "paneflow": { "type": "local", "command": [binary], "enabled": false } }
            }))
            .unwrap(),
        )
        .unwrap();

        let writer = OpenCode::new(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["opencode"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
    }

    #[test]
    fn a_paneflow_entry_running_another_command_is_the_users_and_stays() {
        let (dir, binary) = bridge();
        let config = dir.path().join("config.toml");
        let before = "[mcp_servers.paneflow]\ncommand = \"/usr/local/bin/my-paneflow-server\"\nargs = [\"--stdio\"]\n";
        std::fs::write(&config, before).unwrap();

        let report = run(&binary, &[&Codex::at(config.clone())]);

        assert!(report.removed.is_empty() && report.kept.is_empty());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        assert!(!bak(&config).exists());
        assert!(
            report.binary_deleted,
            "a foreign entry does not point at the bridge binary"
        );
    }

    #[test]
    fn a_malformed_config_that_names_the_bridge_is_not_written_and_the_binary_stays() {
        let (dir, binary) = bridge();
        let config = dir.path().join("settings.json");
        let broken = b"{ \"mcpServers\": { \"paneflow\": { \"command\": \"/x/paneflow-mcp\" broken";
        std::fs::write(&config, broken).unwrap();

        let report = run(&binary, &[&Gemini::at(config.clone())]);

        assert_eq!(report.kept.len(), 1);
        assert_eq!(report.kept[0].0, "gemini");
        assert_eq!(std::fs::read(&config).unwrap(), broken);
        assert!(!bak(&config).exists());
        assert!(!report.binary_deleted && binary.exists());
    }

    #[test]
    fn a_config_that_never_names_the_bridge_is_not_parsed_and_does_not_keep_the_binary() {
        // Unparseable, but no `paneflow-mcp` anywhere in it: nothing of ours
        // can be inside, so it must not hold the binary back forever.
        let (dir, binary) = bridge();
        let config = dir.path().join("settings.json");
        std::fs::write(&config, b"{ \"mcpServers\": { broken").unwrap();

        let report = run(&binary, &[&Gemini::at(config.clone())]);

        assert!(report.kept.is_empty(), "{:?}", report.kept);
        assert_eq!(
            std::fs::read(&config).unwrap(),
            b"{ \"mcpServers\": { broken"
        );
        assert!(report.binary_deleted && !binary.exists());
    }

    /// A `.claude.json` past `paneflow-agent-config`'s 1 MiB read cap.
    fn big_claude_json(entry: Option<&Path>) -> String {
        let history: Vec<String> = (0..30_000)
            .map(|i| format!("\"/Users/me/project-{i:05}\": {{ \"allowedTools\": [], \"n\": 0.1234567890123456789 }}"))
            .collect();
        let servers = match entry {
            Some(binary) => format!(
                "{{\n    \"github\": {{ \"command\": \"gh-mcp\" }},\n    \"paneflow\": {{ \"type\": \"stdio\", \"command\": \"{}\", \"args\": [] }}\n  }}",
                binary.display()
            ),
            None => "{\n    \"github\": { \"command\": \"gh-mcp\" }\n  }".to_string(),
        };
        format!(
            "{{\n  \"numStartups\": 42,\n  \"projects\": {{ {} }},\n  \"mcpServers\": {servers}\n}}\n",
            history.join(", ")
        )
    }

    #[test]
    fn a_large_claude_json_without_the_bridge_is_left_alone_and_the_binary_goes() {
        let (dir, binary) = bridge();
        let config = dir.path().join(".claude.json");
        let before = big_claude_json(None);
        assert!(before.len() > 2 << 20, "{}", before.len());
        std::fs::write(&config, &before).unwrap();

        let report = run(&binary, &[&ClaudeCode::at(config.clone())]);

        assert!(report.kept.is_empty() && report.removed.is_empty());
        assert!(report.binary_deleted && !binary.exists());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        assert!(!bak(&config).exists());
    }

    #[test]
    fn a_large_claude_json_with_the_bridge_is_cleaned_byte_for_byte_elsewhere() {
        let (dir, binary) = bridge();
        let config = dir.path().join(".claude.json");
        let before = big_claude_json(Some(&binary));
        assert!(before.len() > 2 << 20, "{}", before.len());
        std::fs::write(&config, &before).unwrap();

        let writer = ClaudeCode::at(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["claude-code"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            big_claude_json(None)
        );
        assert_eq!(std::fs::read_to_string(bak(&config)).unwrap(), before);
    }

    #[test]
    fn a_commented_gemini_settings_json_is_cleaned_and_its_comments_stay() {
        let (dir, binary) = bridge();
        let config = dir.path().join("settings.json");
        let head = "{\n  // Gemini accepts comments in settings.json\n  \"theme\": \"GitHub\", /* inline */\n  \"mcpServers\": {\n    \"context7\": { \"command\": \"c7\" },\n";
        let entry = format!(
            "    \"paneflow\": {{ \"command\": \"{}\", \"args\": [], \"trust\": true }},\n",
            binary.display()
        );
        let tail = "  },\n}\n";
        let before = format!("{head}{entry}{tail}");
        std::fs::write(&config, &before).unwrap();

        let writer = Gemini::at(config.clone());
        let report = run(&binary, &[&writer]);

        assert_eq!(report.removed, ["gemini"]);
        assert_deleted_on_the_next_pass(&binary, &report, &[&writer]);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            format!("{head}{tail}")
        );
        assert_eq!(std::fs::read_to_string(bak(&config)).unwrap(), before);
    }

    #[test]
    fn a_commented_gemini_settings_json_without_the_bridge_is_left_alone() {
        let (dir, binary) = bridge();
        let config = dir.path().join("settings.json");
        let before = "{\n  // no bridge here\n  \"mcpServers\": { \"paneflow\": { \"command\": \"/usr/local/bin/mine\" }, },\n}\n";
        std::fs::write(&config, before).unwrap();

        let report = run(&binary, &[&Gemini::at(config.clone())]);

        assert!(report.kept.is_empty() && report.removed.is_empty());
        assert!(report.binary_deleted);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        assert!(!bak(&config).exists());
    }

    #[test]
    fn both_opencode_files_are_probed_and_only_the_one_with_the_entry_changes() {
        let (dir, binary) = bridge();
        let opencode = dir.path().join("config").join("opencode");
        std::fs::create_dir_all(&opencode).unwrap();
        let jsonc = opencode.join("opencode.jsonc");
        let json = opencode.join("opencode.json");
        let jsonc_before = "{\n  // mine\n  \"mcp\": { \"weather\": { \"type\": \"local\", \"command\": [\"w\"] } },\n}\n";
        std::fs::write(&jsonc, jsonc_before).unwrap();
        let json_head =
            "{\n  \"mcp\": {\n    \"weather\": { \"type\": \"local\", \"command\": [\"w\"] }";
        let json_entry = format!(
            ",\n    \"paneflow\": {{ \"type\": \"local\", \"command\": [\"{}\"], \"enabled\": true }}",
            binary.display()
        );
        let json_tail = "\n  }\n}\n";
        std::fs::write(&json, format!("{json_head}{json_entry}{json_tail}")).unwrap();
        let env = AgentConfigEnv {
            xdg_config_home: Some(dir.path().join("config").into()),
            ..AgentConfigEnv::default()
        };
        let writers = agents::writers_for(Some(dir.path()), &[env]);
        let writers: Vec<&dyn AgentConfigWriter> = writers.iter().map(AsRef::as_ref).collect();

        let report = run(&binary, &writers);

        assert_eq!(report.removed, ["opencode"]);
        assert_deleted_on_the_next_pass(&binary, &report, &writers);
        assert_eq!(std::fs::read_to_string(&jsonc).unwrap(), jsonc_before);
        assert!(!bak(&jsonc).exists());
        assert_eq!(
            std::fs::read_to_string(&json).unwrap(),
            format!("{json_head}{json_tail}")
        );
    }

    #[test]
    fn an_opencode_config_env_naming_a_missing_file_is_nothing_to_remove() {
        let (dir, binary) = bridge();
        let env = AgentConfigEnv {
            opencode_config: Some(dir.path().join("nowhere").join("custom.jsonc").into()),
            ..AgentConfigEnv::default()
        };
        let writers = agents::writers_for(Some(dir.path()), &[env]);
        let writers: Vec<&dyn AgentConfigWriter> = writers.iter().map(AsRef::as_ref).collect();

        let report = run(&binary, &writers);

        assert!(report.kept.is_empty(), "{:?}", report.kept);
        assert!(report.binary_deleted);
        assert!(!dir.path().join("nowhere").exists());
    }

    #[test]
    fn an_opencode_config_override_does_not_hide_the_global_file() {
        // opencode merges `OPENCODE_CONFIG` with its global config, so the
        // bridge entry can sit in the global file while the override is set.
        let (dir, binary) = bridge();
        let global = dir.path().join(".config").join("opencode");
        std::fs::create_dir_all(&global).unwrap();
        let config = global.join("opencode.json");
        std::fs::write(
            &config,
            format!(
                "{{\n  \"mcp\": {{\n    \"paneflow\": {{ \"type\": \"local\", \"command\": [\"{}\"] }}\n  }}\n}}\n",
                binary.display()
            ),
        )
        .unwrap();
        let custom = dir.path().join("project-opencode.json");
        std::fs::write(&custom, "{}\n").unwrap();
        let process = AgentConfigEnv {
            opencode_config: Some(custom.clone().into()),
            ..AgentConfigEnv::default()
        };

        let envs = probed_envs(process.clone(), None);
        assert_eq!(envs, [process, AgentConfigEnv::default()]);
        let writers = agents::writers_for(Some(dir.path()), &envs);
        let writers: Vec<&dyn AgentConfigWriter> = writers.iter().map(AsRef::as_ref).collect();
        let report = run(&binary, &writers);

        assert_eq!(report.removed, ["opencode"]);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "{\n  \"mcp\": {\n  }\n}\n"
        );
        assert_eq!(std::fs::read_to_string(&custom).unwrap(), "{}\n");
        assert_deleted_on_the_next_pass(&binary, &report, &writers);
    }

    #[test]
    fn probed_envs_add_the_defaults_and_a_login_shell_that_set_anything() {
        let login = AgentConfigEnv {
            codex_home: Some("/work/codex".into()),
            ..AgentConfigEnv::default()
        };
        let process = AgentConfigEnv::default();
        assert_eq!(
            probed_envs(process.clone(), Some(&AgentConfigEnv::default())),
            [process.clone(), AgentConfigEnv::default()]
        );
        assert_eq!(
            probed_envs(process.clone(), Some(&login)),
            [process, AgentConfigEnv::default(), login]
        );
    }

    #[test]
    fn an_entry_only_the_login_shell_env_points_at_is_found_and_removed() {
        // `CODEX_HOME` exported only in the login profile: a GUI launch's
        // own environment points at `~/.codex`, which has no entry.
        let (dir, binary) = bridge();
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let config = codex_home.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[mcp_servers.paneflow]\ncommand = \"{}\"\n",
                binary.display()
            ),
        )
        .unwrap();
        let process = AgentConfigEnv::default();
        let login = AgentConfigEnv {
            codex_home: Some(codex_home.clone().into()),
            ..AgentConfigEnv::default()
        };

        let without = agents::writers_for(Some(dir.path()), std::slice::from_ref(&process));
        let without: Vec<&dyn AgentConfigWriter> = without.iter().map(AsRef::as_ref).collect();
        assert!(
            without
                .iter()
                .all(|w| w.status(Some(&binary)).unwrap() == StatusOutcome::NotInstalled),
            "the process env alone cannot see the entry"
        );

        let writers = agents::writers_for(Some(dir.path()), &[process, login]);
        let writers: Vec<&dyn AgentConfigWriter> = writers.iter().map(AsRef::as_ref).collect();
        let report = run(&binary, &writers);

        assert_eq!(report.removed, ["codex"]);
        assert_deleted_on_the_next_pass(&binary, &report, &writers);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "");
    }

    #[test]
    fn a_users_existing_bak_is_kept_and_the_backup_gets_a_paneflow_name() {
        let (dir, binary) = bridge();
        let config = dir.path().join("config.toml");
        let before = format!(
            "model = \"gpt-5\"\n\n[mcp_servers.paneflow]\ncommand = \"{}\"\n",
            binary.display()
        );
        std::fs::write(&config, &before).unwrap();
        std::fs::write(bak(&config), "the user's own backup").unwrap();
        let writer = Codex::at(config.clone());

        assert_eq!(
            writer.uninstall().unwrap(),
            UninstallOutcome::Removed {
                file: config.clone(),
                backup: dir.path().join("config.toml.paneflow-bak"),
            }
        );
        assert_eq!(
            std::fs::read_to_string(bak(&config)).unwrap(),
            "the user's own backup"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.paneflow-bak")).unwrap(),
            before
        );
    }

    #[test]
    fn a_codex_inline_mcp_servers_table_is_not_written_and_the_binary_stays() {
        let (dir, binary) = bridge();
        let config = dir.path().join("config.toml");
        let before = format!(
            "mcp_servers = {{ paneflow = {{ command = \"{}\", args = [] }} }}\n",
            binary.display()
        );
        std::fs::write(&config, &before).unwrap();

        let report = run(&binary, &[&Codex::at(config.clone())]);

        assert_eq!(report.kept.len(), 1);
        assert!(
            report.kept[0].1.contains("is not a TOML table"),
            "{:?}",
            report.kept
        );
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        assert!(!report.binary_deleted && binary.exists());
    }

    #[test]
    fn a_config_rewritten_between_parse_and_rename_is_left_alone_and_the_binary_stays() {
        // Claude Code rewrites `~/.claude.json` without PaneFlow's lock.
        let (dir, binary) = bridge();
        let config = dir.path().join(".claude.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "mcpServers": { "paneflow": { "type": "stdio", "command": binary, "args": [] } }
            }))
            .unwrap(),
        )
        .unwrap();
        let claude_wrote =
            r#"{"numStartups": 43, "mcpServers": {"paneflow": {"command": "x/paneflow-mcp"}}}"#;
        let racer = config.clone();
        crate::io::set_before_rename_hook(move || std::fs::write(&racer, claude_wrote).unwrap());

        let report = run(&binary, &[&ClaudeCode::at(config.clone())]);

        assert_eq!(report.kept.len(), 1);
        assert!(report.kept[0].1.contains("changed"), "{:?}", report.kept);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), claude_wrote);
        assert!(
            !bak(&config).exists(),
            "the refused write's backup is removed"
        );
        assert!(!report.binary_deleted && binary.exists());
    }

    #[test]
    fn refused_edits_leave_real_configs_byte_for_byte() {
        let (dir, binary) = bridge();
        let config = dir.path().join("config.toml");
        let before = format!(
            "[mcp_servers.paneflow]\ncommand = \"{}\"\nargs = []\n",
            binary.display()
        );
        std::fs::write(&config, &before).unwrap();

        let report = remove_legacy_bridge_with(&binary, &[&Codex::at(config.clone())], Some(GATE));

        assert_eq!(report.kept, [("codex", GATE.to_string())]);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        assert!(!bak(&config).exists());
        assert!(!report.binary_deleted && binary.exists());
    }

    // -- orchestration, Mock writers --------------------------------------

    #[test]
    fn the_binary_is_deleted_only_when_no_entry_still_points_at_it() {
        let (_dir, binary) = bridge();
        let removed = Mock::with_entry("a", "/data/paneflow-mcp");
        let failed = Mock::with_entry("b", "/data/paneflow-mcp")
            .with_uninstall(Err(anyhow::anyhow!("read-only file system")));
        let absent = Mock::without_entry("c");

        let report = run(&binary, &[&removed, &failed, &absent]);

        assert_eq!(report.removed, ["a"]);
        assert_eq!(report.kept, [("b", "read-only file system".to_string())]);
        assert!(!report.binary_deleted && binary.exists());

        // Next launch: the failing agent now succeeds. That pass removed
        // something, so it keeps the binary too.
        let removed_now = Mock::with_entry("b", "/data/paneflow-mcp");
        let report = run(&binary, &[&Mock::without_entry("a"), &removed_now, &absent]);
        assert_eq!(report.removed, ["b"]);
        assert!(!report.binary_deleted && binary.exists());

        // The launch after finds no entry anywhere and deletes it.
        let clean = [Mock::without_entry("a"), Mock::without_entry("b")];
        let report = run(&binary, &[&clean[0], &clean[1]]);
        assert!(!report.found_entry);
        assert!(report.binary_deleted && !binary.exists());
    }

    #[test]
    fn a_status_error_keeps_the_binary_and_skips_the_uninstall() {
        let (_dir, binary) = bridge();
        let broken = Mock::new("a", Err(anyhow::anyhow!("not valid JSON")));

        let report = run(&binary, &[&broken]);

        assert_eq!(report.kept, [("a", "not valid JSON".to_string())]);
        assert_eq!(broken.uninstall_calls.get(), 0);
        assert!(binary.exists());
    }

    #[test]
    fn entries_that_are_not_the_bridge_are_never_uninstalled() {
        let (_dir, binary) = bridge();
        let foreign = Mock::with_entry("a", "/usr/local/bin/other-server");
        let no_command = Mock::new(
            "b",
            Ok(StatusOutcome::NeedsRepair {
                path: None,
                reason: "missing command".into(),
            }),
        );

        let report = run(&binary, &[&foreign, &no_command]);

        assert_eq!(foreign.uninstall_calls.get(), 0);
        assert_eq!(no_command.uninstall_calls.get(), 0);
        assert!(report.removed.is_empty() && report.kept.is_empty());
        assert!(report.binary_deleted);
    }

    #[test]
    fn stale_and_repair_entries_are_uninstalled() {
        let (_dir, binary) = bridge();
        let stale = Mock::new(
            "a",
            Ok(StatusOutcome::StalePath {
                found: "/old/paneflow-mcp".into(),
                expected: binary.display().to_string(),
            }),
        );
        let repair = Mock::new(
            "b",
            Ok(StatusOutcome::NeedsRepair {
                path: Some("/data/paneflow-mcp".into()),
                reason: "disabled".into(),
            }),
        );

        let report = run(&binary, &[&stale, &repair]);

        assert_eq!(report.removed, ["a", "b"]);
        assert!(report.found_entry && !report.binary_deleted);
    }

    #[test]
    fn an_entry_that_turned_foreign_under_the_lock_waits_for_the_next_pass() {
        let (_dir, binary) = bridge();
        let raced = Mock::with_entry("a", "/data/paneflow-mcp").with_uninstall(Ok(
            UninstallOutcome::KeptUserEntry {
                command: Some("/usr/local/bin/other".into()),
            },
        ));

        let report = run(&binary, &[&raced]);

        // Nothing of ours is left, but the file moved under the pass: the
        // next launch confirms before the binary goes.
        assert!(report.removed.is_empty() && report.kept.is_empty());
        assert!(report.found_entry && !report.binary_deleted && binary.exists());
    }

    #[test]
    fn removal_then_deletion_on_the_next_pass_then_nothing_at_all() {
        let (_dir, binary) = bridge();
        let agent = Mock::with_entry("a", "/data/paneflow-mcp");

        // Pass 1 removes the entry and keeps the binary.
        let first = run(&binary, &[&agent]);
        assert_eq!(first.removed, ["a"]);
        assert!(first.found_entry && !first.binary_deleted && binary.exists());

        // Pass 2 finds no entry and deletes the binary.
        let second = run(&binary, &[&agent]);
        assert!(second.removed.is_empty() && second.kept.is_empty() && !second.found_entry);
        assert!(second.binary_deleted && !binary.exists());
        assert_eq!(agent.status_calls.get(), 2);

        // Pass 3 finds no binary and reads no config at all.
        let third = run(&binary, &[&agent]);
        assert_eq!(third, CleanupReport::default());
        assert_eq!(agent.status_calls.get(), 2, "no config may be read");
        assert_eq!(agent.uninstall_calls.get(), 1);
    }

    #[test]
    fn a_refused_run_edits_no_config_and_keeps_the_binary_while_an_entry_remains() {
        let (_dir, binary) = bridge();
        let ours = Mock::with_entry("a", "/data/paneflow-mcp");
        let absent = Mock::without_entry("b");

        let report = remove_legacy_bridge_with(&binary, &[&ours, &absent], Some(GATE));

        assert_eq!(ours.uninstall_calls.get(), 0);
        assert_eq!(absent.uninstall_calls.get(), 0);
        assert_eq!(report.kept, [("a", GATE.to_string())]);
        assert!(!report.binary_deleted && binary.exists());
    }

    #[test]
    fn a_refused_run_with_no_entry_left_still_deletes_the_binary() {
        let (_dir, binary) = bridge();
        let absent = Mock::without_entry("a");

        let report = remove_legacy_bridge_with(&binary, &[&absent], Some(GATE));

        assert_eq!(absent.uninstall_calls.get(), 0);
        assert!(report.binary_deleted && !binary.exists());
    }
}
