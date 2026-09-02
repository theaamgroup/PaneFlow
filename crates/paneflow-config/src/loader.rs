// US-017: JSON config loader with validation

use crate::schema::PaneFlowConfig;
pub use crate::schema::{validate_layout, MAX_PANE_SURFACES};
use serde_json::{Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::warn;

/// Application directory namespace. Switches to `paneflow-dev` in debug
/// builds so a `cargo run` instance (typical dev workflow) never reads
/// or writes the same config / session file as the user's installed
/// `.app` bundle. Mirrors `paneflow_app::runtime_paths::APP_SUBDIR`
/// so per-build isolation is consistent across every persistence
/// surface (config, session, threads, sockets, caches).
pub const APP_SUBDIR: &str = if cfg!(debug_assertions) {
    "paneflow-dev"
} else {
    "paneflow"
};

/// Hard cap on the size of any config file we will read into memory.
/// Real configs are kilobytes; this guards against a runaway or hostile
/// file on disk causing the GPUI main thread to stall while
/// `read_to_string` allocates. 1 MiB is roughly two orders of magnitude
/// above any plausible config.
const MAX_CONFIG_SIZE_BYTES: u64 = 1 << 20;

/// Errors that can occur when loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    IoError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config path is not a regular file: {path}")]
    NotRegularFile { path: PathBuf },
    #[error("config file {path} is {actual} bytes, over the {maximum}-byte cap")]
    TooLarge {
        path: PathBuf,
        actual: u64,
        maximum: u64,
    },
    #[error("invalid config document: {0}")]
    ParseError(#[from] serde_json::Error),
}

/// Returns the macOS config file path:
/// `~/Library/Application Support/paneflow/paneflow.json`.
/// Debug builds use the `paneflow-dev` subdir instead.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_SUBDIR).join("paneflow.json"))
}

/// Filename of the persisted session. Namespaced per build profile so a
/// `cargo run` instance never overwrites the installed app's layout.
fn session_filename() -> &'static str {
    if cfg!(debug_assertions) {
        "session-dev.json"
    } else {
        "session.json"
    }
}

/// Returns the macOS session file path:
/// `~/Library/Application Support/paneflow/session.json`.
///
/// Lives next to `paneflow.json`. Debug builds write `session-dev.json`
/// under the `paneflow-dev` subdir. The previous location was
/// `dirs::cache_dir()`, which macOS may purge; call
/// [`session_path_migrated`] (or [`migrate_session_from_cache`]) so a
/// leftover cache copy is copied forward once.
pub fn session_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_SUBDIR).join(session_filename()))
}

/// Pre-#45 location: `~/Library/Caches/paneflow/{session,session-dev}.json`.
pub fn legacy_session_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join(APP_SUBDIR).join(session_filename()))
}

/// One-shot copy of a leftover cache-dir session onto `dest`.
///
/// No-ops when `src` and `dest` are the same path, `dest` already exists,
/// `src` is missing, or `src` is not a regular file. Deletes `src` only
/// after `std::fs::copy` succeeds. Returns `Ok(true)` when a copy happened.
pub fn migrate_session_from_cache(src: &Path, dest: &Path) -> std::io::Result<bool> {
    if src == dest {
        return Ok(false);
    }
    if dest.exists() {
        return Ok(false);
    }
    let src_meta = match std::fs::metadata(src) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !src_meta.is_file() {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dest)?;
    if let Err(e) = std::fs::remove_file(src) {
        warn!(
            "migrated session to {} but could not remove {}: {e}",
            dest.display(),
            src.display()
        );
    }
    Ok(true)
}

/// [`session_path`], after copying a leftover cache-dir session into place.
///
/// Load and save must use this so an upgrade still restores. [`session_path`]
/// stays a pure path computation so tests can assert the location without
/// touching the user's files.
pub fn session_path_migrated() -> Option<PathBuf> {
    let dest = session_path()?;
    if let Some(src) = legacy_session_cache_path() {
        match migrate_session_from_cache(&src, &dest) {
            Ok(true) => {
                tracing::info!(
                    "migrated session from {} to {}",
                    src.display(),
                    dest.display()
                );
            }
            Ok(false) => {}
            Err(e) => {
                warn!(
                    "failed to migrate session from {} to {}: {e}",
                    src.display(),
                    dest.display()
                );
            }
        }
    }
    Some(dest)
}

/// Load the PaneFlow configuration from the default platform path.
///
/// - If the config file does not exist, returns `PaneFlowConfig::default()`.
/// - If the file is malformed or has a non-object root, logs a warning and
///   returns defaults.
/// - Individual command entries with validation errors are skipped with warnings.
pub fn load_config() -> PaneFlowConfig {
    let Some(path) = config_path() else {
        warn!("could not determine config directory; using defaults");
        return PaneFlowConfig::default();
    };

    load_config_from_path(&path)
}

