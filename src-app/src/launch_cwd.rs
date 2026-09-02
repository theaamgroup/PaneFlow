use std::path::{Path, PathBuf};

/// Resolve the cwd to use when the caller did not request one explicitly.
///
/// GUI launches can inherit the filesystem root as the process cwd on macOS.
/// Treat that as an unhelpful implicit cwd and prefer the user's home dir,
/// while still preserving an explicitly requested `/` or drive root because
/// those paths bypass this helper.
pub(crate) fn implicit_launch_cwd() -> PathBuf {
    resolve_implicit_launch_cwd(std::env::current_dir().ok(), dirs::home_dir())
}

pub(crate) fn title_for_cwd_or(cwd: &Path, fallback: impl Into<String>) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.into())
}

fn resolve_implicit_launch_cwd(current: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    current
        .filter(|current| !is_filesystem_root(current) && current.is_dir())
        .or_else(|| home.filter(|home| home.is_dir()))
        .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR.to_string()))
}

pub(crate) fn is_filesystem_root(path: &Path) -> bool {
    path.has_root() && path.parent().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_root() -> PathBuf {
        std::env::current_dir()
            .ok()
            .and_then(|path| path.ancestors().last().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR.to_string()))
    }

    fn child_of_root(name: &str) -> PathBuf {
        let mut path = platform_root();
        path.push(name);
        path
    }

    fn real_dir(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        std::fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn implicit_launch_cwd_uses_home_when_current_is_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = platform_root();
        let home = real_dir(temp.path(), "home");

        assert_eq!(
            resolve_implicit_launch_cwd(Some(root), Some(home.clone())),
            home
        );
    }

    #[test]
    fn implicit_launch_cwd_keeps_non_root_current() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current = real_dir(temp.path(), "project");
        let home = real_dir(temp.path(), "home");

        assert_eq!(
            resolve_implicit_launch_cwd(Some(current.clone()), Some(home)),
            current
        );
    }

    #[test]
    fn implicit_launch_cwd_uses_home_when_current_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = real_dir(temp.path(), "home");

        assert_eq!(resolve_implicit_launch_cwd(None, Some(home.clone())), home);
    }

    #[test]
    fn implicit_launch_cwd_uses_home_when_current_is_not_a_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current = temp.path().join("deleted");
        let home = real_dir(temp.path(), "home");

        assert_eq!(
            resolve_implicit_launch_cwd(Some(current), Some(home.clone())),
            home
        );
    }

    #[test]
    fn implicit_launch_cwd_uses_existing_directory_when_home_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_home = temp.path().join("no-such-home");

        let resolved = resolve_implicit_launch_cwd(None, Some(missing_home.clone()));

        assert_ne!(resolved, missing_home);
        assert!(resolved.is_dir(), "{} must exist", resolved.display());
    }

    #[test]
    fn implicit_launch_cwd_uses_existing_directory_when_current_and_home_are_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_current = temp.path().join("deleted");
        let missing_home = temp.path().join("no-such-home");

        let resolved =
            resolve_implicit_launch_cwd(Some(missing_current.clone()), Some(missing_home.clone()));

        assert_ne!(resolved, missing_current);
        assert_ne!(resolved, missing_home);
        assert!(resolved.is_dir(), "{} must exist", resolved.display());
    }

    #[test]
    fn title_for_cwd_or_uses_last_path_component() {
        let cwd = child_of_root("paneflow");

        assert_eq!(title_for_cwd_or(&cwd, "Terminal 1"), "paneflow");
    }

    #[test]
    fn title_for_cwd_or_falls_back_for_root() {
        let root = platform_root();

        assert_eq!(title_for_cwd_or(&root, "Terminal 1"), "Terminal 1");
    }
}
