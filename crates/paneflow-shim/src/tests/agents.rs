use crate::hooks::dsh::{hooks_source, render_overlay, DSH_HOOKS_BASENAME, DSH_OVERLAY_BASENAME};
use crate::hooks::muse::{
    remove_muse_settings, MuseHookConfigGuard, MUSE_HOOKS_BASENAME, MUSE_HOOK_ENV_VARS,
    MUSE_HOOK_EVENTS, MUSE_SETTINGS_BASENAME,
};
use crate::hooks::{
    enable_codex_feature_flag, CodexHookConfigGuard, CODEX_HOOK_EVENTS, CODEX_TOML_MARKER,
};
use crate::hooks::{
    hermes_managed_block, is_paneflow_hook_command, merge_codebuddy_hooks, merge_cursor_hooks,
    merge_gemini_hooks, merge_qoder_hooks, remove_cursor_hooks, remove_gemini_hooks,
    remove_paneflow_hooks, remove_qoder_hooks, resolve_hook_command, strip_hermes_managed_block,
    DshOverlayGuard, GrokHookFileGuard, HermesHookConfigGuard, InvalidJsonPolicy,
    ManagedHookConfigGuard, ManagedHookSpec, OpenCodePluginGuard, PiExtensionGuard,
    CLAUDE_HOOK_EVENTS, HERMES_BLOCK_BEGIN, PANEFLOW_TS_BASENAME,
};
use crate::hooks::{render_as_sibling_instance, sibling_hook_program};
use serde_json::json;

fn command_preserves_event_arg(command: &str, event: &str) -> bool {
    command.ends_with(&format!(" {event}"))
        || command.ends_with(&format!(" {event}\\\""))
        || command.contains(&format!(" {event} "))
}

// ---------- Multi-agent: clones + JSON/TS/YAML guards ----------

#[test]
fn qoder_merge_skips_notification_event() {
    // Qoder has no `Notification` hook event - registering it could
    // make its config validator reject the whole file.
    let mut root = json!({});
    merge_qoder_hooks(&mut root).unwrap();
    let hooks = root["hooks"].as_object().unwrap();
    assert!(hooks.contains_key("UserPromptSubmit"));
    assert!(hooks.contains_key("Stop"));
    assert!(
        !hooks.contains_key("Notification"),
        "Notification must not be registered for Qoder"
    );
    let group = &root["hooks"]["UserPromptSubmit"][0];
    assert!(
        group.get("_paneflow_managed").is_none(),
        "Qoder public schema does not document Paneflow-only markers"
    );
    assert!(
        group["hooks"][0].get("commandWindows").is_none(),
        "Qoder public schema does not document commandWindows"
    );
    // Round-trip: removal leaves an empty tree (deletable file).
    remove_qoder_hooks(&mut root);
    assert_eq!(root, json!({}));
}

#[test]
fn gemini_nested_merge_writes_official_shape_and_roundtrips() {
    let mut root = json!({});
    merge_gemini_hooks(&mut root).unwrap();
    // Foreign key on the config side…
    let before_agent = root["hooks"]["BeforeAgent"].as_array().unwrap();
    assert_eq!(before_agent.len(), 1);
    let group = &before_agent[0];
    assert_eq!(group["matcher"], json!("*"));
    let inner = group["hooks"].as_array().unwrap();
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0]["name"], json!("paneflow-status"));
    assert_eq!(inner[0]["type"], json!("command"));
    assert_eq!(
        inner[0]["timeout"],
        json!(5000),
        "Gemini hook timeout is milliseconds"
    );
    // …canonical Claude-shaped event in the command arg.
    let cmd = inner[0]["command"].as_str().unwrap();
    assert!(
        command_preserves_event_arg(cmd, "UserPromptSubmit"),
        "BeforeAgent must invoke the canonical UserPromptSubmit: {cmd}"
    );
    // No Paneflow-only marker field (stricter parsers).
    assert!(group.get("_paneflow_managed").is_none());
    assert!(group.get("command").is_none());
    // Idempotent merge.
    merge_gemini_hooks(&mut root).unwrap();
    assert_eq!(root["hooks"]["BeforeAgent"].as_array().unwrap().len(), 1);
    // Removal restores an empty tree.
    remove_gemini_hooks(&mut root);
    assert_eq!(root, json!({}));
}

#[test]
fn cursor_flat_merge_stamps_version_and_preserves_user_entries() {
    let mut root = json!({
        "hooks": {
            "preToolUse": [ { "command": "/usr/bin/audit-tool" } ]
        }
    });
    merge_cursor_hooks(&mut root).unwrap();
    assert_eq!(root["version"], json!(1), "Cursor requires version: 1");
    let arr = root["hooks"]["preToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "user entry + paneflow entry");
    assert_eq!(arr[0]["command"], json!("/usr/bin/audit-tool"));

    remove_cursor_hooks(&mut root);
    let arr = root["hooks"]["preToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "only the user's entry survives removal");
    // `version` is kept while user content remains.
    assert_eq!(root["version"], json!(1));
}

#[test]
fn cursor_flat_remove_drops_version_when_nothing_else_remains() {
    let mut root = json!({});
    merge_cursor_hooks(&mut root).unwrap();
    remove_cursor_hooks(&mut root);
    assert_eq!(
        root,
        json!({}),
        "a fully-managed file must collapse to empty (then deleted)"
    );
}

#[test]
fn merge_cursor_hooks_refuses_non_object_root() {
    let mut root = json!(["not-an-object"]);
    let error = merge_cursor_hooks(&mut root).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(root, json!(["not-an-object"]));
}

#[test]
fn merge_cursor_hooks_refuses_non_array_event_value() {
    let mut root = json!({
        "hooks": {
            "preToolUse": { "command": "x" }
        }
    });
    let before = root.clone();
    let error = merge_cursor_hooks(&mut root).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(root, before, "a non-array event must not be rewritten");
}

#[test]
fn managed_guard_install_and_drop_roundtrip_in_clone_dir() {
    // End-to-end for the clone path: .codebuddy/settings.local.json is
    // created with Claude-format hooks, then fully cleaned up on drop
    // (file deleted, created dir removed).
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".codebuddy");

    let guard = ManagedHookConfigGuard::install_at(
        &dir,
        ManagedHookSpec::new(
            ".codebuddy",
            "settings.local.json",
            "CodeBuddy",
            merge_codebuddy_hooks,
            remove_paneflow_hooks,
        ),
        InvalidJsonPolicy::Refuse,
    )
    .expect("install in fresh dir must succeed");

    let content = std::fs::read_to_string(dir.join("settings.local.json")).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(root["hooks"]["UserPromptSubmit"].is_array());
    let group = &root["hooks"]["UserPromptSubmit"][0];
    assert!(
        group.get("_paneflow_managed").is_none(),
        "CodeBuddy public schema does not document Paneflow-only markers"
    );
    let handler = &root["hooks"]["UserPromptSubmit"][0]["hooks"][0];
    assert_eq!(handler["type"], json!("command"));
    assert!(
        handler.get("commandWindows").is_none(),
        "CodeBuddy public schema does not document commandWindows"
    );

    drop(guard);
    assert!(
        !dir.exists(),
        "drop must delete the managed file and the created dir"
    );
}

#[test]
fn managed_guard_refuses_invalid_primary_user_config() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".gemini");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    std::fs::write(&path, "{ broken").unwrap();

    let guard = ManagedHookConfigGuard::install_at(
        &dir,
        ManagedHookSpec::new(
            ".gemini",
            "settings.json",
            "Gemini",
            merge_gemini_hooks,
            remove_gemini_hooks,
        ),
        InvalidJsonPolicy::Refuse,
    );

    assert!(guard.is_err());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "{ broken",
        "primary user config must not be overwritten on parse failure"
    );
}

