//! Config file I/O: read-modify-write helpers for `paneflow.json`.
//!
//! All functions operate on raw JSON to preserve unknown fields and formatting.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// US-016: serialize every read-modify-write of `paneflow.json`.
///
/// Settings-tab control handlers persist off the GPUI main thread - each
/// `persist_setting` spawns its own `cx.background_spawn → smol::unblock` task
/// (`settings/window.rs`). Without this lock two rapid toggles run two
/// independent `load_raw_config → mutate → write_config_checked` cycles on the
/// blocking pool: they read the same pre-change file and the later `rename`
/// wins, silently dropping the other key's update (CWE-362 lost update). They
/// also share the PID-suffixed temp path, so concurrent writes can clobber it.
/// Holding this guard across the whole load→write of each writer makes the RMW
/// atomic w.r.t. other writers, so each one observes the previous one's result.
///
/// (It does NOT order two writes of the *same* key - the last task to acquire
/// wins regardless of spawn order.) In-memory `cached_config` is mutated
/// before the spawn; ConfigWatcher reloads are ignored while a persist is
/// in flight or stamped with an older persist generation, so write N cannot
/// replace in-memory write N+1. Same-key last-lock-wins still matters across
/// a restart, and is self-healed by the next write or external reload.
static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Typed `paneflow.json` marker for issue #85's one-time compatibility pass.
/// Once true, install detection is never allowed to promote a launcher again.
const AGENT_BUTTON_VISIBILITY_MIGRATION_KEY: &str = "agent_button_visibility_defaults_migrated";

/// Acquire the config-write lock, recovering from a poisoned mutex (the guarded
/// value is `()`, so a prior panic-while-held left nothing to corrupt). Avoids
/// an `.unwrap()` on the lock per the repo's prod-unwrap lint.
fn config_write_guard() -> MutexGuard<'static, ()> {
    CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Load the raw JSON config.
///
/// Missing file means a fresh empty object. Existing but unreadable, oversized,
/// non-regular, or syntactically invalid files are rejected so a settings write
/// cannot overwrite the user's recoverable `paneflow.json` with `{}`.
fn load_raw_config_with_source(path: &Path) -> Result<(serde_json::Value, Option<String>), ()> {
    match paneflow_config::loader::read_config_string(path) {
        Ok(None) => Ok((serde_json::json!({}), None)),
        Err(error) => {
            log::warn!("config: {error}; refusing to overwrite");
            Err(())
        }
        Ok(Some(contents)) => {
            let value: serde_json::Value = serde_json::from_str(&contents).map_err(|e| {
                log::warn!(
                    "config: invalid JSON at {}; refusing to overwrite: {e}",
                    path.display()
                );
            })?;
            if value.is_object() {
                Ok((value, Some(contents)))
            } else {
                log::warn!(
                    "config: root at {} is not a JSON object; refusing to overwrite",
                    path.display()
                );
                Err(())
            }
        }
    }
}

fn load_raw_config(path: &Path) -> Result<serde_json::Value, ()> {
    load_raw_config_with_source(path).map(|(value, _source)| value)
}

#[derive(Clone, Copy)]
enum ConfigWritePrecondition<'a> {
    Any,
    Missing,
    Contents(&'a str),
}

/// Resolve an existing symlink to its managed target so an atomic replacement
/// updates the target without silently breaking a dotfile-manager link. A
/// dangling link is refused: replacing it would change the user's path policy.
fn config_write_target(path: &Path) -> Result<PathBuf, std::io::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error),
    }
}

/// Write a JSON value back to the config file, creating parent dirs if needed.
/// Returns `true` on success, `false` otherwise (serialization or I/O error -
/// logged at WARN in both cases).
fn write_config_checked(path: &Path, value: &serde_json::Value) -> bool {
    write_config_checked_with_precondition(path, value, ConfigWritePrecondition::Any)
}

