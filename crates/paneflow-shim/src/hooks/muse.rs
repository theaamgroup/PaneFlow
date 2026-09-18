//! Muse Code speaks the Claude Code hook format but runs its hooks with a
//! cleared environment, so the shim cannot rely on the inherited
//! `PANEFLOW_*` variables: it publishes a Paneflow-owned managed hook file
//! and merges `managed_hooks_path` plus `managed_hooks_env_vars` into
//! `settings.json`. Muse Code 1.2 skips `Stop` on the Meta provider, so
//! `PostLLMCall` is also dispatched as `Stop` and `paneflow-ai-hook` drops
//! the calls that go on to schedule tools.
use super::owned_files::{
    cleanup_accepted_owned_file, install_accepted_owned_file, is_own_or_sibling_rendering,
    sweep_accepted_owned_file,
};
use super::{
    config_dir_is_symlink, home_unavailable, hook_config_error, install_hook_config_file,
    paneflow_ipc_reachable, plain_hook_handler, with_last_lease, with_orphan_lease, HookInstall,
    HookInstallResult, HookInstallSkip, HookLease, InvalidJsonPolicy,
};
use paneflow_agent_config::claude_hooks::reconcile_matcher_hooks_replacing_invalid_container;
use paneflow_agent_config::{home_dir, read_optional_text, with_config_lock, write_json_atomic};
use serde_json::{json, Value};
use std::env;
use std::path::{Path, PathBuf};

pub(crate) const MUSE_HOOK_EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "UserPromptSubmit"),
    ("PreToolUse", "PreToolUse"),
    ("PostToolUse", "PostToolUse"),
    ("PermissionRequest", "PermissionRequest"),
    ("PostLLMCall", "Stop"),
    ("Stop", "Stop"),
];
pub(crate) const MUSE_HOOKS_BASENAME: &str = "paneflow-hooks.json";
pub(crate) const MUSE_SETTINGS_BASENAME: &str = "settings.json";
pub(crate) const MUSE_HOOK_ENV_VARS: &[&str] = &[
    "PANEFLOW_WORKSPACE_ID",
    "PANEFLOW_SURFACE_ID",
    "PANEFLOW_SOCKET_PATH",
    "PANEFLOW_AI_TOOL",
    "PANEFLOW_AI_PID",
];
/// The env baseline: the `PANEFLOW_*` names a user had already put in
/// `managed_hooks_env_vars` before PaneFlow first took the settings file
/// (no managed path was set yet). It proves PaneFlow added the managed
/// keys, so cleanup strips them only when it exists and keeps exactly
/// those names: a file the user pointed at the reserved path themselves is
/// never touched, and a pre-existing forward survives even when the last
/// session to exit is not the one that recorded it.
///
/// It is stored in the lease's own durable `.created` marker
/// (`ConfigLease::mark_created_with`) for the resource path below, under
/// PaneFlow's configuration directory. Nothing is ever written at that
/// path inside the Muse config directory, so there is no sidecar for a
/// user, an editor, or a sync tool to replace, and nothing for PaneFlow to
/// read from or delete in user space.
pub(crate) const MUSE_ENV_BASELINE_EXTENSION: &str = "paneflow-env-baseline";
const SCHEMA_VERSION_KEY: &str = "schema_version";
const MANAGED_HOOKS_PATH_KEY: &str = "managed_hooks_path";
const MANAGED_HOOKS_ENV_VARS_KEY: &str = "managed_hooks_env_vars";

pub(crate) struct MuseHookConfigGuard {
    settings_path: PathBuf,
    config_dir: PathBuf,
    created_settings: bool,
    created_dir: bool,
    settings_lease: HookLease,
    hooks_path: PathBuf,
    hooks_source: String,
    hooks_lease: HookLease,
}

