use crate::hooks::muse::{
    env_baseline_path, record_env_baseline_for_test, recorded_env_baseline_for_test,
    remove_muse_settings, MuseHookConfigGuard, MUSE_HOOKS_BASENAME, MUSE_HOOK_ENV_VARS,
    MUSE_HOOK_EVENTS, MUSE_SETTINGS_BASENAME,
};
use crate::hooks::PANEFLOW_TS_BASENAME;
use crate::hooks::{
    enable_codex_feature_flag, CodexHookConfigGuard, CODEX_HOOK_EVENTS, CODEX_TOML_MARKER,
};
use crate::hooks::{
    is_paneflow_hook_command, merge_cursor_hooks, merge_gemini_hooks, remove_cursor_hooks,
    remove_gemini_hooks, resolve_hook_command, GrokHookFileGuard, InvalidJsonPolicy,
    ManagedHookConfigGuard, ManagedHookSpec, OpenCodePluginGuard, CLAUDE_HOOK_EVENTS,
};
use serde_json::json;

fn command_preserves_event_arg(command: &str, event: &str) -> bool {
    command.ends_with(&format!(" {event}"))
        || command.ends_with(&format!(" {event}\\\""))
        || command.contains(&format!(" {event} "))
}

// ---------- Multi-agent: clones + JSON/TS/YAML guards ----------

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
fn gemini_install_preserves_commented_settings() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".gemini");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    let original =
        "{\n  // Gemini accepts comments in settings.json\n  \"theme\": \"Default\"\n}\n";
    std::fs::write(&path, original).unwrap();

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
    )
    .expect("commented Gemini settings must install");

    let installed = std::fs::read_to_string(&path).unwrap();
    assert!(
        installed.contains("// Gemini accepts comments in settings.json"),
        "comment must survive install:\n{installed}"
    );
    let parsed = paneflow_agent_config::jsonc::parse(&installed).expect("installed JSONC");
    assert_eq!(parsed["theme"], json!("Default"));
    for event in ["BeforeAgent", "AfterAgent", "BeforeTool", "AfterTool"] {
        let command = parsed["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or("");
        assert!(
            is_paneflow_hook_command(command),
            "{event} hook missing from {installed}"
        );
    }

    drop(guard);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original.as_bytes(),
        "drop must restore the original bytes"
    );
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
fn enable_codex_feature_flag_keeps_inline_and_dotted_enabled_features() {
    // Issue #1053: `features` spelled as an inline table or as dotted keys is
    // already the features table. Appending `[features]` would declare it a
    // second time and make the whole Codex config invalid.
    let fixtures = [
        "model = \"gpt-5\"\nfeatures = { hooks = true }\n\n[profiles.default]\nmodel = \"o3\"\n",
        "model = \"gpt-5\"\nfeatures.hooks = true\n\n[profiles.default]\nmodel = \"o3\"\n",
        "features = { other = 1, \"hooks\" = true } # inline\n",
        "\"features\" . hooks = true # dotted, quoted, spaced\nfeatures.other = false\n",
        "features.codex_hooks = true\n",
        // A multi-line string that looks like a table header must not end
        // the root table before the real dotted key.
        "notes = \"\"\"\n[other]\n\"\"\"\nfeatures.hooks = true\n",
    ];
    for original in fixtures {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, original).unwrap();

        assert!(
            !enable_codex_feature_flag(&path).unwrap(),
            "an enabled hooks feature must be a no-op:\n{original}"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, original, "bytes must be unchanged");
        assert!(
            !content.contains("[features]") && !content.contains(CODEX_TOML_MARKER),
            "must not declare features a second time:\n{content}"
        );
        let table: toml::Table = toml::from_str(&content).unwrap();
        assert!(table["features"].is_table(), "{content}");
    }
}

