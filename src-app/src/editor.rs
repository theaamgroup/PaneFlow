//! Cross-platform "open this source file at line:col" - invoked when the
//! user Cmd/Ctrl-clicks a `path:42:7` style reference in a terminal pane.
//!
//! An explicit `external_editor` takes precedence. `system` uses macOS's
//! registered file handler; `auto` (or an absent setting) tries `$VISUAL`,
//! `$EDITOR`, the fallback CLI probes, then the system handler. Commands are
//! parsed into binary and flags without invoking a shell; known editors get
//! their own line/column argument syntax.

use std::path::{Path, PathBuf};
use std::process::Command;

use paneflow_process::spawn_detached;

/// Family of recognised editor binaries, each with a distinct argv shape
/// for "open at line and column".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorKind {
    /// VS Code / Cursor / Codium clones - `code -g path:line:col`
    VsCodeLike,
    /// Zed - `zed path:line:col` (no flag needed; the colon syntax is
    /// recognised since 0.130).
    Zed,
    /// Sublime Text - `subl path:line:col`
    Sublime,
    /// Neovim / Vim - `nvim +line path` (col not natively supported as
    /// argv; we drop it). Could be extended with `+call cursor(L, C)`
    /// but that gets messy across remote/server modes.
    VimFamily,
    /// Helix - `hx path:line:col`
    Helix,
    /// Emacs - `emacs +line:col path` (line and optional col separated
    /// by `:`)
    Emacs,
    /// Unknown binary - invoke with bare `path` only (no location).
    Unknown,
}

impl EditorKind {
    fn from_binary_name(name: &str) -> Self {
        // Strip the directory portion and any `.exe` / `.cmd` suffix so the
        // matcher is OS-agnostic - `which code.cmd` on Windows still maps
        // to `VsCodeLike`.
        let base = Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
            .to_ascii_lowercase();
        match base.as_str() {
            "code" | "code-insiders" | "codium" | "cursor" | "windsurf" => Self::VsCodeLike,
            "zed" | "zed-preview" | "zed-nightly" => Self::Zed,
            "subl" | "sublime_text" => Self::Sublime,
            "nvim" | "vim" | "vi" | "nvim-qt" | "gvim" | "mvim" => Self::VimFamily,
            "hx" | "helix" => Self::Helix,
            "emacs" | "emacsclient" => Self::Emacs,
            _ => Self::Unknown,
        }
    }

    /// Build the argv tail that opens `path` at `line` / `col` for this
    /// editor family. Caller prepends the editor binary itself.
    fn argv_for(self, path: &Path, line: Option<u32>, col: Option<u32>) -> Vec<String> {
        let path_str = path.to_string_lossy().into_owned();
        match self {
            Self::VsCodeLike => {
                let mut args = vec!["-g".to_string()];
                args.push(format_path_line_col(&path_str, line, col));
                args
            }
            Self::Zed | Self::Sublime | Self::Helix => {
                // Bare positional, colon syntax recognised by the editor.
                vec![format_path_line_col(&path_str, line, col)]
            }
            Self::VimFamily => {
                let mut args = Vec::new();
                if let Some(l) = line {
                    args.push(format!("+{l}"));
                }
                args.push(path_str);
                args
            }
            Self::Emacs => {
                let mut args = Vec::new();
                if let Some(l) = line {
                    let token = match col {
                        Some(c) => format!("+{l}:{c}"),
                        None => format!("+{l}"),
                    };
                    args.push(token);
                }
                args.push(path_str);
                args
            }
            Self::Unknown => vec![path_str],
        }
    }
}

fn format_path_line_col(path: &str, line: Option<u32>, col: Option<u32>) -> String {
    match (line, col) {
        (Some(l), Some(c)) => format!("{path}:{l}:{c}"),
        (Some(l), None) => format!("{path}:{l}"),
        (None, _) => path.to_string(),
    }
}

/// Parse a shell-style env value into (binary, leading-flags). Splits on
/// whitespace outside quotes; the first token is the binary, the rest are
/// extra flags the user pre-configured (e.g. `EDITOR="code --wait"`).
/// Returns `None` when the value is empty after trim.
fn parse_env_editor(value: &str) -> Option<(String, Vec<String>)> {
    let mut parts = split_editor_command_line(value).into_iter();
    let bin = parts.next()?;
    if bin.is_empty() {
        return None;
    }
    Some((bin, parts.collect()))
}

