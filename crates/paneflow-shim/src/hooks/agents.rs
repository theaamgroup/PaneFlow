use super::{
    cleanup_hook_config_file, home_unavailable, hook_config_error, install_hook_config_file,
    is_paneflow_hook_command, paneflow_ipc_reachable,
    reconcile_matcher_hooks_replacing_invalid_container, remove_matcher_hooks_lenient,
    resolve_plain_hook_command, sweep_orphan_hook_config, HookInstall, HookInstallResult,
    HookInstallSkip, HookLease, InvalidJsonPolicy,
};
use paneflow_agent_config::jsonc;
use paneflow_agent_config::{home_dir, read_optional_text, write_text_atomic};
use std::path::{Path, PathBuf};

const GEMINI_HOOK_EVENTS: &[(&str, &str)] = &[
    ("BeforeAgent", "UserPromptSubmit"),
    ("AfterAgent", "Stop"),
    ("BeforeTool", "PreToolUse"),
    ("AfterTool", "PostToolUse"),
];

const CURSOR_HOOK_EVENTS: &[(&str, &str)] = &[
    ("beforeSubmitPrompt", "UserPromptSubmit"),
    ("stop", "Stop"),
    ("preToolUse", "PreToolUse"),
    ("postToolUse", "PostToolUse"),
    ("subagentStart", "SubagentStart"),
    ("subagentStop", "SubagentStop"),
];

fn gemini_managed_group(foreign: &str) -> serde_json::Value {
    let canonical = GEMINI_HOOK_EVENTS
        .iter()
        .find_map(|(candidate, canonical)| (*candidate == foreign).then_some(*canonical))
        .unwrap_or(foreign);
    serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "name": "paneflow-status",
            "type": "command",
            "command": resolve_plain_hook_command(canonical),
            "timeout": 5000,
        }]
    })
}

pub(crate) fn merge_gemini_hooks(root: &mut serde_json::Value) -> std::io::Result<()> {
    let events: Vec<&str> = GEMINI_HOOK_EVENTS
        .iter()
        .map(|(foreign, _)| *foreign)
        .collect();
    reconcile_matcher_hooks_replacing_invalid_container(root, &events, gemini_managed_group)
        .map(|_| ())
        .map_err(hook_config_error)
}

pub(crate) fn remove_gemini_hooks(root: &mut serde_json::Value) {
    let events: Vec<&str> = GEMINI_HOOK_EVENTS
        .iter()
        .map(|(foreign, _)| *foreign)
        .collect();
    remove_matcher_hooks_lenient(root, &events);
}

pub(crate) fn merge_cursor_hooks(root: &mut serde_json::Value) -> std::io::Result<()> {
    merge_flat_hooks(root, CURSOR_HOOK_EVENTS, true)
}

pub(crate) fn remove_cursor_hooks(root: &mut serde_json::Value) {
    remove_flat_hooks(root, CURSOR_HOOK_EVENTS);
}

fn merge_flat_hooks(
    root: &mut serde_json::Value,
    events: &[(&str, &str)],
    add_version: bool,
) -> std::io::Result<()> {
    if !root.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "hooks config root must be a JSON object, not a {}",
                json_type_name(root)
            ),
        ));
    }
    if let Some(hooks) = root.get("hooks") {
        if !hooks.is_object() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "hooks config key `hooks` must be an object, not a {}",
                    json_type_name(hooks)
                ),
            ));
        }
        if let Some(hooks) = hooks.as_object() {
            for (foreign, _) in events {
                if let Some(value) = hooks.get(*foreign) {
                    if !value.is_array() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "hooks config key `{foreign}` must be an array, not a {}",
                                json_type_name(value)
                            ),
                        ));
                    }
                }
            }
        }
    }

    let Some(root) = root.as_object_mut() else {
        return Ok(());
    };
    if add_version {
        root.entry("version")
            .or_insert_with(|| serde_json::json!(1));
    }
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "hooks config key `hooks` must be an object, not a {}",
                json_type_name(hooks)
            ),
        ));
    }
    let Some(hooks) = hooks.as_object_mut() else {
        return Ok(());
    };

    for (foreign, canonical) in events {
        let entries = hooks
            .entry(*foreign)
            .or_insert_with(|| serde_json::json!([]));
        let Some(entries) = entries.as_array_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("hooks config key `{foreign}` must be an array"),
            ));
        };
        entries.retain(|entry| !is_paneflow_flat_entry(entry));
        entries.push(serde_json::json!({
            "command": resolve_plain_hook_command(canonical),
            "timeout": 5,
        }));
    }
    Ok(())
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn remove_flat_hooks(root: &mut serde_json::Value, events: &[(&str, &str)]) {
    let Some(root) = root.as_object_mut() else {
        return;
    };
    if let Some(hooks) = root
        .get_mut("hooks")
        .and_then(|value| value.as_object_mut())
    {
        for (foreign, _) in events {
            if let Some(entries) = hooks
                .get_mut(*foreign)
                .and_then(|value| value.as_array_mut())
            {
                entries.retain(|entry| !is_paneflow_flat_entry(entry));
            }
        }
        hooks.retain(|_, value| value.as_array().is_none_or(|entries| !entries.is_empty()));
    }
    let hooks_empty = root
        .get("hooks")
        .and_then(|value| value.as_object())
        .is_none_or(serde_json::Map::is_empty);
    if hooks_empty {
        root.remove("hooks");
        if root.len() == 1 && root.contains_key("version") {
            root.remove("version");
        }
    }
}