/// Atomically publish `value`, optionally refusing the write if the source has
/// changed since the caller read it. This protects automatic migrations from
/// racing an editor or a second PaneFlow process.
fn write_config_checked_with_precondition(
    path: &Path,
    value: &serde_json::Value,
    precondition: ConfigWritePrecondition<'_>,
) -> bool {
    let write_target = match config_write_target(path) {
        Ok(target) => target,
        Err(error) => {
            log::warn!(
                "config: cannot resolve write target {}: {error}",
                path.display()
            );
            return false;
        }
    };
    if let Some(parent) = write_target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json_str = match serde_json::to_string_pretty(value) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("config: failed to serialize: {e}");
            return false;
        }
    };

    // US-031: write atomically (exclusive random tmp + rename) so a crash
    // mid-write can't
    // truncate `paneflow.json`. A truncated file parses as invalid JSON, which
    // `load_raw_config` silently swallows as an empty object - discarding the
    // user's entire config. The temp file lives in the target's own directory
    // so the rename stays on one filesystem. `NamedTempFile` creates a random,
    // exclusive 0600 file, preventing a planted symlink from being followed.
    let Some(parent) = write_target.parent() else {
        log::warn!(
            "config: write target {} has no parent; refusing a non-atomic write",
            write_target.display()
        );
        return false;
    };
    let file_name = write_target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("paneflow.json");
    let mut tmp = match tempfile::Builder::new()
        .prefix(&format!(".{file_name}.tmp."))
        .tempfile_in(parent)
    {
        Ok(tmp) => tmp,
        Err(error) => {
            log::warn!("config: failed to create temp file: {error}");
            return false;
        }
    };
    if let Err(error) = tmp.write_all(json_str.as_bytes()) {
        log::warn!("config: failed to write temp file: {error}");
        return false;
    }
    if let Err(error) = tmp.as_file().sync_all() {
        log::warn!("config: failed to sync temp file: {error}");
        return false;
    }

    let permissions = std::fs::metadata(&write_target)
        .map(|metadata| metadata.permissions())
        .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o600));
    if let Err(error) = tmp.as_file().set_permissions(permissions) {
        log::warn!("config: failed to preserve permissions: {error}");
        return false;
    }

    let current = match precondition {
        ConfigWritePrecondition::Any => None,
        ConfigWritePrecondition::Missing | ConfigWritePrecondition::Contents(_) => {
            match paneflow_config::loader::read_config_string(&write_target) {
                Ok(contents) => Some(contents),
                Err(error) => {
                    log::warn!("config: source changed or became unreadable: {error}");
                    return false;
                }
            }
        }
    };
    let precondition_holds = match (precondition, current.as_ref()) {
        (ConfigWritePrecondition::Any, _) => true,
        (ConfigWritePrecondition::Missing, Some(None)) => true,
        (ConfigWritePrecondition::Contents(expected), Some(Some(actual))) => actual == expected,
        _ => false,
    };
    if !precondition_holds {
        log::warn!(
            "config: {} changed while it was being migrated; refusing to overwrite",
            write_target.display()
        );
        return false;
    }

    match tmp.persist(&write_target) {
        Ok(_file) => true,
        Err(error) => {
            log::warn!("config: failed to promote temp file: {}", error.error);
            false
        }
    }
}

/// Preserve the old "every installed CLI is visible" behavior for configs
/// that existed before the fresh-config allowlist was introduced.
///
/// `path` and install detection are injected for tests. The caller must invoke
/// this only after the GUI process has adopted the login shell's `PATH`, or a
/// Dock launch could miss Homebrew/version-manager agents. Missing files are
/// fresh installs: they receive only the marker and retain the new defaults.
/// Existing valid objects promote installed agents whose raw key is absent,
/// null, or malformed; explicit booleans and unknown keys are preserved. The
/// promoted values and marker are committed by one atomic temp-file rename.
fn migrate_agent_button_visibility_at(
    path: &Path,
    mut is_installed: impl FnMut(crate::agent_launcher::TerminalAgent) -> bool,
) -> bool {
    use crate::agent_launcher::TerminalAgent;

    let _guard = config_write_guard();
    let write_target = match config_write_target(path) {
        Ok(target) => target,
        Err(error) => {
            log::warn!(
                "config: cannot resolve {} for agent visibility migration: {error}",
                path.display()
            );
            return false;
        }
    };
    let Ok((mut json, source)) = load_raw_config_with_source(&write_target) else {
        return false;
    };
    let Some(root) = json.as_object_mut() else {
        return false;
    };

    if root
        .get(AGENT_BUTTON_VISIBILITY_MIGRATION_KEY)
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return true;
    }

    if source.is_some() {
        for agent in TerminalAgent::ALL {
            let key = agent.button_visibility_key();
            let has_explicit_bool = root.get(key).is_some_and(serde_json::Value::is_boolean);
            if !has_explicit_bool && is_installed(agent) {
                root.insert(key.to_string(), serde_json::Value::Bool(true));
            }
        }
    }

    root.insert(
        AGENT_BUTTON_VISIBILITY_MIGRATION_KEY.to_string(),
        serde_json::Value::Bool(true),
    );
    let precondition = match source.as_deref() {
        Some(contents) => ConfigWritePrecondition::Contents(contents),
        None => ConfigWritePrecondition::Missing,
    };
    write_config_checked_with_precondition(&write_target, &json, precondition)
}

/// Run issue #85's one-time agent-button compatibility migration against the
/// real config file. Call this from startup immediately after login-shell
/// `PATH` adoption and before loading [`paneflow_config::schema::PaneFlowConfig`].
/// Returns `false` only when the path cannot be resolved or the existing config
/// is unsafe to overwrite / cannot be written.
pub fn migrate_agent_button_visibility_defaults() -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path for agent visibility migration");
        return false;
    };
    migrate_agent_button_visibility_at(&path, |agent| agent.is_installed())
}

