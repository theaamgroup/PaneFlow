//! The scan: walk the catalog against injected roots and produce the
//! inventory. Pure over its inputs - no `dirs::` call, no environment read -
//! so a fake home is injectable in tests; [`Roots::resolve`] is the one edge
//! that touches the real environment.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::catalog::{self, GlobalEntry, GlobalRoot, ProjectEntry, Shape};
use crate::classify::{ArtifactType, Harness, Scope};
use crate::title;

/// Rows kept in one inventory. Past this the scan reports `omitted` instead
/// of growing without bound.
pub const MAX_ROWS: usize = 200;

/// Skill (and rule) entries read from one directory. An unbounded `skills/`
/// tree cannot walk forever; the overflow counts toward `omitted`.
pub const MAX_SKILLS_PER_DIR: usize = 100;

/// Largest file a row may open in the dock editor. Pinned by a test in the
/// app crate to the editor's own `MAX_FILE_BYTES` so the two ceilings cannot
/// drift. A larger file is still listed, and a config file past the cap is
/// still parsed for its hook / MCP rows - the cap is about opening a 10 MB
/// buffer in an editor, not about reading a key out of it.
pub const MAX_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;

/// Hard ceiling on a config file the scan parses whole. Far above anything
/// real (`~/.claude.json` measures ~84 KB on a heavy-use machine); only a
/// guard against pulling a runaway file into memory.
const MAX_CONFIG_READ_BYTES: u64 = 64 * 1024 * 1024;

/// One line of the inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupRow {
    pub artifact_type: ArtifactType,
    pub harness: Harness,
    pub scope: Scope,
    /// The file to open: the artifact itself, or the config file declaring a
    /// hook / MCP entry. As the catalog names it, not canonicalized, so the
    /// editor tab shows the path the user knows.
    pub path: PathBuf,
    /// Repo-relative, or `~`-shortened outside the project.
    pub display_path: String,
    /// Frontmatter name / first heading / file stem for a file row; the hook
    /// event and matcher, or the server name, for a config-declared row.
    pub title: Option<String>,
    /// `false` when the file exceeds [`MAX_ARTIFACT_BYTES`].
    pub openable: bool,
}

/// The scan result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Sorted: `Project` before `Global`, then by harness, path, title.
    pub rows: Vec<SetupRow>,
    /// Rows the caps dropped. Skills go first, never instruction files, hooks
    /// or MCP entries, so a large skills tree cannot evict the global hook rows.
    pub omitted: usize,
}

impl Inventory {
    /// How many rows fall under `scope`.
    pub fn count(&self, scope: Scope) -> usize {
        self.rows.iter().filter(|row| row.scope == scope).count()
    }
}

/// Where the scan looks. Every field is a parameter: the scan itself never
/// asks the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    /// The workspace root: the Tab's canonical cwd.
    pub project: PathBuf,
    /// The user's home, for `~`-shortening and the home-relative files.
    pub home: Option<PathBuf>,
    /// `$CLAUDE_CONFIG_DIR`, default `~/.claude`.
    pub claude_dir: Option<PathBuf>,
    /// `~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json` when set.
    pub claude_json: Option<PathBuf>,
    /// `$CODEX_HOME`, default `~/.codex`.
    pub codex_dir: Option<PathBuf>,
    /// `~/.gemini/settings.json`.
    pub gemini_settings: Option<PathBuf>,
    /// opencode global config candidates, first existing wins.
    pub opencode_candidates: Vec<PathBuf>,
}

impl Roots {
    /// The real environment: `dirs` home plus the agents' override variables.
    pub fn resolve(project: PathBuf) -> Self {
        Self::from_env(project, paneflow_agent_config::home_dir(), |name| {
            std::env::var_os(name)
        })
    }

    /// A fake home with no override variables set: everything hangs off
    /// `home`. What tests inject.
    pub fn from_home(project: PathBuf, home: Option<PathBuf>) -> Self {
        Self::from_env(project, home, |_| None)
    }

