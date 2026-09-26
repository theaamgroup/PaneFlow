//! Per-agent config writers for the bridge cleanup (issue #857).
//!
//! Each supported agent (Claude Code, Codex, Gemini CLI, opencode)
//! implements [`AgentConfigWriter`], one writer per config file.
//! [`crate::cleanup`] iterates [`writers_for`]: a read-only
//! [`AgentConfigWriter::status`] probe first, then
//! [`AgentConfigWriter::uninstall`] only for an entry whose command runs a
//! `paneflow-mcp` binary.
//!
//! Outcomes are structured, not pre-formatted strings, so the cleanup owns
//! logging and the keep-the-binary decision in one place.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod opencode;
mod support;

pub(crate) use support::names_bridge_binary;

#[cfg(test)]
pub(crate) mod testutil;

/// Result of an `uninstall` on one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// The `paneflow` entry was removed from `file`, and `backup` holds the
    /// bytes from before the write.
    Removed { file: PathBuf, backup: PathBuf },
    /// No `paneflow` entry was present - nothing to do.
    NothingToRemove,
    /// A `paneflow` entry exists but its command does not run a
    /// `paneflow-mcp` binary. It is the user's own and stays untouched.
    KeptUserEntry { command: Option<String> },
}

/// Result of a read-only `status` probe on one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusOutcome {
    /// A `paneflow` entry exists and points at the expected bridge path.
    Installed { path: String },
    /// A `paneflow` entry exists but points at a different path than the
    /// expected one - typically a path left by an older PaneFlow install
    /// that moved data dirs.
    StalePath { found: String, expected: String },
    /// A `paneflow` entry exists but does not match the shape PaneFlow wrote.
    NeedsRepair {
        path: Option<String>,
        reason: String,
    },
    /// The agent carries no `paneflow` entry.
    NotInstalled,
}

impl StatusOutcome {
    /// The command path the entry names, when it names one.
    pub(crate) fn command(&self) -> Option<&str> {
        match self {
            Self::Installed { path } => Some(path),
            Self::StalePath { found, .. } => Some(found),
            Self::NeedsRepair {
                path: Some(path), ..
            } => Some(path),
            Self::NeedsRepair { path: None, .. } | Self::NotInstalled => None,
        }
    }
}

/// One agent's config surface: how to inspect and remove the `paneflow`
/// MCP entry.
///
/// Implementations live in `agents/<name>.rs`. Removal is no-clobber - see
/// [`crate::merge`] and [`crate::io`] for the shared safe-write primitives.
pub trait AgentConfigWriter {
    /// Stable machine id, e.g. `"claude-code"`.
    fn id(&self) -> &'static str;

    /// Human-readable label, e.g. `"Claude Code"`. Used in log lines.
    fn label(&self) -> &'static str;

    /// Remove only the `paneflow` entry, and only when its command runs a
    /// `paneflow-mcp` binary, leaving every other entry intact. The check
    /// and the removal happen on one parse under the config lock.
    fn uninstall(&self) -> Result<UninstallOutcome>;

    /// Inspect the current `paneflow` entry without writing. `bridge_path`
    /// is the path the entry was written with, used to flag a stale path.
    fn status(&self, bridge_path: Option<&Path>) -> Result<StatusOutcome>;
}

/// The environment variables that move an agent's config file, as one
/// environment sets them.
///
/// A GUI launch does not inherit variables set only in the login shell's
/// profile, so the cleanup probes the files the process environment names
/// and, when the app captured them, the files the login shell names too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentConfigEnv {
    /// `CLAUDE_CONFIG_DIR`: holds `.claude.json`.
    pub claude_config_dir: Option<OsString>,
    /// `CODEX_HOME`: holds `config.toml`.
    pub codex_home: Option<OsString>,
    /// `OPENCODE_CONFIG`: the opencode config file itself.
    pub opencode_config: Option<OsString>,
    /// `OPENCODE_CONFIG_DIR`: holds `opencode.json[c]`.
    pub opencode_config_dir: Option<OsString>,
    /// `XDG_CONFIG_HOME`: holds `opencode/opencode.json[c]`.
    pub xdg_config_home: Option<OsString>,
}

impl AgentConfigEnv {
    /// Every variable this struct reads, in field order.
    pub const VARS: [&'static str; 5] = [
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
    ];

