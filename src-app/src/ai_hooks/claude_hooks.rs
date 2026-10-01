//! `paneflow hooks setup | status | uninstall` - persistent user-scope
//! agent notification hooks.
//!
//! Claude hook shape, command rendering, detection, and reconciliation live in
//! `paneflow-agent-config`, shared with the project-local shim. This module owns
//! only user-scope path resolution, safe persistence, and CLI presentation.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use paneflow_agent_config::claude_hooks::{self, HookStatus};

/// Result of `setup` on Claude's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOutcome {
    /// The PaneFlow hooks were added where none existed.
    Installed,
    /// Existing PaneFlow hooks were rewritten.
    Updated,
    /// The hooks already matched - no write.
    AlreadyCurrent,
}

/// Result of `uninstall` on Claude's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UninstallOutcome {
    /// The PaneFlow hooks were removed.
    Removed,
    /// No PaneFlow hooks were present - nothing to do.
    NothingToRemove,
}

/// `$CLAUDE_CONFIG_DIR/settings.json` (default `~/.claude/settings.json`) -
/// where Claude Code reads user-scope hooks. NOT `~/.claude.json`.
fn claude_settings_path() -> Option<PathBuf> {
    paneflow_agent_config::claude_settings_json()
}

/// `claude` on PATH, or `$CLAUDE_CONFIG_DIR` (else `~/.claude`) exists.
fn claude_detected() -> bool {
    which::which("claude").is_ok()
        || paneflow_agent_config::claude_config_dir().is_some_and(|d| d.exists())
}

/// Read + parse a JSON config. Missing file → empty object. Present but
/// unparseable → `Err`, so a file we could not parse is never overwritten.
/// Claude's `settings.json` is plain JSON (never JSONC).
fn read_json_or_default(path: &Path) -> Result<serde_json::Value> {
    match paneflow_agent_config::read_optional_text(path)
        .with_context(|| format!("read {} failed", path.display()))?
    {
        Some(text) => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON - refusing to overwrite it; \
                 fix or remove it, then re-run",
                path.display()
            )
        }),
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

/// Pretty-printed with a trailing newline, as editors and Claude leave it.
/// A serialization error propagates instead of writing `{}` over the file.
fn json_to_string(root: &serde_json::Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(root)?;
    text.push('\n');
    Ok(text)
}

/// Atomically write `contents` to `path`: temp file in the same directory,
/// flush + fsync, then `rename`. The rename is atomic on POSIX.
///
/// A symlinked `path` (stow, chezmoi, yadm) is resolved to its target first so
/// the rename updates the managed file instead of replacing the link with a
/// regular file; a dangling link is refused. The error messages are the ones
/// `paneflow hooks` printed before it moved here (issue #857).
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let target = paneflow_agent_config::io::write_target(path)
        .with_context(|| format!("resolve write target for {} failed", path.display()))?;
    let path = target.as_path();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create parent dir {} failed", parent.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("tempfile in {} failed", parent.display()))?;
    std::io::Write::write_all(&mut tmp, contents).context("write_all to tempfile failed")?;
    // Preserve the existing file's mode: persist replaces the inode, which
    // would otherwise silently reset the user's permissions to the temp
    // file's 0600. A missing target keeps the temp file's 0600 default.
    match std::fs::metadata(path) {
        Ok(metadata) => tmp
            .as_file()
            .set_permissions(metadata.permissions())
            .context("preserve existing file mode failed")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context(format!("stat {} failed", path.display()));
        }
    }
    tmp.as_file_mut()
        .sync_all()
        .context("sync_all on tempfile failed")?;
    tmp.persist(path)
        .map_err(|e| anyhow!("atomic rename into {} failed: {}", path.display(), e.error))?;
    Ok(())
}

