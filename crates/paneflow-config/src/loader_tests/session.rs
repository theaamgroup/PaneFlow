use crate::schema::*;

// --- Session persistence round-trip tests (US-017) ---

fn make_workspace(title: &str, cwd: &str, tabs: Vec<TabSession>) -> WorkspaceSession {
    WorkspaceSession {
        title: title.to_string(),
        cwd: cwd.to_string(),
        tabs,
        active_tab: 0,
        legacy_layout: None,
        legacy_empty: false,
        custom_buttons: vec![],
        expanded_paths: vec![],
        managed_worktrees: vec![],
        pinned: false,
    }
}

fn make_surface(cwd: &str) -> SurfaceDefinition {
    SurfaceDefinition {
        surface_type: Some("terminal".to_string()),
        cwd: Some(cwd.to_string()),
        ..Default::default()
    }
}

#[test]
fn test_session_roundtrip_single_workspace() {
    let state = SessionState {
        version: SESSION_SCHEMA_VERSION,
        active_workspace: 0,
        workspaces: vec![make_workspace(
            "main",
            "/home/user/project",
            vec![TabSession::with_layout(LayoutNode::Pane {
                surfaces: vec![make_surface("/home/user/project")],
            })],
        )],
        mode: AppMode::default(),
        diff_scope: None,
        primary_sidebar_collapsed: false,
    };
    let json = serde_json::to_string_pretty(&state).unwrap();
    let restored: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, restored);
}

#[test]
fn test_session_roundtrip_multiple_workspaces() {
    let state = SessionState {
        version: SESSION_SCHEMA_VERSION,
        active_workspace: 1,
        workspaces: vec![
            make_workspace(
                "frontend",
                "/home/user/web",
                vec![TabSession::with_layout(LayoutNode::Pane {
                    surfaces: vec![make_surface("/home/user/web")],
                })],
            ),
            make_workspace(
                "backend",
                "/home/user/api",
                vec![TabSession::with_layout(LayoutNode::Pane {
                    surfaces: vec![make_surface("/home/user/api")],
                })],
            ),
            // An empty folder: one tab, no pane (v2 needs no `empty` marker).
            make_workspace("devops", "/home/user/infra", vec![TabSession::empty()]),
        ],
        mode: AppMode::default(),
        diff_scope: None,
        primary_sidebar_collapsed: false,
    };
    let json = serde_json::to_string_pretty(&state).unwrap();
    let restored: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, restored);
    assert_eq!(restored.active_workspace, 1);
    assert_eq!(restored.workspaces.len(), 3);
}

#[test]
fn test_session_roundtrip_nested_splits() {
    let state = SessionState {
        version: SESSION_SCHEMA_VERSION,
        active_workspace: 0,
        workspaces: vec![make_workspace(
            "dev",
            "/home/user",
            vec![TabSession::with_layout(LayoutNode::Split {
                direction: "horizontal".to_string(),
                ratio: None,
                ratios: Some(vec![0.6, 0.4]),
                children: vec![
                    LayoutNode::Pane {
                        surfaces: vec![make_surface("/home/user/code")],
                    },
                    LayoutNode::Split {
                        direction: "vertical".to_string(),
                        ratio: None,
                        ratios: Some(vec![0.5, 0.5]),
                        children: vec![
                            LayoutNode::Pane {
                                surfaces: vec![make_surface("/home/user/tests")],
                            },
                            LayoutNode::Pane {
                                surfaces: vec![make_surface("/home/user/logs")],
                            },
                        ],
                    },
                ],
            })],
        )],
        mode: AppMode::default(),
        diff_scope: None,
        primary_sidebar_collapsed: false,
    };
    let json = serde_json::to_string_pretty(&state).unwrap();
    let restored: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, restored);
    // Verify structure: root is split with 2 children, second child is also a split
    let layout = restored.workspaces[0].tabs[0].layout.as_ref().unwrap();
    assert_eq!(layout.leaf_count(), 3);
}