fn is_paneflow_flat_entry(value: &serde_json::Value) -> bool {
    value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_paneflow_hook_command)
}

pub(crate) struct ManagedHookConfigGuard {
    settings_path: PathBuf,
    config_dir: PathBuf,
    created_file: bool,
    created_dir: bool,
    remove_fn: fn(&mut serde_json::Value),
    /// Gemini `settings.json` was spliced as JSONC. Drop must not reserialize it.
    jsonc: bool,
    /// Project-local files must not be cleaned through a symlink swapped in
    /// after install (#892). Home-scope guards leave this false.
    lease: HookLease,
}

#[derive(Clone, Copy)]
pub(crate) struct ManagedHookSpec {
    directory_name: &'static str,
    config_filename: &'static str,
    tool_label: &'static str,
    merge: fn(&mut serde_json::Value) -> std::io::Result<()>,
    remove: fn(&mut serde_json::Value),
}

impl ManagedHookSpec {
    pub(crate) const fn new(
        directory_name: &'static str,
        config_filename: &'static str,
        tool_label: &'static str,
        merge: fn(&mut serde_json::Value) -> std::io::Result<()>,
        remove: fn(&mut serde_json::Value),
    ) -> Self {
        Self {
            directory_name,
            config_filename,
            tool_label,
            merge,
            remove,
        }
    }
}

impl ManagedHookConfigGuard {
    pub(crate) fn install_in_home(spec: ManagedHookSpec) -> HookInstallResult<Self> {
        let home = home_dir().ok_or_else(home_unavailable)?;
        Self::install_anchored(
            &home.join(spec.directory_name),
            spec,
            InvalidJsonPolicy::Refuse,
        )
    }

    fn install_anchored(
        config_dir: &Path,
        spec: ManagedHookSpec,
        invalid_json_policy: InvalidJsonPolicy,
    ) -> HookInstallResult<Self> {
        if !paneflow_ipc_reachable() {
            let path = config_dir.join(spec.config_filename);
            if is_gemini_settings(&spec) {
                sweep_gemini_config(&path);
            } else {
                sweep_orphan_hook_config(&path, spec.remove);
            }
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        Self::install_at(config_dir, spec, invalid_json_policy).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(
        config_dir: &Path,
        spec: ManagedHookSpec,
        invalid_json_policy: InvalidJsonPolicy,
    ) -> std::io::Result<Self> {
        let installed = if is_gemini_settings(&spec) {
            super::install_hook_config_file_parsing_jsonc(
                config_dir,
                spec.config_filename,
                spec.tool_label,
                spec.merge,
                invalid_json_policy,
                gemini_jsonc_edit,
            )?
        } else {
            install_hook_config_file(
                config_dir,
                spec.config_filename,
                spec.tool_label,
                spec.merge,
                invalid_json_policy,
            )?
        };
        Ok(Self {
            settings_path: installed.path,
            config_dir: config_dir.to_path_buf(),
            created_file: installed.created_file,
            created_dir: installed.created_directory,
            remove_fn: spec.remove,
            jsonc: installed.jsonc,
            lease: installed.lease,
        })
    }
}

impl Drop for ManagedHookConfigGuard {
    fn drop(&mut self) {
        if self.jsonc {
            cleanup_jsonc_gemini(
                &self.settings_path,
                &self.config_dir,
                self.created_file,
                self.created_dir,
                &mut self.lease,
            );
        } else {
            cleanup_hook_config_file(
                &self.settings_path,
                &self.config_dir,
                self.created_file,
                self.created_dir,
                false,
                self.remove_fn,
                &mut self.lease,
            );
        }
    }
}

fn is_gemini_settings(spec: &ManagedHookSpec) -> bool {
    spec.directory_name == ".gemini" && spec.config_filename == "settings.json"
}

/// `Ok(None)` keeps the strict JSON installer. A commented (or otherwise
/// JSONC) Gemini file is spliced; text that is neither is left for the
/// strict path to refuse.
fn gemini_jsonc_edit(existing: &str) -> std::io::Result<Option<String>> {
    if existing.trim().is_empty() || serde_json::from_str::<serde_json::Value>(existing).is_ok() {
        return Ok(None);
    }
    if jsonc::parse(existing).is_err() {
        return Ok(None);
    }
    splice_gemini_install(existing).map(Some)
}

fn splice_gemini_install(existing: &str) -> std::io::Result<String> {
    let root = jsonc::parse(existing).map_err(jsonc_io)?;
    validate_gemini_object(&root)?;
    let base = match remove_gemini_jsonc(existing)? {
        Some(updated) => updated,
        None => existing.to_string(),
    };
    let root = jsonc::parse(&base).map_err(jsonc_io)?;
    validate_gemini_object(&root)?;
    if root.get("hooks").is_none() {
        return insert_gemini_hooks_object(&base);
    }
    let mut text = base;
    for (foreign, _) in GEMINI_HOOK_EVENTS {
        let group = gemini_managed_group(foreign);
        let current = jsonc::parse(&text).map_err(jsonc_io)?;
        match current.get("hooks").and_then(|hooks| hooks.get(*foreign)) {
            None => {
                text = jsonc::insert_entry(&text, &["hooks"], foreign, &serde_json::json!([group]))
                    .map_err(jsonc_io)?;
            }
            Some(value) if value.is_array() => {
                text = jsonc::append_array_element(&text, &["hooks", foreign], &group)
                    .map_err(jsonc_io)?;
            }
            Some(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("hook event `{foreign}` must be an array"),
                ));
            }
        }
    }
    Ok(text)
}

