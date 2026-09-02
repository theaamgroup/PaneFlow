use super::super::*;
use crate::schema::*;

#[test]
fn test_default_config() {
    let config = PaneFlowConfig::default();
    assert!(config.shortcuts.is_empty());
    assert!(config.default_shell.is_none());
    assert!(config.commands.is_empty());
}

#[test]
fn test_config_path_is_some() {
    // On most systems dirs::config_dir() succeeds. The subdir varies
    // by build profile (`paneflow` in release, `paneflow-dev` in
    // debug -- see `APP_SUBDIR`) so tests assert against the const,
    // not a hardcoded `paneflow` literal.
    let path = config_path();
    assert!(path.is_some());
    let p = path.unwrap();
    let suffix_unix = format!("{APP_SUBDIR}/paneflow.json");
    assert!(
        p.ends_with(&suffix_unix),
        "config path {p:?} does not end with {suffix_unix}"
    );
}

#[test]
fn test_session_path_uses_config_dir_not_cache_dir() {
    let path = session_path().expect("config dir must resolve on macOS");
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(session_filename())
    );
    let parent = path.parent().expect("session path has a parent");
    assert_eq!(
        parent.file_name().and_then(|name| name.to_str()),
        Some(APP_SUBDIR),
        "session_path parent must be APP_SUBDIR under config_dir, got {path:?}"
    );

    let config_dir = dirs::config_dir().expect("config dir must resolve on macOS");
    assert_eq!(
        parent,
        config_dir.join(APP_SUBDIR),
        "session_path {path:?} must sit next to paneflow.json"
    );

    if let Some(cache_dir) = dirs::cache_dir() {
        assert!(
            !path.starts_with(&cache_dir),
            "session_path {path:?} must not live under cache_dir {cache_dir:?}"
        );
        let legacy = legacy_session_cache_path().expect("cache dir resolved");
        assert_eq!(legacy.parent(), Some(cache_dir.join(APP_SUBDIR).as_path()));
        assert_ne!(
            path, legacy,
            "session_path must not still be the cache-dir location"
        );
    }

    if cfg!(debug_assertions) {
        assert_eq!(APP_SUBDIR, "paneflow-dev");
        assert_eq!(session_filename(), "session-dev.json");
        assert!(
            path.to_string_lossy().contains("paneflow-dev"),
            "debug session must live under paneflow-dev, got {path:?}"
        );
    }
}

#[test]
fn test_session_path_migration_copies_from_cache_when_dest_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp
        .path()
        .join("cache")
        .join(APP_SUBDIR)
        .join("session.json");
    let dest = tmp
        .path()
        .join("config")
        .join(APP_SUBDIR)
        .join("session.json");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, r#"{"version":1}"#).unwrap();

    let copied = migrate_session_from_cache(&src, &dest).unwrap();
    assert!(copied, "expected a one-shot copy into the config dir");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), r#"{"version":1}"#);
    assert!(
        !src.exists(),
        "old cache file must be removed after a successful copy"
    );
}

#[test]
fn test_session_path_migration_skips_when_dest_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("old.json");
    let dest = tmp.path().join("new.json");
    std::fs::write(&src, "from-cache").unwrap();
    std::fs::write(&dest, "already-new").unwrap();

    let copied = migrate_session_from_cache(&src, &dest).unwrap();
    assert!(!copied);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "already-new");
    assert_eq!(
        std::fs::read_to_string(&src).unwrap(),
        "from-cache",
        "cache copy must stay when dest already exists"
    );
}

#[test]
fn test_session_path_migration_noop_when_src_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("missing.json");
    let dest = tmp.path().join("config").join("session.json");

    let copied = migrate_session_from_cache(&src, &dest).unwrap();
    assert!(!copied);
    assert!(
        !dest.exists(),
        "must not create dest when there is nothing to copy"
    );
}

#[test]
fn test_session_path_migration_keeps_src_if_copy_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("old.json");
    std::fs::write(&src, "payload").unwrap();
    let blocker = tmp.path().join("not-a-dir");
    std::fs::write(&blocker, "nope").unwrap();
    let dest = blocker.join("session.json");

    assert!(migrate_session_from_cache(&src, &dest).is_err());
    assert!(src.exists(), "src must remain when the copy fails");
    assert!(!dest.exists());
}

#[test]
fn test_missing_file_returns_defaults() {
    let config = load_config_from_path(std::path::Path::new("/nonexistent/path/config.json"));
    assert_eq!(config, PaneFlowConfig::default());
}