#[test]
fn test_session_roundtrip_with_scrollback() {
    let state = SessionState {
        version: SESSION_SCHEMA_VERSION,
        active_workspace: 0,
        workspaces: vec![make_workspace(
            "main",
            "/tmp",
            vec![TabSession::with_layout(LayoutNode::Pane {
                surfaces: vec![SurfaceDefinition {
                    surface_type: Some("terminal".to_string()),
                    cwd: Some("/tmp".to_string()),
                    scrollback: Some("$ ls\nfile1.txt\nfile2.txt\n$ echo hello\nhello".to_string()),
                    ..Default::default()
                }],
            })],
        )],
        mode: AppMode::default(),
        diff_scope: None,
        primary_sidebar_collapsed: false,
    };
    let json = serde_json::to_string_pretty(&state).unwrap();
    let restored: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, restored);
    let surface = match &restored.workspaces[0].tabs[0].layout {
        Some(LayoutNode::Pane { surfaces }) => &surfaces[0],
        _ => panic!("expected pane"),
    };
    assert!(surface.scrollback.as_ref().unwrap().contains("hello"));
}

#[test]
fn test_session_corrupted_json_returns_none() {
    // Truncated JSON - simulates crash during write
    let corrupted = r#"{"version":1,"active_workspace":0,"workspaces":[{"title":"ma"#;
    let result: Result<SessionState, _> = serde_json::from_str(corrupted);
    assert!(result.is_err(), "Corrupted JSON should fail to parse");
}

#[test]
fn test_session_scrollback_none_omitted_from_json() {
    let surface = SurfaceDefinition {
        scrollback: None,
        ..Default::default()
    };
    let json = serde_json::to_string(&surface).unwrap();
    assert!(
        !json.contains("scrollback"),
        "None scrollback should be omitted from JSON"
    );
}

// SessionState keeps a tolerant `mode` so a session.json written by a
// build that still had the Agents view restores in CLI. Unknown keys
// (`projects`, `chats`, `agents_target`, …) are ignored.

#[test]
fn test_session_with_removed_agents_view_restores_in_cli_mode() {
    // A session.json written by a build that still had the Agents view: an
    // unknown `"mode"` string plus keys this schema no longer declares. It
    // must restore silently in CLI mode - a rejected parse would quarantine
    // the file and cost the user every workspace in it.
    let legacy = r#"{
        "version": 1,
        "active_workspace": 0,
        "workspaces": [
            { "title": "main", "cwd": "/tmp", "layout": null }
        ],
        "projects": [
            { "id": 1, "title": "Paneflow", "cwd": "/tmp", "threads": [] }
        ],
        "active_project": 0,
        "chats": [],
        "agents_target": { "type": "chat", "thread_id": 3 },
        "mode": "agents"
    }"#;
    let restored: SessionState = serde_json::from_str(legacy).unwrap();
    assert_eq!(restored.workspaces.len(), 1, "the workspaces survive");
    assert_eq!(
        restored.mode,
        AppMode::Cli,
        "an unknown mode falls back to CLI"
    );
}

#[test]
fn test_session_backward_compat_pre_us007() {
    // A literal pre-US-007 session.json: no `mode` key. Must deserialise
    // to the default `AppMode::Cli`.
    let legacy = r#"{
        "version": 1,
        "active_workspace": 0,
        "workspaces": [
            { "title": "main", "cwd": "/tmp", "layout": null }
        ]
    }"#;
    let restored: SessionState = serde_json::from_str(legacy).unwrap();
    assert_eq!(restored.workspaces.len(), 1);
    assert_eq!(
        restored.mode,
        AppMode::Cli,
        "legacy session.json must restore in CLI mode"
    );
}

#[test]
fn test_app_mode_serializes_snake_case() {
    assert_eq!(serde_json::to_string(&AppMode::Cli).unwrap(), "\"cli\"");
    // US-001 (prd-git-diff-mode-2026-Q3.md): the Review mode.
    assert_eq!(serde_json::to_string(&AppMode::Diff).unwrap(), "\"diff\"");
}

#[test]
fn test_app_mode_diff_round_trips() {
    // US-001 (prd-git-diff-mode-2026-Q3.md): `Diff` survives a
    // serialize -> deserialize cycle and a session.json carrying it
    // restores into `AppMode::Diff` (not the `Cli` default).
    let json = serde_json::to_string(&AppMode::Diff).unwrap();
    let back: AppMode = serde_json::from_str(&json).unwrap();
    assert_eq!(back, AppMode::Diff);

    let session = r#"{
        "version": 1,
        "active_workspace": 0,
        "workspaces": [],
        "mode": "diff"
    }"#;
    let restored: SessionState = serde_json::from_str(session).unwrap();
    assert_eq!(restored.mode, AppMode::Diff);
}