fn split_editor_command_line(value: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    let mut quote: Option<char> = None;
    let mut token_started = false;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' if matches!(chars.peek(), Some('"') | Some('\\')) => {
                    current.push(chars.next().expect("peeked char exists"));
                }
                _ => current.push(ch),
            },
            Some(_) => unreachable!("only single and double quotes are set"),
            None if ch.is_whitespace() => {
                if token_started {
                    args.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            None if matches!(ch, '\'' | '"') => {
                quote = Some(ch);
                token_started = true;
            }
            None => {
                current.push(ch);
                token_started = true;
            }
        }
    }

    if token_started {
        args.push(current);
    }
    args
}

fn resolve_editor_command(command: &str) -> PathBuf {
    let path = Path::new(command);
    if path.is_absolute() || path.components().count() > 1 {
        PathBuf::from(command)
    } else {
        crate::app::workspace_ops::resolve_editor_binary(command)
    }
}

/// Ordered probe list for the fallback chain when no `$VISUAL`/`$EDITOR`
/// is set. Order matters: GUI editors first (more likely the user's
/// daily driver), then terminal editors.
const FALLBACK_PROBES: &[&str] = &[
    "code",
    "cursor",
    "zed",
    "subl",
    "code-insiders",
    "windsurf",
    "hx",
    "nvim",
    "vim",
    "emacs",
];

// Explicit selection and automatic environment fallback share this resolver.
fn preferred_editor_commands(
    configured: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
) -> Vec<(String, Vec<String>)> {
    let configured = configured.map(str::trim).filter(|value| !value.is_empty());
    if configured == Some("system") {
        return Vec::new();
    }
    [configured.filter(|value| *value != "auto"), visual, editor]
        .into_iter()
        .flatten()
        .filter_map(parse_env_editor)
        .collect()
}

/// What [`workspace_editor_launch`] selected. The workspace launcher appends
/// `.` in the workspace directory; this does not build a line:col argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceEditorLaunch {
    /// Binary plus flags parsed from the command line.
    Command { bin: String, args: Vec<String> },
    /// macOS file handler. `external_editor = system`, or nothing resolved.
    System,
}

/// The editor `OpenWorkspaceInEditor` launches for a workspace directory.
///
/// Same order as [`open_at_location`]: an explicit `external_editor`, else
/// `$VISUAL`, else `$EDITOR`, else the first [`FALLBACK_PROBES`] entry
/// `installed` accepts. Does not spawn. A later spawn failure does not try
/// the next candidate; the workspace path reports that one launch. There is
/// no per-editor chord; this is the only workspace-open command.
pub(crate) fn workspace_editor_launch(
    configured: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
    mut installed: impl FnMut(&str) -> bool,
) -> WorkspaceEditorLaunch {
    if configured.map(str::trim) == Some("system") {
        return WorkspaceEditorLaunch::System;
    }
    if let Some((bin, args)) = preferred_editor_commands(configured, visual, editor)
        .into_iter()
        .next()
    {
        return WorkspaceEditorLaunch::Command { bin, args };
    }
    for probe in FALLBACK_PROBES {
        if installed(probe) {
            return WorkspaceEditorLaunch::Command {
                bin: (*probe).to_string(),
                args: Vec::new(),
            };
        }
    }
    WorkspaceEditorLaunch::System
}