/// Save a top-level config field, returning `true` on success and `false`
/// when the config path could not be resolved or the file write failed.
///
/// Callers that need to surface persistence failures to the user use this
/// variant. The write is skipped (returning `true`) when `seq` is no longer
/// the newest generation [`FieldPersistSeq::bump`] handed out for `key`, so a
/// stale captured value cannot land after a newer one (issue #242).
pub fn save_config_value_checked(
    key: &str,
    value: serde_json::Value,
    seqs: &FieldPersistSeq,
    seq: u64,
) -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    save_field_at_if_current(&path, FieldScope::TopLevel, key, value, seqs, seq)
}

/// Insert a top-level key, or remove it when `value` is `Null`.
fn apply_top_level_field(json: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if let Some(root) = json.as_object_mut() {
        if value.is_null() {
            root.remove(key);
        } else {
            root.insert(key.to_string(), value);
        }
    }
}

/// Save several top-level config fields in one read-modify-write cycle.
pub fn save_config_values_checked<const N: usize>(values: [(&str, serde_json::Value); N]) -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    let _guard = config_write_guard();
    let Ok(mut json) = load_raw_config(&path) else {
        return false;
    };
    for (key, value) in values {
        apply_top_level_field(&mut json, key, value);
    }
    write_config_checked(&path, &json)
}

/// Pure read-modify-write of the `shortcuts` map. Extracted from
/// [`save_shortcut`] so the dedupe + collision semantics can be unit-tested
/// without touching the real config path (mirrors [`apply_terminal_field`]).
///
/// Removes (a) any prior binding for `action_name` (so a remap doesn't leave
/// the old key live) and (b) any binding whose key collides with `new_key`
/// (US-021: `new_key` now belongs to `action_name`, so its previous owner
/// loses it - otherwise two user entries on the same physical chord would
/// produce a GPUI-ambiguous double binding). The collision test is
/// normalization-aware (`ctrl+shift+f` stored vs `ctrl-shift-f` recorded).
fn merge_shortcut(
    shortcuts_obj: &mut serde_json::Map<String, serde_json::Value>,
    new_key: &str,
    action_name: &str,
) {
    let keys_to_remove: Vec<String> = shortcuts_obj
        .iter()
        .filter(|(k, v)| {
            v.as_str() == Some(action_name) || crate::keybindings::keystrokes_conflict(k, new_key)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for k in keys_to_remove {
        shortcuts_obj.remove(&k);
    }

    shortcuts_obj.insert(
        new_key.to_string(),
        serde_json::Value::String(action_name.to_string()),
    );
}

/// Save a single shortcut override to `paneflow.json`.
///
/// Merges the new binding into `shortcuts`, removing any previous key for the
/// same action and any other action that already held `new_key`.
pub fn save_shortcut_checked(new_key: &str, action_name: &str) -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    let _guard = config_write_guard();
    let Ok(mut json) = load_raw_config(&path) else {
        return false;
    };

    // Guard rather than `.expect()` so a future loader contract change cannot
    // panic on the UI thread.
    let Some(root) = json.as_object_mut() else {
        log::warn!("config: root is not a JSON object, not saving shortcut");
        return false;
    };
    // Ensure `shortcuts` exists and is an object (replace a non-object).
    let shortcuts = root
        .entry("shortcuts")
        .or_insert_with(|| serde_json::json!({}));
    if !shortcuts.is_object() {
        *shortcuts = serde_json::json!({});
    }
    let Some(shortcuts_obj) = shortcuts.as_object_mut() else {
        return false;
    };

    merge_shortcut(shortcuts_obj, new_key, action_name);

    write_config_checked(&path, &json)
}

/// Remove the `shortcuts` object from a raw config value. Extracted from
/// [`reset_shortcuts_checked`] so the mutation can be unit-tested without
/// resolving (or touching) the real config path.
fn apply_reset_shortcuts(json: &mut serde_json::Value) {
    if let Some(root) = json.as_object_mut() {
        root.remove("shortcuts");
    }
}

/// Remove all user shortcut overrides from `path`, restoring defaults.
/// Returns `false` when the file cannot be loaded or the write fails.
fn reset_shortcuts_at(path: &Path) -> bool {
    let _guard = config_write_guard();
    let Ok(mut json) = load_raw_config(path) else {
        return false;
    };
    apply_reset_shortcuts(&mut json);
    write_config_checked(path, &json)
}

/// Remove all user shortcut overrides from `paneflow.json`, restoring defaults.
/// Returns `true` on success and `false` when the config path could not be
/// resolved, the existing file was unreadable, or the write failed.
pub fn reset_shortcuts_checked() -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    reset_shortcuts_at(&path)
}