#[test]
fn pi_extension_guard_roundtrip() {
    let td = tempfile::TempDir::new().unwrap();
    let ext_dir = td.path().join(".pi/agent/extensions");
    let guard = PiExtensionGuard::install_at(&ext_dir).expect("install must succeed");
    let ext = ext_dir.join(PANEFLOW_TS_BASENAME);
    let content = std::fs::read_to_string(&ext).unwrap();
    assert!(
        content.contains("PANEFLOW_SOCKET_PATH"),
        "extension must be env-gated to stay inert outside Paneflow"
    );
    drop(guard);
    assert!(!ext.exists(), "drop must remove the extension file");
}

#[test]
fn opencode_guard_declares_plugin_and_cleans_up() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join("opencode");

    let guard = OpenCodePluginGuard::install_at(&dir).expect("fresh install must succeed");
    let plugin = dir.join("plugins").join(PANEFLOW_TS_BASENAME);
    assert!(plugin.is_file());
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
    let entries = root["plugin"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].as_str().unwrap().ends_with(PANEFLOW_TS_BASENAME));

    drop(guard);
    assert!(!plugin.exists(), "drop must remove the plugin file");
    assert!(
        !dir.join("opencode.json").exists(),
        "a config we created and fully own must be deleted on drop"
    );
}

#[test]
fn opencode_guard_preserves_user_config_and_refuses_unparseable() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join("opencode");
    std::fs::create_dir_all(&dir).unwrap();

    // User config with their own plugin entry survives the roundtrip.
    std::fs::write(
        dir.join("opencode.json"),
        r#"{"model": "anthropic/claude-opus-4-8", "plugin": ["./mine.ts"]}"#,
    )
    .unwrap();
    let guard = OpenCodePluginGuard::install_at(&dir).unwrap();
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
    assert_eq!(root["plugin"].as_array().unwrap().len(), 2);
    drop(guard);
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
    assert_eq!(root["plugin"], json!(["./mine.ts"]));
    assert_eq!(root["model"], json!("anthropic/claude-opus-4-8"));

    // PRIMARY config that doesn't parse must never be clobbered.
    std::fs::write(dir.join("opencode.json"), "{ definitely not json").unwrap();
    assert!(
        OpenCodePluginGuard::install_at(&dir).is_err(),
        "unparseable primary config must skip the install"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("opencode.json")).unwrap(),
        "{ definitely not json",
        "the user's file must be byte-identical after the refusal"
    );
}

#[test]
fn drop_leaves_plugin_file_when_opencode_json_is_unparseable() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join("opencode");
    let guard = OpenCodePluginGuard::install_at(&dir).unwrap();
    let plugin = dir.join("plugins").join(PANEFLOW_TS_BASENAME);
    assert!(plugin.is_file());
    std::fs::write(dir.join("opencode.json"), "{").unwrap();
    drop(guard);
    assert!(
        plugin.exists(),
        "drop must leave the plugin file when opencode.json does not parse"
    );
}

#[test]
fn merge_opencode_plugin_entry_rejects_non_array_plugin() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    let plugin = dir.join("plugins").join(PANEFLOW_TS_BASENAME);

    for (fixture, kind) in [
        (r#"{"plugin": "other"}"#, "string"),
        (r#"{"plugin": {}}"#, "object"),
    ] {
        std::fs::write(dir.join("opencode.json"), fixture).unwrap();
        let Err(error) = OpenCodePluginGuard::install_at(&dir) else {
            panic!("install_at must return Err when plugin is a {kind}");
        };
        let message = error.to_string();
        assert!(
            message.contains(kind),
            "error must name the existing type {kind}, got {message}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("opencode.json")).unwrap(),
            fixture,
            "non-array plugin config must be left byte-identical"
        );
        assert!(
            !plugin.exists(),
            "failed install must not leave the plugin file behind"
        );
    }
}

#[test]
fn opencode_guard_skips_jsonc_only_setup() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    // serde_json can't round-trip comments - a .jsonc-only setup must
    // be left alone entirely.
    std::fs::write(dir.join("opencode.jsonc"), "{ /* user comment */ }").unwrap();
    assert!(OpenCodePluginGuard::install_at(&dir).is_err());
    assert!(!dir.join("opencode.json").exists());
}