#[test]
fn test_session_diff_scope_round_trips_and_defaults() {
    // US-015 (prd-git-diff-mode-2026-Q3.md): diff_scope persists, and a
    // session.json written before this field restores it as `None`.
    let legacy = r#"{ "version": 1, "active_workspace": 0, "workspaces": [] }"#;
    let restored: SessionState = serde_json::from_str(legacy).unwrap();
    assert_eq!(restored.diff_scope, None);

    let with_scope = r#"{
        "version": 1,
        "active_workspace": 0,
        "workspaces": [],
        "diff_scope": "worktree"
    }"#;
    let restored2: SessionState = serde_json::from_str(with_scope).unwrap();
    assert_eq!(restored2.diff_scope.as_deref(), Some("worktree"));
}

#[test]
fn test_v2_workspace_writes_no_legacy_keys() {
    // US-018: `layout` and `empty` are v1-only. A v2 workspace must not grow
    // either key back, or an older Paneflow would read the file as v1 data
    // under a v2 version number.
    let ws = make_workspace("main", "/tmp", vec![TabSession::empty()]);
    let value = serde_json::to_value(&ws).unwrap();
    let keys = value.as_object().expect("a JSON object");
    assert!(
        !keys.contains_key("layout"),
        "no workspace-level layout in v2"
    );
    assert!(!keys.contains_key("empty"), "no empty marker in v2");
    assert!(!keys.contains_key("active_tab"), "index 0 stays implicit");
    assert!(keys.contains_key("tabs"), "the tab list is always written");
}

// ─── v1 -> v2 migration (US-018, prd-cli-tab-hierarchy-2026-Q3) ────────

/// Frozen v1 `session.json`: one workspace, a horizontal split, and a pane
/// stacking three surfaces (the second one focused) next to a single-surface
/// pane. Deliberately a literal - it is the shape shipped Paneflow versions
/// actually wrote, and must keep migrating even as the v2 structs move on.
const V1_FIXTURE: &str = r#"{
    "version": 1,
    "active_workspace": 0,
    "workspaces": [
        {
            "title": "paneflow",
            "cwd": "/home/user/dev/paneflow",
            "layout": {
                "type": "split",
                "direction": "horizontal",
                "ratios": [0.6, 0.4],
                "children": [
                    {
                        "type": "pane",
                        "surfaces": [
                            { "surface_type": "terminal", "name": "zsh", "cwd": "/home/user/dev/paneflow" },
                            { "surface_type": "terminal", "name": "cargo-run", "cwd": "/home/user/dev/paneflow", "focus": true },
                            { "surface_type": "terminal", "name": "claude", "cwd": "/home/user/dev/paneflow" }
                        ]
                    },
                    {
                        "type": "pane",
                        "surfaces": [
                            { "surface_type": "terminal", "name": "vite", "cwd": "/home/user/dev/paneflow/web" }
                        ]
                    }
                ]
            },
            "custom_buttons": [],
            "expanded_paths": ["src"]
        }
    ],
    "active_project": 0
}"#;

fn count_surfaces(node: &LayoutNode) -> usize {
    match node {
        LayoutNode::Pane { surfaces } => surfaces.len(),
        LayoutNode::Split { children, .. } => children.iter().map(count_surfaces).sum(),
    }
}

fn count_workspace_surfaces(ws: &WorkspaceSession) -> usize {
    ws.tabs
        .iter()
        .filter_map(|tab| tab.layout.as_ref())
        .map(count_surfaces)
        .sum()
}