    /// Pure core: the precedence rules over an injected environment.
    pub fn from_env(
        project: PathBuf,
        home: Option<PathBuf>,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        let claude_dir =
            paneflow_agent_config::claude_config_dir_from(home.clone(), env("CLAUDE_CONFIG_DIR"));
        // `$CLAUDE_CONFIG_DIR/.claude.json` when set, else `$HOME/.claude.json`
        // (NOT `~/.claude/.claude.json` - that is the settings dir).
        let claude_json = match env("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => Some(PathBuf::from(dir).join(".claude.json")),
            None => home.as_ref().map(|home| home.join(".claude.json")),
        };
        let codex_dir =
            paneflow_agent_config::codex_config_dir_from(home.clone(), env("CODEX_HOME"));
        let gemini_settings = home
            .as_ref()
            .map(|home| home.join(".gemini").join("settings.json"));
        let opencode_candidates = opencode_candidates(
            home.as_deref(),
            env("XDG_CONFIG_HOME"),
            env("OPENCODE_CONFIG"),
            env("OPENCODE_CONFIG_DIR"),
        );
        Self {
            project,
            home,
            claude_dir,
            claude_json,
            codex_dir,
            gemini_settings,
            opencode_candidates,
        }
    }
}

/// Mirrors the MCP installer's opencode resolution: an explicit config file,
/// else an explicit config directory, else `$XDG_CONFIG_HOME` / `~/.config`,
/// with `opencode.jsonc` tried before `opencode.json`.
fn opencode_candidates(
    home: Option<&Path>,
    xdg_config_home: Option<OsString>,
    opencode_config: Option<OsString>,
    opencode_config_dir: Option<OsString>,
) -> Vec<PathBuf> {
    if let Some(config) = opencode_config.filter(|value| !value.is_empty()) {
        return vec![PathBuf::from(config)];
    }
    let dir = match opencode_config_dir.filter(|value| !value.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => xdg_config_home
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".config")))
            .map(|base| base.join("opencode")),
    };
    dir.map(|dir| vec![dir.join("opencode.jsonc"), dir.join("opencode.json")])
        .unwrap_or_default()
}

/// Walk the catalog under `roots`.
pub fn scan(roots: &Roots) -> Inventory {
    let mut scanner = Scanner {
        roots,
        rows: Vec::new(),
        seen: HashSet::new(),
        omitted: 0,
    };
    for entry in catalog::PROJECT {
        scanner.project_entry(entry);
    }
    scanner.claude_project_mcp();
    for entry in catalog::GLOBAL {
        scanner.global_entry(entry);
    }
    scanner.finish()
}

struct Scanner<'a> {
    roots: &'a Roots,
    rows: Vec<SetupRow>,
    /// Canonical paths of the file rows already listed: a symlinked
    /// `AGENTS.md -> CLAUDE.md` or a symlinked skill directory lists once.
    seen: HashSet<PathBuf>,
    omitted: usize,
}

