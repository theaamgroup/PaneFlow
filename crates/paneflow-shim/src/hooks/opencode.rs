use super::owned_files::{remove_created_file, PANEFLOW_TS_BASENAME};
use super::{
    home_unavailable, paneflow_ipc_reachable, refuse_symlink, with_last_lease, with_orphan_lease,
    HookInstall, HookInstallResult, HookInstallSkip, HookLease,
};
use paneflow_agent_config::{
    home_dir, read_optional_text, with_config_lock, write_json_atomic, write_text_atomic,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const OPENCODE_PLUGIN_SOURCE: &str = include_str!("../../assets/opencode-paneflow-status.ts");

fn opencode_config_dir() -> Option<PathBuf> {
    opencode_config_dir_from(
        home_dir(),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("OPENCODE_CONFIG"),
        std::env::var_os("OPENCODE_CONFIG_DIR"),
    )
}

fn opencode_config_dir_from(
    home: Option<PathBuf>,
    xdg_config_home: Option<OsString>,
    opencode_config: Option<OsString>,
    opencode_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(config) = opencode_config
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return config.parent().map(Path::to_path_buf);
    }
    if let Some(directory) = opencode_config_dir
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return Some(directory);
    }
    xdg_config_home
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.map(|directory| directory.join(".config")))
        .map(|directory| directory.join("opencode"))
}

pub(crate) struct OpenCodePluginGuard {
    plugin_path: PathBuf,
    config_path: PathBuf,
    created_config: bool,
    plugin_lease: HookLease,
    config_lease: HookLease,
}

impl OpenCodePluginGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let directory = opencode_config_dir().ok_or_else(home_unavailable)?;
        if !paneflow_ipc_reachable() {
            Self::sweep_orphan(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        let config_path = directory.join("opencode.json");
        // serde_json cannot round-trip comments. If a jsonc file is present
        // (alone or beside json), OpenCode loads jsonc first — writing json
        // would either clobber comments or register the plugin in a file
        // OpenCode does not read.
        if directory.join("opencode.jsonc").exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "OpenCode uses opencode.jsonc; refusing a competing JSON config",
            ));
        }

        let plugins_dir = directory.join("plugins");
        refuse_symlink(&plugins_dir, "OpenCode plugin")?;
        std::fs::create_dir_all(&plugins_dir)?;
        let plugin_path = plugins_dir.join(PANEFLOW_TS_BASENAME);
        let plugin_config_path = plugin_path.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "OpenCode plugin path is not valid Unicode",
            )
        })?;
        let mut plugin_lease = HookLease::acquire(&plugin_path)?;
        with_config_lock(&plugin_path, || {
            let created_plugin = !plugin_path.exists();
            write_text_atomic(&plugin_path, OPENCODE_PLUGIN_SOURCE)?;
            if created_plugin {
                plugin_lease.mark_created()?;
            }
            Ok(())
        })?;

        let mut config_lease = match HookLease::acquire(&config_path) {
            Ok(lease) => lease,
            Err(error) => {
                rollback_plugin_file(&plugin_path, &mut plugin_lease);
                return Err(error);
            }
        };
        let result = with_config_lock(&config_path, || {
            let existing = read_optional_text(&config_path)?;
            let created_config = existing.is_none();
            let mut root = match existing {
                None => serde_json::json!({}),
                Some(content) => serde_json::from_str(&content)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
            };
            merge_opencode_plugin_entry(&mut root, plugin_config_path)?;
            write_json_atomic(&config_path, &root)?;
            if created_config {
                config_lease.mark_created()?;
            }
            Ok(created_config)
        });
        let created_config = match result {
            Ok(created) => created,
            Err(error) => {
                rollback_plugin_file(&plugin_path, &mut plugin_lease);
                return Err(error);
            }
        };

        Ok(Self {
            plugin_path,
            config_path,
            created_config,
            plugin_lease,
            config_lease,
        })
    }

    fn sweep_orphan(directory: &Path) {
        let plugin_path = directory.join("plugins").join(PANEFLOW_TS_BASENAME);
        let config_path = directory.join("opencode.json");
        let config_ok = with_orphan_lease(&config_path, &config_path, |created_config| {
            let Some(content) = read_optional_text(&config_path)? else {
                return Ok(());
            };
            let mut root = serde_json::from_str::<serde_json::Value>(&content)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            let before = root.clone();
            remove_opencode_plugin_entry(&mut root)?;
            if root == before {
                return Ok(());
            }
            if created_config && root.as_object().is_some_and(serde_json::Map::is_empty) {
                std::fs::remove_file(&config_path)
            } else {
                write_json_atomic(&config_path, &root)
            }
        });
        if config_ok.is_ok() {
            let _ = with_orphan_lease(&plugin_path, &plugin_path, |created_plugin| {
                remove_created_file(&plugin_path, created_plugin)
            });
        }
    }
}