/// US-029: read the config file to a string with the oversize guard applied
/// BEFORE allocating (cheap `metadata` stat first). Shared by the cold loader
/// and the hot watcher reload so the DoS guard can never be missing on either
/// path. `Ok(None)` means the file is absent; every other failure remains typed
/// so cold start and hot reload can apply different policies deliberately.
pub fn read_config_string(path: &Path) -> Result<Option<String>, ConfigError> {
    // Issue #241: `open(O_RDONLY)` on a FIFO with no writer blocks forever, so
    // the post-open type check below can never be reached for that case. Stat
    // the path first (following symlinks, so a dotfile-manager link to a
    // regular file still loads) and refuse anything that is not a regular file
    // before the blocking open. The check on the opened descriptor stays: it
    // is what closes the stat-to-open swap window.
    match std::fs::metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(ConfigError::NotRegularFile {
                path: path.to_path_buf(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::IoError {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::IoError {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let metadata = file.metadata().map_err(|source| ConfigError::IoError {
        path: path.to_path_buf(),
        source,
    })?;
    match metadata {
        // U-028: a FIFO/character device reports len 0 (passing the size cap)
        // but `read_to_string` would then block indefinitely or stream unbounded
        // bytes. Metadata is taken from the already-open file, so a path swap
        // between stat and read cannot bypass the guard.
        meta if !meta.file_type().is_file() => Err(ConfigError::NotRegularFile {
            path: path.to_path_buf(),
        }),
        meta if meta.len() > MAX_CONFIG_SIZE_BYTES => Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
            actual: meta.len(),
            maximum: MAX_CONFIG_SIZE_BYTES,
        }),
        _ => {
            let mut contents = String::new();
            match file
                .take(MAX_CONFIG_SIZE_BYTES + 1)
                .read_to_string(&mut contents)
            {
                Ok(_) if contents.len() as u64 <= MAX_CONFIG_SIZE_BYTES => Ok(Some(contents)),
                Ok(_) => Err(ConfigError::TooLarge {
                    path: path.to_path_buf(),
                    actual: contents.len() as u64,
                    maximum: MAX_CONFIG_SIZE_BYTES,
                }),
                Err(source) => Err(ConfigError::IoError {
                    path: path.to_path_buf(),
                    source,
                }),
            }
        }
    }
}

/// Load and validate configuration from a specific file path.
///
/// This is the core loading function, also useful for testing.
pub fn load_config_from_path(path: &std::path::Path) -> PaneFlowConfig {
    match read_config_string(path) {
        Ok(Some(contents)) => parse_and_validate_with_path(&contents, path),
        Ok(None) => PaneFlowConfig::default(),
        Err(error) => {
            warn!("{error}; using defaults");
            PaneFlowConfig::default()
        }
    }
}

/// Parse a JSON string into a validated `PaneFlowConfig`.
///
/// A malformed document or non-object root produces a warning and returns
/// defaults.
/// Individual commands with validation errors are filtered out with warnings.
pub fn parse_and_validate(json: &str) -> PaneFlowConfig {
    parse_and_validate_with_path(json, Path::new("<config>"))
}

/// Parse + validate. `path` is threaded into the warning so a malformed save
/// names the offending file instead of an anonymous "config".
pub fn parse_and_validate_with_path(json: &str, path: &Path) -> PaneFlowConfig {
    try_parse_and_validate(json).unwrap_or_else(|e| {
        warn!("invalid config {}: {e}; using defaults", path.display());
        PaneFlowConfig::default()
    })
}

/// US-029: parse + validate, parsing the JSON exactly once and surfacing a
/// syntax or root-shape error as `Err` instead of silently returning defaults.
/// The hot reload path uses this so it can keep the previous config on a malformed
/// save (never broadcasting defaults) AND avoid the old double-parse (a
/// syntax-guard `from_str` followed by a second parse inside
/// `parse_and_validate_with_path`). Command filtering + layout fixups are
/// applied on the success path, unchanged.
pub fn try_parse_and_validate(json: &str) -> Result<PaneFlowConfig, ConfigError> {
    // Parsing directly into a map makes a non-object root a typed error. Cold
    // start may choose defaults; hot reload can keep the last valid config.
    let root: Map<String, Value> = serde_json::from_str(json)?;
    match root.get("$schemaVersion") {
        Some(Value::String(version)) if version != "1.0.0" => {
            warn!("config schema version `{version}` is not recognized; loading leniently");
        }
        Some(Value::String(_)) | None => {}
        Some(_) => warn!("config schema version is not a string; ignoring it"),
    }

    let mut config: PaneFlowConfig = serde_json::from_value(Value::Object(root))?;

    // Validate and fix layout nodes in-place.
    for cmd in &mut config.commands {
        if let Some(ref mut ws) = cmd.workspace {
            if let Some(ref mut layout) = ws.layout {
                validate_layout(layout);
            }
        }
    }

    Ok(config)
}

#[cfg(test)]
#[path = "loader_tests/core.rs"]
mod core_tests;
#[cfg(test)]
#[path = "loader_tests/session.rs"]
mod session_tests;
#[cfg(test)]
#[path = "loader_tests/settings.rs"]
mod settings_tests;