impl Scanner<'_> {
    fn project_entry(&mut self, entry: &ProjectEntry) {
        let path = self.roots.project.join(entry.path);
        self.shape(Scope::Project, entry.harness, entry.kind, entry.shape, path);
    }

    fn global_entry(&mut self, entry: &GlobalEntry) {
        let root = match entry.root {
            GlobalRoot::ClaudeDir => self.roots.claude_dir.clone(),
            GlobalRoot::ClaudeJson => self.roots.claude_json.clone(),
            GlobalRoot::CodexDir => self.roots.codex_dir.clone(),
            GlobalRoot::GeminiSettings => self.roots.gemini_settings.clone(),
            GlobalRoot::OpencodeConfig => self
                .roots
                .opencode_candidates
                .iter()
                .find(|candidate| candidate.is_file())
                .cloned(),
        };
        let Some(root) = root else {
            return;
        };
        let path = if entry.path.is_empty() {
            root
        } else {
            root.join(entry.path)
        };
        self.shape(Scope::Global, entry.harness, entry.kind, entry.shape, path);
    }

    fn shape(
        &mut self,
        scope: Scope,
        harness: Harness,
        kind: ArtifactType,
        shape: Shape,
        path: PathBuf,
    ) {
        match shape {
            Shape::File => self.file_row(scope, harness, kind, path),
            Shape::Skills => self.skills(scope, harness, &path),
            Shape::RuleDir => self.rule_dir(scope, harness, &path),
            Shape::HooksJson => self.hooks_json(scope, harness, path),
            Shape::McpJson(key) => self.mcp_json(scope, harness, path, key),
            Shape::McpToml => self.mcp_toml(scope, harness, path),
        }
    }

    /// One row for the file at `path`, if it is a file not yet listed.
    fn file_row(&mut self, scope: Scope, harness: Harness, kind: ArtifactType, path: PathBuf) {
        // `metadata` follows symlinks: a link to a file is that file.
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        if !meta.is_file() {
            return;
        }
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !self.seen.insert(canonical) {
            return;
        }
        let openable = fits_editor(meta.len());
        // Past the cap the file is not read at all, not even its head.
        let title = if openable {
            title::read_title(&path)
        } else {
            title::file_stem(&path)
        };
        let display_path = self.display_path(&path);
        self.rows.push(SetupRow {
            artifact_type: kind,
            harness,
            scope,
            path,
            display_path,
            title: Some(title),
            openable,
        });
    }

    /// `<dir>/*/SKILL.md`, capped per directory.
    fn skills(&mut self, scope: Scope, harness: Harness, dir: &Path) {
        let candidates = sorted_children(dir)
            .into_iter()
            .map(|child| child.join("SKILL.md"))
            .filter(|skill| skill.is_file())
            .collect::<Vec<_>>();
        self.capped(candidates, |this, path| {
            this.file_row(scope, harness, ArtifactType::Skill, path)
        });
    }

    /// `<dir>/*.mdc`, capped like a skills directory.
    fn rule_dir(&mut self, scope: Scope, harness: Harness, dir: &Path) {
        let candidates = sorted_children(dir)
            .into_iter()
            .filter(|child| child.extension().is_some_and(|ext| ext == "mdc") && child.is_file())
            .collect::<Vec<_>>();
        self.capped(candidates, |this, path| {
            this.file_row(scope, harness, ArtifactType::Rule, path)
        });
    }

    fn capped(&mut self, candidates: Vec<PathBuf>, mut emit: impl FnMut(&mut Self, PathBuf)) {
        let overflow = candidates.len().saturating_sub(MAX_SKILLS_PER_DIR);
        self.omitted += overflow;
        for path in candidates.into_iter().take(MAX_SKILLS_PER_DIR) {
            emit(self, path);
        }
    }

    /// One row per hook entry under the file's top-level `hooks` object:
    /// event + matcher only, never the command string.
    fn hooks_json(&mut self, scope: Scope, harness: Harness, path: PathBuf) {
        let Some((root, openable)) = read_json(&path) else {
            return;
        };
        let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
            return;
        };
        let mut events = hooks.keys().collect::<Vec<_>>();
        events.sort();
        for event in events {
            let Some(groups) = hooks.get(event).and_then(Value::as_array) else {
                continue;
            };
            for group in groups {
                let matcher = group
                    .get("matcher")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|matcher| !matcher.is_empty());
                let count = match group.get("hooks").and_then(Value::as_array) {
                    Some(entries) => entries.len(),
                    // A group with no `hooks` array is still one declared entry.
                    None => 1,
                };
                for index in 0..count {
                    let mut label = event.to_string();
                    if let Some(matcher) = matcher {
                        label.push_str(" · ");
                        label.push_str(matcher);
                    }
                    if count > 1 {
                        label.push_str(&format!(" #{}", index + 1));
                    }
                    self.config_row(scope, harness, ArtifactType::Hook, &path, label, openable);
                }
            }
        }
    }

    /// One row per server name under `key`: the name only, never `command`,
    /// `args` or `env` (MCP env blocks routinely hold API tokens).
    fn mcp_json(&mut self, scope: Scope, harness: Harness, path: PathBuf, key: &str) {
        let Some((root, openable)) = read_json(&path) else {
            return;
        };
        self.mcp_rows_from(scope, harness, &path, root.get(key), openable);
    }

    fn mcp_rows_from(
        &mut self,
        scope: Scope,
        harness: Harness,
        path: &Path,
        servers: Option<&Value>,
        openable: bool,
    ) {
        let Some(servers) = servers.and_then(Value::as_object) else {
            return;
        };
        let mut names = servers.keys().collect::<Vec<_>>();
        names.sort();
        for name in names {
            self.config_row(
                scope,
                harness,
                ArtifactType::Mcp,
                path,
                name.clone(),
                openable,
            );
        }
    }

    /// `[mcp_servers.<name>]` tables of a Codex `config.toml`.
    fn mcp_toml(&mut self, scope: Scope, harness: Harness, path: PathBuf) {
        let Some((text, openable)) = read_config(&path) else {
            return;
        };
        let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
            return;
        };
        let Some(servers) = doc.get("mcp_servers").and_then(|item| item.as_table_like()) else {
            return;
        };
        let mut names = servers
            .iter()
            .map(|(name, _)| name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            self.config_row(scope, harness, ArtifactType::Mcp, &path, name, openable);
        }
    }

    /// Claude Code keeps a project's own MCP servers inside the *global*
    /// `~/.claude.json`, under `projects.<path>.mcpServers`. Those apply to
    /// this project only, so they are `Project` rows whose declaring file
    /// stays visible through `display_path`.
    fn claude_project_mcp(&mut self) {
        let Some(path) = self.roots.claude_json.clone() else {
            return;
        };
        let Some((root, openable)) = read_json(&path) else {
            return;
        };
        let Some(projects) = root.get("projects").and_then(Value::as_object) else {
            return;
        };
        let project = &self.roots.project;
        let canonical = std::fs::canonicalize(project).ok();
        let entry = projects.iter().find(|(key, _)| {
            let key = Path::new(key);
            key == project
                || canonical.as_ref().is_some_and(|canonical| {
                    std::fs::canonicalize(key).ok().as_ref() == Some(canonical)
                })
        });
        let Some((_, entry)) = entry else {
            return;
        };
        self.mcp_rows_from(
            Scope::Project,
            Harness::ClaudeCode,
            &path,
            entry.get("mcpServers"),
            openable,
        );
    }

    fn config_row(
        &mut self,
        scope: Scope,
        harness: Harness,
        kind: ArtifactType,
        path: &Path,
        title: String,
        openable: bool,
    ) {
        self.rows.push(SetupRow {
            artifact_type: kind,
            harness,
            scope,
            path: path.to_path_buf(),
            display_path: self.display_path(path),
            title: Some(title),
            openable,
        });
    }

    fn display_path(&self, path: &Path) -> String {
        display_path(path, &self.roots.project, self.roots.home.as_deref())
    }

    fn finish(self) -> Inventory {
        let Scanner {
            mut rows,
            mut omitted,
            ..
        } = self;
        omitted += truncate(&mut rows, MAX_ROWS);
        rows.sort_by(|a, b| {
            (a.scope, a.harness, &a.path, &a.title).cmp(&(b.scope, b.harness, &b.path, &b.title))
        });
        Inventory { rows, omitted }
    }
}