#[test]
fn opencode_guard_skips_when_jsonc_and_json_both_exist() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    std::fs::write(&json, r#"{"plugin":[]}"#).unwrap();
    std::fs::write(&jsonc, "{ /* user comment */ }").unwrap();
    assert!(OpenCodePluginGuard::install_at(&dir).is_err());
    assert_eq!(std::fs::read_to_string(&json).unwrap(), r#"{"plugin":[]}"#);
    assert_eq!(
        std::fs::read_to_string(&jsonc).unwrap(),
        "{ /* user comment */ }"
    );
    assert!(!dir.join("plugins").join(PANEFLOW_TS_BASENAME).exists());
}

#[test]
fn grok_guard_writes_dedicated_file_and_removes_on_drop() {
    let td = tempfile::TempDir::new().unwrap();
    let hooks_dir = td.path().join(".grok/hooks");
    let guard = GrokHookFileGuard::install_at(&hooks_dir).expect("install must succeed");
    let path = hooks_dir.join("paneflow.json");
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // Claude matcher-group shape, reduced event set with explicit
    // permission requests.
    assert!(root["hooks"]["UserPromptSubmit"].is_array());
    assert!(root["hooks"]["PermissionRequest"].is_array());
    assert!(root["hooks"]["Stop"].is_array());
    assert!(
        root["hooks"].get("Notification").is_none(),
        "Notification must not be registered for Grok; PermissionRequest handles approvals"
    );
    assert!(
        root["hooks"]["PreToolUse"][0]
            .get("_paneflow_managed")
            .is_none(),
        "Grok public docs do not document Paneflow-only markers"
    );
    let cmd = root["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command_preserves_event_arg(cmd, "PreToolUse"));
    drop(guard);
    assert!(!path.exists(), "drop must delete the dedicated hook file");
}

#[test]
fn hermes_guard_appends_block_and_strips_on_drop() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".hermes");
    std::fs::create_dir_all(&dir).unwrap();
    let user_yaml = "model: hermes-4\n# my comment\nverbose: true\n";
    std::fs::write(dir.join("config.yaml"), user_yaml).unwrap();

    let guard = HermesHookConfigGuard::install_at(&dir).expect("install must succeed");
    let content = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(content.starts_with(user_yaml), "user content untouched");
    assert!(content.contains(HERMES_BLOCK_BEGIN));
    assert!(content.contains("pre_llm_call:"));
    assert!(content.contains(" UserPromptSubmit"));
    assert!(content.contains(" PermissionRequest"));

    drop(guard);
    let content = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert_eq!(
        content, user_yaml,
        "drop must restore the file byte-identical (comments included)"
    );
}

#[test]
fn hermes_guard_refuses_when_user_has_hooks_key() {
    // A duplicate top-level `hooks:` key would silently override the
    // user's own hooks under PyYAML-family last-wins semantics.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".hermes");
    std::fs::create_dir_all(&dir).unwrap();
    let user_yaml = "hooks:\n  pre_tool_call:\n    - command: \"~/mine.sh\"\n";
    std::fs::write(dir.join("config.yaml"), user_yaml).unwrap();

    assert!(HermesHookConfigGuard::install_at(&dir).is_err());
    assert_eq!(
        std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
        user_yaml,
        "refusal must leave the file untouched"
    );
}

#[test]
fn hermes_guard_reinstall_is_idempotent_and_fresh_file_deleted() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".hermes");
    std::fs::create_dir_all(&dir).unwrap();

    // Simulate a previous process that died after writing the managed
    // block but before Drop. A real crash kills that process too, so no
    // live lease remains in this process.
    std::fs::write(dir.join("config.yaml"), hermes_managed_block()).unwrap();
    let g2 = HermesHookConfigGuard::install_at(&dir).unwrap();
    let content = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert_eq!(
        content.matches(HERMES_BLOCK_BEGIN).count(),
        1,
        "re-install must replace, not stack, the managed block"
    );
    drop(g2);
    // g2 was created over a file g1 made - created_file=false for g2, so
    // the file survives but holds no managed block.
    let content = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(strip_hermes_managed_block(&content).is_none());
    assert!(content.trim().is_empty());
}

#[test]
fn strip_hermes_block_handles_absent_and_partial_markers() {
    assert!(strip_hermes_managed_block("model: x\n").is_none());
    // Begin without end (truncated write) → refuse to strip.
    let partial = format!("a: 1\n{HERMES_BLOCK_BEGIN}\nhooks:\n");
    assert!(strip_hermes_managed_block(&partial).is_none());
}

// ---------- US-006: CodexHookConfigGuard (Unix) ----------

#[test]
fn codex_install_at_refuses_symlinked_hooks_file() {
    // #234: same guard as Claude's settings.local.json - a project-local
    // `.codex/hooks.json` FILE symlink must not be followed out of the repo.
    let td = tempfile::TempDir::new().unwrap();
    let outside = td.path().join("outside.json");
    let original = "{\"untouched\": true}\n";
    std::fs::write(&outside, original).unwrap();
    let codex_dir = td.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::os::unix::fs::symlink(&outside, codex_dir.join("hooks.json")).unwrap();

    assert!(
        CodexHookConfigGuard::install_at(&codex_dir, None).is_err(),
        "install_at must refuse a symlinked hooks.json"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), original);
}

#[test]
fn codex_install_at_creates_hooks_json_with_all_six_events() {
    let td = tempfile::TempDir::new().unwrap();
    let codex_dir = td.path().join(".codex");
    // Pass None for config.toml path so tests don't touch real `~/.codex`.
    let guard = CodexHookConfigGuard::install_at(&codex_dir, None)
        .expect("install_at on empty tempdir must succeed");

    let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();

    for event in CODEX_HOOK_EVENTS {
        let handlers = root["hooks"][*event].as_array().unwrap();
        assert_eq!(
            handlers.len(),
            1,
            "expected exactly one matcher-group for Codex {event}"
        );
        assert_eq!(
            handlers[0].get("_paneflow_managed"),
            Some(&json!(true)),
            "outer wrapper must carry the managed marker"
        );
        let cmd = handlers[0]
            .pointer("/hooks/0/command")
            .and_then(|v| v.as_str())
            .expect("command must be a string");
        assert!(
            is_paneflow_hook_command(cmd),
            "{event}: command {cmd:?} must be recognized as paneflow-managed"
        );
        assert!(
            command_preserves_event_arg(cmd, event),
            "{event}: command {cmd:?} must preserve the event name"
        );
    }

    // `Notification` is NOT a Codex hook - confirm the registration
    // respects the platform's actual event surface even though the
    // `paneflow-ai-hook` binary happens to accept that event name.
    assert!(
        root["hooks"].get("Notification").is_none(),
        "Codex hooks.json must not register a Notification event - it is not a Codex hook"
    );

    drop(guard);
    assert!(!codex_dir.join("hooks.json").exists());
    assert!(!codex_dir.exists());
}