/// Backup-then-atomic-write `contents` to `path`, **only if** the bytes
/// differ from what is already on disk. Assumes the caller already holds the
/// config lock for `path`.
///
/// Returns `true` when a write happened, `false` when the on-disk bytes
/// already matched (a no-op - no backup, no rename, no mtime change).
fn write_if_changed_unlocked(path: &Path, contents: &[u8]) -> Result<bool> {
    // Only a missing file may proceed to the write; any other read failure
    // (permission, I/O, not a regular file) must not be mistaken for "the
    // bytes differ" and replace a config we could not inspect.
    let existing = paneflow_agent_config::read_optional_text(path)
        .with_context(|| format!("read {} failed", path.display()))?;
    if existing
        .as_ref()
        .is_some_and(|text| text.as_bytes() == contents)
    {
        return Ok(false);
    }
    if let Some(existing) = existing {
        // Back up the bounded bytes already inspected, without reopening a
        // path that could have become a FIFO or grown since the read.
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        write_atomic(Path::new(&bak), existing.as_bytes())?;
    }
    write_atomic(path, contents)?;
    Ok(true)
}

fn lock_settings(settings: &Path) -> Result<paneflow_agent_config::ConfigLock> {
    paneflow_agent_config::lock_config(settings)
        .with_context(|| format!("lock {} failed", settings.display()))
}

fn install(hook_path: &Path) -> Result<(PathBuf, InstallOutcome)> {
    let settings =
        claude_settings_path().ok_or_else(|| anyhow!("cannot resolve ~/.claude/settings.json"))?;
    let outcome = install_at(&settings, hook_path)?;
    Ok((settings, outcome))
}

fn install_at(settings: &Path, hook_path: &Path) -> Result<InstallOutcome> {
    let _lock = lock_settings(settings)?;
    let mut root = read_json_or_default(settings)?;
    let reconciled = claude_hooks::reconcile_hooks(&mut root, |event| {
        claude_hooks::render_hook_command(hook_path, event)
    })?;
    if !reconciled.changed {
        return Ok(InstallOutcome::AlreadyCurrent);
    }
    write_if_changed_unlocked(settings, json_to_string(&root)?.as_bytes())?;
    Ok(if reconciled.had_prior {
        InstallOutcome::Updated
    } else {
        InstallOutcome::Installed
    })
}

fn uninstall() -> Result<UninstallOutcome> {
    let settings =
        claude_settings_path().ok_or_else(|| anyhow!("cannot resolve ~/.claude/settings.json"))?;
    uninstall_at(&settings)
}

fn uninstall_at(settings: &Path) -> Result<UninstallOutcome> {
    if !settings.exists() {
        return Ok(UninstallOutcome::NothingToRemove);
    }
    let _lock = lock_settings(settings)?;
    if !settings.exists() {
        return Ok(UninstallOutcome::NothingToRemove);
    }
    let mut root = read_json_or_default(settings)?;
    if !claude_hooks::remove_hooks(&mut root)? {
        return Ok(UninstallOutcome::NothingToRemove);
    }
    write_if_changed_unlocked(settings, json_to_string(&root)?.as_bytes())?;
    Ok(UninstallOutcome::Removed)
}

fn status(expected_hook_path: Option<&Path>) -> Result<HookStatus> {
    let settings =
        claude_settings_path().ok_or_else(|| anyhow!("cannot resolve ~/.claude/settings.json"))?;
    status_at(&settings, expected_hook_path)
}

fn status_at(settings: &Path, expected_hook_path: Option<&Path>) -> Result<HookStatus> {
    if !settings.exists() {
        return Ok(HookStatus::NotInstalled);
    }
    let root = read_json_or_default(settings)?;
    Ok(claude_hooks::inspect_hooks(&root, expected_hook_path))
}

const HOOKS_USAGE: &str = "\
paneflow hooks - register the PaneFlow agent-notification hooks with your agents

Usage:
  paneflow hooks setup       Install persistent hooks for every supported agent
  paneflow hooks uninstall   Remove the PaneFlow hooks
  paneflow hooks status      Report the hook installation state per agent";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HooksCommand {
    Setup,
    Uninstall,
    Status,
}