#[test]
fn enable_codex_feature_flag_refuses_unsupported_inline_and_dotted_features() {
    // Issue #1053: an existing `features` definition that does not enable
    // hooks cannot be extended by appending a table, so refuse untouched.
    let fixtures = [
        "features = { other = true }\n",
        "features = {}\n",
        "features = { hooks = false }\n",
        "features.hooks = false\n",
        "features.other = true\n",
        "'features'.other = true\n",
        "[[features]]\nhooks = true\n",
        // A `[features.hooks]` table already defines the flag itself.
        "[features.hooks]\nx = 1\n",
        "[[features.hooks]]\nx = 1\n",
        "[features.hooks.deep]\nx = 1\n",
        "[features.codex_hooks]\nx = 1\n",
    ];
    for original in fixtures {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, original).unwrap();

        assert!(
            enable_codex_feature_flag(&path).is_err(),
            "an existing features definition without hooks must refuse:\n{original}"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, original, "a refusal must not write");
        assert!(toml::from_str::<toml::Table>(&content).is_ok(), "{content}");
    }
}

#[test]
fn enable_codex_feature_flag_refuses_config_it_cannot_read() {
    // Content the scanner cannot follow is not valid TOML either; appending
    // to it cannot help, so refuse and leave the bytes alone.
    for original in ["features = [1, 2\n", "model = \"gpt-5\n"] {
        assert!(toml::from_str::<toml::Table>(original).is_err());
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, original).unwrap();

        assert!(
            enable_codex_feature_flag(&path).is_err(),
            "unreadable config must refuse:\n{original}"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, original, "a refusal must not write");
    }
}