#[test]
fn codex_install_at_preserves_user_hooks_and_cleanup() {
    let td = tempfile::TempDir::new().unwrap();
    let codex_dir = td.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let initial = json!({
        "hooks": {
            "PreToolUse": [
                { "hooks": [{ "type": "command", "command": "echo codex-user-hook" }] }
            ]
        }
    });
    std::fs::write(
        codex_dir.join("hooks.json"),
        serde_json::to_string_pretty(&initial).unwrap(),
    )
    .unwrap();

    let guard = CodexHookConfigGuard::install_at(&codex_dir, None).unwrap();
    let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let arr = root["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "user + paneflow entries coexist");

    drop(guard);
    let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let arr = root["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(
        arr[0].pointer("/hooks/0/command"),
        Some(&json!("echo codex-user-hook"))
    );
}

// ---------- US-006: TOML feature-flag mutation (Unix) ----------

#[test]
fn enable_codex_feature_flag_creates_block_on_empty_file() {
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("config.toml");

    let result = enable_codex_feature_flag(&path);
    assert!(result.unwrap(), "empty file should trigger an append");

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(CODEX_TOML_MARKER));
    assert!(content.contains("[features]"));
    assert!(content.contains("hooks = true"));
}

#[test]
fn enable_codex_feature_flag_noop_when_already_enabled() {
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("config.toml");
    std::fs::write(&path, "[features]\nhooks = true\nother = false\n").unwrap();

    let result = enable_codex_feature_flag(&path);
    assert!(!result.unwrap(), "already-enabled must be a no-op");

    // File unchanged.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains(CODEX_TOML_MARKER));
}

#[test]
fn enable_codex_feature_flag_concurrent_no_duplicate_features() {
    // US-027: two concurrent shims racing to enable the flag must not
    // produce a duplicate `[features]` section (invalid TOML). The flock
    // serializes the read-modify-write, so the second caller re-reads the
    // now-updated config and no-ops.
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("config.toml");
    std::fs::write(&path, "model = \"gpt-5\"\n").unwrap();

    let p1 = path.clone();
    let p2 = path.clone();
    let t1 = std::thread::spawn(move || enable_codex_feature_flag(&p1));
    let t2 = std::thread::spawn(move || enable_codex_feature_flag(&p2));
    let _ = t1.join();
    let _ = t2.join();

    let content = std::fs::read_to_string(&path).unwrap();
    let features = content.lines().filter(|l| l.trim() == "[features]").count();
    assert_eq!(
        features, 1,
        "exactly one [features] section after a concurrent enable, got:\n{content}"
    );
    assert!(content.contains("hooks = true"));
}

#[test]
fn enable_codex_feature_flag_abstains_on_commented_features_header() {
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("config.toml");
    std::fs::write(&path, "[features] # experimental\nfoo = true\n").unwrap();

    let result = enable_codex_feature_flag(&path);
    assert!(result.is_err());

    let content = std::fs::read_to_string(&path).unwrap();
    let features = content
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            if line.starts_with('#') {
                return false;
            }
            line.split_once('#').map_or(line, |(value, _)| value).trim() == "[features]"
        })
        .count();
    assert_eq!(
        features, 1,
        "must not append a second [features]:\n{content}"
    );
    assert!(!content.contains("hooks = true"));
}

#[test]
fn enable_codex_feature_flag_ignores_hooks_key_outside_features() {
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("config.toml");
    std::fs::write(&path, "[mcp]\nhooks = true\n").unwrap();

    let result = enable_codex_feature_flag(&path);
    assert!(
        result.unwrap(),
        "hooks = true outside [features] must not skip the install"
    );

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(CODEX_TOML_MARKER));
    assert!(content.contains("[features]"));
    assert_eq!(
        content.matches("hooks = true").count(),
        2,
        "mcp block keeps its hooks key and [features] must add one:\n{content}"
    );
    assert!(
        content.contains("[features]\nhooks = true\n"),
        "features block must still set hooks = true:\n{content}"
    );
}

#[test]
fn enable_codex_feature_flag_abstains_on_existing_features_section() {
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("config.toml");
    // User already has `[features]` without `hooks` - appending
    // another `[features]` would trigger a duplicate-section TOML
    // parse error on Codex's side, so the shim must abstain.
    std::fs::write(&path, "[features]\nother_flag = false\n").unwrap();

    let result = enable_codex_feature_flag(&path);
    assert!(result.is_err());

    // File untouched.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains(CODEX_TOML_MARKER));
    assert!(!content.contains("hooks = true"));
}

#[test]
fn enable_codex_feature_flag_recognizes_equivalent_features_headers() {
    for header in ["[ features ]", "[\"features\"]", "['features']"] {
        let td = tempfile::TempDir::new().unwrap();
        let with_hooks = td.path().join("with-hooks.toml");
        std::fs::write(&with_hooks, format!("{header}\nhooks = true\n")).unwrap();
        assert!(
            !enable_codex_feature_flag(&with_hooks).unwrap(),
            "{header} with hooks = true must skip install"
        );
        let with_hooks_content = std::fs::read_to_string(&with_hooks).unwrap();
        assert!(
            !with_hooks_content.contains(CODEX_TOML_MARKER),
            "{header} with hooks must be left alone:\n{with_hooks_content}"
        );
        assert_eq!(
            with_hooks_content.matches("[features]").count(),
            0,
            "{header} must not gain a second [features]:\n{with_hooks_content}"
        );

        let without_hooks = td.path().join("without-hooks.toml");
        std::fs::write(&without_hooks, format!("{header}\nother_flag = false\n")).unwrap();
        assert!(
            enable_codex_feature_flag(&without_hooks).is_err(),
            "{header} without hooks must abstain rather than append [features]"
        );
        let without_hooks_content = std::fs::read_to_string(&without_hooks).unwrap();
        assert_eq!(
            without_hooks_content,
            format!("{header}\nother_flag = false\n"),
            "{header} without hooks must be untouched"
        );
    }
}

