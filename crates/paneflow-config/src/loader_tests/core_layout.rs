use super::super::*;
use crate::schema::*;
use tempfile::NamedTempFile;

/// Parse one layout node and run it through `validate_layout`, the same
/// fixups session restore applies.
fn validated(layout: &str) -> LayoutNode {
    let mut node: LayoutNode = serde_json::from_str(layout).unwrap();
    validate_layout(&mut node);
    node
}

#[test]
fn test_split_ratio_clamped_low() {
    let node = validated(
        r#"{
        "type": "split",
        "direction": "vertical",
        "ratio": 0.01,
        "children": [
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]},
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]}
        ]
    }"#,
    );
    match &node {
        LayoutNode::Split { ratio, .. } => {
            assert!((ratio.unwrap() - 0.1).abs() < f64::EPSILON);
        }
        _ => panic!("expected split"),
    }
}

#[test]
fn test_split_ratio_clamped_high() {
    let node = validated(
        r#"{
        "type": "split",
        "direction": "vertical",
        "ratio": 0.99,
        "children": [
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]},
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]}
        ]
    }"#,
    );
    match &node {
        LayoutNode::Split { ratio, .. } => {
            assert!((ratio.unwrap() - 0.9).abs() < f64::EPSILON);
        }
        _ => panic!("expected split"),
    }
}

#[test]
fn test_per_child_ratios_floor_respected_after_normalize() {
    // US-057: clamp -> normalize can push a value back below the 0.01 floor.
    // The re-clamp must restore it. ratios [100.0, 0.001] -> clamp
    // [1.0, 0.01] -> normalize ~[0.990, 0.0099] (2nd below floor) -> re-clamp.
    let mut node = LayoutNode::Split {
        direction: "vertical".to_string(),
        ratio: None,
        ratios: Some(vec![100.0, 0.001]),
        children: vec![
            LayoutNode::Pane {
                surfaces: vec![Default::default()],
            },
            LayoutNode::Pane {
                surfaces: vec![Default::default()],
            },
        ],
    };
    validate_layout(&mut node);
    match node {
        LayoutNode::Split { ratios, .. } => {
            let rs = ratios.unwrap();
            assert!(
                rs.iter().all(|r| *r >= 0.01),
                "every ratio must respect the 0.01 floor after normalize: {rs:?}"
            );
        }
        _ => panic!("expected split"),
    }
}

#[test]
fn test_split_nary_children_accepted() {
    // 3+ children are valid in N-ary layout.
    let node = validated(
        r#"{
        "type": "split",
        "direction": "horizontal",
        "ratios": [0.33, 0.33, 0.34],
        "children": [
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]},
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]},
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]}
        ]
    }"#,
    );
    match &node {
        LayoutNode::Split {
            children, ratios, ..
        } => {
            assert_eq!(children.len(), 3);
            let rs = ratios.as_ref().unwrap();
            assert_eq!(rs.len(), 3);
            assert!((rs[0] - 0.33).abs() < f64::EPSILON);
        }
        _ => panic!("expected split"),
    }
}

#[test]
fn test_split_zero_children_padded() {
    let node = validated(
        r#"{
        "type": "split",
        "direction": "horizontal",
        "children": []
    }"#,
    );
    match &node {
        LayoutNode::Split { children, .. } => {
            assert_eq!(children.len(), 2);
        }
        _ => panic!("expected split"),
    }
}

#[test]
fn test_pane_no_surfaces_gets_default() {
    let node = validated(
        r#"{
        "type": "pane",
        "surfaces": []
    }"#,
    );
    match &node {
        LayoutNode::Pane { surfaces } => {
            assert_eq!(surfaces.len(), 1);
            assert_eq!(surfaces[0].surface_type.as_deref(), Some("terminal"));
        }
        _ => panic!("expected pane"),
    }
}

#[test]
fn test_load_from_file() {
    use std::io::Write;
    let mut tmp = NamedTempFile::new().unwrap();
    write!(
        tmp,
        r#"{{"default_shell": "/bin/bash", "font_size": 15.0}}"#
    )
    .unwrap();

    let config = load_config_from_path(tmp.path());
    assert_eq!(config.default_shell, Some("/bin/bash".to_string()));
    assert_eq!(config.font_size, Some(15.0));
}

#[test]
fn test_load_from_file_invalid_json() {
    use std::io::Write;
    let mut tmp = NamedTempFile::new().unwrap();
    write!(tmp, "not valid json!!").unwrap();

    let config = load_config_from_path(tmp.path());
    assert_eq!(config, PaneFlowConfig::default());
}

#[test]
fn test_nested_split_validation() {
    let node = validated(
        r#"{
        "type": "split",
        "direction": "horizontal",
        "ratio": 0.5,
        "children": [
            {
                "type": "split",
                "direction": "vertical",
                "ratio": 0.05,
                "children": [
                    {"type": "pane", "surfaces": [{"surface_type": "terminal"}]},
                    {"type": "pane", "surfaces": [{"surface_type": "terminal"}]}
                ]
            },
            {"type": "pane", "surfaces": [{"surface_type": "terminal"}]}
        ]
    }"#,
    );
    match &node {
        LayoutNode::Split { children, .. } => {
            // Inner split should have ratio clamped to 0.1.
            match &children[0] {
                LayoutNode::Split { ratio, .. } => {
                    assert!((ratio.unwrap() - 0.1).abs() < f64::EPSILON);
                }
                _ => panic!("expected nested split"),
            }
        }
        _ => panic!("expected split"),
    }
}