#[test]
fn enable_codex_feature_flag_ignores_features_keys_that_are_not_root_features() {
    // `features` nested under another table, or inside a string value, is
    // not the root features table, so the install still appends one.
    let fixtures = [
        "[profiles.default]\nfeatures.hooks = true\n",
        "[profiles.default]\nfeatures = { hooks = true }\n",
        "notes = \"features.hooks = true\"\n",
        "notes = '''\nfeatures = { hooks = true }\n'''\n",
    ];
    for original in fixtures {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, original).unwrap();

        assert!(
            enable_codex_feature_flag(&path).unwrap(),
            "a non-root features key must not skip the install:\n{original}"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with(original) && content.ends_with("[features]\nhooks = true\n"),
            "the managed block must be appended after the user's bytes:\n{content}"
        );
        let table: toml::Table = toml::from_str(&content).unwrap();
        assert_eq!(
            table["features"]["hooks"].as_bool(),
            Some(true),
            "{content}"
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
        &[],
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
        &[],
    );
    assert_eq!(
        root, before,
        "same basename under a foreign directory is not ours"
    );

    let mut ours = before.clone();
    remove_muse_settings(
        &mut ours,
        std::path::Path::new("/etc/muse/paneflow-hooks.json"),
        &[],
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

#[test]
fn muse_install_refusing_a_user_hook_file_leaves_user_managed_settings_alone() {
    // The user pointed managed_hooks_path at the reserved path themselves
    // and wrote their own hook file there. The hook file is refused, and
    // the rollback must not strip the user's settings on the way out.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(MUSE_HOOKS_BASENAME);
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let user_hooks = "{\"hooks\": {\"Stop\": []}}\n";
    std::fs::write(&hooks_path, user_hooks).unwrap();
    let original = json!({
        "schema_version": 1,
        "managed_hooks_path": hooks_path.to_str().unwrap(),
        "managed_hooks_env_vars": ["PANEFLOW_SOCKET_PATH", "MY_VAR"]
    });
    std::fs::write(&settings_path, original.to_string()).unwrap();

    let error = match MuseHookConfigGuard::install_at(&dir) {
        Ok(_) => panic!("a user hook file at the reserved path must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), user_hooks);
    assert_eq!(
        read_json(&settings_path),
        original,
        "settings the user wrote must survive the rollback"
    );
    assert!(!env_baseline_path(&settings_path).exists());
}

#[test]
fn muse_orphan_sweep_leaves_user_managed_settings_without_a_baseline_alone() {
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(MUSE_HOOKS_BASENAME);
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({
        "schema_version": 1,
        "managed_hooks_path": hooks_path.to_str().unwrap(),
        "managed_hooks_env_vars": ["PANEFLOW_SOCKET_PATH"]
    });
    std::fs::write(&settings_path, original.to_string()).unwrap();

    crate::hooks::muse::sweep_orphan_for_test(&dir);
    assert_eq!(
        read_json(&settings_path),
        original,
        "no baseline sidecar means PaneFlow never took this file"
    );
}

#[test]
fn muse_guard_keeps_a_pre_existing_paneflow_env_var_forward() {
    // The user listed PANEFLOW_SOCKET_PATH themselves, with no managed
    // path. Two sessions overlap; the last one to exit must give back the
    // file exactly as the user left it, even though it never saw the
    // original.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({
        "schema_version": 1,
        "managed_hooks_env_vars": ["PANEFLOW_SOCKET_PATH", "MY_VAR"]
    });
    std::fs::write(&settings_path, original.to_string()).unwrap();

    let first = MuseHookConfigGuard::install_at(&dir).expect("install must succeed");
    assert_eq!(
        recorded_env_baseline_for_test(&settings_path).as_deref(),
        Some(&["PANEFLOW_SOCKET_PATH".to_owned()][..]),
        "the first session records the user's own PANEFLOW_* names"
    );
    assert!(
        !env_baseline_path(&settings_path).exists(),
        "nothing is written into the Muse config directory for the record"
    );
    let second = MuseHookConfigGuard::install_at(&dir).expect("a second session joins");
    let merged = read_json(&settings_path);
    let env_vars = merged["managed_hooks_env_vars"].as_array().unwrap();
    assert_eq!(env_vars[0], json!("PANEFLOW_SOCKET_PATH"));
    assert_eq!(env_vars[1], json!("MY_VAR"));
    assert_eq!(env_vars.len(), 1 + MUSE_HOOK_ENV_VARS.len());

    drop(first);
    assert!(
        recorded_env_baseline_for_test(&settings_path).is_some(),
        "an earlier session leaves the record for the last one"
    );
    drop(second);
    assert_eq!(read_json(&settings_path), original);
    assert_eq!(
        recorded_env_baseline_for_test(&settings_path),
        None,
        "the last session clears the record"
    );
    assert!(dir.exists());
}

#[test]
fn muse_orphan_sweep_keeps_a_pre_existing_paneflow_env_var_forward() {
    // A crashed session left the managed keys and its record behind; the
    // orphan sweep on the next launch restores the user's file.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let hooks_path = dir.join(MUSE_HOOKS_BASENAME);
    let original = json!({
        "schema_version": 1,
        "managed_hooks_env_vars": ["PANEFLOW_AI_TOOL"]
    });
    std::fs::write(
        &settings_path,
        json!({
            "schema_version": 1,
            "managed_hooks_env_vars": [
                "PANEFLOW_AI_TOOL", "PANEFLOW_WORKSPACE_ID", "PANEFLOW_SURFACE_ID",
                "PANEFLOW_SOCKET_PATH", "PANEFLOW_AI_PID"
            ],
            "managed_hooks_path": hooks_path.to_str().unwrap()
        })
        .to_string(),
    )
    .unwrap();
    record_env_baseline_for_test(&settings_path, &["PANEFLOW_AI_TOOL".to_owned()]);

    crate::hooks::muse::sweep_orphan_for_test(&dir);
    assert_eq!(read_json(&settings_path), original);
    assert_eq!(recorded_env_baseline_for_test(&settings_path), None);
}

#[test]
fn muse_baseline_survives_a_failed_restore_write() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(MUSE_HOOKS_BASENAME);
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({
        "schema_version": 1,
        "managed_hooks_env_vars": ["PANEFLOW_AI_TOOL"]
    });
    std::fs::write(
        &settings_path,
        json!({
            "schema_version": 1,
            "managed_hooks_env_vars": ["PANEFLOW_AI_TOOL", "PANEFLOW_SURFACE_ID"],
            "managed_hooks_path": hooks_path.to_str().unwrap()
        })
        .to_string(),
    )
    .unwrap();
    record_env_baseline_for_test(&settings_path, &["PANEFLOW_AI_TOOL".to_owned()]);

    // A read-only directory makes the atomic replace fail after the
    // record has been read.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    crate::hooks::muse::sweep_orphan_for_test(&dir);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        recorded_env_baseline_for_test(&settings_path).is_some(),
        "a restore that never reached disk must not clear the record"
    );
    assert!(read_json(&settings_path)["managed_hooks_path"].is_string());

    crate::hooks::muse::sweep_orphan_for_test(&dir);
    assert_eq!(read_json(&settings_path), original);
    assert_eq!(recorded_env_baseline_for_test(&settings_path), None);
}