#[test]
fn codex_guard_wires_feature_flag_through_config_toml() {
    let td = tempfile::TempDir::new().unwrap();
    let codex_dir = td.path().join(".codex");
    let config_toml = td.path().join("config.toml");
    let user_config = "[user_stuff]\nkey = 1\n";
    std::fs::write(&config_toml, user_config).unwrap();

    let guard = CodexHookConfigGuard::install_at(&codex_dir, Some(&config_toml)).unwrap();

    let toml_content = std::fs::read_to_string(&config_toml).unwrap();
    assert!(toml_content.contains("hooks = true"));
    assert!(toml_content.contains(CODEX_TOML_MARKER));

    drop(guard);
    assert_eq!(
        std::fs::read_to_string(config_toml).unwrap(),
        user_config,
        "feature cleanup must restore the user's TOML byte-for-byte"
    );
}

// ---------- Hook-command detection (basename rule) ----------

/// The legacy bare-name format MUST stay recognized so a shim upgrade
/// can clean up `settings.local.json` files written by the previous
/// version (which used `format!("paneflow-ai-hook {event}")` directly).
#[test]
fn is_paneflow_hook_command_accepts_legacy_bare_name() {
    for event in CLAUDE_HOOK_EVENTS {
        let cmd = format!("paneflow-ai-hook {event}");
        assert!(
            is_paneflow_hook_command(&cmd),
            "legacy bare-name format must be recognized: {cmd:?}"
        );
    }
}

/// New absolute-path format produced by `resolve_hook_command` when a
/// sibling binary is present. This is the production case for end users.
#[test]
fn is_paneflow_hook_command_accepts_unix_absolute_path() {
    let cmd = "/home/user/.cache/paneflow/bin/0.1.0/paneflow-ai-hook Stop";
    assert!(is_paneflow_hook_command(cmd));

    let cmd = "/usr/local/bin/paneflow-ai-hook PreToolUse";
    assert!(is_paneflow_hook_command(cmd));
}

/// Fix B (orphan cleanup): even if the binary at the absolute path no
/// longer exists on disk, the command must still be recognized so
/// `remove_paneflow_hooks` can purge stale entries written by an
/// earlier paneflow install that has since been removed.
#[test]
fn is_paneflow_hook_command_recognizes_orphans_without_filesystem_check() {
    // Path that almost certainly does not exist - the function must NOT
    // touch the filesystem.
    let cmd = "/nonexistent/old/cache/paneflow-ai-hook UserPromptSubmit";
    assert!(
        is_paneflow_hook_command(cmd),
        "orphaned absolute paths must be detectable for cleanup"
    );
}

/// User hooks must NOT be misclassified as paneflow-managed. The
/// basename rule narrows the namespace collision risk vs. the previous
/// bare-prefix rule, but rejection of common user patterns is the
/// primary safety property.
#[test]
fn is_paneflow_hook_command_rejects_user_hooks() {
    let user_hooks = [
        "echo hello",
        "/usr/bin/git status",
        "node my-hook.js",
        "paneflow-shim Stop",               // sibling binary, different name
        "my-paneflow-ai-hook Stop",         // similar but distinct basename
        "/path/to/paneflow-ai-hook-2 Stop", // suffixed name
        "",                                 // empty
        "   ",                              // whitespace only
        "notarealcommand",                  // no event
    ];
    for cmd in user_hooks {
        assert!(
            !is_paneflow_hook_command(cmd),
            "user hook {cmd:?} must NOT be classified as paneflow-managed"
        );
    }
}

/// Round-trip property: `resolve_hook_command` must produce a string
/// that `is_paneflow_hook_command` recognizes, regardless of which
/// branch (sibling-found or bare-name fallback) was taken. Without
/// this, a user could end up with hooks they cannot clean up.
#[test]
fn resolve_hook_command_output_is_recognized_by_detector() {
    for event in CLAUDE_HOOK_EVENTS {
        let cmd = resolve_hook_command(event);
        assert!(
            is_paneflow_hook_command(&cmd),
            "resolve_hook_command output must be detectable: {cmd:?}"
        );
        assert!(
            command_preserves_event_arg(&cmd, event),
            "resolve_hook_command output must preserve the event name: {cmd:?}"
        );
    }
}

#[test]
fn dsh_guard_writes_hooks_and_overlay_and_removes_both_on_drop() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    let guard = DshOverlayGuard::install_at(&dir).expect("install must succeed");
    // The guard keys its files on the canonical directory (macOS temp dirs
    // sit behind the /var -> /private/var symlink).
    let dir = std::fs::canonicalize(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);

    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
    for event in ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"] {
        assert!(
            root["hooks"][event].is_array(),
            "{event} must be registered for the DeepSeek Harness bridge"
        );
    }
    assert!(
        root["hooks"].get("Notification").is_none(),
        "dsh-hooks-claude-code emits no Notification event"
    );
    let cmd = root["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command_preserves_event_arg(cmd, "Stop"));

    let overlay = std::fs::read_to_string(&overlay_path).unwrap();
    assert!(overlay.starts_with("- insert:"));
    assert!(overlay.contains("name: '@deepseek-ai/dsh-hooks-claude-code'"));
    assert!(overlay.contains(&hooks_path.to_string_lossy().to_string()));
    assert_eq!(guard.overlay_path(), overlay_path);

    drop(guard);
    assert!(!overlay_path.exists(), "drop must delete the overlay");
    assert!(!hooks_path.exists(), "drop must delete the hook config");
}

#[test]
fn dsh_overlay_escapes_a_single_quote_in_the_config_path() {
    let overlay = render_overlay(std::path::Path::new("/it's/hooks.json"));
    assert!(
        overlay.contains("configPath: '/it''s/hooks.json'"),
        "a single quote must be doubled inside a single-quoted YAML scalar, got {overlay}"
    );
}

