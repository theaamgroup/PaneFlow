//! The fixed path catalog the scan walks.
//!
//! This is a list, not a tree walk: the Files sidebar hides dotfiles and
//! gitignored entries on purpose, and this crate must not become the
//! un-filtered walk it refuses to be. Every path here is one a verified
//! harness actually reads. A harness whose layout nobody verified contributes
//! no entry - see `UNMAPPED` on the surface side - rather than a guess.

use crate::classify::{ArtifactType, Harness};

/// How a catalog path turns into rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// One file at the path: one row.
    File,
    /// `<path>/*/SKILL.md`: one row per skill directory, capped per directory.
    Skills,
    /// `<path>/*.mdc`: one row per rule file.
    RuleDir,
    /// JSON whose top-level `hooks` object holds `event -> [ { matcher, hooks } ]`
    /// (Claude Code settings, Codex `hooks.json`): one row per hook entry.
    HooksJson,
    /// JSON(C) whose object at `key` maps server name -> definition: one row
    /// per server, name only.
    McpJson(&'static str),
    /// TOML whose `[mcp_servers.<name>]` tables are the servers.
    McpToml,
}

/// Which root a global entry is relative to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalRoot {
    /// `$CLAUDE_CONFIG_DIR`, default `~/.claude`.
    ClaudeDir,
    /// The user-scope `.claude.json` (`~/.claude.json`, or inside
    /// `$CLAUDE_CONFIG_DIR` when that is set).
    ClaudeJson,
    /// `$CODEX_HOME`, default `~/.codex`.
    CodexDir,
    /// `~/.gemini/settings.json`.
    GeminiSettings,
    /// The opencode global config file (`~/.config/opencode/opencode.json[c]`).
    OpencodeConfig,
}

/// One project-scoped catalog line, relative to the workspace root.
#[derive(Clone, Copy, Debug)]
pub struct ProjectEntry {
    pub harness: Harness,
    pub kind: ArtifactType,
    pub shape: Shape,
    pub path: &'static str,
}

/// One global catalog line. `path` is relative to `root`; an empty `path`
/// means the root itself is the file.
#[derive(Clone, Copy, Debug)]
pub struct GlobalEntry {
    pub harness: Harness,
    pub kind: ArtifactType,
    pub shape: Shape,
    pub root: GlobalRoot,
    pub path: &'static str,
}

const fn project(
    harness: Harness,
    kind: ArtifactType,
    shape: Shape,
    path: &'static str,
) -> ProjectEntry {
    ProjectEntry {
        harness,
        kind,
        shape,
        path,
    }
}

const fn global(
    harness: Harness,
    kind: ArtifactType,
    shape: Shape,
    root: GlobalRoot,
    path: &'static str,
) -> GlobalEntry {
    GlobalEntry {
        harness,
        kind,
        shape,
        root,
        path,
    }
}

/// Project catalog, in scan order. `AGENTS.md` is read at the workspace root
/// only in this slice: nested per-package instruction files are a real
/// monorepo pattern, but a bounded recursive walk waits for a workspace that
/// has one.
pub const PROJECT: &[ProjectEntry] = &[
    project(
        Harness::Portable,
        ArtifactType::Doc,
        Shape::File,
        "AGENTS.md",
    ),
    project(
        Harness::Portable,
        ArtifactType::Doc,
        Shape::File,
        "CLAUDE.md",
    ),
    project(
        Harness::Portable,
        ArtifactType::Doc,
        Shape::File,
        ".claude/CLAUDE.md",
    ),
    project(
        Harness::Portable,
        ArtifactType::Skill,
        Shape::Skills,
        "skills",
    ),
    project(
        Harness::Portable,
        ArtifactType::Skill,
        Shape::Skills,
        ".agents/skills",
    ),
    project(
        Harness::ClaudeCode,
        ArtifactType::Skill,
        Shape::Skills,
        ".claude/skills",
    ),
    project(
        Harness::ClaudeCode,
        ArtifactType::Hook,
        Shape::HooksJson,
        ".claude/settings.json",
    ),
    project(
        Harness::ClaudeCode,
        ArtifactType::Hook,
        Shape::HooksJson,
        ".claude/settings.local.json",
    ),
    project(
        Harness::ClaudeCode,
        ArtifactType::Mcp,
        Shape::McpJson("mcpServers"),
        ".mcp.json",
    ),
    project(
        Harness::Codex,
        ArtifactType::Hook,
        Shape::HooksJson,
        ".codex/hooks.json",
    ),
    project(
        Harness::Cursor,
        ArtifactType::Rule,
        Shape::RuleDir,
        ".cursor/rules",
    ),
    project(
        Harness::Cursor,
        ArtifactType::Rule,
        Shape::File,
        ".cursorrules",
    ),
];

/// Global catalog, in scan order. Cursor has no verified user-rules location
/// on disk (neither `~/.cursor/rules/` nor a `globalStorage` entry exists on a
/// machine with Cursor installed), so it has no global line.
pub const GLOBAL: &[GlobalEntry] = &[
    global(
        Harness::ClaudeCode,
        ArtifactType::Doc,
        Shape::File,
        GlobalRoot::ClaudeDir,
        "CLAUDE.md",
    ),
    global(
        Harness::ClaudeCode,
        ArtifactType::Hook,
        Shape::HooksJson,
        GlobalRoot::ClaudeDir,
        "settings.json",
    ),
    global(
        Harness::ClaudeCode,
        ArtifactType::Skill,
        Shape::Skills,
        GlobalRoot::ClaudeDir,
        "skills",
    ),
    global(
        Harness::ClaudeCode,
        ArtifactType::Mcp,
        Shape::McpJson("mcpServers"),
        GlobalRoot::ClaudeJson,
        "",
    ),
    global(
        Harness::Codex,
        ArtifactType::Doc,
        Shape::File,
        GlobalRoot::CodexDir,
        "AGENTS.md",
    ),
    global(
        Harness::Codex,
        ArtifactType::Hook,
        Shape::HooksJson,
        GlobalRoot::CodexDir,
        "hooks.json",
    ),
    global(
        Harness::Codex,
        ArtifactType::Mcp,
        Shape::McpToml,
        GlobalRoot::CodexDir,
        "config.toml",
    ),
    global(
        Harness::Gemini,
        ArtifactType::Mcp,
        Shape::McpJson("mcpServers"),
        GlobalRoot::GeminiSettings,
        "",
    ),
    global(
        Harness::OpenCode,
        ArtifactType::Mcp,
        Shape::McpJson("mcp"),
        GlobalRoot::OpencodeConfig,
        "",
    ),
];
