//! Resolve the PaneFlow runtime directory with a macOS-aware fallback chain,
//! and enforce the `sockaddr_un.sun_path` length limit (macOS: 104 bytes).
//!
//! Public helpers:
//! - `ipc::start_server` consumes `socket_path()` for the main JSON-RPC socket,
//! - `terminal::paneflow_socket_path` propagates the same path as the
//!   `PANEFLOW_SOCKET_PATH` env var passed into each PTY child shell.
//!
//! Keeping the chain in one place prevents the two sites from drifting -
//! a difference in one branch would silently break IPC on macOS
//! without any visible error.
//!
//! US-013 removed the former third consumer (the AI-hook wrapper-scripts
//! bin-dir helper) along with its call sites - the extraction targets
//! never existed in the embed set, so the helper and its PATH-injection
//! caller were dead code.
//!
//! `PANEFLOW_SOCKET_PATH` overrides the computed path so isolated debug/test
//! instances and panes launched from a running instance agree on the exact
//! IPC endpoint. Without this, clients can point at one socket while the
//! server keeps binding the default one.
//!
//! LOCKSTEP: `crates/paneflow-ipc-client/src/lib.rs` (`resolve_socket_path`)
//! re-implements this exact chain without the `dirs` dependency (that crate
//! deliberately keeps its tree minimal), and the two must be kept in lockstep
//! (#217). Any change here to the `PANEFLOW_SOCKET_PATH` handling (read as
//! `OsString`, absolute-only), the `$TMPDIR` acceptance (non-UTF-8, empty, or
//! not an existing directory reads as unset), the fallback chain, or the
//! `sun_path` ceiling must be
//! mirrored there, or the CLI/MCP client dials an endpoint the server never
//! bound.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// macOS `sockaddr_un.sun_path` is `[c_char; 104]`.
#[cfg(unix)]
pub(crate) const MAX_SOCKET_PATH_BYTES: usize = 104;

/// Application directory namespace. Switches to `paneflow-dev` in debug
/// builds so `cargo run`-launched instances stop colliding with the
/// release-installed `/usr/bin/paneflow` on the same machine: distinct
/// data dir (no shared `threads.db` / `session.json` lock), distinct
/// config dir, distinct cache dir, distinct shell helper dir, distinct
/// IPC socket dir. The user can run an installed Paneflow and a
/// from-source build side by side and each holds its own state. Same
/// rule applies cross-crate -- see `paneflow_config::APP_SUBDIR` and
/// `paneflow_threads::APP_SUBDIR` which mirror this const so per-build
/// isolation reaches every persistence surface.
pub const APP_SUBDIR: &str = if cfg!(debug_assertions) {
    "paneflow-dev"
} else {
    "paneflow"
};

#[cfg(unix)]
const PANEFLOW_SUBDIR: &str = APP_SUBDIR;
/// Socket filename, namespaced per build profile so a `cargo run` debug
/// instance and an installed release instance can coexist on the same host
/// without one silently stealing the other's socket. Each instance binds
/// its own listener and the AI-hook wrapper scripts (which read
/// `PANEFLOW_SOCKET_PATH` from the PTY env) route to the right one.
#[cfg(unix)]
const SOCKET_FILE: &str = if cfg!(debug_assertions) {
    "paneflow-dev.sock"
} else {
    "paneflow.sock"
};

/// IPC endpoint plus ownership metadata for the server-side binder.
///
/// `PANEFLOW_SOCKET_PATH` is useful for tests and intentionally isolated debug
/// instances, but the path belongs to the caller, not Paneflow. The IPC server
/// must therefore not create/chmod its parent directory or reclaim a non-socket
/// file there. The default path is Paneflow-owned and may be prepared by the
/// server before bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IpcSocketPath {
    path: PathBuf,
    owned_parent: bool,
}

impl IpcSocketPath {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    pub(crate) fn owned_parent(&self) -> bool {
        self.owned_parent
    }
}

