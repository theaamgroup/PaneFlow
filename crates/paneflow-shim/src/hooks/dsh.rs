use super::owned_files::{
    cleanup_matching_owned_file, install_owned_file, sweep_matching_owned_file,
};
use super::{
    home_unavailable, merge_strict_matcher_hooks_for_events, paneflow_ipc_reachable,
    refuse_symlink, HookInstall, HookInstallResult, HookInstallSkip, HookLease,
};
use paneflow_agent_config::{home_dir, with_config_lock};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const DSH_HOOK_EVENTS: &[&str] = &["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"];
pub(crate) const DSH_HOOKS_BASENAME: &str = "hooks.json";
pub(crate) const DSH_OVERLAY_BASENAME: &str = "paneflow-overlay.yml";
const BRIDGE_PACKAGE: &str = "@deepseek-ai/dsh-hooks-claude-code";
const BRIDGE_ROW_ID: &str = "paneflow-hooks";

pub(crate) struct DshOverlayGuard {
    hooks_path: PathBuf,
    overlay_path: PathBuf,
    /// Device and inode of the overlay directory at install time.
    directory_identity: (u64, u64),
    hooks_source: String,
    overlay_source: String,
    hooks_lease: HookLease,
    overlay_lease: HookLease,
}

impl DshOverlayGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let home = dsh_home().ok_or_else(home_unavailable)?;
        let directory = home.join("paneflow");
        if !paneflow_ipc_reachable() {
            sweep_overlay(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        if !bridge_is_resolvable(&home) {
            sweep_overlay(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::BridgeUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlink(directory, "DeepSeek Harness overlay")?;
        std::fs::create_dir_all(directory)?;
        // The directory exists now, so the leases and ownership markers are
        // keyed on its one canonical spelling, whatever alias reached here.
        let directory = &std::fs::canonicalize(directory)?;
        let directory_identity = directory_identity(directory)?;
        let hooks_path = directory.join(DSH_HOOKS_BASENAME);
        let overlay_path = directory.join(DSH_OVERLAY_BASENAME);
        let mut hooks_lease = HookLease::acquire(&hooks_path)?;
        let mut overlay_lease = HookLease::acquire(&overlay_path)?;
        let hooks_source = hooks_source()?;
        let overlay_source = render_overlay(&hooks_path);
        with_config_lock(&hooks_path, || {
            install_owned_file(&hooks_path, &hooks_source, &mut hooks_lease)
        })?;
        if let Err(error) = with_config_lock(&overlay_path, || {
            install_owned_file(&overlay_path, &overlay_source, &mut overlay_lease)
        }) {
            cleanup_matching_owned_file(&hooks_path, &mut hooks_lease, &hooks_source);
            return Err(error);
        }
        Ok(Self {
            hooks_path,
            overlay_path,
            directory_identity,
            hooks_source,
            overlay_source,
            hooks_lease,
            overlay_lease,
        })
    }

    pub(crate) fn overlay_path(&self) -> &Path {
        &self.overlay_path
    }
}

impl Drop for DshOverlayGuard {
    fn drop(&mut self) {
        // Cleanup is anchored to the directory opened at install time: if
        // it, or any ancestor, has since been swapped so the path resolves
        // to a different directory, the files behind it are not the ones
        // this guard created, so leave them alone (lease and all).
        if !overlay_directory_is_intact(&self.overlay_path, self.directory_identity) {
            return;
        }
        cleanup_matching_owned_file(
            &self.overlay_path,
            &mut self.overlay_lease,
            &self.overlay_source,
        );
        cleanup_matching_owned_file(&self.hooks_path, &mut self.hooks_lease, &self.hooks_source);
    }
}

pub(crate) fn dsh_home() -> Option<PathBuf> {
    resolve_dsh_home(env::var_os("DSH_HOME"), env::current_dir().ok())
        .map(normalize_existing_prefix)
}

/// Canonicalizes the longest existing prefix of `path` and re-appends the
/// rest, so `/tmp/dsh`, `/tmp/x/../dsh`, and a symlinked spelling all derive
/// the same overlay and lease paths even before the directory exists.
fn normalize_existing_prefix(path: PathBuf) -> PathBuf {
    let path = lexically_normalize(&path);
    let mut missing = Vec::new();
    let mut probe = path.as_path();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(probe) {
            return missing
                .iter()
                .rev()
                .fold(canonical, |acc, segment| acc.join(segment));
        }
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                probe = parent;
            }
            _ => return path,
        }
    }
}

/// Resolves `.` and `..` components lexically so a missing suffix such as
/// `new/../dsh` collapses to `dsh` before the existing prefix is
/// canonicalized; `..` never climbs above the root.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A relative `DSH_HOME` is anchored on the shim's working directory before
/// any lease or ownership marker is derived from it: `ConfigLease` hashes the
/// path's spelling, so two shims launched from different directories with
/// `DSH_HOME=.dsh` would otherwise share locks for different files.
fn resolve_dsh_home(configured: Option<OsString>, cwd: Option<PathBuf>) -> Option<PathBuf> {
    match configured {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Some(path)
            } else {
                cwd.map(|cwd| cwd.join(path))
            }
        }
        _ => home_dir().map(|home| home.join(".dsh")),
    }
}

