//! The taxonomy every inventory row is filed under: what kind of artifact it
//! is, which harness reads it, and whether it applies to one project or to
//! every project the user opens.
//!
//! The types mirror Blume's Setup taxonomy (rules / skills / hooks / docs, plus
//! MCP) so a later surface can attach to the same rows without renaming them.

/// What kind of rulebook artifact a row describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArtifactType {
    /// An editor rule file (`.cursor/rules/*.mdc`, `.cursorrules`).
    Rule,
    /// A `SKILL.md` skill directory.
    Skill,
    /// One hook entry declared in a hooks config file.
    Hook,
    /// An instruction file (`AGENTS.md`, `CLAUDE.md`).
    Doc,
    /// One MCP server entry declared in a config file.
    Mcp,
}

impl ArtifactType {
    /// Every type, in the order the tab's filter offers them.
    pub const ALL: [ArtifactType; 5] = [
        ArtifactType::Rule,
        ArtifactType::Skill,
        ArtifactType::Hook,
        ArtifactType::Doc,
        ArtifactType::Mcp,
    ];

    /// The chip label a row carries.
    pub fn label(self) -> &'static str {
        match self {
            ArtifactType::Rule => "Rule",
            ArtifactType::Skill => "Skill",
            ArtifactType::Hook => "Hook",
            ArtifactType::Doc => "Doc",
            ArtifactType::Mcp => "MCP",
        }
    }

    /// The plural the filter control shows.
    pub fn plural(self) -> &'static str {
        match self {
            ArtifactType::Rule => "Rules",
            ArtifactType::Skill => "Skills",
            ArtifactType::Hook => "Hooks",
            ArtifactType::Doc => "Docs",
            ArtifactType::Mcp => "MCP",
        }
    }
}

/// Whether an artifact applies to the scanned project only or to every
/// project the user opens. Grouping is by EFFECT, not by file location: a
/// project's MCP servers declared inside the global `~/.claude.json` are
/// `Project` rows, and `~/.claude/CLAUDE.md` is `Global` wherever it sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Project => "Project",
            Scope::Global => "Global",
        }
    }
}

/// Which agent reads the artifact. Only harnesses whose file layout is
/// verified appear here; a launcher with no mapping contributes no rows and
/// is named as "not inspected" by the surface instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    /// Read by more than one agent (`AGENTS.md`, `skills/`).
    Portable,
    ClaudeCode,
    Codex,
    Cursor,
    Gemini,
    OpenCode,
}

impl Harness {
    pub fn label(self) -> &'static str {
        match self {
            Harness::Portable => "Portable",
            Harness::ClaudeCode => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Cursor => "Cursor",
            Harness::Gemini => "Gemini",
            Harness::OpenCode => "OpenCode",
        }
    }
}