#[test]
fn dsh_patch_overlay_leads_the_launcher_flags() {
    let overlay = std::path::Path::new("/tmp/overlay.yml");
    for argv in [
        vec!["--profile", "tui"],
        vec!["--profile=tui"],
        vec!["--profile", "tui", "--resume", "abc"],
        vec!["-p", "tui"],
        vec!["-p", "tui", "chat"],
        vec!["-p", "tui", "--resume", "abc"],
    ] {
        let args: Vec<std::ffi::OsString> = argv.iter().map(std::ffi::OsString::from).collect();
        let patched = crate::with_dsh_patch_overlay(args.clone(), overlay);
        assert_eq!(patched[0], "--patch", "{argv:?}");
        assert_eq!(patched[1], overlay.as_os_str(), "{argv:?}");
        assert_eq!(&patched[2..], args, "{argv:?}");
    }
}

#[test]
fn dsh_patch_overlay_stays_out_of_plugin_help_version_and_dumps() {
    let overlay = std::path::Path::new("/tmp/overlay.yml");
    for argv in [
        vec!["plugin", "--profile", "tui", "add", "pkg"],
        vec!["--profile", "tui", "plugin", "add", "pkg"],
        vec!["--profile=tui", "plugin", "add", "pkg"],
        vec!["-p", "tui", "plugin", "add", "pkg"],
        vec!["-p", "tui", "--patch", "extra.yml", "plugin", "add", "pkg"],
        vec!["--from-default-profile", "web", "plugin", "add", "pkg"],
        vec!["--patch", "extra.yml", "plugin", "add", "pkg"],
        vec!["--help"],
        vec!["-h"],
        vec!["--version"],
        vec!["-V"],
        vec!["--profile", "tui", "--dump-config"],
        vec!["--profile", "tui", "--dump-default-config"],
    ] {
        let args: Vec<std::ffi::OsString> = argv.iter().map(std::ffi::OsString::from).collect();
        assert_eq!(
            crate::with_dsh_patch_overlay(args.clone(), overlay),
            args,
            "{argv:?} must reach dsh untouched"
        );
    }
}

#[test]
fn dsh_patch_overlay_ignores_opt_out_flags_after_a_double_dash() {
    // After `--` every token is a positional for the subcommand, so
    // `dsh chat -- --version` is a chat session and still gets the overlay.
    let overlay = std::path::Path::new("/tmp/overlay.yml");
    for argv in [
        vec!["chat", "--", "--version"],
        vec!["chat", "--", "-V"],
        vec!["chat", "--", "--help"],
        vec!["chat", "--", "-h"],
        vec!["-p", "tui", "chat", "--", "--dump-config"],
        vec!["--", "--dump-default-config"],
    ] {
        let args: Vec<std::ffi::OsString> = argv.iter().map(std::ffi::OsString::from).collect();
        let patched = crate::with_dsh_patch_overlay(args.clone(), overlay);
        assert_eq!(patched[0], "--patch", "{argv:?}");
        assert_eq!(patched[1], overlay.as_os_str(), "{argv:?}");
        assert_eq!(&patched[2..], args, "{argv:?}");
    }
}

#[test]
fn dsh_drop_refuses_a_directory_swapped_for_a_symlink() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    let guard = DshOverlayGuard::install_at(&dir).expect("install must succeed");
    let dir = std::fs::canonicalize(&dir).unwrap();
    let hooks_source = std::fs::read_to_string(dir.join(DSH_HOOKS_BASENAME)).unwrap();
    let overlay_source = std::fs::read_to_string(dir.join(DSH_OVERLAY_BASENAME)).unwrap();

    // Swap the overlay directory for a symlink to a user-managed directory
    // holding byte-identical files.
    let elsewhere = td.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join(DSH_HOOKS_BASENAME), &hooks_source).unwrap();
    std::fs::write(elsewhere.join(DSH_OVERLAY_BASENAME), &overlay_source).unwrap();
    let parked = td.path().join("parked");
    std::fs::rename(&dir, &parked).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();

    drop(guard);
    assert_eq!(
        std::fs::read_to_string(elsewhere.join(DSH_HOOKS_BASENAME)).unwrap(),
        hooks_source,
        "drop must not delete through a swapped-in directory symlink"
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere.join(DSH_OVERLAY_BASENAME)).unwrap(),
        overlay_source
    );
}

#[test]
fn dsh_drop_refuses_an_ancestor_swapped_for_a_symlink() {
    let td = tempfile::TempDir::new().unwrap();
    let dsh_home = td.path().join(".dsh");
    let dir = dsh_home.join("paneflow");
    let guard = DshOverlayGuard::install_at(&dir).expect("install must succeed");
    let dir = std::fs::canonicalize(&dir).unwrap();
    let hooks_source = std::fs::read_to_string(dir.join(DSH_HOOKS_BASENAME)).unwrap();
    let overlay_source = std::fs::read_to_string(dir.join(DSH_OVERLAY_BASENAME)).unwrap();

    // Swap the `.dsh` ancestor for a symlink; the final `paneflow` component
    // behind it is a real directory holding byte-identical user files.
    let elsewhere = td.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("paneflow")).unwrap();
    std::fs::write(
        elsewhere.join("paneflow").join(DSH_HOOKS_BASENAME),
        &hooks_source,
    )
    .unwrap();
    std::fs::write(
        elsewhere.join("paneflow").join(DSH_OVERLAY_BASENAME),
        &overlay_source,
    )
    .unwrap();
    std::fs::rename(&dsh_home, td.path().join("parked")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &dsh_home).unwrap();

    drop(guard);
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("paneflow").join(DSH_HOOKS_BASENAME)).unwrap(),
        hooks_source,
        "drop must not delete through a swapped-in ancestor symlink"
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("paneflow").join(DSH_OVERLAY_BASENAME)).unwrap(),
        overlay_source
    );
}