/// Open `path` in the user's preferred editor at the given location.
/// Spawns the editor process detached - does not wait for it to exit.
///
/// Errors are logged at `warn` level and swallowed so a misconfigured
/// editor never panics the renderer. The boolean return signals only
/// whether something was actually spawned (useful for tests).
pub fn open_at_location(
    path: &Path,
    line: Option<u32>,
    col: Option<u32>,
    configured: Option<&str>,
) -> bool {
    if configured.map(str::trim) == Some("system") {
        return open::that(path).is_ok();
    }
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    for (bin, mut args) in
        preferred_editor_commands(configured, visual.as_deref(), editor.as_deref())
    {
        let kind = EditorKind::from_binary_name(&bin);
        args.extend(kind.argv_for(path, line, col));
        let resolved = resolve_editor_command(&bin);
        if try_spawn(&resolved.to_string_lossy(), &args) {
            return true;
        }
    }

    // 2. Fallback probe
    for probe in FALLBACK_PROBES {
        let found = resolve_editor_command(probe);
        if found == PathBuf::from(probe) {
            continue;
        }
        let kind = EditorKind::from_binary_name(probe);
        let args = kind.argv_for(path, line, col);
        if try_spawn(&found.to_string_lossy(), &args) {
            return true;
        }
    }

    // 3. Last-resort: OS handler (loses line/col).
    log::warn!(
        "editor: no $VISUAL/$EDITOR and none of {:?} on PATH - falling back to OS handler",
        FALLBACK_PROBES
    );
    open::that(path).is_ok()
}