impl MuseHookConfigGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let directory = muse_config_dir().ok_or_else(home_unavailable)?;
        if !paneflow_ipc_reachable() {
            sweep_orphan(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        let hooks_path = directory.join(MUSE_HOOKS_BASENAME);
        let baseline_path = env_baseline_path(&directory.join(MUSE_SETTINGS_BASENAME));
        // The baseline is recorded inside the settings merge, which runs
        // under the one global config lock: it is durable before the
        // managed keys are, so a kill in between leaves a recorded
        // baseline (replaced by the next first take) and never a settings
        // file that points at the reserved path with no proof PaneFlow put
        // it there. Nothing in here takes the config lock again (the lease
        // lock is a separate file).
        let mut baseline_recorded = false;
        let installed = install_hook_config_file(
            directory,
            MUSE_SETTINGS_BASENAME,
            "Muse Code",
            |root| {
                if let Some(pre_existing) = merge_muse_settings(root, &hooks_path)? {
                    record_env_baseline(&baseline_path, &pre_existing)?;
                    baseline_recorded = true;
                }
                Ok(())
            },
            InvalidJsonPolicy::Refuse,
        );
        let mut installed = match installed {
            Ok(installed) => installed,
            Err(error) => {
                // The settings write failed after the baseline was recorded.
                if baseline_recorded {
                    consume_env_baseline(&baseline_path);
                }
                return Err(error);
            }
        };
        // The hook file embeds this instance's ai-hook path; a sibling
        // PaneFlow instance (a different `PANEFLOW_BIN_DIR`) renders
        // different bytes for the same hooks, and that file serves this
        // session too. Any other content is a user's file and is refused.
        let hooks_lease = hooks_source().and_then(|hooks_source| {
            let mut lease = HookLease::acquire(&hooks_path)?;
            with_config_lock(&hooks_path, || {
                install_accepted_owned_file(&hooks_path, &hooks_source, &mut lease, &|existing| {
                    is_own_or_sibling_rendering(existing, &hooks_source)
                })
            })?;
            Ok((hooks_source, lease))
        });
        let (hooks_source, hooks_lease) = match hooks_lease {
            Ok(installed) => installed,
            Err(error) => {
                release_settings(
                    &installed.path,
                    &hooks_path,
                    &mut installed.lease,
                    installed.created_file,
                );
                if installed.created_directory {
                    let _ = std::fs::remove_dir(directory);
                }
                return Err(error);
            }
        };
        Ok(Self {
            settings_path: installed.path,
            config_dir: directory.to_path_buf(),
            created_settings: installed.created_file,
            created_dir: installed.created_directory,
            settings_lease: installed.lease,
            hooks_path,
            hooks_source,
            hooks_lease,
        })
    }

    #[cfg(test)]
    pub(crate) fn hooks_path(&self) -> &Path {
        &self.hooks_path
    }
}

impl Drop for MuseHookConfigGuard {
    fn drop(&mut self) {
        let hooks_source = std::mem::take(&mut self.hooks_source);
        cleanup_accepted_owned_file(&self.hooks_path, &mut self.hooks_lease, &|existing| {
            is_own_or_sibling_rendering(existing, &hooks_source)
        });
        release_settings(
            &self.settings_path,
            &self.hooks_path,
            &mut self.settings_lease,
            self.created_settings,
        );
        if self.created_dir {
            let _ = std::fs::remove_dir(&self.config_dir);
        }
    }
}

pub(crate) fn hooks_source() -> std::io::Result<String> {
    let mut root = json!({});
    merge_muse_hooks(&mut root)?;
    Ok(serde_json::to_string_pretty(&root).map_err(std::io::Error::other)? + "\n")
}

fn merge_muse_hooks(root: &mut Value) -> std::io::Result<()> {
    let events: Vec<&str> = MUSE_HOOK_EVENTS
        .iter()
        .map(|(foreign, _)| *foreign)
        .collect();
    reconcile_matcher_hooks_replacing_invalid_container(root, &events, |foreign| {
        let canonical = MUSE_HOOK_EVENTS
            .iter()
            .find_map(|(candidate, canonical)| (*candidate == foreign).then_some(*canonical))
            .unwrap_or(foreign);
        json!({ "hooks": [plain_hook_handler(canonical)] })
    })
    .map(|_| ())
    .map_err(hook_config_error)
}

pub(crate) fn muse_config_dir() -> Option<PathBuf> {
    match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value).join("muse")),
        _ => home_dir().map(|home| home.join(".config").join("muse")),
    }
}