#[test]
fn test_invalid_json_returns_defaults() {
    let config = parse_and_validate("this is not json {{{");
    assert_eq!(config, PaneFlowConfig::default());
}

#[test]
fn test_unknown_terminal_enum_falls_back_not_wipes_config() {
    // Regression guard: a typo in a terminal enum (`"squiggle"`,
    // `"blinky"`) must fall back to that enum's default WITHOUT discarding
    // the rest of the config. Before the custom `Deserialize`, serde hard-
    // errored here and `parse_and_validate` returned `default()` for the
    // whole file -- theme, shell, and shortcuts all silently lost.
    let json = r#"{
        "theme": "One Dark",
        "default_shell": "/bin/zsh",
        "terminal": { "cursor_shape": "squiggle", "cursor_blink": "blinky" }
    }"#;
    let config = parse_and_validate(json);

    // The surrounding config survives the bad enum values.
    assert_eq!(config.theme.as_deref(), Some("One Dark"));
    assert_eq!(config.default_shell.as_deref(), Some("/bin/zsh"));

    // Each unrecognised enum value resolves to its documented default.
    let term = config
        .terminal
        .expect("terminal block must survive unknown enum values");
    assert_eq!(term.cursor_shape, Some(CursorShapeConfig::Block));
    assert_eq!(
        term.cursor_blink,
        Some(CursorBlinkConfig::TerminalControlled)
    );
}

#[test]
fn test_empty_json_object_returns_defaults() {
    let config = parse_and_validate("{}");
    assert_eq!(config, PaneFlowConfig::default());
}

#[test]
fn test_non_object_root_is_a_typed_parse_error() {
    // Hot reload must not treat a non-object root as a successful
    // defaults load: the watcher broadcasts Ok, then Settings refuses
    // to overwrite a non-object file.
    assert!(
        try_parse_and_validate("[]").is_err(),
        "array root must not load as defaults"
    );
    assert!(
        try_parse_and_validate("null").is_err(),
        "null root must not load as defaults"
    );
    assert!(
        try_parse_and_validate("\"x\"").is_err(),
        "string root must not load as defaults"
    );
    let empty = try_parse_and_validate("{}").expect("empty object is a valid config");
    assert_eq!(empty, PaneFlowConfig::default());
}

#[test]
fn test_valid_minimal_config() {
    let json = r#"{
        "default_shell": "/bin/zsh",
        "shortcuts": {"ctrl+t": "new_tab"},
        "commands": []
    }"#;
    let config = parse_and_validate(json);
    assert_eq!(config.default_shell, Some("/bin/zsh".to_string()));
    assert_eq!(config.shortcuts.get("ctrl+t"), Some(&"new_tab".to_string()));
    assert!(config.commands.is_empty());
}

#[test]
fn test_leftover_telemetry_key_is_ignored_and_rest_of_config_is_used() {
    // Existing paneflow.json files may still contain a telemetry block
    // after the subsystem was removed. Unknown keys are ignored; the
    // rest of the file must still load.
    let config = parse_and_validate(
        r#"{"theme": "Vercel", "default_shell": "/bin/zsh", "telemetry": {"enabled": true}}"#,
    );
    assert_eq!(config.theme.as_deref(), Some("Vercel"));
    assert_eq!(config.default_shell.as_deref(), Some("/bin/zsh"));
}

#[test]
fn test_legacy_windows_material_and_backend_keys_still_load() {
    // Existing paneflow.json files may still carry the retired Windows
    // material keys and a `"backend"` key from before #184 made the engine
    // non-configurable. Those must load, not error; the key is ignored.
    let json = r#"{
        "theme": "One Dark",
        "windows_chrome_material": true,
        "windows_terminal_material": true,
        "terminal": { "backend": "ghostty" }
    }"#;
    let config = try_parse_and_validate(json)
        .expect("legacy windows material and ghostty backend must not fail to parse");
    assert_eq!(config.theme.as_deref(), Some("One Dark"));
    assert!(config.terminal.is_some(), "terminal block must survive");

    let via_serde: PaneFlowConfig = serde_json::from_str(json).unwrap();
    assert!(via_serde.terminal.is_some(), "terminal block must survive");
}

#[test]
fn test_blank_name_skipped() {
    let json = r#"{
        "commands": [
            {"name": "", "keywords": []},
            {"name": "  ", "keywords": []},
            {"name": "valid", "keywords": ["test"], "command": "echo valid"}
        ]
    }"#;
    let config = parse_and_validate(json);
    assert_eq!(config.commands.len(), 1);
    assert_eq!(config.commands[0].name, "valid");
}