#[test]
fn dsh_patch_overlay_yields_to_a_user_supplied_patch() {
    let overlay = std::path::Path::new("/tmp/overlay.yml");
    for argv in [
        vec!["--patch", "mine.yml", "chat"],
        vec!["--patch=mine.yml", "chat"],
        vec!["--profile", "tui", "--patch", "mine.yml"],
    ] {
        let args: Vec<std::ffi::OsString> = argv.iter().map(std::ffi::OsString::from).collect();
        assert_eq!(
            crate::with_dsh_patch_overlay(args.clone(), overlay),
            args,
            "{argv:?} must keep the user's --patch alone"
        );
    }
    // A `--patch` after `--` is a positional for the subcommand, not dsh's.
    let args: Vec<std::ffi::OsString> = ["chat", "--", "--patch"]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    let patched = crate::with_dsh_patch_overlay(args.clone(), overlay);
    assert_eq!(patched[0], "--patch");
    assert_eq!(&patched[2..], args);
}

#[test]
fn dsh_preexisting_files_survive_install() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    std::fs::write(&hooks_path, "{\"user\": true}\n").unwrap();
    std::fs::write(&overlay_path, "- insert:\n    - id: user\n").unwrap();

    let error = match DshOverlayGuard::install_at(&dir) {
        Ok(_) => panic!("pre-existing DSH files must not be overwritten"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(&hooks_path).unwrap(),
        "{\"user\": true}\n"
    );
    assert_eq!(
        std::fs::read_to_string(&overlay_path).unwrap(),
        "- insert:\n    - id: user\n"
    );
}

#[test]
fn dsh_sibling_instance_hooks_file_is_shared_not_owned() {
    // Two PaneFlow instances (different `PANEFLOW_BIN_DIR`) render different
    // hooks.json bytes for the same hooks. The second one must still get its
    // overlay (and so its `--patch`), and must never delete the first one's
    // file.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    let program = sibling_hook_program(&td.path().join("elsewhere"));
    let sibling = render_as_sibling_instance(&hooks_source().unwrap(), &program);
    assert_ne!(sibling, hooks_source().unwrap());
    std::fs::write(&hooks_path, &sibling).unwrap();

    let guard = DshOverlayGuard::install_at(&dir)
        .expect("a sibling instance's hooks.json must serve this session");
    assert!(overlay_path.exists(), "the overlay must be installed");
    assert_eq!(
        guard.overlay_path(),
        std::fs::canonicalize(&overlay_path).unwrap()
    );
    assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), sibling);

    drop(guard);
    assert!(!overlay_path.exists(), "drop must delete the overlay");
    assert_eq!(
        std::fs::read_to_string(&hooks_path).unwrap(),
        sibling,
        "the sibling instance's hooks.json is not ours to delete"
    );
}

#[test]
fn dsh_last_session_removes_a_sibling_rendering_paneflow_created() {
    // Instance A created hooks.json (ownership bit set) and exits first;
    // instance B adopted it and exits last, so B removes it: the bit proves
    // PaneFlow wrote it and the shape check proves nobody added to it.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    let mut instance_a = crate::hooks::HookLease::acquire(&hooks_path).unwrap();
    let program = sibling_hook_program(&td.path().join("elsewhere"));
    std::fs::write(
        &hooks_path,
        render_as_sibling_instance(&hooks_source().unwrap(), &program),
    )
    .unwrap();
    instance_a.mark_created().unwrap();

    let instance_b = DshOverlayGuard::install_at(&dir).unwrap();
    drop(instance_a);
    assert!(hooks_path.exists() && overlay_path.exists());
    drop(instance_b);
    assert!(!overlay_path.exists());
    assert!(
        !hooks_path.exists(),
        "the last session must remove a hooks.json PaneFlow created"
    );
}

#[test]
fn dsh_hooks_file_with_a_user_command_is_refused() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    let program = sibling_hook_program(&td.path().join("elsewhere"));
    let sibling = render_as_sibling_instance(&hooks_source().unwrap(), &program);
    let mut with_user: serde_json::Value = serde_json::from_str(&sibling).unwrap();
    with_user["hooks"]["Stop"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "command", "command": "my-hook Stop"}));
    for user in [
        sibling.replacen(
            &format!("{} Stop", program.to_string_lossy()),
            "my-hook Stop",
            1,
        ),
        with_user.to_string(),
    ] {
        std::fs::write(&hooks_path, &user).unwrap();
        let error = match DshOverlayGuard::install_at(&dir) {
            Ok(_) => panic!("a hooks.json with a user command must be refused: {user}"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), user);
        assert!(
            !overlay_path.exists(),
            "a refused install must not leave an overlay behind"
        );
    }
}

#[test]
fn dsh_preexisting_overlay_survives_when_hooks_would_be_created() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    std::fs::write(&overlay_path, "- insert:\n    - id: user\n").unwrap();

    let error = match DshOverlayGuard::install_at(&dir) {
        Ok(_) => panic!("a pre-existing overlay must not be overwritten"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        !hooks_path.exists(),
        "a failed overlay install must roll back a hooks.json this session created"
    );
    assert_eq!(
        std::fs::read_to_string(&overlay_path).unwrap(),
        "- insert:\n    - id: user\n"
    );
}

#[test]
fn dsh_mid_session_edits_survive_drop() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    let guard = DshOverlayGuard::install_at(&dir).expect("install must succeed");
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    std::fs::write(&hooks_path, "user hooks").unwrap();
    std::fs::write(&overlay_path, "user overlay").unwrap();

    drop(guard);
    assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), "user hooks");
    assert_eq!(
        std::fs::read_to_string(&overlay_path).unwrap(),
        "user overlay"
    );
}