/// Pure read-modify-write of the `"terminal"` sub-object. Extracted from
/// [`save_terminal_field_checked`] so the nesting/removal semantics can be unit-tested
/// without resolving (or touching) the real config path.
fn apply_terminal_field(json: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    let Some(root) = json.as_object_mut() else {
        return;
    };
    // Ensure `terminal` exists and is an object (replace a non-object).
    let terminal = root
        .entry("terminal")
        .or_insert_with(|| serde_json::json!({}));
    if !terminal.is_object() {
        *terminal = serde_json::json!({});
    }
    if let Some(obj) = terminal.as_object_mut() {
        if value.is_null() {
            obj.remove(key);
        } else {
            obj.insert(key.to_string(), value);
        }
    }
}

/// Pure read-modify-write of the `"agent_panel"` sub-object. Mirrors
/// [`apply_terminal_field`] for settings that are scoped to the Agents panel,
/// such as the native OS notification gate.
fn apply_agent_panel_field(json: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    let Some(root) = json.as_object_mut() else {
        return;
    };
    let agent_panel = root
        .entry("agent_panel")
        .or_insert_with(|| serde_json::json!({}));
    if !agent_panel.is_object() {
        *agent_panel = serde_json::json!({});
    }
    if let Some(obj) = agent_panel.as_object_mut() {
        if value.is_null() {
            obj.remove(key);
        } else {
            obj.insert(key.to_string(), value);
        }
    }
}

/// US-016: return a copy of `config` with a single field updated *in memory*,
/// mirroring the on-disk merge of [`save_config_value`] / [`save_terminal_field_checked`]
/// without touching disk. A settings handler uses this to refresh its render
/// cache instantly, then persists asynchronously. `nested` routes the field
/// into the `terminal` block; a `Null` value clears it. The config is the typed
/// view (no unknown fields), so the JSON round-trip is lossless for it.
pub fn with_field(
    config: &paneflow_config::schema::PaneFlowConfig,
    nested: bool,
    key: &str,
    value: serde_json::Value,
) -> paneflow_config::schema::PaneFlowConfig {
    let mut json = serde_json::to_value(config).unwrap_or_else(|_| serde_json::json!({}));
    if nested {
        apply_terminal_field(&mut json, key, value);
    } else if let Some(root) = json.as_object_mut() {
        if value.is_null() {
            root.remove(key);
        } else {
            root.insert(key.to_string(), value);
        }
    }
    serde_json::from_value(json).unwrap_or_else(|_| config.clone())
}

/// In-memory companion for [`save_agent_panel_field_checked`].
pub fn with_agent_panel_field(
    config: &paneflow_config::schema::PaneFlowConfig,
    key: &str,
    value: serde_json::Value,
) -> paneflow_config::schema::PaneFlowConfig {
    let mut json = serde_json::to_value(config).unwrap_or_else(|_| serde_json::json!({}));
    apply_agent_panel_field(&mut json, key, value);
    serde_json::from_value(json).unwrap_or_else(|_| config.clone())
}

/// In-memory companion for [`save_commands_checked_if_current`]. Replaces the
/// full user-defined command/template list while preserving every other config
/// field.
pub fn with_commands(
    config: &paneflow_config::schema::PaneFlowConfig,
    commands: Vec<paneflow_config::schema::CommandDefinition>,
) -> paneflow_config::schema::PaneFlowConfig {
    let mut next = config.clone();
    next.commands = commands;
    next
}

/// Which `paneflow.json` block a single-field settings write targets. Field
/// names are only unique within a block, so [`FieldPersistSeq`] scopes its
/// generations by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldScope {
    /// A top-level key.
    TopLevel,
    /// A key inside the `"terminal": { ... }` block.
    Terminal,
    /// A key inside the `"agent_panel": { ... }` block.
    AgentPanel,
}

/// Per-field persist generations for the single-field settings writers
/// (issue #242).
///
/// `persist_setting` captures a value at spawn time and writes it from
/// `smol::unblock`, so two rapid persists of one field become two independent
/// tasks and [`CONFIG_WRITE_LOCK`] alone still lets the older task acquire the
/// lock last and publish its stale value. The caller bumps the field's
/// generation on the main thread before spawning and the writer re-checks it
/// while holding the lock. The generation is per field, not global, because a
/// later write of a *different* field must not cancel this one.
#[derive(Default)]
pub struct FieldPersistSeq(Mutex<HashMap<(FieldScope, String), u64>>);

impl FieldPersistSeq {
    /// Mark a new pending write of `key` and return its generation.
    pub fn bump(&self, scope: FieldScope, key: &str) -> u64 {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = map.entry((scope, key.to_string())).or_insert(0);
        *seq += 1;
        *seq
    }

    fn is_current(&self, scope: FieldScope, key: &str, seq: u64) -> bool {
        let map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&(scope, key.to_string())) == Some(&seq)
    }
}