#[test]
fn muse_stale_baseline_record_is_replaced_on_first_take() {
    // A crashed session recorded its baseline but never wrote its
    // settings; the next first take starts over from the file as it is now.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({"schema_version": 1, "managed_hooks_env_vars": ["PANEFLOW_AI_PID"]});
    std::fs::write(&settings_path, original.to_string()).unwrap();
    record_env_baseline_for_test(&settings_path, &["PANEFLOW_SOCKET_PATH".to_owned()]);

    let guard = MuseHookConfigGuard::install_at(&dir).expect("install must succeed");
    assert_eq!(
        recorded_env_baseline_for_test(&settings_path).as_deref(),
        Some(&["PANEFLOW_AI_PID".to_owned()][..])
    );
    drop(guard);
    assert_eq!(read_json(&settings_path), original);
    assert_eq!(recorded_env_baseline_for_test(&settings_path), None);
}

#[test]
fn muse_user_files_at_the_baseline_path_are_never_read_or_touched() {
    // The record lives in PaneFlow's lease marker, so whatever a user puts
    // at the old sidecar path (a string array, prose, a symlink) is not
    // a baseline, is not read, and survives install, drop, and the sweep.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    let original = json!({"schema_version": 1, "managed_hooks_env_vars": ["PANEFLOW_AI_TOOL"]});
    std::fs::write(&settings_path, original.to_string()).unwrap();
    let decoy = env_baseline_path(&settings_path);
    std::fs::write(&decoy, "[\"PANEFLOW_SOCKET_PATH\"]\n").unwrap();

    let guard = MuseHookConfigGuard::install_at(&dir).expect("install must succeed");
    assert_eq!(
        recorded_env_baseline_for_test(&settings_path).as_deref(),
        Some(&["PANEFLOW_AI_TOOL".to_owned()][..]),
        "the record comes from the settings file, never from the decoy"
    );
    drop(guard);
    assert_eq!(read_json(&settings_path), original);
    assert_eq!(
        std::fs::read_to_string(&decoy).unwrap(),
        "[\"PANEFLOW_SOCKET_PATH\"]\n"
    );

    let target = td.path().join("precious.txt");
    std::fs::write(&target, "keep me\n").unwrap();
    std::fs::remove_file(&decoy).unwrap();
    std::os::unix::fs::symlink(&target, &decoy).unwrap();
    let guard = MuseHookConfigGuard::install_at(&dir).expect("a symlink there is irrelevant");
    drop(guard);
    crate::hooks::muse::sweep_orphan_for_test(&dir);
    assert!(decoy.is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me\n");
    assert_eq!(read_json(&settings_path), original);
}

#[test]
fn muse_settings_with_our_path_but_no_record_are_left_alone() {
    // Both reserved paths set by hand: the hook path points at the user's
    // own hook file, and no session ever recorded a baseline. The refused
    // install and the orphan sweep must not strip the user's settings.
    let td = tempfile::TempDir::new().unwrap();
    let dir = td.path().join(".config/muse");
    std::fs::create_dir_all(&dir).unwrap();
    let hooks_path = dir.join(MUSE_HOOKS_BASENAME);
    let settings_path = dir.join(MUSE_SETTINGS_BASENAME);
    std::fs::write(&hooks_path, "{\"hooks\": {}}\n").unwrap();
    let original = json!({
        "schema_version": 1,
        "managed_hooks_path": hooks_path.to_str().unwrap(),
        "managed_hooks_env_vars": ["PANEFLOW_SOCKET_PATH", "PANEFLOW_AI_TOOL"]
    });
    std::fs::write(&settings_path, original.to_string()).unwrap();

    assert!(MuseHookConfigGuard::install_at(&dir).is_err());
    assert_eq!(read_json(&settings_path), original);
    crate::hooks::muse::sweep_orphan_for_test(&dir);
    assert_eq!(read_json(&settings_path), original);
    assert_eq!(
        std::fs::read_to_string(&hooks_path).unwrap(),
        "{\"hooks\": {}}\n"
    );
}