#[test]
fn test_migrate_v1_preserves_surface_count() {
    // AC7: the migration re-homes surfaces, it never drops them.
    let mut state: SessionState = serde_json::from_str(V1_FIXTURE).unwrap();
    assert_eq!(state.version, SESSION_SCHEMA_VERSION_V1);
    let before = count_surfaces(state.workspaces[0].legacy_layout.as_ref().unwrap());
    assert_eq!(before, 4, "fixture holds 4 surfaces across 2 panes");

    migrate_session_v1(&mut state);

    assert_eq!(state.version, SESSION_SCHEMA_VERSION);
    let ws = &state.workspaces[0];
    assert_eq!(count_workspace_surfaces(ws), before, "no surface is lost");
    // The tree becomes the first tab, each pane reduced to its focused surface.
    assert_eq!(ws.tabs.len(), 3, "1 tree tab + 2 promoted surfaces");
    let first = ws.tabs[0].layout.as_ref().unwrap();
    assert_eq!(count_surfaces(first), 2, "one surface per pane");
    assert_eq!(first.leaf_count(), 2, "the split survives intact");
    // Traversal order: the non-focused surfaces of the first pane, in order.
    assert_eq!(ws.tabs[1].title, "zsh");
    assert_eq!(ws.tabs[2].title, "claude");
    // The focused surface is the one that stayed in the tree.
    let kept = match first {
        LayoutNode::Split { children, .. } => match &children[0] {
            LayoutNode::Pane { surfaces } => surfaces[0].name.clone(),
            _ => panic!("expected a pane"),
        },
        _ => panic!("expected a split"),
    };
    assert_eq!(kept.as_deref(), Some("cargo-run"), "the focused one stays");
    // Untouched v1 fields survive, and the legacy keys are drained.
    assert_eq!(ws.expanded_paths, vec!["src".to_string()]);
    assert!(ws.legacy_layout.is_none());
    assert!(!ws.legacy_empty);
    assert_eq!(ws.active_tab, 0);
}

#[test]
fn test_migrate_v1_caps_promoted_tabs() {
    // AC4: a v1 pane carrying the 64 allowed surfaces migrates under
    // MAX_SESSION_TABS - surplus dropped loudly, never silently.
    let surfaces: Vec<String> = (0..64)
        .map(|i| format!(r#"{{ "surface_type": "terminal", "name": "s{i}" }}"#))
        .collect();
    let json = format!(
        r#"{{ "version": 1, "active_workspace": 0, "workspaces": [
            {{ "title": "big", "cwd": "/tmp", "layout": {{ "type": "pane", "surfaces": [{}] }} }}
        ] }}"#,
        surfaces.join(",")
    );
    let mut state: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(
        count_surfaces(state.workspaces[0].legacy_layout.as_ref().unwrap()),
        64
    );

    migrate_session_v1(&mut state);

    let ws = &state.workspaces[0];
    assert_eq!(ws.tabs.len(), MAX_SESSION_TABS, "capped, not unbounded");
    assert_eq!(
        count_workspace_surfaces(ws),
        MAX_SESSION_TABS,
        "one surface per surviving tab"
    );
    // Order is preserved: the first surface stays in the tree tab (no `focus`
    // key means index 0), the next ones are promoted in order.
    assert_eq!(ws.tabs[1].title, "s1");
    assert_eq!(
        ws.tabs[MAX_SESSION_TABS - 1].title,
        format!("s{}", MAX_SESSION_TABS - 1)
    );
}

#[test]
fn test_migrate_v1_null_layout_becomes_default_pane() {
    // AC6: v1 `layout: null` with no tabs key meant "one default pane".
    let json = r#"{ "version": 1, "active_workspace": 0, "workspaces": [
        { "title": "main", "cwd": "/tmp", "layout": null }
    ] }"#;
    let mut state: SessionState = serde_json::from_str(json).unwrap();
    migrate_session_v1(&mut state);

    let ws = &state.workspaces[0];
    assert_eq!(ws.tabs.len(), 1);
    let layout = ws.tabs[0].layout.as_ref().expect("a default pane");
    assert_eq!(count_surfaces(layout), 1);
}

#[test]
fn test_migrate_v1_empty_marker_becomes_paneless_tab() {
    // The EP-003 `empty` marker is the one v1 case that must NOT gain a pane:
    // an empty folder restores empty.
    let json = r#"{ "version": 1, "active_workspace": 0, "workspaces": [
        { "title": "main", "cwd": "/tmp", "layout": null, "empty": true }
    ] }"#;
    let mut state: SessionState = serde_json::from_str(json).unwrap();
    migrate_session_v1(&mut state);

    let ws = &state.workspaces[0];
    assert_eq!(ws.tabs.len(), 1, "FR-01: a workspace always keeps one tab");
    assert!(ws.tabs[0].layout.is_none(), "and it holds no pane");
    assert!(!ws.legacy_empty, "the marker is drained");
}