    /// Read the variables through `lookup`. An empty value counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Self {
        let get = |name: &str| lookup(name).filter(|value| !value.is_empty());
        Self {
            claude_config_dir: get(Self::VARS[0]),
            codex_home: get(Self::VARS[1]),
            opencode_config: get(Self::VARS[2]),
            opencode_config_dir: get(Self::VARS[3]),
            xdg_config_home: get(Self::VARS[4]),
        }
    }

    /// The variables as this process sees them.
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Whether no variable is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One writer per distinct config file the four agents read under any of
/// `envs` (in order, duplicates dropped), with `home` as the fallback root.
/// Every opencode candidate is its own writer, so `opencode.jsonc` and
/// `opencode.json` are both probed when both exist.
#[must_use]
pub fn writers_for(
    home: Option<&Path>,
    envs: &[AgentConfigEnv],
) -> Vec<Box<dyn AgentConfigWriter>> {
    let home = home.map(Path::to_path_buf);
    let mut seen: Vec<(&'static str, Option<PathBuf>)> = Vec::new();
    let mut writers: Vec<Box<dyn AgentConfigWriter>> = Vec::new();
    let mut push = |id: &'static str, path: Option<PathBuf>, writer: Box<dyn AgentConfigWriter>| {
        if !seen.contains(&(id, path.clone())) {
            seen.push((id, path));
            writers.push(writer);
        }
    };
    for env in envs {
        let claude = support::claude_config_from(home.clone(), env.claude_config_dir.clone());
        push(
            "claude-code",
            claude.clone(),
            Box::new(claude_code::ClaudeCode::new(claude)),
        );
        let codex = support::codex_config_from(home.clone(), env.codex_home.clone());
        push("codex", codex.clone(), Box::new(codex::Codex::new(codex)));
        let gemini = support::gemini_config_from(home.clone());
        push(
            "gemini",
            gemini.clone(),
            Box::new(gemini::Gemini::new(gemini)),
        );
        for path in support::opencode_configs_from(
            home.clone(),
            env.xdg_config_home.clone(),
            env.opencode_config.clone(),
            env.opencode_config_dir.clone(),
        ) {
            push(
                "opencode",
                Some(path.clone()),
                Box::new(opencode::OpenCode::new(path)),
            );
        }
    }
    writers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_env_reads_each_variable_and_treats_empty_as_unset() {
        let env = AgentConfigEnv::from_lookup(|name| match name {
            "CLAUDE_CONFIG_DIR" => Some("/c".into()),
            "CODEX_HOME" => Some("/x".into()),
            "OPENCODE_CONFIG" => Some(OsString::new()),
            "OPENCODE_CONFIG_DIR" => Some("/o".into()),
            "XDG_CONFIG_HOME" => Some("/cfg".into()),
            _ => None,
        });
        assert_eq!(
            env,
            AgentConfigEnv {
                claude_config_dir: Some("/c".into()),
                codex_home: Some("/x".into()),
                opencode_config: None,
                opencode_config_dir: Some("/o".into()),
                xdg_config_home: Some("/cfg".into()),
            }
        );
        assert!(AgentConfigEnv::from_lookup(|_| None).is_empty());
    }

    #[test]
    fn writers_for_probes_each_file_once_across_environments() {
        let home = Path::new("/home/alice");
        let default = AgentConfigEnv::default();
        let moved = AgentConfigEnv {
            codex_home: Some("/work/codex".into()),
            ..AgentConfigEnv::default()
        };
        let ids = |writers: &[Box<dyn AgentConfigWriter>]| -> Vec<&'static str> {
            writers.iter().map(|w| w.id()).collect()
        };

        let one = writers_for(Some(home), std::slice::from_ref(&default));
        assert_eq!(
            ids(&one),
            ["claude-code", "codex", "gemini", "opencode", "opencode"]
        );
        let same_twice = writers_for(Some(home), &[default.clone(), default.clone()]);
        assert_eq!(ids(&same_twice), ids(&one));
        let with_moved = writers_for(Some(home), &[default, moved]);
        assert_eq!(
            ids(&with_moved),
            [
                "claude-code",
                "codex",
                "gemini",
                "opencode",
                "opencode",
                "codex"
            ]
        );
    }
}
