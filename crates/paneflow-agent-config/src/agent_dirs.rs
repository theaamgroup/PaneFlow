use std::ffi::OsString;
use std::path::PathBuf;

use crate::io::home_dir;

/// `$CLAUDE_CONFIG_DIR` when set and non-empty, else `~/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    claude_config_dir_from(home_dir(), std::env::var_os("CLAUDE_CONFIG_DIR"))
}

/// Pure core: the precedence rule, unit-testable without mutating process env.
pub fn claude_config_dir_from(
    home: Option<PathBuf>,
    claude_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    claude_config_dir
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| h.join(".claude")))
}

/// `$CLAUDE_CONFIG_DIR/settings.json` (default `~/.claude/settings.json`).
pub fn claude_settings_json() -> Option<PathBuf> {
    claude_config_dir().map(|dir| dir.join("settings.json"))
}

/// `$CODEX_HOME` when set and non-empty, else `~/.codex`.
pub fn codex_config_dir() -> Option<PathBuf> {
    codex_config_dir_from(home_dir(), std::env::var_os("CODEX_HOME"))
}

pub fn codex_config_dir_from(
    home: Option<PathBuf>,
    codex_home: Option<OsString>,
) -> Option<PathBuf> {
    codex_home
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| h.join(".codex")))
}

/// `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`).
pub fn codex_config_toml() -> Option<PathBuf> {
    codex_config_dir().map(|dir| dir.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    #[test]
    fn claude_config_dir_from_honors_claude_config_dir() {
        assert_eq!(
            claude_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("/tmp/claude-cfg")),
            ),
            Some(PathBuf::from("/tmp/claude-cfg")),
        );
    }

    #[test]
    fn claude_config_dir_from_default_is_home_dot_claude() {
        assert_eq!(
            claude_config_dir_from(Some(PathBuf::from("/home/alice")), None),
            Some(PathBuf::from("/home/alice/.claude")),
        );
        assert_eq!(
            claude_config_dir_from(Some(PathBuf::from("/home/alice")), Some(OsString::from("")),),
            Some(PathBuf::from("/home/alice/.claude")),
        );
    }

    /// Process-env path: `CLAUDE_CONFIG_DIR` pointed at a temp dir must win
    /// over `HOME/.claude` when resolving durable Claude hooks.
    #[test]
    fn claude_settings_json_reads_claude_config_dir_env() {
        let td = tempfile::TempDir::new().unwrap();
        let _guard = ClaudeConfigDirGuard::set(td.path());
        assert_eq!(
            claude_settings_json(),
            Some(td.path().join("settings.json")),
        );
    }

    struct ClaudeConfigDirGuard {
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static CLAUDE_CONFIG_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(deprecated)]
    impl ClaudeConfigDirGuard {
        fn set(path: &Path) -> Self {
            let lock = CLAUDE_CONFIG_DIR_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
            std::env::set_var("CLAUDE_CONFIG_DIR", path);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    #[allow(deprecated)]
    impl Drop for ClaudeConfigDirGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }
    }

    #[test]
    fn codex_config_dir_from_honors_codex_home() {
        let td = tempfile::TempDir::new().unwrap();
        assert_eq!(
            super::codex_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(td.path().as_os_str().to_os_string()),
            ),
            Some(td.path().to_path_buf()),
        );
    }

    #[test]
    fn codex_config_dir_from_falls_back_when_codex_home_empty() {
        assert_eq!(
            super::codex_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("")),
            ),
            Some(PathBuf::from("/home/alice/.codex")),
        );
        assert_eq!(
            super::codex_config_dir_from(Some(PathBuf::from("/home/alice")), None),
            Some(PathBuf::from("/home/alice/.codex")),
        );
    }

    /// Process-env path: `CODEX_HOME` pointed at a temp dir must win over
    /// `HOME/.codex` when resolving the Unix `hooks = true` flag file.
    #[test]
    fn codex_config_toml_reads_codex_home_env() {
        let td = tempfile::TempDir::new().unwrap();
        let _guard = CodexHomeGuard::set(td.path());
        assert_eq!(
            super::codex_config_toml(),
            Some(td.path().join("config.toml")),
        );
    }

    struct CodexHomeGuard {
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static CODEX_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(deprecated)]
    impl CodexHomeGuard {
        fn set(path: &Path) -> Self {
            let lock = CODEX_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("CODEX_HOME");
            // Edition 2021: `set_var` is still safe (unsafe only in 2024).
            std::env::set_var("CODEX_HOME", path);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    #[allow(deprecated)]
    impl Drop for CodexHomeGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }
}