impl Drop for OpenCodePluginGuard {
    fn drop(&mut self) {
        let config_ok = with_last_lease(
            &self.config_path,
            &mut self.config_lease,
            |lease_created_config| {
                let Some(content) = read_optional_text(&self.config_path)? else {
                    return Ok(());
                };
                let mut root = serde_json::from_str::<serde_json::Value>(&content)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                remove_opencode_plugin_entry(&mut root)?;
                if (self.created_config || lease_created_config)
                    && root.as_object().is_some_and(serde_json::Map::is_empty)
                {
                    std::fs::remove_file(&self.config_path)
                } else {
                    write_json_atomic(&self.config_path, &root)
                }
            },
        );
        if config_ok.is_ok() {
            let _ = with_last_lease(
                &self.plugin_path,
                &mut self.plugin_lease,
                |created_plugin| remove_created_file(&self.plugin_path, created_plugin),
            );
        }
    }
}

/// Undo a partially finished install: remove the plugin file only when this
/// session's lease shows PaneFlow created it and no other session still
/// holds it.
fn rollback_plugin_file(plugin_path: &Path, plugin_lease: &mut HookLease) {
    let _ = with_last_lease(plugin_path, plugin_lease, |created_plugin| {
        remove_created_file(plugin_path, created_plugin)
    });
}

fn merge_opencode_plugin_entry(
    root: &mut serde_json::Value,
    plugin_path: &str,
) -> std::io::Result<()> {
    let Some(root) = root.as_object_mut() else {
        return Err(non_object_root_error(root));
    };
    let plugins = root
        .entry("plugin")
        .or_insert_with(|| serde_json::json!([]));
    let Some(plugins) = plugins.as_array_mut() else {
        return Err(non_array_plugin_error(plugins));
    };
    plugins.retain(|entry| !is_paneflow_plugin_entry(entry));
    plugins.push(serde_json::Value::String(plugin_path.to_owned()));
    Ok(())
}

fn remove_opencode_plugin_entry(root: &mut serde_json::Value) -> std::io::Result<()> {
    let Some(root) = root.as_object_mut() else {
        return Ok(());
    };
    if let Some(plugin) = root.get("plugin") {
        if !plugin.is_array() {
            return Err(non_array_plugin_error(plugin));
        }
    }
    if let Some(plugins) = root
        .get_mut("plugin")
        .and_then(|value| value.as_array_mut())
    {
        plugins.retain(|entry| !is_paneflow_plugin_entry(entry));
    }
    if root
        .get("plugin")
        .and_then(|value| value.as_array())
        .is_some_and(Vec::is_empty)
    {
        root.remove("plugin");
    }
    Ok(())
}

fn non_object_root_error(value: &serde_json::Value) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "OpenCode config root must be an object, found {}",
            json_kind(value)
        ),
    )
}