/// Enforce the row cap, dropping skills first (from the end, so the project's
/// own skills outlive the global ones), then whatever remains past the cap.
/// Returns how many rows went.
fn truncate(rows: &mut Vec<SetupRow>, cap: usize) -> usize {
    let before = rows.len();
    if before <= cap {
        return 0;
    }
    let mut excess = before - cap;
    let mut index = rows.len();
    while excess > 0 && index > 0 {
        index -= 1;
        if rows[index].artifact_type == ArtifactType::Skill {
            rows.remove(index);
            excess -= 1;
        }
    }
    rows.truncate(cap);
    before - rows.len()
}

/// Repo-relative inside the project, `~`-shortened under home, else absolute.
fn display_path(path: &Path, project: &Path, home: Option<&Path>) -> String {
    if let Ok(relative) = path.strip_prefix(project) {
        let relative = relative.to_string_lossy();
        if !relative.is_empty() {
            return relative.into_owned();
        }
    }
    if let Some(relative) = home.and_then(|home| path.strip_prefix(home).ok()) {
        return format!("~/{}", relative.to_string_lossy());
    }
    path.to_string_lossy().into_owned()
}

fn fits_editor(len: u64) -> bool {
    usize::try_from(len).is_ok_and(|len| len <= MAX_ARTIFACT_BYTES)
}