/// Merge PaneFlow's managed keys. When no managed path was set (PaneFlow is
/// taking the file for the first time) returns the `PANEFLOW_*` names the
/// user had already listed, possibly none, so the caller records the
/// baseline; `None` means the managed keys were already PaneFlow's.
fn merge_muse_settings(
    root: &mut Value,
    hooks_path: &Path,
) -> std::io::Result<Option<Vec<String>>> {
    let invalid = |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let Some(object) = root.as_object_mut() else {
        return Err(invalid("Muse Code settings root is not an object"));
    };
    let hooks_path = hooks_path
        .to_str()
        .ok_or_else(|| invalid("Muse Code hook path is not valid UTF-8"))?;
    let first_take = match object.get(MANAGED_HOOKS_PATH_KEY) {
        None | Some(Value::Null) => true,
        Some(Value::String(existing)) if existing == hooks_path => false,
        Some(_) => {
            return Err(invalid(
                "user Muse Code settings already point managed_hooks_path elsewhere",
            ))
        }
    };
    if object
        .get(MANAGED_HOOKS_ENV_VARS_KEY)
        .is_some_and(|value| !value.is_null() && !value.is_array())
    {
        return Err(invalid(
            "user Muse Code settings hold a non-array managed_hooks_env_vars",
        ));
    }
    // Only a settings file PaneFlow is creating gets a `schema_version`: a
    // pre-existing file that lacked one keeps lacking it, so there is
    // nothing to restore at cleanup.
    if object.is_empty() {
        object.insert(SCHEMA_VERSION_KEY.to_owned(), json!(1));
    }
    object.insert(MANAGED_HOOKS_PATH_KEY.to_owned(), json!(hooks_path));
    let env_vars = object
        .entry(MANAGED_HOOKS_ENV_VARS_KEY)
        .or_insert_with(|| json!([]));
    if env_vars.is_null() {
        *env_vars = json!([]);
    }
    if !first_take {
        // The managed path is already ours: either a PaneFlow session added
        // the names, or the user pointed the reserved path at their own
        // hook file and their env list is theirs to keep.
        return Ok(None);
    }
    let mut pre_existing = Vec::new();
    if let Some(entries) = env_vars.as_array_mut() {
        for name in MUSE_HOOK_ENV_VARS {
            if entries.iter().any(|entry| entry.as_str() == Some(name)) {
                pre_existing.push((*name).to_owned());
            } else {
                entries.push(json!(name));
            }
        }
    }
    Ok(Some(pre_existing))
}

/// The lease resource the baseline is keyed on. Nothing is created at
/// this path; it only names the marker.
pub(crate) fn env_baseline_path(settings_path: &Path) -> PathBuf {
    settings_path.with_extension(MUSE_ENV_BASELINE_EXTENSION)
}

/// Record the baseline in the lease marker. Runs under the settings config
/// lock (never takes it again), so no other session is between the record
/// and the settings write. A stale record a crashed first take left behind
/// (no live holder) is cleared first; the lease is not held past this
/// call, since the marker is the durable state and the session that
/// restores the settings needs to be able to take it.
fn record_env_baseline(path: &Path, names: &[String]) -> std::io::Result<()> {
    let mut lease = HookLease::acquire(path)?;
    if let Some(mut last) = lease.try_take_last()? {
        last.take_created()?;
        drop(last);
        lease = HookLease::acquire(path)?;
    }
    lease.mark_created_with(&json!(names).to_string())
}

/// The recorded baseline, `None` when there is none: PaneFlow never took
/// the settings file, so the managed keys are not its to strip. Read
/// before any last-lease step, which clears the marker.
fn read_env_baseline(path: &Path) -> Option<Vec<String>> {
    let lease = HookLease::acquire(path).ok()?;
    let note = lease.created_note()?;
    serde_json::from_str::<Vec<String>>(&note).ok()
}

/// Clear the record once a restore has reached disk (or a failed install
/// is being undone). Only the last holder clears it; no session holds the
/// lease past recording, so that is whoever gets here.
fn consume_env_baseline(path: &Path) {
    let _ = with_orphan_lease(path, path, |_| Ok(()));
}

#[cfg(test)]
pub(crate) fn recorded_env_baseline_for_test(settings_path: &Path) -> Option<Vec<String>> {
    read_env_baseline(&env_baseline_path(settings_path))
}

#[cfg(test)]
pub(crate) fn record_env_baseline_for_test(settings_path: &Path, names: &[String]) {
    record_env_baseline(&env_baseline_path(settings_path), names).unwrap();
}