#[test]
fn test_migrate_v1_is_idempotent_on_v2_shape() {
    // Re-running the migration over an already-migrated state must not
    // re-promote anything or duplicate a tab.
    let mut state: SessionState = serde_json::from_str(V1_FIXTURE).unwrap();
    migrate_session_v1(&mut state);
    let once = state.clone();
    migrate_session_v1(&mut state);
    assert_eq!(once, state);
}

// --- Primary sidebar collapse persistence (issue #106) ---

#[test]
fn test_session_roundtrip_primary_sidebar_collapsed() {
    // Issue #106: the primary rail's collapsed state is the user's intent and
    // survives a quit. Additive on the v2 schema - `SESSION_SCHEMA_VERSION`
    // must NOT move for it, because `load_session_at` routes any version that
    // is neither 2 nor 1 to the corruption-backup path, so a bump would
    // discard every existing user's workspaces.
    let state = SessionState {
        version: SESSION_SCHEMA_VERSION,
        active_workspace: 0,
        workspaces: vec![make_workspace("main", "/tmp", vec![TabSession::empty()])],
        mode: AppMode::default(),
        diff_scope: None,
        primary_sidebar_collapsed: true,
    };
    let json = serde_json::to_string_pretty(&state).unwrap();
    assert!(
        json.contains("\"primary_sidebar_collapsed\": true"),
        "a collapsed rail has to reach disk to be restorable: {json}"
    );
    let restored: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, restored);
    assert_eq!(
        restored.version, SESSION_SCHEMA_VERSION,
        "the field is additive: the schema version stays where it was"
    );
}

#[test]
fn test_session_without_primary_sidebar_key_restores_visible() {
    // Every session.json written before issue #106 lacks the key entirely and
    // must keep today's behaviour: the rail starts visible.
    let json = r#"{ "version": 2, "active_workspace": 0, "workspaces": [] }"#;
    let restored: SessionState = serde_json::from_str(json).unwrap();
    assert!(
        !restored.primary_sidebar_collapsed,
        "an older session must not silently collapse the rail"
    );

    // And the common case stays out of the file entirely, so adding the field
    // does not rewrite every user's session.json on the next save.
    let visible = SessionState {
        version: SESSION_SCHEMA_VERSION,
        active_workspace: 0,
        workspaces: vec![],
        mode: AppMode::default(),
        diff_scope: None,
        primary_sidebar_collapsed: false,
    };
    let written = serde_json::to_string(&visible).unwrap();
    assert!(
        !written.contains("primary_sidebar_collapsed"),
        "the default must be skipped on write: {written}"
    );
}

/// Issue #107: the sidebar pin is user state, so it has to outlive a quit.
/// Additive on v2 exactly like `expanded_paths` - `SESSION_SCHEMA_VERSION`
/// must NOT move for it, or every existing session.json takes the
/// unsupported-version corruption-backup path.
#[test]
fn workspace_pinned_survives_a_session_round_trip() {
    let mut ws = make_workspace("pinned", "/tmp/pinned", vec![TabSession::empty()]);
    ws.pinned = true;

    let json = serde_json::to_string(&ws).unwrap();
    assert!(json.contains("\"pinned\":true"), "{json}");

    let back: WorkspaceSession = serde_json::from_str(&json).unwrap();
    assert!(back.pinned);
    assert_eq!(back, ws);
}

#[test]
fn workspace_pinned_defaults_to_false_when_the_key_is_absent() {
    // A session.json written before the field existed.
    let ws: WorkspaceSession =
        serde_json::from_str(r#"{"title":"old","cwd":"/tmp/old","tabs":[]}"#).unwrap();
    assert!(
        !ws.pinned,
        "an older session must restore unpinned, not fail to load"
    );

    // And the default is skipped on write, so no existing file gains a key.
    let written = serde_json::to_string(&ws).unwrap();
    assert!(!written.contains("pinned"), "{written}");
}