/// Resolve the PaneFlow runtime directory. Fallback chain (macOS):
/// 1. `$TMPDIR` - populated by launchd and shells (usually `/var/folders/xx/.../T/`),
///    used only when it names an existing directory.
/// 2. `dirs::cache_dir().join("run")` - last-resort fallback (`~/Library/Caches/run`).
///
/// `$XDG_RUNTIME_DIR` and `dirs::runtime_dir()` are **not** consulted. Finder
/// and Dock launches inherit only PATH from the login shell, so a GUI process
/// typically has no XDG while a terminal CLI that sourced a profile does.
/// Preferring XDG would bind the GUI under `$TMPDIR` and send `paneflow ls`
/// to a different socket. `PANEFLOW_SOCKET_PATH` is the explicit override.
///
/// Returns `None` only if every layer fails (neither TMPDIR nor a cache dir).
/// Callers should `log::warn!` and disable IPC rather than panic.
///
/// `env` is the process-env seam: production passes `std::env::var_os`,
/// tests pass a closure over a fixed table so they never mutate the
/// process-global `$TMPDIR` that every `tempfile` caller in the test binary
/// reads concurrently (#66).
#[cfg(unix)]
fn runtime_dir_from(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("TMPDIR")
        // Same acceptance as the former `std::env::var(..).ok()`: a
        // non-UTF-8 value is treated as unset and falls through.
        .and_then(|raw| raw.into_string().ok())
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        // A stale value (a `/var/folders` slice from a previous boot, or a
        // path that was never created) must not pin the socket under a
        // directory that does not exist; treat it as unset so the cache-dir
        // fallback below still gives the server somewhere to bind (#289).
        // Existence-only on purpose: a write probe would leave droppings in
        // a directory the resolver does not own, and `prepare_socket_parent`
        // already reports an unwritable parent at bind time.
        .filter(|p| p.is_dir())
        .or_else(|| dirs::cache_dir().map(|d| d.join("run")))
}

/// Full path to the IPC socket.
///
/// `<runtime_dir>/paneflow/paneflow.sock`, or `None` if the runtime
/// dir cannot be resolved or the composed path would exceed the `sun_path`
/// limit. A `log::warn!` is emitted in the over-length case so the user
/// can see why IPC is disabled.
#[cfg(unix)]
pub(crate) fn socket_path_spec() -> Option<IpcSocketPath> {
    socket_path_spec_from(&|key| std::env::var_os(key))
}

/// [`socket_path_spec`] over an explicit env lookup (see [`runtime_dir_from`]).
#[cfg(unix)]
fn socket_path_spec_from(env: &impl Fn(&str) -> Option<OsString>) -> Option<IpcSocketPath> {
    if let Some(path) = socket_path_from_env(env("PANEFLOW_SOCKET_PATH")) {
        return check_sun_path_fits(&path).then_some(IpcSocketPath {
            path,
            owned_parent: false,
        });
    }
    let path = runtime_dir_from(env)?
        .join(PANEFLOW_SUBDIR)
        .join(SOCKET_FILE);
    check_sun_path_fits(&path).then_some(IpcSocketPath {
        path,
        owned_parent: true,
    })
}

pub(crate) fn socket_path() -> Option<PathBuf> {
    socket_path_spec().map(|spec| spec.path)
}

pub(crate) fn shell_integration_dir() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("shell"))
}

fn socket_path_from_env(raw: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(raw?);
    path.is_absolute().then_some(path)
}