fn non_array_plugin_error(value: &serde_json::Value) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "OpenCode config key `plugin` must be an array, found {}",
            json_kind(value)
        ),
    )
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn is_paneflow_plugin_entry(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(|value| {
        Path::new(value).file_name().and_then(OsStr::to_str) == Some(PANEFLOW_TS_BASENAME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_config_dir_matches_xdg_not_application_support() {
        let home = Some(PathBuf::from("/Users/alice"));
        assert_eq!(
            opencode_config_dir_from(
                home.clone(),
                Some(OsString::from("/Users/alice/.config")),
                None,
                None,
            ),
            Some(PathBuf::from("/Users/alice/.config/opencode")),
        );
        assert_eq!(
            opencode_config_dir_from(
                home.clone(),
                None,
                Some(OsString::from("/tmp/custom/opencode.json")),
                Some(OsString::from("/tmp/ignored")),
            ),
            Some(PathBuf::from("/tmp/custom")),
        );
        assert_eq!(
            opencode_config_dir_from(
                home.clone(),
                None,
                None,
                Some(OsString::from("/tmp/opencode")),
            ),
            Some(PathBuf::from("/tmp/opencode")),
        );
        assert_eq!(
            opencode_config_dir_from(home, None, None, None),
            Some(PathBuf::from("/Users/alice/.config/opencode")),
        );
    }

    #[test]
    fn primary_config_survives_invalid_utf8() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("opencode");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("opencode.json");
        std::fs::write(&config, [0xff]).unwrap();

        assert!(OpenCodePluginGuard::install_at(&directory).is_err());
        assert_eq!(std::fs::read(config).unwrap(), [0xff]);
    }

    #[test]
    fn preexisting_plugin_file_survives_cleanup() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("opencode");
        let plugins = directory.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let plugin = plugins.join(PANEFLOW_TS_BASENAME);
        std::fs::write(&plugin, "// user-managed copy\n").unwrap();

        drop(OpenCodePluginGuard::install_at(&directory).unwrap());

        assert!(
            plugin.exists(),
            "cleanup must not delete a plugin file PaneFlow did not create"
        );
        assert_eq!(
            std::fs::read_to_string(&plugin).unwrap(),
            OPENCODE_PLUGIN_SOURCE
        );
    }

    #[test]
    fn preexisting_empty_config_survives_cleanup() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("opencode");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("opencode.json");
        std::fs::write(&config, "{}").unwrap();

        let guard = OpenCodePluginGuard::install_at(&directory).unwrap();
        drop(guard);

        assert!(config.exists());
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
        assert_eq!(root, serde_json::json!({}));
    }

    #[test]
    fn merge_opencode_plugin_entry_rejects_non_object_root() {
        let mut root = serde_json::json!(["other"]);
        let error = merge_opencode_plugin_entry(&mut root, "/tmp/plugin.js").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("array"),
            "error must name the existing type, got {error}"
        );
        assert_eq!(root, serde_json::json!(["other"]));
    }

    #[test]
    fn install_refuses_non_object_config_without_writing() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("opencode");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("opencode.json");
        std::fs::write(&config, "[1, 2]").unwrap();

        let error = match OpenCodePluginGuard::install_at(&directory) {
            Ok(_) => panic!("install must fail on a non-object config root"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "[1, 2]");
        assert!(!directory
            .join("plugins")
            .join(PANEFLOW_TS_BASENAME)
            .exists());
    }

    /// Return the `{...}` object literal starting at `open` (brace-balanced).
    fn brace_block(source: &str, open: usize) -> &str {
        let mut depth = 0usize;
        for (offset, ch) in source[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[open..open + offset + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced object literal in plugin source");
    }

    /// Top-level property names of one object literal (`key: value` or the
    /// `key` shorthand); nested objects and spreads contribute no keys.
    fn top_level_keys(literal: &str) -> Vec<String> {
        let inner = literal
            .strip_prefix('{')
            .and_then(|text| text.strip_suffix('}'))
            .expect("object literal");
        let mut keys = Vec::new();
        let mut depth = 0usize;
        let mut entry = String::new();
        for ch in inner.chars().chain(std::iter::once(',')) {
            match ch {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    let key = entry.split(':').next().unwrap().trim();
                    if !key.is_empty() && !key.starts_with("...") {
                        keys.push(key.to_owned());
                    }
                    entry.clear();
                    continue;
                }
                _ => {}
            }
            entry.push(ch);
        }
        keys
    }

    #[test]
    fn plugin_frames_are_notifications_shaped_like_ai_hook_frame() {
        // Mirrors `AiHookFrame::to_value` in paneflow-ipc-client: a JSON-RPC
        // notification (no `id`), with agent-specific keys nested only under
        // `hook_payload`.
        let stringify = OPENCODE_PLUGIN_SOURCE
            .find("JSON.stringify(")
            .expect("plugin builds its frame with JSON.stringify");
        let open = stringify + OPENCODE_PLUGIN_SOURCE[stringify..].find('{').unwrap();
        let mut frame_keys = top_level_keys(brace_block(OPENCODE_PLUGIN_SOURCE, open));
        frame_keys.sort();
        assert_eq!(
            frame_keys,
            ["jsonrpc", "method", "params"],
            "frame must be a JSON-RPC notification: no id, nothing else"
        );

        const CANONICAL_PARAMS: [&str; 9] = [
            "workspace_id",
            "tool",
            "pid",
            "surface_id",
            "tool_name",
            "exit_code",
            "event_source",
            "emitted_at_ms",
            "hook_payload",
        ];
        let mut sends = 0;
        for (index, _) in OPENCODE_PLUGIN_SOURCE.match_indices("send(\"ai.") {
            let open = index + OPENCODE_PLUGIN_SOURCE[index..].find('{').unwrap();
            let literal = brace_block(OPENCODE_PLUGIN_SOURCE, open);
            for key in top_level_keys(literal) {
                assert!(
                    CANONICAL_PARAMS.contains(&key.as_str()),
                    "`{key}` is not a top-level AiHookParams field; nest it under hook_payload: {literal}"
                );
            }
            sends += 1;
        }
        assert!(
            sends >= 4,
            "expected every lifecycle handler to call send, found {sends}"
        );
    }

    #[test]
    fn merge_opencode_plugin_entry_remove_rejects_non_array() {
        let mut root = serde_json::json!({"plugin": "other"});
        let error = remove_opencode_plugin_entry(&mut root).unwrap_err();
        assert!(
            error.to_string().contains("string"),
            "error must name the existing type, got {error}"
        );
        assert_eq!(root, serde_json::json!({"plugin": "other"}));

        let mut root = serde_json::json!({"plugin": {}});
        let error = remove_opencode_plugin_entry(&mut root).unwrap_err();
        assert!(
            error.to_string().contains("object"),
            "error must name the existing type, got {error}"
        );
        assert_eq!(root, serde_json::json!({"plugin": {}}));
    }
}