#[test]
fn test_malformed_command_entry_does_not_drop_valid_siblings_or_config() {
    let json = r#"{
        "theme": "One Dark",
        "commands": [
            {"description": "missing name", "command": "bad"},
            {"name": "valid", "keywords": ["test"], "command": "echo ok"}
        ]
    }"#;

    let config = parse_and_validate(json);

    assert_eq!(config.theme.as_deref(), Some("One Dark"));
    assert_eq!(config.commands.len(), 1);
    assert_eq!(config.commands[0].name, "valid");
}

#[test]
fn test_command_requires_exactly_one_payload() {
    let config = parse_and_validate(
        r#"{
            "commands": [
                {"name": "missing"},
                {"name": "both", "command": "echo bad", "workspace": {}},
                {"name": "blank", "command": "   "},
                {"name": "valid", "command": "echo ok"}
            ]
        }"#,
    );
    assert_eq!(config.commands.len(), 1);
    assert_eq!(config.commands[0].name, "valid");
}

#[test]
fn test_command_with_workspace() {
    let json = r#"{
        "commands": [{
            "name": "dev",
            "description": "Development workspace",
            "keywords": ["dev", "work"],
            "workspace": {
                "name": "Dev Workspace",
                "cwd": "/home/user/projects",
                "color": "ff6600",
                "layout": {
                    "type": "split",
                    "direction": "horizontal",
                    "ratio": 0.5,
                    "children": [
                        {
                            "type": "pane",
                            "surfaces": [{"surface_type": "terminal", "command": "vim"}]
                        },
                        {
                            "type": "pane",
                            "surfaces": [{"surface_type": "terminal", "command": "cargo watch"}]
                        }
                    ]
                }
            }
        }]
    }"#;
    let config = parse_and_validate(json);
    assert_eq!(config.commands.len(), 1);
    let cmd = &config.commands[0];
    assert_eq!(cmd.name, "dev");
    assert_eq!(cmd.description.as_deref(), Some("Development workspace"));

    let ws = cmd.workspace.as_ref().unwrap();
    assert_eq!(ws.name.as_deref(), Some("Dev Workspace"));
    assert_eq!(ws.color.as_deref(), Some("ff6600"));

    match ws.layout.as_ref().unwrap() {
        LayoutNode::Split {
            direction,
            ratio,
            children,
            ..
        } => {
            assert_eq!(direction, "horizontal");
            assert_eq!(*ratio, Some(0.5));
            assert_eq!(children.len(), 2);
        }
        _ => panic!("expected split layout"),
    }
}

#[test]
fn test_command_with_shell_command() {
    let json = r#"{
        "commands": [{
            "name": "htop",
            "keywords": ["monitor"],
            "command": "htop"
        }]
    }"#;
    let config = parse_and_validate(json);
    assert_eq!(config.commands.len(), 1);
    assert_eq!(config.commands[0].command.as_deref(), Some("htop"));
    assert!(config.commands[0].workspace.is_none());
}

/// Issue #241: `open(O_RDONLY)` on a FIFO with no writer blocks forever, so the
/// regular-file guard has to run before the blocking open, not only on the
/// opened descriptor. The read runs on a helper thread with a bounded wait so a
/// regression fails the test instead of hanging the suite.
#[test]
fn read_config_string_refuses_a_fifo_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("paneflow.json");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be runnable");
    assert!(status.success(), "mkfifo failed: {status}");

    let (tx, rx) = std::sync::mpsc::channel();
    let worker_path = fifo.clone();
    std::thread::spawn(move || {
        let _ = tx.send(read_config_string(&worker_path));
    });

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Err(ConfigError::NotRegularFile { path })) => assert_eq!(path, fifo),
        Ok(other) => panic!("expected NotRegularFile for a FIFO, got {other:?}"),
        Err(_) => {
            // Release the reader blocked inside `open` so the thread does not
            // linger, then fail loudly.
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
            panic!("read_config_string blocked on a FIFO at the config path");
        }
    }
}

/// A dotfile-manager symlink to a regular file must keep loading: the pre-open
/// type check follows symlinks so only the target's type is judged.
#[test]
fn read_config_string_follows_a_symlink_to_a_regular_file() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("real.json");
    std::fs::write(&target, "{\"font_size\": 14.0}").unwrap();
    let link = tmp.path().join("paneflow.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let contents = read_config_string(&link).unwrap();
    assert_eq!(contents.as_deref(), Some("{\"font_size\": 14.0}"));
}