/// Prepend the common per-user `bin/` directories to the process `PATH`
/// so PATH-based lookups see binaries installed under the user's home - `~/.bun/bin`,
/// `~/.cargo/bin`, `~/.local/bin`, plus `/opt/homebrew/bin` on macOS.
///
/// Why: when Paneflow is launched from Finder or the Dock, it inherits
/// launchd's PATH, which does NOT include `~/.bun/bin`. Agent launch and CLI helper
/// paths then fail to find user-installed tools even though they are available
/// in a normal terminal. Zed, VS Code, and most GUI dev tools all patch their
/// own PATH at startup for the same reason.
///
/// Dirs are prepended (not appended), so user installs always win over any
/// system-shadowed name. Existing entries in PATH are skipped - no
/// duplicates. Idempotent: safe to call multiple times.
///
/// Safety: mutates a process-global env var. Must be called from `main`
/// before any other thread is spawned (i.e. before GPUI initialises),
/// otherwise concurrent readers may observe a torn PATH. Rust 2024 marks
/// `set_var` as `unsafe` for this exact reason.
pub fn augment_path_for_gui_launch() {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".bun").join("bin"));
        candidates.push(home.join(".cargo").join("bin"));
        candidates.push(home.join(".local").join("bin"));
    }

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/opt/homebrew/bin"));
        candidates.push(PathBuf::from("/usr/local/bin"));
    }

    let current = std::env::var_os("PATH").unwrap_or_default();
    let existing: Vec<PathBuf> = std::env::split_paths(&current).collect();

    let mut to_prepend: Vec<PathBuf> = Vec::new();
    for cand in candidates {
        if !cand.is_dir() {
            continue;
        }
        if existing.iter().any(|p| p == &cand) {
            continue;
        }
        if to_prepend.contains(&cand) {
            continue;
        }
        to_prepend.push(cand);
    }

    if to_prepend.is_empty() {
        return;
    }

    let mut merged: Vec<PathBuf> = to_prepend.clone();
    merged.extend(existing);

    match std::env::join_paths(&merged) {
        Ok(joined) => {
            log::info!(
                "paneflow: augmented PATH with user bin dirs: {}",
                to_prepend
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            // SAFETY: called from `main` before GPUI / IPC / PTY threads start,
            // so no other thread is reading PATH concurrently.
            unsafe { std::env::set_var("PATH", joined) };
        }
        Err(e) => {
            log::warn!("paneflow: failed to join augmented PATH ({e}); leaving PATH unchanged");
        }
    }
}

/// Resolve the PaneFlow per-user data directory.
///
/// macOS: `~/Library/Application Support/paneflow` (`paneflow-dev` in debug).
///
/// The directory is created if it does not already exist. Returns `None` if
/// either the platform helper returns `None` (broken environment) or the
/// `create_dir_all` call fails (read-only FS, permission denied, etc.).
pub fn data_dir() -> Option<PathBuf> {
    let dir = dirs::data_local_dir()?.join(APP_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::debug!(
            "paneflow: data_dir {} is unwritable ({e}); callers will use ephemeral state",
            dir.display()
        );
        return None;
    }
    Some(dir)
}

/// Stable, **non-versioned** absolute path of the embedded `paneflow-mcp`
/// bridge binary (EP-001 US-003).
///
/// Unlike the shim / ai-hook helpers - which live under
/// `cache_dir()/paneflow/bin/<VERSION>/` and are re-resolved by Paneflow on
/// every launch - the bridge path is written into **external, persistent
/// agent configs** (`~/.claude.json`, `~/.codex/config.toml`, ...) by
/// `paneflow mcp install`. A version-pinned path would go stale on the next
/// Paneflow update, and `cache_dir()` can be purged by the OS. So the bridge
/// lives under `data_dir()` (durable, non-versioned):
///
/// macOS: `~/Library/Application Support/paneflow/bin/paneflow-mcp`
///
/// Returns `None` when `data_dir()` is unresolvable or unwritable. Callers
/// (`ai_hooks::extract::ensure_bridge_extracted`, and later `paneflow mcp
/// install`) must treat `None` as "refuse to register a config pointing at a
/// path that does not exist" rather than fabricating a path.
///
/// This only **computes** the path; it does not extract. The byte
/// materialization + SHA-compared atomic rewrite is
/// `ai_hooks::extract::ensure_bridge_extracted`.
pub fn bridge_binary_path() -> Option<PathBuf> {
    Some(data_dir()?.join("bin").join("paneflow-mcp"))
}

/// Stable, non-versioned path of the `paneflow-ai-hook` callback binary
/// (EP-004 US-016, prd-cli-agent-orchestration). Same rationale as
/// [`bridge_binary_path`]: `paneflow hooks setup` writes this path into
/// **external, persistent agent configs** (`~/.claude/settings.json`, …), so it
/// must survive Paneflow updates - unlike the version-pinned shim/ai-hook copy
/// under `cache_dir()/paneflow/bin/<VERSION>/` that the shim itself resolves at
/// launch. Lives alongside the bridge under `data_dir()/paneflow/bin/`:
///
/// macOS: `~/Library/Application Support/paneflow/bin/paneflow-ai-hook`
///
/// Returns `None` when `data_dir()` is unresolvable. Computes the path only;
/// the byte materialization is `ai_hooks::extract::ensure_ai_hook_extracted`.
pub fn ai_hook_binary_path() -> Option<PathBuf> {
    Some(data_dir()?.join("bin").join("paneflow-ai-hook"))
}