#[test]
fn dsh_symlink_file_is_refused_and_target_is_unchanged() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    std::fs::create_dir_all(&dir).unwrap();
    let target = td.path().join("user-hooks.json");
    std::fs::write(&target, "{\"keep\": true}\n").unwrap();
    std::os::unix::fs::symlink(&target, dir.join(DSH_HOOKS_BASENAME)).unwrap();

    let error = match DshOverlayGuard::install_at(&dir) {
        Ok(_) => panic!("a symlinked hooks.json must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "{\"keep\": true}\n"
    );
}

#[test]
fn dsh_created_files_are_removed_by_the_last_session() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".dsh/paneflow");
    let first = DshOverlayGuard::install_at(&dir).unwrap();
    let second = DshOverlayGuard::install_at(&dir).unwrap();
    let hooks_path = dir.join(DSH_HOOKS_BASENAME);
    let overlay_path = dir.join(DSH_OVERLAY_BASENAME);
    assert!(hooks_path.exists());
    assert!(overlay_path.exists());

    drop(first);
    assert!(
        hooks_path.exists() && overlay_path.exists(),
        "an earlier session must leave the files for the last one"
    );
    drop(second);
    assert!(
        !hooks_path.exists() && !overlay_path.exists(),
        "the last session must remove the files PaneFlow created"
    );
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn muse_guard_writes_hooks_file_and_managed_settings_and_removes_both_on_drop() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    let guard = MuseHookConfigGuard::install_at(&dir).expect("install must succeed");
    let hooks_path = dir.join(MUSE_HOOKS_BASENAME);
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    assert_eq!(guard.hooks_path(), hooks_path);

    let hooks = read_json(&hooks_path);
    for (event, canonical) in MUSE_HOOK_EVENTS {
        let cmd = hooks["hooks"][*event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("{event} must be registered for Muse Code"));
        assert!(
            command_preserves_event_arg(cmd, canonical),
            "{event} must dispatch as {canonical}, got {cmd}"
        );
    }
    assert!(
        hooks["hooks"].get("Notification").is_none(),
        "Muse Code documents no Notification event"
    );
    let settings = read_json(&settings_path);
    assert_eq!(settings["schema_version"], json!(1));
    assert_eq!(
        settings["managed_hooks_path"].as_str().unwrap(),
        hooks_path.to_str().unwrap()
    );
    let env_vars = settings["managed_hooks_env_vars"].as_array().unwrap();
    for name in MUSE_HOOK_ENV_VARS {
        assert!(
            env_vars.iter().any(|entry| entry.as_str() == Some(name)),
            "{name} must be forwarded to managed hooks"
        );
    }

    drop(guard);
    assert!(!hooks_path.exists(), "drop must delete the hook file");
    assert!(
        !settings_path.exists(),
        "drop must delete a settings file Paneflow created"
    );
    assert!(
        !dir.exists(),
        "drop must delete a config dir Paneflow created"
    );
}

#[test]
fn muse_guard_preserves_user_settings_and_env_vars() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({
        "schema_version": 1,
        "model": "muse-spark-1.3-contributor",
        "managed_hooks_env_vars": ["MY_VAR"],
        "hooks": {"Stop": []}
    });
    std::fs::write(&settings_path, original.to_string()).unwrap();

    let guard = MuseHookConfigGuard::install_at(&dir).expect("install must succeed");
    let merged = read_json(&settings_path);
    assert_eq!(merged["model"], json!("muse-spark-1.3-contributor"));
    assert_eq!(merged["hooks"], json!({"Stop": []}));
    let env_vars = merged["managed_hooks_env_vars"].as_array().unwrap();
    assert_eq!(env_vars[0], json!("MY_VAR"));
    assert_eq!(env_vars.len(), 1 + MUSE_HOOK_ENV_VARS.len());

    drop(guard);
    assert_eq!(read_json(&settings_path), original);
    assert!(dir.exists(), "a pre-existing config dir must survive");
}

#[test]
fn muse_guard_refuses_a_foreign_managed_hooks_path() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({
        "schema_version": 1,
        "managed_hooks_path": "/etc/muse/enterprise-hooks.json"
    });
    std::fs::write(&settings_path, original.to_string()).unwrap();

    assert!(MuseHookConfigGuard::install_at(&dir).is_err());
    assert_eq!(read_json(&settings_path), original);
    assert!(!dir.join(MUSE_HOOKS_BASENAME).exists());
}

#[test]
fn muse_guard_refuses_invalid_settings_json() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    std::fs::write(&settings_path, "{not json").unwrap();

    assert!(MuseHookConfigGuard::install_at(&dir).is_err());
    assert_eq!(
        std::fs::read_to_string(&settings_path).unwrap(),
        "{not json"
    );
}

#[test]
fn muse_remove_leaves_a_foreign_managed_hooks_path_alone() {
    let mut root = json!({
        "schema_version": 1,
        "managed_hooks_path": "/etc/muse/enterprise-hooks.json",
        "managed_hooks_env_vars": ["PANEFLOW_SURFACE_ID"]
    });
    let before = root.clone();
    remove_muse_settings(
        &mut root,
        std::path::Path::new("/home/u/.config/muse/paneflow-hooks.json"),
    );
    assert_eq!(root, before);
}

#[test]
fn muse_remove_leaves_a_foreign_managed_hooks_path_with_our_basename_alone() {
    let mut root = json!({
        "schema_version": 1,
        "managed_hooks_path": "/etc/muse/paneflow-hooks.json",
        "managed_hooks_env_vars": ["PANEFLOW_SURFACE_ID", "PANEFLOW_SOCKET_PATH"]
    });
    let before = root.clone();
    remove_muse_settings(
        &mut root,
        std::path::Path::new("/home/u/.config/muse/paneflow-hooks.json"),
    );
    assert_eq!(
        root, before,
        "same basename under a foreign directory is not ours"
    );

    let mut ours = before.clone();
    remove_muse_settings(
        &mut ours,
        std::path::Path::new("/etc/muse/paneflow-hooks.json"),
    );
    assert_eq!(
        ours,
        json!({"schema_version": 1}),
        "the exact path is stripped"
    );
}

#[test]
fn muse_guard_never_adds_schema_version_to_a_pre_existing_settings_file() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({"model": "x"});
    std::fs::write(&settings_path, original.to_string()).unwrap();

    let guard = MuseHookConfigGuard::install_at(&dir).expect("install must succeed");
    let merged = read_json(&settings_path);
    assert!(
        merged.get("schema_version").is_none(),
        "a pre-existing file that lacked schema_version must not gain one"
    );
    assert!(merged.get("managed_hooks_path").is_some());
    assert!(merged.get("managed_hooks_env_vars").is_some());

    drop(guard);
    assert_eq!(read_json(&settings_path), original);
}