/// The directory's children, sorted by name so row order never depends on
/// `read_dir` order. Missing or unreadable: empty.
fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut children = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    children.sort();
    children
}

/// Read a config file whole, with whether a row over it may open in the
/// editor. `None` when it is not a file or is too large to parse at all.
fn read_config(path: &Path) -> Option<(String, bool)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_READ_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some((text, fits_editor(meta.len())))
}

/// [`read_config`] parsed as JSONC (comments and trailing commas accepted).
fn read_json(path: &Path) -> Option<(Value, bool)> {
    let (text, openable) = read_config(path)?;
    let value = paneflow_agent_config::jsonc::parse(&text).ok()?;
    Some((value, openable))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, body: &str) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    /// A project and a fake home side by side in one temp dir.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        (dir, project, home)
    }

    fn scan_fixture(project: &Path, home: &Path) -> Inventory {
        scan(&Roots::from_home(
            project.to_path_buf(),
            Some(home.to_path_buf()),
        ))
    }

    fn kinds(inventory: &Inventory) -> Vec<(ArtifactType, String)> {
        inventory
            .rows
            .iter()
            .map(|row| (row.artifact_type, row.display_path.clone()))
            .collect()
    }

    #[test]
    fn the_classifier_lists_agent_artifacts_and_ignores_other_hidden_files() {
        let (_dir, project, home) = fixture();
        write(&project, "AGENTS.md", "# Repository Guidelines\n");
        write(
            &project,
            ".claude/skills/foo/SKILL.md",
            "---\nname: foo-skill\n---\n",
        );
        write(
            &project,
            ".codex/hooks.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"secret-cmd --token abc"}]}]}}"#,
        );
        write(
            &project,
            ".cursor/rules/a.mdc",
            "---\ndescription: API rules\n---\n",
        );
        write(&project, ".env", "SECRET=1\n");

        let inventory = scan_fixture(&project, &home);
        assert_eq!(
            kinds(&inventory),
            vec![
                (ArtifactType::Doc, "AGENTS.md".to_string()),
                (
                    ArtifactType::Skill,
                    ".claude/skills/foo/SKILL.md".to_string()
                ),
                (ArtifactType::Hook, ".codex/hooks.json".to_string()),
                (ArtifactType::Rule, ".cursor/rules/a.mdc".to_string()),
            ]
        );
        assert!(inventory
            .rows
            .iter()
            .all(|row| !row.display_path.contains(".env")));
        assert_eq!(inventory.omitted, 0);

        let titles = inventory
            .rows
            .iter()
            .map(|row| row.title.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            ["Repository Guidelines", "foo-skill", "Stop", "API rules"]
        );
        assert!(
            !titles.iter().any(|title| title.contains("secret-cmd")),
            "hook rows never carry the command string"
        );
    }

    #[test]
    fn scope_follows_effect_project_files_then_global_files() {
        let (_dir, project, home) = fixture();
        write(&project, "CLAUDE.md", "# Project\n");
        write(&home, ".claude/CLAUDE.md", "# Everywhere\n");
        write(&home, ".codex/AGENTS.md", "# Codex everywhere\n");

        let inventory = scan_fixture(&project, &home);
        let scopes = inventory
            .rows
            .iter()
            .map(|row| (row.scope, row.display_path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            scopes,
            vec![
                (Scope::Project, "CLAUDE.md".to_string()),
                (Scope::Global, "~/.claude/CLAUDE.md".to_string()),
                (Scope::Global, "~/.codex/AGENTS.md".to_string()),
            ]
        );
        assert_eq!(inventory.count(Scope::Project), 1);
        assert_eq!(inventory.count(Scope::Global), 2);
    }

    #[test]
    fn three_hundred_skills_hit_the_caps_without_panicking() {
        let (_dir, project, home) = fixture();
        for index in 0..300 {
            write(
                &project,
                &format!(".claude/skills/skill-{index:03}/SKILL.md"),
                "# s\n",
            );
        }
        write(
            &home,
            ".claude/settings.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        );

        let inventory = scan_fixture(&project, &home);
        assert!(inventory.rows.len() <= MAX_ROWS);
        assert_eq!(
            inventory
                .rows
                .iter()
                .filter(|row| row.artifact_type == ArtifactType::Skill)
                .count(),
            MAX_SKILLS_PER_DIR
        );
        assert_eq!(inventory.omitted, 300 - MAX_SKILLS_PER_DIR);
        // The global hook row survives a skills flood.
        assert!(inventory
            .rows
            .iter()
            .any(|row| row.artifact_type == ArtifactType::Hook));
    }

    #[test]
    fn the_row_cap_drops_skills_before_anything_else() {
        let mut rows = Vec::new();
        let row = |kind: ArtifactType, index: usize| SetupRow {
            artifact_type: kind,
            harness: Harness::Portable,
            scope: Scope::Project,
            path: PathBuf::from(format!("/p/{index}")),
            display_path: index.to_string(),
            title: None,
            openable: true,
        };
        for index in 0..5 {
            rows.push(row(ArtifactType::Hook, index));
        }
        for index in 5..12 {
            rows.push(row(ArtifactType::Skill, index));
        }
        rows.push(row(ArtifactType::Mcp, 12));

        assert_eq!(truncate(&mut rows, 6), 7);
        assert_eq!(rows.len(), 6);
        assert!(rows
            .iter()
            .all(|row| row.artifact_type != ArtifactType::Skill));
        assert_eq!(
            rows.last().map(|row| row.artifact_type),
            Some(ArtifactType::Mcp)
        );

        // Skills alone cannot make room: the tail goes next.
        let mut rows = (0..4)
            .map(|index| row(ArtifactType::Doc, index))
            .collect::<Vec<_>>();
        assert_eq!(truncate(&mut rows, 2), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(truncate(&mut rows, 2), 0);
    }

    #[test]
    fn mcp_rows_are_one_per_server_and_never_show_command_args_or_env() {
        let (_dir, project, home) = fixture();
        write(
            &home,
            ".claude.json",
            r#"{
              "mcpServers": {
                "zeta": {"command": "/usr/local/bin/zeta-server", "args": ["--port", "9911"], "env": {"ZETA_TOKEN": "sk-verysecret"}},
                "alpha": {"command": "alpha-cmd", "args": ["--flag-alpha"], "env": {"ALPHA_KEY": "hunter2"}},
                "mid": {"command": "mid-binary"}
              }
            }"#,
        );

        let inventory = scan_fixture(&project, &home);
        let mcp = inventory
            .rows
            .iter()
            .filter(|row| row.artifact_type == ArtifactType::Mcp)
            .collect::<Vec<_>>();
        assert_eq!(mcp.len(), 3);
        let titles = mcp
            .iter()
            .map(|row| row.title.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["alpha", "mid", "zeta"]);
        for row in &mcp {
            assert_eq!(row.scope, Scope::Global);
            assert_eq!(row.harness, Harness::ClaudeCode);
            assert_eq!(row.display_path, "~/.claude.json");
            let rendered = format!(
                "{} {}",
                row.display_path,
                row.title.clone().unwrap_or_default()
            );
            for secret in [
                "zeta-server",
                "9911",
                "sk-verysecret",
                "alpha-cmd",
                "--flag-alpha",
                "hunter2",
                "mid-binary",
            ] {
                assert!(!rendered.contains(secret), "{rendered} leaks {secret}");
            }
        }
    }

    #[test]
    fn a_projects_own_claude_mcp_servers_are_project_rows_from_the_global_file() {
        let (_dir, project, home) = fixture();
        let claude_json = format!(
            r#"{{"mcpServers": {{"global-one": {{"command": "g"}}}},
                "projects": {{
                  "{}": {{"mcpServers": {{"project-one": {{"command": "p"}}}}}},
                  "/somewhere/else": {{"mcpServers": {{"other": {{"command": "o"}}}}}}
                }}}}"#,
            project.display()
        );
        write(&home, ".claude.json", &claude_json);

        let inventory = scan_fixture(&project, &home);
        let rows = inventory
            .rows
            .iter()
            .map(|row| {
                (
                    row.scope,
                    row.title.clone().unwrap_or_default(),
                    row.display_path.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                (
                    Scope::Project,
                    "project-one".to_string(),
                    "~/.claude.json".to_string()
                ),
                (
                    Scope::Global,
                    "global-one".to_string(),
                    "~/.claude.json".to_string()
                ),
            ]
        );
    }

    #[test]
    fn codex_toml_servers_and_gemini_and_opencode_configs_yield_mcp_rows() {
        let (_dir, project, home) = fixture();
        write(
            &home,
            ".codex/config.toml",
            "model = \"x\"\n[mcp_servers.paneflow]\ncommand = \"paneflow-mcp\"\n[mcp_servers.other]\ncommand = \"o\"\n",
        );
        write(
            &home,
            ".gemini/settings.json",
            "{\n  // comment\n  \"mcpServers\": {\"gem\": {\"command\": \"g\"}},\n}\n",
        );
        write(
            &home,
            ".config/opencode/opencode.json",
            r#"{"mcp": {"oc": {"type": "local", "command": ["oc"]}}}"#,
        );

        let inventory = scan_fixture(&project, &home);
        let rows = inventory
            .rows
            .iter()
            .map(|row| (row.harness, row.title.clone().unwrap_or_default()))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                (Harness::Codex, "other".to_string()),
                (Harness::Codex, "paneflow".to_string()),
                (Harness::Gemini, "gem".to_string()),
                (Harness::OpenCode, "oc".to_string()),
            ]
        );
    }

    #[test]
    fn hook_rows_carry_event_and_matcher_and_number_siblings() {
        let (_dir, project, home) = fixture();
        write(
            &project,
            ".claude/settings.json",
            r#"{"hooks":{
              "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"a"},{"type":"command","command":"b"}]}],
              "Stop":[{"hooks":[{"type":"command","command":"c"}]}]
            }}"#,
        );
        let inventory = scan_fixture(&project, &home);
        let titles = inventory
            .rows
            .iter()
            .map(|row| row.title.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            ["PreToolUse · Bash #1", "PreToolUse · Bash #2", "Stop"]
        );
        assert!(inventory
            .rows
            .iter()
            .all(|row| row.path == project.join(".claude/settings.json")));
    }

    #[test]
    fn an_oversized_file_is_listed_but_not_openable_and_not_read() {
        let (_dir, project, home) = fixture();
        let path = write(&project, "CLAUDE.md", "# Huge heading\n");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        // Sparse: the length is what the cap reads, no bytes are written.
        file.set_len(MAX_ARTIFACT_BYTES as u64 + 1).unwrap();

        let inventory = scan_fixture(&project, &home);
        assert_eq!(inventory.rows.len(), 1);
        let row = &inventory.rows[0];
        assert!(!row.openable);
        // Had the head been read, the heading would have won over the stem.
        assert_eq!(row.title.as_deref(), Some("CLAUDE"));

        // A config file past the cap still yields its rows, just not openable.
        let settings = write(
            &project,
            ".claude/settings.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"c"}]}]}}      "#,
        );
        let padded = format!(
            "{}{}",
            std::fs::read_to_string(&settings).unwrap(),
            " ".repeat(MAX_ARTIFACT_BYTES + 1)
        );
        std::fs::write(&settings, padded).unwrap();
        let inventory = scan_fixture(&project, &home);
        let hook = inventory
            .rows
            .iter()
            .find(|row| row.artifact_type == ArtifactType::Hook)
            .expect("hook row");
        assert!(!hook.openable);
    }

    #[test]
    fn symlinked_instruction_files_and_skills_list_once() {
        let (_dir, project, home) = fixture();
        write(&project, "CLAUDE.md", "# One\n");
        std::os::unix::fs::symlink(project.join("CLAUDE.md"), project.join("AGENTS.md")).unwrap();
        write(&project, ".claude/skills/real/SKILL.md", "# Real\n");
        std::fs::create_dir_all(project.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(
            project.join(".claude/skills/real"),
            project.join(".agents/skills/linked"),
        )
        .unwrap();

        let inventory = scan_fixture(&project, &home);
        let docs = inventory
            .rows
            .iter()
            .filter(|row| row.artifact_type == ArtifactType::Doc)
            .count();
        let skills = inventory
            .rows
            .iter()
            .filter(|row| row.artifact_type == ArtifactType::Skill)
            .count();
        assert_eq!(docs, 1, "{:?}", kinds(&inventory));
        assert_eq!(skills, 1, "{:?}", kinds(&inventory));
        // The first catalog line wins: `AGENTS.md` is scanned before `CLAUDE.md`,
        // and `.agents/skills` before `.claude/skills`.
        let listed = inventory
            .rows
            .iter()
            .map(|row| row.display_path.as_str())
            .collect::<Vec<_>>();
        assert!(listed.contains(&"AGENTS.md"), "{listed:?}");
        assert!(
            listed.contains(&".agents/skills/linked/SKILL.md"),
            "{listed:?}"
        );
    }

    #[test]
    fn roots_honor_the_agents_override_variables() {
        let env = |name: &str| -> Option<OsString> {
            match name {
                "CLAUDE_CONFIG_DIR" => Some("/cfg/claude".into()),
                "CODEX_HOME" => Some("/cfg/codex".into()),
                "OPENCODE_CONFIG" => Some("/cfg/oc.json".into()),
                _ => None,
            }
        };
        let roots = Roots::from_env(PathBuf::from("/p"), Some(PathBuf::from("/home/u")), env);
        assert_eq!(roots.claude_dir, Some(PathBuf::from("/cfg/claude")));
        assert_eq!(
            roots.claude_json,
            Some(PathBuf::from("/cfg/claude/.claude.json"))
        );
        assert_eq!(roots.codex_dir, Some(PathBuf::from("/cfg/codex")));
        assert_eq!(
            roots.opencode_candidates,
            vec![PathBuf::from("/cfg/oc.json")]
        );

        let roots = Roots::from_home(PathBuf::from("/p"), Some(PathBuf::from("/home/u")));
        assert_eq!(roots.claude_dir, Some(PathBuf::from("/home/u/.claude")));
        assert_eq!(
            roots.claude_json,
            Some(PathBuf::from("/home/u/.claude.json"))
        );
        assert_eq!(roots.codex_dir, Some(PathBuf::from("/home/u/.codex")));
        assert_eq!(
            roots.gemini_settings,
            Some(PathBuf::from("/home/u/.gemini/settings.json"))
        );
        assert_eq!(
            roots.opencode_candidates,
            vec![
                PathBuf::from("/home/u/.config/opencode/opencode.jsonc"),
                PathBuf::from("/home/u/.config/opencode/opencode.json"),
            ]
        );
        let none = Roots::from_home(PathBuf::from("/p"), None);
        assert_eq!(none.claude_dir, None);
        assert!(none.opencode_candidates.is_empty());
        assert_eq!(scan(&none), Inventory::default());
    }

    #[test]
    fn display_paths_are_relative_then_tilde_then_absolute() {
        let project = Path::new("/work/repo");
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            display_path(Path::new("/work/repo/AGENTS.md"), project, home),
            "AGENTS.md"
        );
        assert_eq!(
            display_path(Path::new("/home/u/.claude.json"), project, home),
            "~/.claude.json"
        );
        assert_eq!(display_path(Path::new("/etc/x"), project, home), "/etc/x");
        assert_eq!(display_path(Path::new("/etc/x"), project, None), "/etc/x");
    }
}