/// Opt-in for writing a debug-build MCP-bridge / ai-hook path into durable
/// agent configs (`~/.claude.json`, `$CLAUDE_CONFIG_DIR/settings.json`, …).
/// Value-gated (`=1` only), matching `PANEFLOW_ALLOW_MULTIPLE` (#53): unset,
/// empty, `0`, and `false` still refuse.
pub(crate) const ALLOW_DEBUG_MCP_INSTALL_ENV: &str = "PANEFLOW_ALLOW_DEBUG_MCP_INSTALL";

/// Whether this process may register the extracted debug-namespaced
/// `paneflow-mcp` / `paneflow-ai-hook` path with Claude/Codex/Gemini/opencode.
///
/// Release builds always allow. Debug builds extract under `paneflow-dev/`
/// and must not persist that path unless `PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1`.
/// Does not bake `PANEFLOW_SOCKET_PATH` into the agent entry (D5).
pub(crate) fn durable_agent_install_allowed() -> bool {
    durable_agent_install_allowed_from(
        cfg!(debug_assertions),
        std::env::var(ALLOW_DEBUG_MCP_INSTALL_ENV).ok().as_deref(),
    )
}

/// Pure truth table so tests do not mutate process env.
pub(crate) fn durable_agent_install_allowed_from(
    debug_build: bool,
    override_value: Option<&str>,
) -> bool {
    !debug_build || matches!(override_value, Some("1"))
}

/// Body of the debug-install refusal. CLI callers prefix `paneflow <cmd>: `.
pub(crate) fn durable_agent_install_refusal_message() -> String {
    format!(
        "refusing to write a debug-build path (`paneflow-dev`) into durable agent configs. \
         Use a release build or the installed .app, or set {ALLOW_DEBUG_MCP_INSTALL_ENV}=1 to override."
    )
}

#[cfg(unix)]
fn check_sun_path_fits(path: &std::path::Path) -> bool {
    let bytes = path.as_os_str().len();
    // `MAX_SOCKET_PATH_BYTES` is `sizeof(sun_path)`, and `bind()` needs room for
    // the trailing NUL inside that array - so a path of *exactly* the array size
    // does not fit. Reject `>=`, not `>` (the usable maximum is the array size
    // minus one).
    if bytes >= MAX_SOCKET_PATH_BYTES {
        log::warn!(
            "paneflow: computed IPC socket path does not fit sun_path ({} >= {} bytes, no room for the NUL terminator): {} - IPC will be disabled. Set $PANEFLOW_SOCKET_PATH to a shorter absolute path, or shorten $TMPDIR to enable it.",
            bytes,
            MAX_SOCKET_PATH_BYTES,
            path.display()
        );
        false
    } else {
        true
    }
}

#[cfg(test)]
mod debug_install_tests {
    use super::*;

    #[test]
    fn release_builds_always_allow_durable_install() {
        assert!(durable_agent_install_allowed_from(false, None));
        assert!(durable_agent_install_allowed_from(false, Some("")));
        assert!(durable_agent_install_allowed_from(false, Some("0")));
        assert!(durable_agent_install_allowed_from(false, Some("1")));
    }

    #[test]
    fn debug_builds_require_override_eq_1() {
        assert!(!durable_agent_install_allowed_from(true, None));
        assert!(!durable_agent_install_allowed_from(true, Some("")));
        assert!(!durable_agent_install_allowed_from(true, Some("0")));
        assert!(!durable_agent_install_allowed_from(true, Some("false")));
        assert!(!durable_agent_install_allowed_from(true, Some("true")));
        assert!(durable_agent_install_allowed_from(true, Some("1")));
    }