/// Save a single field inside the `"terminal": { ... }` block in `paneflow.json`
/// (US-016 Terminal settings tab). A `Null` value removes the key (restoring
/// the schema default on next load); the `"terminal"` object itself is left in
/// place (an empty block is harmless - `#[serde(default)]` handles it). The
/// write is skipped (returning `true`) when `seq` has been superseded by a
/// newer [`FieldPersistSeq::bump`] of the same key.
pub fn save_terminal_field_checked(
    key: &str,
    value: serde_json::Value,
    seqs: &FieldPersistSeq,
    seq: u64,
) -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    save_field_at_if_current(&path, FieldScope::Terminal, key, value, seqs, seq)
}

/// Save a single field inside the `"agent_panel": { ... }` block in
/// `paneflow.json`, preserving sibling agent-panel settings and unknown fields.
/// The write is skipped (returning `true`) when `seq` has been superseded by a
/// newer [`FieldPersistSeq::bump`] of the same key.
pub fn save_agent_panel_field_checked(
    key: &str,
    value: serde_json::Value,
    seqs: &FieldPersistSeq,
    seq: u64,
) -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    save_field_at_if_current(&path, FieldScope::AgentPanel, key, value, seqs, seq)
}

/// Read-modify-write one field of `path` only when `seq` is still the newest
/// generation for `(scope, key)`. The generation check happens while holding
/// the shared config-write lock so an older task cannot overwrite a newer task
/// that acquired the lock first (mirrors [`save_commands_at_if_current`]).
fn save_field_at_if_current(
    path: &Path,
    scope: FieldScope,
    key: &str,
    value: serde_json::Value,
    seqs: &FieldPersistSeq,
    seq: u64,
) -> bool {
    let _guard = config_write_guard();
    if !seqs.is_current(scope, key, seq) {
        return true;
    }
    let Ok(mut json) = load_raw_config(path) else {
        return false;
    };
    match scope {
        FieldScope::TopLevel => apply_top_level_field(&mut json, key, value),
        FieldScope::Terminal => apply_terminal_field(&mut json, key, value),
        FieldScope::AgentPanel => apply_agent_panel_field(&mut json, key, value),
    }
    write_config_checked(path, &json)
}

/// Save the full `commands` array only when this is still the newest workspace
/// template snapshot. The generation check happens while holding the shared
/// config-write lock so an older task cannot overwrite a newer task that
/// acquired the lock first.
pub fn save_commands_checked_if_current(
    commands: Vec<paneflow_config::schema::CommandDefinition>,
    save_seq: &AtomicU64,
    seq: u64,
) -> bool {
    let Some(path) = paneflow_config::loader::config_path() else {
        log::warn!("config: cannot determine config path, not saving");
        return false;
    };
    save_commands_at_if_current(&path, commands, save_seq, seq)
}