fn insert_gemini_hooks_object(text: &str) -> std::io::Result<String> {
    let mut hooks = serde_json::Map::new();
    for (foreign, _) in GEMINI_HOOK_EVENTS {
        hooks.insert(
            (*foreign).to_string(),
            serde_json::json!([gemini_managed_group(foreign)]),
        );
    }
    jsonc::insert_entry(text, &[], "hooks", &serde_json::Value::Object(hooks)).map_err(jsonc_io)
}

fn remove_gemini_jsonc(input: &str) -> std::io::Result<Option<String>> {
    let root = match jsonc::parse(input) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let Some(hooks) = root.get("hooks").and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    if hooks.is_empty() {
        return Ok(None);
    }
    if hooks_are_only_managed(hooks)
        && !jsonc::value_contains_comment(input, &["hooks"]).unwrap_or(true)
    {
        return jsonc::remove_at(input, &[], "hooks").map_err(jsonc_io);
    }

    let mut text = input.to_string();
    let mut changed = false;
    for (event, _) in GEMINI_HOOK_EVENTS {
        if let Some(updated) =
            jsonc::remove_array_elements(&text, &["hooks", event], is_wholly_managed_group)
                .map_err(jsonc_io)?
        {
            text = updated;
            changed = true;
            if event_array_is_empty(&text, event)
                && !jsonc::value_contains_comment(&text, &["hooks", event]).unwrap_or(true)
            {
                if let Some(updated) =
                    jsonc::remove_at(&text, &["hooks"], event).map_err(jsonc_io)?
                {
                    text = updated;
                }
            }
        }
        if let Some(updated) = jsonc::remove_nested_array_elements(
            &text,
            &["hooks", event],
            "hooks",
            is_managed_handler,
        )
        .map_err(jsonc_io)?
        {
            text = updated;
            changed = true;
        }
    }
    if changed
        && hooks_object_is_empty(&text)
        && !jsonc::value_contains_comment(&text, &["hooks"]).unwrap_or(true)
    {
        if let Some(updated) = jsonc::remove_at(&text, &[], "hooks").map_err(jsonc_io)? {
            text = updated;
        }
    }
    if changed {
        Ok(Some(text))
    } else {
        Ok(None)
    }
}

fn hooks_are_only_managed(hooks: &serde_json::Map<String, serde_json::Value>) -> bool {
    hooks.iter().all(|(key, value)| {
        GEMINI_HOOK_EVENTS.iter().any(|(event, _)| event == key)
            && value.as_array().is_some_and(|groups| {
                !groups.is_empty() && groups.iter().all(is_wholly_managed_group)
            })
    })
}

fn is_wholly_managed_group(value: &serde_json::Value) -> bool {
    let Some(hooks) = value.get("hooks").and_then(serde_json::Value::as_array) else {
        return false;
    };
    !hooks.is_empty() && hooks.iter().all(is_managed_handler)
}