impl HooksCommand {
    fn parse(argument: Option<&str>) -> Option<Self> {
        match argument {
            Some("setup") => Some(Self::Setup),
            Some("uninstall") => Some(Self::Uninstall),
            Some("status") => Some(Self::Status),
            _ => None,
        }
    }
}

/// Entry point for `paneflow hooks <cmd>`. `args` is everything after
/// `paneflow hooks`; `hook_path` is the stable ai-hook binary, or `None` when
/// the data dir is unresolvable. Returns the process exit code: `0` success,
/// `1` an error, `2` a usage error.
#[must_use]
pub fn run_hooks_cli(args: &[String], hook_path: Option<PathBuf>) -> i32 {
    run_hooks_with(
        args,
        hook_path.as_deref(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

fn run_hooks_with(
    args: &[String],
    hook_path: Option<&Path>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let Some(command) = HooksCommand::parse(args.first().map(String::as_str)) else {
        let _ = writeln!(err, "{HOOKS_USAGE}");
        return 2;
    };
    if args.len() != 1 {
        let _ = writeln!(
            err,
            "unexpected argument after `{}`\n\n{HOOKS_USAGE}",
            args[0]
        );
        return 2;
    }

    match command {
        HooksCommand::Setup => run_setup(hook_path, out, err),
        HooksCommand::Uninstall => run_uninstall(out, err),
        HooksCommand::Status => run_status(hook_path, out, err),
    }
}

fn run_setup(hook_path: Option<&Path>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(hook_path) = hook_path else {
        let _ = writeln!(
            err,
            "hooks: the paneflow-ai-hook binary is unavailable (data dir unresolvable); cannot install"
        );
        return 1;
    };
    let code = if !claude_detected() {
        let _ = writeln!(out, "claude-code: not detected (skipped)");
        0
    } else {
        match install(hook_path) {
            Ok((path, outcome)) => {
                let verb = match outcome {
                    InstallOutcome::Installed => "installed",
                    InstallOutcome::Updated => "updated",
                    InstallOutcome::AlreadyCurrent => "already current",
                };
                let _ = writeln!(out, "claude-code: hooks {verb} ({})", path.display());
                0
            }
            Err(error) => {
                let _ = writeln!(err, "claude-code: error: {error:#}");
                1
            }
        }
    };
    report_other_agents(out);
    code
}

fn run_uninstall(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match uninstall() {
        Ok(UninstallOutcome::Removed) => {
            let _ = writeln!(out, "claude-code: hooks removed");
            0
        }
        Ok(UninstallOutcome::NothingToRemove) => {
            let _ = writeln!(out, "claude-code: no PaneFlow hooks present");
            0
        }
        Err(error) => {
            let _ = writeln!(err, "claude-code: error: {error:#}");
            1
        }
    }
}

fn run_status(expected_hook_path: Option<&Path>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let code = match status(expected_hook_path) {
        Ok(HookStatus::Installed { path }) => {
            let _ = writeln!(out, "claude-code: installed ({path})");
            0
        }
        Ok(HookStatus::Stale { found, expected }) => {
            let _ = writeln!(
                out,
                "claude-code: stale (found {found}, expected {expected})"
            );
            0
        }
        Ok(HookStatus::NeedsRepair { path, reason }) => {
            let suffix = path
                .as_deref()
                .map(|path| format!(" at {path}"))
                .unwrap_or_default();
            let _ = writeln!(out, "claude-code: needs repair{suffix} ({reason})");
            0
        }
        Ok(HookStatus::NotInstalled) => {
            let _ = writeln!(out, "claude-code: not installed");
            0
        }
        Err(error) => {
            let _ = writeln!(err, "claude-code: error: {error:#}");
            1
        }
    };
    report_other_agents(out);
    code
}

fn report_other_agents(out: &mut dyn Write) {
    report_detected_other_agents(
        out,
        which::which("codex").is_ok(),
        which::which("opencode").is_ok(),
    );
}

fn report_detected_other_agents(
    out: &mut dyn Write,
    codex_detected: bool,
    opencode_detected: bool,
) {
    if codex_detected {
        let _ = writeln!(
            out,
            "codex: hooks injected per-launch by the shim (no user-scope install)"
        );
    }
    if opencode_detected {
        let _ = writeln!(
            out,
            "opencode: hooks injected per-launch by the shim (no user-scope install)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::ffi::OsString;

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn claude_settings_path_honors_claude_config_dir() {
        assert_eq!(
            paneflow_agent_config::claude_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("/tmp/claude-cfg")),
            )
            .map(|d| d.join("settings.json")),
            Some(PathBuf::from("/tmp/claude-cfg").join("settings.json")),
        );
    }

    #[test]
    fn claude_settings_path_default_is_home_dot_claude() {
        assert_eq!(
            paneflow_agent_config::claude_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                None,
            )
            .map(|d| d.join("settings.json")),
            Some(PathBuf::from("/home/alice/.claude/settings.json")),
        );
        assert_eq!(
            paneflow_agent_config::claude_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("")),
            )
            .map(|d| d.join("settings.json")),
            Some(PathBuf::from("/home/alice/.claude/settings.json")),
        );
    }

    #[test]
    fn write_if_changed_is_a_noop_for_identical_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"same").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        let wrote = write_if_changed_unlocked(&p, b"same").unwrap();

        assert!(!wrote, "identical bytes must not be rewritten");
        let mtime_after = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "no-op must not bump mtime");
        assert!(
            !dir.path().join("settings.json.bak").exists(),
            "no-op must not write a .bak"
        );
    }

    #[test]
    fn write_if_changed_backs_up_the_old_bytes_and_creates_a_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"old").unwrap();
        assert!(write_if_changed_unlocked(&p, b"new").unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join("settings.json.bak")).unwrap(),
            b"old"
        );

        let fresh = dir.path().join("nested").join("settings.json");
        assert!(write_if_changed_unlocked(&fresh, b"data").unwrap());
        assert_eq!(std::fs::read(&fresh).unwrap(), b"data");
        assert!(!dir.path().join("nested").join("settings.json.bak").exists());
    }

    #[test]
    fn write_if_changed_refuses_an_unreadable_file_before_backup() {
        use std::os::unix::fs::PermissionsExt;

        // Root reads a 0o000 file fine, so the probe below is meaningless.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            eprintln!("skipping: process can read a 0o000 file (running as root?)");
            return;
        }

        let err = write_if_changed_unlocked(&p, b"new").unwrap_err();

        assert_eq!(err.to_string(), format!("read {} failed", p.display()));
        let io_kind = err
            .chain()
            .find_map(|e| e.downcast_ref::<std::io::Error>().map(std::io::Error::kind));
        assert_eq!(io_kind, Some(std::io::ErrorKind::PermissionDenied));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"old",
            "original must be untouched"
        );
        assert!(
            !dir.path().join("settings.json.bak").exists(),
            "no backup may be attempted for a file that could not be read"
        );
    }

    #[test]
    fn a_dangling_symlinked_settings_file_is_refused_with_the_original_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(dir.path().join("missing.json"), &link).unwrap();

        let err = write_if_changed_unlocked(&link, b"{}").unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("resolve write target for {} failed", link.display())
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn install_status_uninstall_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = dir.path().join("settings.json");
        let hook = Path::new("/opt/Pane Flow/paneflow-ai-hook");
        std::fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "theme": "dark",
                "hooks": {
                    "Stop": [{ "hooks": [{ "type": "command", "command": "my-hook" }] }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            install_at(&settings, hook).unwrap(),
            InstallOutcome::Installed
        );
        assert_eq!(
            install_at(&settings, hook).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
        assert_eq!(
            status_at(&settings, Some(hook)).unwrap(),
            HookStatus::Installed {
                path: claude_hooks::display_hook_program(hook),
            }
        );
        assert_eq!(read(&settings)["theme"], json!("dark"));
        assert_eq!(uninstall_at(&settings).unwrap(), UninstallOutcome::Removed);
        assert_eq!(
            read(&settings)["hooks"]["Stop"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn install_backs_up_the_previous_settings_only_when_it_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = dir.path().join("settings.json");
        let backup = dir.path().join("settings.json.bak");
        let hook = Path::new("/bin/paneflow-ai-hook");
        let before = "{\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&settings, before).unwrap();

        assert_eq!(
            install_at(&settings, hook).unwrap(),
            InstallOutcome::Installed
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), before);
        let installed = std::fs::read_to_string(&settings).unwrap();
        assert!(installed.ends_with("}\n"), "{installed}");

        std::fs::remove_file(&backup).unwrap();
        assert_eq!(
            install_at(&settings, hook).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
        assert!(!backup.exists(), "a no-op must not write a .bak");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), installed);
    }

    #[test]
    fn install_creates_a_missing_settings_file_and_its_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = dir.path().join("claude-cfg").join("settings.json");

        assert_eq!(
            install_at(&settings, Path::new("/bin/paneflow-ai-hook")).unwrap(),
            InstallOutcome::Installed
        );
        assert!(read(&settings)["hooks"].is_object());
        assert!(
            !dir.path()
                .join("claude-cfg")
                .join("settings.json.bak")
                .exists()
        );
    }

    #[test]
    fn install_refuses_invalid_hook_boundaries_without_clobbering() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = dir.path().join("settings.json");
        let original = br#"{"hooks":{"Stop":"broken"}}"#;
        std::fs::write(&settings, original).unwrap();

        assert!(install_at(&settings, Path::new("/bin/paneflow-ai-hook")).is_err());
        assert_eq!(std::fs::read(&settings).unwrap(), original);
    }

    #[test]
    fn install_refuses_malformed_json_without_clobbering() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, b"{ broken").unwrap();

        let error = install_at(&settings, Path::new("/bin/paneflow-ai-hook")).unwrap_err();

        assert!(format!("{error:#}").contains("not valid JSON"), "{error:#}");
        assert_eq!(std::fs::read(&settings).unwrap(), b"{ broken");
    }

    #[test]
    fn status_rejects_partial_hook_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = dir.path().join("settings.json");
        let hook = Path::new("/bin/paneflow-ai-hook");
        install_at(&settings, hook).unwrap();
        let mut root = read(&settings);
        root["hooks"].as_object_mut().unwrap().remove("Stop");
        std::fs::write(&settings, serde_json::to_vec(&root).unwrap()).unwrap();

        assert!(matches!(
            status_at(&settings, Some(hook)).unwrap(),
            HookStatus::NeedsRepair { .. }
        ));
    }

    #[test]
    fn cli_rejects_bad_or_trailing_arguments() {
        for args in [
            vec!["bogus".to_string()],
            vec!["status".to_string(), "extra".to_string()],
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            assert_eq!(run_hooks_with(&args, None, &mut out, &mut err), 2);
            assert!(String::from_utf8_lossy(&err).contains("Usage"));
        }
    }

    #[test]
    fn setup_without_hook_path_errors() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_hooks_with(&["setup".to_string()], None, &mut out, &mut err);
        assert_eq!(code, 1);
        assert!(String::from_utf8_lossy(&err).contains("unavailable"));
    }

    #[test]
    fn report_other_agents_describes_codex_and_opencode_as_shim_injected() {
        let mut out = Vec::new();
        report_detected_other_agents(&mut out, true, true);
        let output = String::from_utf8(out).unwrap();

        for agent in ["codex", "opencode"] {
            assert!(
                output.contains(&format!("{agent}: hooks injected per-launch by the shim")),
                "missing shim-injection status for {agent}: {output}"
            );
        }
        assert!(
            !output.contains("gemini"),
            "Gemini CLI is retired: {output}"
        );
        assert!(!output.contains("no notification-hook mechanism"));
        assert!(!output.contains("unsupported"));
    }
}