    #[test]
    fn refusal_names_release_app_and_override() {
        let msg = durable_agent_install_refusal_message();
        assert!(msg.contains("paneflow-dev"), "{msg}");
        assert!(msg.contains("release build"), "{msg}");
        assert!(msg.contains(".app"), "{msg}");
        assert!(msg.contains("PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1"), "{msg}");
        assert!(
            !msg.contains("PANEFLOW_SOCKET_PATH"),
            "must not tell the operator to bake a debug socket into agent env: {msg}"
        );
    }
}

#[cfg(test)]
mod socket_env_tests {
    use super::*;

    #[test]
    fn socket_path_env_helper_requires_absolute_path() {
        let absolute = "/tmp/paneflow-test.sock";
        assert_eq!(
            socket_path_from_env(Some(OsString::from(absolute))),
            Some(PathBuf::from(absolute))
        );
        assert_eq!(
            socket_path_from_env(Some(OsString::from("relative-paneflow.sock"))),
            None
        );
        assert_eq!(socket_path_from_env(None), None);
    }
}

// US-009 - these tests assert Unix socket path composition and sun_path
// length limits, so they are structurally Unix-only.
//
// Every test here drives the resolver through the `_from` seam with a fixed
// lookup table. None of them touches `std::env`, so they cannot race the
// `tempfile` readers of `$TMPDIR` that run concurrently in this binary (#66)
// and they need no lock.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Env lookup over a fixed table; anything not listed reads as unset.
    fn fake_env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn socket_path_with(vars: &[(&str, &str)]) -> Option<PathBuf> {
        socket_path_spec_from(&fake_env(vars)).map(|spec| spec.path)
    }

    #[test]
    fn paneflow_socket_path_env_wins_when_absolute() {
        let env = [
            ("PANEFLOW_SOCKET_PATH", "/tmp/paneflow-isolated.sock"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ];
        assert_eq!(
            socket_path_with(&env),
            Some(PathBuf::from("/tmp/paneflow-isolated.sock"))
        );
        let spec = socket_path_spec_from(&fake_env(&env)).expect("env socket path resolves");
        assert_eq!(spec.path(), Path::new("/tmp/paneflow-isolated.sock"));
        assert!(
            !spec.owned_parent(),
            "env override parent must not be treated as Paneflow-owned"
        );
    }

    #[test]
    fn relative_socket_path_override_falls_through_to_tmpdir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp_str = tmp.path().to_str().expect("utf-8 tempdir");
        let env = [
            ("PANEFLOW_SOCKET_PATH", "relative-paneflow.sock"),
            ("TMPDIR", tmp_str),
        ];
        assert_eq!(
            socket_path_with(&env),
            Some(tmp.path().join(APP_SUBDIR).join(SOCKET_FILE)),
            "a non-absolute override is ignored, not honoured"
        );
    }

    #[test]
    fn xdg_runtime_dir_is_ignored_when_tmpdir_is_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp_str = tmp.path().to_str().expect("utf-8 tempdir");
        let env = [("XDG_RUNTIME_DIR", "/run/user/1000"), ("TMPDIR", tmp_str)];
        let p = socket_path_with(&env).expect("runtime dir must resolve");
        assert_eq!(
            p,
            tmp.path().join(APP_SUBDIR).join(SOCKET_FILE),
            "GUI/CLI must agree on $TMPDIR even when $XDG_RUNTIME_DIR is set in the CLI"
        );
        assert!(
            !p.starts_with("/run/user/1000"),
            "must not compose the socket under $XDG_RUNTIME_DIR; got {}",
            p.display()
        );
        assert!(
            socket_path_spec_from(&fake_env(&env))
                .expect("socket spec")
                .owned_parent(),
            "default runtime-dir socket is Paneflow-owned"
        );
    }

    #[test]
    fn xdg_runtime_dir_is_not_used_when_tmpdir_is_unset() {
        let env = [("XDG_RUNTIME_DIR", "/run/user/1000")];
        let p = socket_path_with(&env).expect("cache dir must resolve");
        assert!(
            !p.starts_with("/run/user/1000"),
            "must not fall back to $XDG_RUNTIME_DIR; got {}",
            p.display()
        );
        assert!(
            p.ends_with(format!("Library/Caches/run/{APP_SUBDIR}/{SOCKET_FILE}")),
            "last resort is ~/Library/Caches/run; got {}",
            p.display()
        );
    }

    #[test]
    fn empty_tmpdir_falls_back_to_cache_dir() {
        let env = [("TMPDIR", "")];
        let p = socket_path_with(&env).expect("cache dir must resolve");
        assert!(
            p.ends_with(format!("Library/Caches/run/{APP_SUBDIR}/{SOCKET_FILE}")),
            "an empty $TMPDIR is treated as unset; got {}",
            p.display()
        );
    }

    #[test]
    fn tmpdir_used_when_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp_str = tmp.path().to_str().expect("utf-8 tempdir");
        let env = [("TMPDIR", tmp_str)];
        let p = socket_path_with(&env).expect("TMPDIR must resolve");
        assert_eq!(p, tmp.path().join(APP_SUBDIR).join(SOCKET_FILE));
    }

    #[test]
    fn missing_tmpdir_falls_back_to_cache_dir() {
        // A stale $TMPDIR (left over from a previous boot, or a
        // `/var/folders` slice that was purged) names a path that no longer
        // exists. It must read as unset so the server binds under the
        // cache-dir fallback instead of disabling IPC (#289).
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("gone");
        assert!(!missing.exists(), "fixture must not exist");
        let missing_str = missing.to_str().expect("utf-8 tempdir");
        let env = [("TMPDIR", missing_str)];
        let p = socket_path_with(&env).expect("cache dir must resolve");
        assert!(
            p.ends_with(format!("Library/Caches/run/{APP_SUBDIR}/{SOCKET_FILE}")),
            "a $TMPDIR that is not an existing directory is treated as unset; got {}",
            p.display()
        );
        assert!(
            !p.starts_with(&missing),
            "must not compose the socket under a missing $TMPDIR; got {}",
            p.display()
        );
    }

    #[test]
    fn tmpdir_that_is_a_file_falls_back_to_cache_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"").expect("fixture file");
        let file_str = file.to_str().expect("utf-8 tempdir");
        let env = [("TMPDIR", file_str)];
        let p = socket_path_with(&env).expect("cache dir must resolve");
        assert!(
            p.ends_with(format!("Library/Caches/run/{APP_SUBDIR}/{SOCKET_FILE}")),
            "a $TMPDIR that names a file is treated as unset; got {}",
            p.display()
        );
    }

    #[test]
    fn overlong_path_returns_none() {
        // A TMPDIR of at least 120 bytes → joined path blows past 104. It
        // has to exist on disk, or the resolver reads it as unset and falls
        // through to the cache dir instead of hitting the ceiling.
        let tmp = tempfile::tempdir().expect("tempdir");
        let long_dir = tmp.path().join("x".repeat(120));
        std::fs::create_dir(&long_dir).expect("long fixture dir");
        let long = long_dir.to_str().expect("utf-8 tempdir");
        assert!(long.len() >= 120);
        let env = [("TMPDIR", long)];
        assert!(
            socket_path_with(&env).is_none(),
            "AC6: over-long sun_path must return None rather than a bind-time error"
        );
    }

    #[test]
    fn sun_path_ceiling_is_exclusive() {
        // `bind()` needs the trailing NUL inside `sun_path`, so a path of
        // exactly `MAX_SOCKET_PATH_BYTES` does not fit; one byte shorter does.
        let at_limit = "/".to_string() + &"x".repeat(MAX_SOCKET_PATH_BYTES - 1);
        assert_eq!(at_limit.len(), MAX_SOCKET_PATH_BYTES);
        assert!(
            socket_path_with(&[("PANEFLOW_SOCKET_PATH", at_limit.as_str())]).is_none(),
            "{MAX_SOCKET_PATH_BYTES} bytes leaves no room for the NUL terminator"
        );

        let under_limit = "/".to_string() + &"x".repeat(MAX_SOCKET_PATH_BYTES - 2);
        assert_eq!(under_limit.len(), MAX_SOCKET_PATH_BYTES - 1);
        assert_eq!(
            socket_path_with(&[("PANEFLOW_SOCKET_PATH", under_limit.as_str())]),
            Some(PathBuf::from(&under_limit))
        );
    }
}