fn save_commands_at_if_current(
    path: &Path,
    commands: Vec<paneflow_config::schema::CommandDefinition>,
    save_seq: &AtomicU64,
    seq: u64,
) -> bool {
    let value = match serde_json::to_value(commands) {
        Ok(value) => value,
        Err(e) => {
            log::warn!("config: failed to serialize commands: {e}");
            return false;
        }
    };
    let _guard = config_write_guard();
    if save_seq.load(Ordering::SeqCst) != seq {
        return true;
    }
    let Ok(mut json) = load_raw_config(path) else {
        return false;
    };
    if let Some(root) = json.as_object_mut() {
        root.insert("commands".to_string(), value);
    }
    write_config_checked(path, &json)
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_BUTTON_VISIBILITY_MIGRATION_KEY, ConfigWritePrecondition, FieldPersistSeq,
        FieldScope, apply_agent_panel_field, apply_reset_shortcuts, apply_terminal_field,
        load_raw_config, merge_shortcut, migrate_agent_button_visibility_at, reset_shortcuts_at,
        save_commands_at_if_current, save_field_at_if_current, with_commands, write_config_checked,
        write_config_checked_with_precondition,
    };
    use crate::agent_launcher::TerminalAgent;
    use paneflow_config::schema::CommandDefinition;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn shell_command(name: &str, command: &str) -> CommandDefinition {
        CommandDefinition {
            name: name.to_string(),
            description: None,
            keywords: Vec::new(),
            workspace: None,
            command: Some(command.to_string()),
        }
    }

    #[test]
    fn persist_workspace_commands_keeps_newer_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let save_seq = AtomicU64::new(2);
        let older = vec![shell_command("Dev", "old")];
        let newer = vec![shell_command("Dev", "new")];
        let cached = with_commands(
            &with_commands(
                &paneflow_config::schema::PaneFlowConfig::default(),
                older.clone(),
            ),
            newer.clone(),
        );

        assert!(save_commands_at_if_current(
            &path,
            newer.clone(),
            &save_seq,
            2
        ));
        assert!(save_commands_at_if_current(&path, older, &save_seq, 1));

        let got: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(got["commands"], serde_json::to_value(&newer).unwrap());
        assert_eq!(cached.commands, newer);
        assert_eq!(save_seq.load(Ordering::SeqCst), 2);
    }

    /// Issue #242: persist A then persist B of one field, with A's task
    /// acquiring the config-write lock last. B must stay on disk, for the
    /// top-level, `terminal`, and `agent_panel` writers alike.
    #[test]
    fn stale_field_persist_does_not_overwrite_newer_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let seqs = FieldPersistSeq::default();

        for scope in [
            FieldScope::TopLevel,
            FieldScope::Terminal,
            FieldScope::AgentPanel,
        ] {
            let seq_a = seqs.bump(scope, "font_size");
            let seq_b = seqs.bump(scope, "font_size");
            assert!(save_field_at_if_current(
                &path,
                scope,
                "font_size",
                json!(14.0),
                &seqs,
                seq_b
            ));
            assert!(save_field_at_if_current(
                &path,
                scope,
                "font_size",
                json!(13.0),
                &seqs,
                seq_a
            ));
        }

        let got: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            got["font_size"],
            json!(14.0),
            "top-level: stale A must not win"
        );
        assert_eq!(
            got["terminal"]["font_size"],
            json!(14.0),
            "terminal: stale A must not win"
        );
        assert_eq!(
            got["agent_panel"]["font_size"],
            json!(14.0),
            "agent_panel: stale A must not win"
        );
    }

    /// The generation is per field and per block: a newer write of another
    /// key (or the same key in another block) must not cancel this one, or
    /// this key would never reach disk. A `Null` value still removes the key.
    #[test]
    fn field_persist_seq_is_scoped_per_field() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let seqs = FieldPersistSeq::default();

        let font = seqs.bump(FieldScope::TopLevel, "font_size");
        let opacity = seqs.bump(FieldScope::TopLevel, "opacity");
        let nested = seqs.bump(FieldScope::Terminal, "opacity");
        assert!(save_field_at_if_current(
            &path,
            FieldScope::Terminal,
            "opacity",
            json!(0.5),
            &seqs,
            nested
        ));
        assert!(save_field_at_if_current(
            &path,
            FieldScope::TopLevel,
            "opacity",
            json!(0.9),
            &seqs,
            opacity
        ));
        assert!(save_field_at_if_current(
            &path,
            FieldScope::TopLevel,
            "font_size",
            json!(15.0),
            &seqs,
            font
        ));

        let got: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(got["font_size"], json!(15.0));
        assert_eq!(got["opacity"], json!(0.9));
        assert_eq!(got["terminal"]["opacity"], json!(0.5));

        let clear = seqs.bump(FieldScope::TopLevel, "font_size");
        assert!(save_field_at_if_current(
            &path,
            FieldScope::TopLevel,
            "font_size",
            Value::Null,
            &seqs,
            clear
        ));
        let got: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(got.get("font_size").is_none());
        assert_eq!(got["opacity"], json!(0.9));
    }

    #[test]
    fn agent_visibility_migration_marks_missing_config_without_promoting_agents() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");

        assert!(migrate_agent_button_visibility_at(&path, |_| true));
        let got: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(got, json!({(AGENT_BUTTON_VISIBILITY_MIGRATION_KEY): true}));

        assert!(migrate_agent_button_visibility_at(&path, |_| {
            unreachable!("completed migration must not probe PATH again")
        }));
        let second: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(second, got, "migration must be idempotent");
    }

    #[test]
    fn agent_visibility_migration_promotes_installed_legacy_defaults_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let original = json!({
            "theme": "Custom",
            "unknown_extension": {"keep": true},
            "codex_button_visible": false,
            "grok_button_visible": true,
            "pi_button_visible": null,
            "hermes_agent_button_visible": "malformed",
            "amp_button_visible": null
        });
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        let installed = HashSet::from([
            TerminalAgent::Codex,
            TerminalAgent::OpenCode,
            TerminalAgent::Pi,
            TerminalAgent::Hermes,
            TerminalAgent::Grok,
        ]);

        assert!(migrate_agent_button_visibility_at(&path, |agent| {
            installed.contains(&agent)
        }));

        let got: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(got[AGENT_BUTTON_VISIBILITY_MIGRATION_KEY], true);
        assert_eq!(got["codex_button_visible"], false, "explicit false wins");
        assert_eq!(got["grok_button_visible"], true, "explicit true wins");
        assert_eq!(got["opencode_button_visible"], true, "absent is promoted");
        assert_eq!(got["pi_button_visible"], true, "null is promoted");
        assert_eq!(
            got["hermes_agent_button_visible"], true,
            "malformed legacy value is promoted"
        );
        assert_eq!(
            got["amp_button_visible"],
            Value::Null,
            "uninstalled values stay untouched"
        );
        assert_eq!(got["theme"], original["theme"]);
        assert_eq!(got["unknown_extension"], original["unknown_extension"]);
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(
            leftovers, 0,
            "marker and promoted values use one atomic write"
        );
    }

    #[test]
    fn agent_visibility_migration_leaves_invalid_config_untouched() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        std::fs::write(&path, "{").unwrap();

        assert!(!migrate_agent_button_visibility_at(&path, |_| true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{");
    }

    #[test]
    fn agent_visibility_migration_preserves_a_symlinked_config() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("managed-paneflow.json");
        let path = dir.path().join("paneflow.json");
        std::fs::write(&target, r#"{"theme":"Custom"}"#).unwrap();
        symlink(&target, &path).unwrap();

        assert!(migrate_agent_button_visibility_at(&path, |agent| {
            agent == TerminalAgent::OpenCode
        }));

        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "migration must update the managed target without replacing its symlink"
        );
        let got: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(got["opencode_button_visible"], true);
        assert_eq!(got[AGENT_BUTTON_VISIBILITY_MIGRATION_KEY], true);
    }

    #[test]
    fn write_config_is_atomic_and_leaves_no_temp() {
        // US-031: the write goes through tmp+rename, the target ends up with
        // the full content, and no temp file is left behind in the directory.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        assert!(write_config_checked(
            &p,
            &json!({"theme": "One Dark", "font_size": 14.0})
        ));
        let got: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(got["theme"], "One Dark");
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0, "the temp file must be renamed away");
    }

    #[test]
    fn write_config_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        std::fs::write(&path, r#"{"theme":"Before"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        assert!(write_config_checked(&path, &json!({"theme": "After"})));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn write_config_ignores_a_preplanted_legacy_temp_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let planted = dir
            .path()
            .join(format!(".paneflow.json.tmp.{}", std::process::id()));
        std::fs::write(&planted, b"do not touch").unwrap();

        assert!(write_config_checked(&path, &json!({"safe": true})));
        assert_eq!(std::fs::read(&planted).unwrap(), b"do not touch");
    }

    #[test]
    fn conditional_write_refuses_a_stale_source_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let old = r#"{"theme":"Before"}"#;
        let edited = r#"{"theme":"Edited"}"#;
        std::fs::write(&path, edited).unwrap();

        assert!(!write_config_checked_with_precondition(
            &path,
            &json!({"theme": "Migrated"}),
            ConfigWritePrecondition::Contents(old),
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn write_config_does_not_truncate_on_repeated_writes() {
        // A second write fully replaces the first (no partial/truncated file).
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        assert!(write_config_checked(&p, &json!({"a": 1})));
        assert!(write_config_checked(&p, &json!({"b": 2})));
        let got: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(got.get("a").is_none() && got["b"] == 2);
    }

    #[test]
    fn write_config_checked_returns_false_when_parent_is_not_a_directory() {
        // chmod-a-w on the *file* is not a write failure: tmp+rename replaces
        // the inode and ignores the destination mode. A non-directory parent
        // is a real I/O failure on the temp-file create / rename path.
        let dir = tempfile::TempDir::new().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let p = blocker.join("paneflow.json");
        assert!(!write_config_checked(&p, &json!({"a": 1})));
    }

    #[test]
    fn apply_reset_shortcuts_removes_shortcuts_preserving_siblings() {
        let mut j = json!({
            "theme": "One Dark",
            "shortcuts": {"ctrl-shift-d": "split_horizontally"}
        });
        apply_reset_shortcuts(&mut j);
        assert!(j.get("shortcuts").is_none());
        assert_eq!(j["theme"], json!("One Dark"));
    }

    #[test]
    fn apply_reset_shortcuts_is_noop_without_shortcuts_key() {
        let mut j = json!({"theme": "One Dark"});
        apply_reset_shortcuts(&mut j);
        assert_eq!(j, json!({"theme": "One Dark"}));
    }

    #[test]
    fn reset_shortcuts_at_removes_shortcuts_preserving_siblings() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        assert!(write_config_checked(
            &p,
            &json!({
                "theme": "One Dark",
                "shortcuts": {"ctrl-shift-d": "split_horizontally"}
            })
        ));
        assert!(reset_shortcuts_at(&p));
        let got: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(got.get("shortcuts").is_none());
        assert_eq!(got["theme"], "One Dark");
    }

    #[test]
    fn reset_shortcuts_at_returns_false_on_invalid_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        std::fs::write(&p, "{").unwrap();
        assert!(!reset_shortcuts_at(&p));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{");
    }

    #[test]
    fn reset_shortcuts_at_returns_false_when_write_fails() {
        // chmod-a-w on the *file* is ignored by tmp+rename (new inode).
        // A non-writable parent directory is the real write-failure path.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        let original = json!({
            "theme": "One Dark",
            "shortcuts": {"ctrl-shift-d": "split_horizontally"}
        });
        assert!(write_config_checked(&p, &original));

        struct RestorePerms {
            path: std::path::PathBuf,
            perms: std::fs::Permissions,
        }
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.path, self.perms.clone());
            }
        }

        let restore = RestorePerms {
            path: dir.path().to_path_buf(),
            perms: std::fs::metadata(dir.path()).unwrap().permissions(),
        };
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let ok = reset_shortcuts_at(&p);
        drop(restore);
        assert!(
            !ok,
            "tmp+rename must fail when the config directory is not writable"
        );
        let got: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(got, original);
    }

    #[test]
    fn load_raw_config_rejects_invalid_json_instead_of_emptying_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        std::fs::write(&p, "{").unwrap();

        assert!(
            load_raw_config(&p).is_err(),
            "invalid existing config must fail closed so writers do not replace it with an empty object"
        );
    }

    #[test]
    fn load_raw_config_rejects_non_object_roots() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("paneflow.json");
        std::fs::write(&p, "[]").unwrap();

        assert!(
            load_raw_config(&p).is_err(),
            "a valid JSON non-object is not a writable paneflow config root"
        );
    }

    fn shortcuts(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn merge_shortcut_dedupes_prior_key_for_same_action() {
        // Rebinding an action moves its key: the old key must not stay live.
        let mut m = shortcuts(&[("ctrl-alt-h", "split_horizontally")]);
        merge_shortcut(&mut m, "ctrl-alt-j", "split_horizontally");
        assert!(!m.contains_key("ctrl-alt-h"), "old key should be removed");
        assert_eq!(m["ctrl-alt-j"], json!("split_horizontally"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn merge_shortcut_collision_evicts_other_action() {
        // US-021: binding a key already owned by another action must evict the
        // previous owner, not leave both entries live (GPUI double binding).
        let mut m = shortcuts(&[("ctrl-shift-f", "toggle_search")]);
        merge_shortcut(&mut m, "ctrl-shift-f", "close_pane");
        assert_eq!(m["ctrl-shift-f"], json!("close_pane"));
        assert_eq!(m.len(), 1, "no leftover binding for the evicted action");
    }

    #[test]
    fn merge_shortcut_collision_is_normalization_aware() {
        // A stored "+"-separated key and a recorded "-"-separated key denote
        // the same chord; the collision filter must collapse them.
        let mut m = shortcuts(&[("ctrl+shift+f", "toggle_search")]);
        merge_shortcut(&mut m, "ctrl-shift-f", "close_pane");
        assert!(
            !m.contains_key("ctrl+shift+f"),
            "the '+'-separated variant must be evicted"
        );
        assert_eq!(m["ctrl-shift-f"], json!("close_pane"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn upserts_into_terminal_block_creating_it() {
        let mut j = json!({});
        apply_terminal_field(&mut j, "ligatures", json!(true));
        assert_eq!(j["terminal"]["ligatures"], json!(true));
    }

    #[test]
    fn preserves_other_terminal_keys() {
        let mut j = json!({"terminal": {"cursor_shape": "beam"}});
        apply_terminal_field(&mut j, "ligatures", json!(true));
        assert_eq!(j["terminal"]["cursor_shape"], json!("beam"));
        assert_eq!(j["terminal"]["ligatures"], json!(true));
    }

    #[test]
    fn null_removes_key_but_keeps_block() {
        let mut j = json!({"terminal": {"cursor_shape": "beam", "ligatures": true}});
        apply_terminal_field(&mut j, "cursor_shape", Value::Null);
        assert!(j["terminal"].get("cursor_shape").is_none());
        assert_eq!(j["terminal"]["ligatures"], json!(true));
        assert!(j["terminal"].is_object());
    }

    #[test]
    fn replaces_non_object_terminal_value() {
        let mut j = json!({"terminal": "garbage"});
        apply_terminal_field(&mut j, "cursor_shape", json!("block"));
        assert_eq!(j["terminal"]["cursor_shape"], json!("block"));
    }

    #[test]
    fn leaves_top_level_keys_untouched() {
        let mut j = json!({"theme": "One Dark", "font_size": 14.0});
        apply_terminal_field(&mut j, "scrollback_lines", json!(5000));
        assert_eq!(j["theme"], json!("One Dark"));
        assert_eq!(j["font_size"], json!(14.0));
        assert_eq!(j["terminal"]["scrollback_lines"], json!(5000));
    }

    #[test]
    fn upserts_into_agent_panel_preserving_siblings() {
        let mut j = json!({
            "agent_panel": {
                "max_content_width": 760,
                "notify_when_agent_waiting": "PrimaryScreen"
            }
        });
        apply_agent_panel_field(&mut j, "notify_when_agent_waiting", json!("Never"));
        assert_eq!(j["agent_panel"]["max_content_width"], json!(760));
        assert_eq!(
            j["agent_panel"]["notify_when_agent_waiting"],
            json!("Never")
        );
    }

    #[test]
    fn replaces_non_object_agent_panel_value() {
        let mut j = json!({"agent_panel": "garbage"});
        apply_agent_panel_field(&mut j, "notify_when_agent_waiting", json!("Never"));
        assert_eq!(
            j["agent_panel"]["notify_when_agent_waiting"],
            json!("Never")
        );
    }
}