fn bridge_is_resolvable(dsh_home: &Path) -> bool {
    dsh_home
        .join("profiles")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh-hooks-claude-code")
        .is_dir()
}

fn hooks_source() -> std::io::Result<String> {
    let mut root = serde_json::json!({});
    merge_strict_matcher_hooks_for_events(&mut root, DSH_HOOK_EVENTS)?;
    Ok(serde_json::to_string_pretty(&root).map_err(std::io::Error::other)? + "\n")
}

pub(crate) fn render_overlay(hooks_path: &Path) -> String {
    let config_path = yaml_single_quoted(&hooks_path.to_string_lossy());
    format!(
        "- insert:\n    - id: {BRIDGE_ROW_ID}\n      name: '{BRIDGE_PACKAGE}'\n      config:\n        configPath: '{config_path}'\n"
    )
}

fn yaml_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

/// Device and inode of a directory, the identity a later cleanup is
/// checked against so no swap along the path can redirect it.
fn directory_identity(directory: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(directory)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// True when the overlay file's parent is still the very directory that was
/// installed into: not itself a symlink, and resolving (through every
/// ancestor) to the same device and inode recorded at install time.
fn overlay_directory_is_intact(file: &Path, installed: (u64, u64)) -> bool {
    file.parent().is_some_and(|directory| {
        std::fs::symlink_metadata(directory)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && directory_identity(directory).is_ok_and(|identity| identity == installed)
    })
}

fn sweep_overlay(directory: &Path) {
    if refuse_symlink(directory, "DeepSeek Harness overlay").is_err() {
        return;
    }
    let hooks_path = directory.join(DSH_HOOKS_BASENAME);
    let overlay_path = directory.join(DSH_OVERLAY_BASENAME);
    if let Ok(source) = hooks_source() {
        sweep_matching_owned_file(&hooks_path, &source);
    }
    sweep_matching_owned_file(&overlay_path, &render_overlay(&hooks_path));
}

#[cfg(test)]
mod dsh_home_tests {
    use super::{lexically_normalize, normalize_existing_prefix, resolve_dsh_home};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn relative_dsh_home_is_anchored_on_the_working_directory() {
        let resolved = resolve_dsh_home(
            Some(OsString::from(".dsh")),
            Some(PathBuf::from("/work/project-a")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/work/project-a/.dsh")));
        let other = resolve_dsh_home(
            Some(OsString::from(".dsh")),
            Some(PathBuf::from("/work/project-b")),
        );
        assert_ne!(resolved, other);
    }

    #[test]
    fn absolute_dsh_home_is_kept_as_spelled() {
        let resolved = resolve_dsh_home(
            Some(OsString::from("/srv/dsh")),
            Some(PathBuf::from("/work/project-a")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/srv/dsh")));
    }

    #[test]
    fn relative_dsh_home_without_a_working_directory_is_unavailable() {
        assert_eq!(resolve_dsh_home(Some(OsString::from(".dsh")), None), None);
    }

    #[test]
    fn empty_dsh_home_falls_back_to_the_home_directory_default() {
        let resolved = resolve_dsh_home(Some(OsString::new()), Some(PathBuf::from("/work")));
        assert_eq!(resolved.map(|p| p.ends_with(".dsh")), Some(true));
    }

    #[test]
    fn aliased_spellings_of_one_directory_normalize_to_the_same_path() {
        let root = std::env::temp_dir().join(format!("dsh-home-{}", std::process::id()));
        std::fs::create_dir_all(root.join("x")).unwrap();
        let direct = normalize_existing_prefix(root.join("dsh"));
        let aliased = normalize_existing_prefix(root.join("x").join("..").join("dsh"));
        assert_eq!(direct, aliased);
        assert!(direct.ends_with("dsh"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_tail_is_re_appended_to_the_canonical_prefix() {
        let root = std::env::temp_dir().join(format!("dsh-tail-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let resolved = normalize_existing_prefix(root.join("a").join("b"));
        assert_eq!(resolved, canonical_root.join("a").join("b"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dot_dot_after_a_missing_component_collapses_lexically() {
        let root = std::env::temp_dir().join(format!("dsh-missing-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let aliased = normalize_existing_prefix(root.join("new").join("..").join("dsh"));
        assert_eq!(aliased, canonical_root.join("dsh"));
        assert_eq!(aliased, normalize_existing_prefix(root.join("dsh")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lexical_normalization_never_climbs_above_the_root() {
        assert_eq!(
            lexically_normalize(std::path::Path::new("/../a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }
}