fn try_spawn(bin: &str, args: &[String]) -> bool {
    match spawn_detached(Command::new(bin).args(args)) {
        Ok(()) => {
            log::info!("editor: spawned {bin} {args:?}");
            true
        }
        Err(e) => {
            log::warn!("editor: spawn {bin} {args:?} failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_editor_precedes_environment_and_auto_preserves_it() {
        assert_eq!(
            preferred_editor_commands(Some("zed --wait"), None, None),
            vec![("zed".into(), vec!["--wait".into()])]
        );
        assert_eq!(
            preferred_editor_commands(Some("cursor"), Some("zed"), Some("code"))[0].0,
            "cursor"
        );
        assert_eq!(
            preferred_editor_commands(Some("auto"), Some("zed"), Some("code"))[0].0,
            "zed"
        );
        assert!(preferred_editor_commands(Some("system"), Some("zed"), Some("code")).is_empty());
    }

    /// `OpenWorkspaceInEditor` must launch whatever `external_editor` names.
    /// Hardcoding one of the old four CLIs fails this, including when those
    /// CLIs are the ones the fallback probe would otherwise find.
    #[test]
    fn open_workspace_in_editor_launches_the_configured_external_editor() {
        assert_eq!(
            workspace_editor_launch(Some("hx --wait"), Some("zed"), Some("code"), |_| true),
            WorkspaceEditorLaunch::Command {
                bin: "hx".into(),
                args: vec!["--wait".into()],
            }
        );
        assert_eq!(
            workspace_editor_launch(
                Some(r#""/opt/editors/My Editor" --wait"#),
                Some("zed"),
                None,
                |_| true,
            ),
            WorkspaceEditorLaunch::Command {
                bin: "/opt/editors/My Editor".into(),
                args: vec!["--wait".into()],
            }
        );
        assert_eq!(
            workspace_editor_launch(Some("cursor"), Some("zed"), None, |_| false),
            WorkspaceEditorLaunch::Command {
                bin: "cursor".into(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            workspace_editor_launch(Some("auto"), Some("nvim"), Some("emacs"), |command| {
                matches!(command, "zed" | "cursor" | "code" | "windsurf")
            }),
            WorkspaceEditorLaunch::Command {
                bin: "nvim".into(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            workspace_editor_launch(None, None, None, |command| command == "hx"),
            WorkspaceEditorLaunch::Command {
                bin: "hx".into(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            workspace_editor_launch(Some("system"), Some("zed"), Some("code"), |_| true),
            WorkspaceEditorLaunch::System
        );
    }

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn editor_kind_recognises_vscode_family() {
        assert_eq!(EditorKind::from_binary_name("code"), EditorKind::VsCodeLike);
        assert_eq!(
            EditorKind::from_binary_name("cursor"),
            EditorKind::VsCodeLike
        );
        assert_eq!(
            EditorKind::from_binary_name("/usr/bin/code"),
            EditorKind::VsCodeLike
        );
        assert_eq!(
            EditorKind::from_binary_name("code.cmd"),
            EditorKind::VsCodeLike
        );
    }

    #[test]
    fn editor_kind_recognises_zed_vim_helix_emacs() {
        assert_eq!(EditorKind::from_binary_name("zed"), EditorKind::Zed);
        assert_eq!(EditorKind::from_binary_name("nvim"), EditorKind::VimFamily);
        assert_eq!(EditorKind::from_binary_name("vim"), EditorKind::VimFamily);
        assert_eq!(EditorKind::from_binary_name("hx"), EditorKind::Helix);
        assert_eq!(EditorKind::from_binary_name("emacs"), EditorKind::Emacs);
        assert_eq!(
            EditorKind::from_binary_name("emacsclient"),
            EditorKind::Emacs
        );
    }

    #[test]
    fn editor_kind_unknown_falls_back() {
        assert_eq!(
            EditorKind::from_binary_name("my-weird-editor"),
            EditorKind::Unknown
        );
        assert_eq!(EditorKind::from_binary_name(""), EditorKind::Unknown);
    }

    #[test]
    fn argv_vscode_uses_g_flag() {
        let args = EditorKind::VsCodeLike.argv_for(p("/tmp/x.rs"), Some(42), Some(7));
        assert_eq!(args, vec!["-g".to_string(), "/tmp/x.rs:42:7".to_string()]);
    }

    #[test]
    fn argv_vim_uses_plus_line_no_col() {
        let args = EditorKind::VimFamily.argv_for(p("/tmp/x.rs"), Some(42), Some(7));
        assert_eq!(args, vec!["+42".to_string(), "/tmp/x.rs".to_string()]);
    }

    #[test]
    fn argv_emacs_uses_plus_line_col() {
        let args = EditorKind::Emacs.argv_for(p("/tmp/x.rs"), Some(42), Some(7));
        assert_eq!(args, vec!["+42:7".to_string(), "/tmp/x.rs".to_string()]);
    }

    #[test]
    fn argv_zed_bare_path_colon_line() {
        let args = EditorKind::Zed.argv_for(p("/tmp/x.rs"), Some(42), None);
        assert_eq!(args, vec!["/tmp/x.rs:42".to_string()]);
    }

    #[test]
    fn argv_unknown_drops_location() {
        let args = EditorKind::Unknown.argv_for(p("/tmp/x.rs"), Some(42), Some(7));
        assert_eq!(args, vec!["/tmp/x.rs".to_string()]);
    }

    #[test]
    fn argv_no_line_no_col_just_path() {
        let args = EditorKind::VsCodeLike.argv_for(p("/tmp/x.rs"), None, None);
        assert_eq!(args, vec!["-g".to_string(), "/tmp/x.rs".to_string()]);
    }

    #[test]
    fn parse_env_editor_splits_binary_and_flags() {
        let (bin, args) = parse_env_editor("code --wait").unwrap();
        assert_eq!(bin, "code");
        assert_eq!(args, vec!["--wait".to_string()]);
    }

    #[test]
    fn parse_env_editor_preserves_quoted_windows_binary() {
        let (bin, args) =
            parse_env_editor(r#""C:\Program Files\Microsoft VS Code\bin\code.cmd" --wait"#)
                .unwrap();
        assert_eq!(bin, r"C:\Program Files\Microsoft VS Code\bin\code.cmd");
        assert_eq!(args, vec!["--wait".to_string()]);
    }

    #[test]
    fn parse_env_editor_preserves_quoted_flag_value() {
        let (bin, args) = parse_env_editor(r#"code --profile "Arthur Dev""#).unwrap();
        assert_eq!(bin, "code");
        assert_eq!(
            args,
            vec!["--profile".to_string(), "Arthur Dev".to_string()]
        );
    }

    #[test]
    fn parse_env_editor_empty_is_none() {
        assert!(parse_env_editor("").is_none());
        assert!(parse_env_editor("   ").is_none());
    }

    #[test]
    fn parse_env_editor_only_binary() {
        let (bin, args) = parse_env_editor("nvim").unwrap();
        assert_eq!(bin, "nvim");
        assert!(args.is_empty());
    }

    #[test]
    fn format_path_line_col_combinations() {
        assert_eq!(format_path_line_col("x.rs", None, None), "x.rs");
        assert_eq!(format_path_line_col("x.rs", Some(1), None), "x.rs:1");
        assert_eq!(format_path_line_col("x.rs", Some(1), Some(2)), "x.rs:1:2");
        // No line + col is invalid: col is dropped silently.
        assert_eq!(format_path_line_col("x.rs", None, Some(7)), "x.rs");
    }
}