fn is_managed_handler(value: &serde_json::Value) -> bool {
    value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_paneflow_hook_command)
}

fn event_array_is_empty(text: &str, event: &str) -> bool {
    jsonc::parse(text)
        .ok()
        .and_then(|root| {
            root.get("hooks")
                .and_then(|hooks| hooks.get(event))
                .and_then(serde_json::Value::as_array)
                .map(Vec::is_empty)
        })
        .unwrap_or(false)
}

fn hooks_object_is_empty(text: &str) -> bool {
    jsonc::parse(text)
        .ok()
        .and_then(|root| {
            root.get("hooks")
                .and_then(serde_json::Value::as_object)
                .map(serde_json::Map::is_empty)
        })
        .unwrap_or(false)
}

fn jsonc_text_is_empty_object(text: &str) -> bool {
    jsonc::parse(text)
        .ok()
        .and_then(|value| value.as_object().map(serde_json::Map::is_empty))
        .unwrap_or(false)
}

fn validate_gemini_object(root: &serde_json::Value) -> std::io::Result<()> {
    if !root.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config root must be a JSON object",
        ));
    }
    let Some(hooks) = root.get("hooks") else {
        return Ok(());
    };
    let Some(hooks) = hooks.as_object() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config key `hooks` must be an object",
        ));
    };
    for (event, _) in GEMINI_HOOK_EVENTS {
        if hooks.get(*event).is_some_and(|value| !value.is_array()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("hook event `{event}` must be an array"),
            ));
        }
    }
    Ok(())
}

fn jsonc_io(error: jsonc::JsoncError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

fn cleanup_jsonc_gemini(
    path: &Path,
    directory: &Path,
    created_file: bool,
    created_directory: bool,
    lease: &mut HookLease,
) {
    let remove_directory = super::with_last_lease(path, lease, |lease_created_file| {
        let Some(content) = read_optional_text(path)? else {
            return Ok(false);
        };
        let Some(updated) = remove_gemini_jsonc(&content)? else {
            return Ok(false);
        };
        if updated == content {
            return Ok(false);
        }
        let empty = jsonc_text_is_empty_object(&updated);
        let owned_file = created_file || lease_created_file;
        if empty && owned_file {
            std::fs::remove_file(path)?;
        } else {
            write_text_atomic(path, &updated)?;
        }
        Ok(empty && owned_file && created_directory)
    })
    .unwrap_or_else(|error| {
        eprintln!(
            "paneflow-shim: could not clean up {}: {error}",
            super::safe_path_display(path)
        );
        None
    })
    .unwrap_or(false);
    if remove_directory {
        let _ = std::fs::remove_dir(directory);
    }
}

fn sweep_gemini_config(path: &Path) {
    if path.parent().is_some_and(super::config_dir_is_symlink) || super::config_dir_is_symlink(path)
    {
        return;
    }
    let Ok(Some(content)) = read_optional_text(path) else {
        return;
    };
    if serde_json::from_str::<serde_json::Value>(&content).is_ok() {
        super::sweep_orphan_hook_config(path, remove_gemini_hooks);
        return;
    }
    if jsonc::parse(&content).is_err() {
        return;
    }
    let _ = super::with_orphan_lease(path, path, |created_file| {
        let Some(content) = read_optional_text(path)? else {
            return Ok(());
        };
        let Some(updated) = remove_gemini_jsonc(&content)? else {
            return Ok(());
        };
        if updated == content {
            return Ok(());
        }
        if created_file && jsonc_text_is_empty_object(&updated) {
            std::fs::remove_file(path)
        } else {
            write_text_atomic(path, &updated)
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_config_survives_invalid_utf8() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".gemini");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("settings.json");
        std::fs::write(&config, [0xff]).unwrap();

        assert!(ManagedHookConfigGuard::install_at(
            &directory,
            ManagedHookSpec::new(
                ".gemini",
                "settings.json",
                "Gemini",
                merge_gemini_hooks,
                remove_gemini_hooks,
            ),
            InvalidJsonPolicy::Refuse,
        )
        .is_err());
        assert_eq!(std::fs::read(config).unwrap(), [0xff]);
    }

    #[test]
    fn gemini_mixed_groups_preserve_user_handlers() {
        let mut root = serde_json::json!({
            "hooks": {
                "BeforeAgent": [{
                    "matcher": "*",
                    "hooks": [
                        { "type": "command", "command": "paneflow-ai-hook UserPromptSubmit" },
                        { "type": "command", "command": "my-user-hook" }
                    ]
                }]
            }
        });

        merge_gemini_hooks(&mut root).unwrap();
        remove_gemini_hooks(&mut root);

        let groups = root["hooks"]["BeforeAgent"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "my-user-hook");
    }
}
