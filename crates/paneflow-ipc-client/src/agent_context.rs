//! Explicit caller routing shared by CLI and MCP. Never fall back to focus.

use serde_json::{json, Value};

pub fn identity_from_env() -> Result<Value, String> {
    identity_from_values(
        std::env::var("PANEFLOW_SURFACE_ID").ok().as_deref(),
        std::env::var("PANEFLOW_WORKSPACE_ID").ok().as_deref(),
    )
}

fn identity_from_values(surface: Option<&str>, workspace: Option<&str>) -> Result<Value, String> {
    fn id(name: &str, value: Option<&str>) -> Result<u64, String> {
        value
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or_else(|| {
                format!("{name} is missing or invalid; launch this client inside a PaneFlow pane")
            })
    }
    Ok(json!({
        "surface_id": id("PANEFLOW_SURFACE_ID", surface)?,
        "workspace_id": id("PANEFLOW_WORKSPACE_ID", workspace)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_malformed_identity_never_uses_the_active_pane() {
        for (surface, workspace) in [
            (None, Some("1")),
            (Some("3"), None),
            (Some("-1"), Some("1")),
            (Some("x"), Some("1")),
        ] {
            assert!(identity_from_values(surface, workspace).is_err());
        }
        assert_eq!(
            identity_from_values(Some("3"), Some("1")).expect("identity"),
            json!({"surface_id": 3, "workspace_id": 1})
        );
    }
}