/// Last-session release of the settings file: strip the managed keys
/// (keeping the baseline's names), then clear the baseline only when this
/// session was the last one and the restore reached disk, so a failed
/// write leaves the next orphan sweep the same names to keep.
fn release_settings(settings_path: &Path, hooks_path: &Path, lease: &mut HookLease, owned: bool) {
    let baseline_path = env_baseline_path(settings_path);
    let keep = read_env_baseline(&baseline_path);
    let restored = with_last_lease(settings_path, lease, |lease_created| {
        restore_settings(
            settings_path,
            hooks_path,
            owned || lease_created,
            keep.as_deref(),
        )
    });
    if matches!(restored, Ok(Some(()))) && keep.is_some() {
        consume_env_baseline(&baseline_path);
    }
}

/// Strip PaneFlow's managed keys, but only when `managed_hooks_path` is
/// exactly `hooks_path`: a foreign file that happens to share the basename
/// (`/etc/muse/paneflow-hooks.json`) is someone else's and stays intact,
/// `PANEFLOW_*` env entries included. Names in `keep` were the user's own
/// before PaneFlow took the file and stay in place.
pub(crate) fn remove_muse_settings(root: &mut Value, hooks_path: &Path, keep: &[String]) {
    let Some(object) = root.as_object_mut() else {
        return;
    };
    let owns_managed_path = object
        .get(MANAGED_HOOKS_PATH_KEY)
        .and_then(Value::as_str)
        .is_some_and(|path| Some(path) == hooks_path.to_str());
    if !owns_managed_path {
        return;
    }
    object.remove(MANAGED_HOOKS_PATH_KEY);
    if let Some(entries) = object
        .get_mut(MANAGED_HOOKS_ENV_VARS_KEY)
        .and_then(Value::as_array_mut)
    {
        entries.retain(|entry| {
            !entry.as_str().is_some_and(|name| {
                MUSE_HOOK_ENV_VARS.contains(&name) && !keep.iter().any(|kept| kept == name)
            })
        });
    }
    if object
        .get(MANAGED_HOOKS_ENV_VARS_KEY)
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        object.remove(MANAGED_HOOKS_ENV_VARS_KEY);
    }
}

fn settings_is_bare(root: &Value) -> bool {
    root.as_object().is_some_and(|object| {
        object.is_empty()
            || (object.len() == 1 && object.get(SCHEMA_VERSION_KEY) == Some(&json!(1)))
    })
}

/// Strip the managed keys, keeping `keep`. `None` means PaneFlow never
/// took this file (no baseline): the managed keys were the user's own, so
/// they stay.
fn restore_settings(
    path: &Path,
    hooks_path: &Path,
    owned: bool,
    keep: Option<&[String]>,
) -> std::io::Result<()> {
    let Some(keep) = keep else {
        return Ok(());
    };
    let Some(content) = read_optional_text(path)? else {
        return Ok(());
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&content) else {
        return Ok(());
    };
    let before = root.clone();
    remove_muse_settings(&mut root, hooks_path, keep);
    if root == before {
        return Ok(());
    }
    if owned && settings_is_bare(&root) {
        std::fs::remove_file(path)
    } else {
        write_json_atomic(path, &root)
    }
}

#[cfg(test)]
pub(crate) fn sweep_orphan_for_test(directory: &Path) {
    sweep_orphan(directory);
}

fn sweep_orphan(directory: &Path) {
    if config_dir_is_symlink(directory) {
        return;
    }
    let settings_path = directory.join(MUSE_SETTINGS_BASENAME);
    let hooks_path = directory.join(MUSE_HOOKS_BASENAME);
    let baseline_path = env_baseline_path(&settings_path);
    let keep = read_env_baseline(&baseline_path);
    let restored = with_orphan_lease(&settings_path, &settings_path, |created| {
        restore_settings(&settings_path, &hooks_path, created, keep.as_deref())
    });
    if matches!(restored, Ok(Some(()))) && keep.is_some() {
        consume_env_baseline(&baseline_path);
    }
    if let Ok(source) = hooks_source() {
        sweep_accepted_owned_file(&hooks_path, &|existing| {
            is_own_or_sibling_rendering(existing, &source)
        });
    }
}
