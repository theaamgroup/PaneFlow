//! No-clobber config parse and entry-removal primitives.
//!
//! Two families, two rules:
//! - **JSON and JSONC** (Claude Code, Gemini, opencode) are all parsed as
//!   JSONC and edited by `paneflow_agent_config::jsonc`'s surgical splice, so
//!   comments, trailing commas, number spellings, key order, and every byte
//!   outside the removed member stay as they were - whatever the file's
//!   extension. A `settings.json` with comments is common.
//! - **TOML** (Codex) is edited via `toml_edit::DocumentMut`, which
//!   preserves comments and key order.
//!
//! A present-but-invalid file is an error, so we never overwrite a config we
//! could not parse - the user repairs it by hand. This is the no-clobber
//! guarantee.

use std::path::Path;

use anyhow::{Context, Result};
use paneflow_agent_config::jsonc;

/// Parse `text`, read from `path`, as JSONC (a superset of JSON).
/// Unparseable → `Err` naming the file.
pub(crate) fn parse_json_family(path: &Path, text: &str) -> Result<serde_json::Value> {
    jsonc::parse(text).with_context(|| {
        format!(
            "{} is not valid JSON or JSONC - refusing to overwrite it; \
             fix or remove it, then re-run",
            path.display()
        )
    })
}

/// Remove `container_key.entry_name` from JSON or JSONC `text` without
/// reserializing it. `None` when the entry is absent.
pub(crate) fn remove_json_family_entry(
    path: &Path,
    text: &str,
    container_key: &str,
    entry_name: &str,
) -> Result<Option<String>> {
    jsonc::remove_entry(text, container_key, entry_name)
        .with_context(|| format!("edit {} failed", path.display()))
}

/// Parse `text`, read from `path`, as TOML. Unparseable → `Err` naming the
/// file.
pub(crate) fn parse_toml(path: &Path, text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse::<toml_edit::DocumentMut>().with_context(|| {
        format!(
            "{} is not valid TOML - refusing to overwrite it; \
             fix or remove it, then re-run",
            path.display()
        )
    })
}

/// Remove `[<table_path>.<name>]`. Returns `true` iff the document changed.
///
/// `toml_edit` stores the comment and blank lines above a table header in
/// that header's decor prefix, so a plain removal would also drop a comment
/// that ends the previous table (`# env = …` below its last key) and a note
/// written above the removed header. When that prefix holds a comment, it
/// moves to the next header's prefix, or to the document's trailing text when
/// the removed table was the last one, so the comments stay where they were
/// in the file. A prefix of blank lines only is the removed table's own
/// separator and goes with it.
pub(crate) fn remove_toml_entry(
    doc: &mut toml_edit::DocumentMut,
    table_path: &str,
    name: &str,
) -> bool {
    let Some(parent) = doc
        .get_mut(table_path)
        .and_then(toml_edit::Item::as_table_mut)
    else {
        return false;
    };
    let Some(removed) = parent.remove(name) else {
        return false;
    };
    if let Some(table) = removed.as_table() {
        let prefix = table
            .decor()
            .prefix()
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default();
        if prefix.contains(|c: char| !c.is_whitespace()) {
            carry_prefix(doc, table.position(), prefix);
        }
    }
    true
}

/// Prepend `prefix` to the first header after document position `after`,
/// or to the document's trailing text when there is none.
fn carry_prefix(doc: &mut toml_edit::DocumentMut, after: Option<isize>, prefix: &str) {
    let next = after.and_then(|after| next_header_position(doc.as_table(), after, None));
    if let Some(position) = next {
        if prepend_to_header(doc.as_table_mut(), position, prefix) {
            return;
        }
    }
    let trailing = doc.trailing().as_str().unwrap_or_default().to_string();
    doc.set_trailing(format!("{prefix}{trailing}"));
}

/// Tables rendered with their own `[header]`: not implicit, not dotted.
fn has_header(table: &toml_edit::Table) -> bool {
    !table.is_implicit() && !table.is_dotted()
}

/// The smallest header position greater than `after` under `table`.
fn next_header_position(
    table: &toml_edit::Table,
    after: isize,
    mut best: Option<isize>,
) -> Option<isize> {
    for (_, item) in table.iter() {
        let children: Vec<&toml_edit::Table> = match item {
            toml_edit::Item::Table(child) => vec![child],
            toml_edit::Item::ArrayOfTables(array) => array.iter().collect(),
            _ => continue,
        };
        for child in children {
            if has_header(child) {
                if let Some(position) = child.position().filter(|p| *p > after) {
                    best = Some(best.map_or(position, |b| b.min(position)));
                }
            }
            best = next_header_position(child, after, best);
        }
    }
    best
}

/// Prepend `prefix` to the header at `position`. `false` when none matched.
fn prepend_to_header(table: &mut toml_edit::Table, position: isize, prefix: &str) -> bool {
    for (_, item) in table.iter_mut() {
        let children: Vec<&mut toml_edit::Table> = match item {
            toml_edit::Item::Table(child) => vec![child],
            toml_edit::Item::ArrayOfTables(array) => array.iter_mut().collect(),
            _ => continue,
        };
        for child in children {
            if has_header(child) && child.position() == Some(position) {
                let existing = child
                    .decor()
                    .prefix()
                    .and_then(toml_edit::RawString::as_str)
                    .unwrap_or_default()
                    .to_string();
                child.decor_mut().set_prefix(format!("{prefix}{existing}"));
                return true;
            }
            if prepend_to_header(child, position, prefix) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_removal_keeps_every_other_byte_including_float_digits_and_key_order() {
        // serde_json would round `0.1234567890123456789` to an f64 and
        // reprint it; the splice leaves the spelling alone.
        let input = "{\n  \"zeta\": 0.1234567890123456789,\n  \"mcpServers\": {\n    \"b\": { \"command\": \"b\" },\n    \"paneflow\": { \"command\": \"/x/paneflow-mcp\" },\n    \"a\": { \"command\": \"a\" }\n  },\n  \"big\": 12345678901234567890123,\n  \"alpha\": 1e3\n}\n";
        let out = remove_json_family_entry(Path::new("c.json"), input, "mcpServers", "paneflow")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            "{\n  \"zeta\": 0.1234567890123456789,\n  \"mcpServers\": {\n    \"b\": { \"command\": \"b\" },\n    \"a\": { \"command\": \"a\" }\n  },\n  \"big\": 12345678901234567890123,\n  \"alpha\": 1e3\n}\n"
        );
    }

    #[test]
    fn a_json_file_with_comments_and_trailing_commas_parses() {
        let v = parse_json_family(
            Path::new("settings.json"),
            "{\n  // user comment\n  \"mcpServers\": { \"paneflow\": { \"command\": [\"/p\"], }, },\n  \"url\": \"https://example.com/path//kept\"\n}\n",
        )
        .unwrap();
        assert_eq!(v["mcpServers"]["paneflow"]["command"], json!(["/p"]));
        assert_eq!(v["url"], json!("https://example.com/path//kept"));
    }

    #[test]
    fn invalid_json_is_an_error_naming_the_file() {
        let err = parse_json_family(Path::new("/cfg/broken.json"), "{ not json").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("/cfg/broken.json") && msg.contains("not valid JSON"),
            "{msg}"
        );
    }

    #[test]
    fn remove_toml_only_removes_target() {
        let input = "\
[mcp_servers.existing]
command = \"keepme\"

[mcp_servers.paneflow]
command = \"/p\"
args = []
";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(remove_toml_entry(&mut doc, "mcp_servers", "paneflow"));
        let out = doc.to_string();
        assert!(out.contains("keepme"), "sibling preserved");
        assert!(!out.contains("[mcp_servers.paneflow]"));
        assert!(!remove_toml_entry(&mut doc, "mcp_servers", "paneflow"));
    }

    #[test]
    fn removing_a_table_keeps_the_comments_around_its_header() {
        let head = "# codex config\n[mcp_servers.github]\ncommand = \"gh-mcp\"\n";
        let around =
            "# env = { GH_TOKEN = \"...\" }\n\n# PaneFlow bridge, added by paneflow mcp install\n";
        let entry = "[mcp_servers.paneflow]\ncommand = \"/x/paneflow-mcp\"\nargs = []\n\n[mcp_servers.paneflow.env]\nA = \"1\"\n";
        let tail = "\n# profiles below\n[profiles.fast]\nmodel = \"gpt-5-mini\"\n";
        let mut doc = format!("{head}{around}{entry}{tail}")
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert!(remove_toml_entry(&mut doc, "mcp_servers", "paneflow"));
        assert_eq!(doc.to_string(), format!("{head}{around}{tail}"));

        // The removed table was the last header: its comments move to the
        // document's trailing text.
        let mut doc = format!("{head}{around}{entry}")
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert!(remove_toml_entry(&mut doc, "mcp_servers", "paneflow"));
        assert_eq!(doc.to_string(), format!("{head}{around}"));
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let err = parse_toml(Path::new("broken.toml"), "this = = invalid").unwrap_err();
        assert!(err.to_string().contains("not valid TOML"));
    }

    #[test]
    fn remove_jsonc_preserves_comments_and_siblings() {
        let input = r#"{
  // keep
  "mcp": {
    "weather": { "type": "local", "command": ["weather-mcp"], "enabled": true },
    "paneflow": { "type": "local", "command": ["/p"], "enabled": true }
  }
}
"#;
        let out = remove_json_family_entry(Path::new("o.jsonc"), input, "mcp", "paneflow")
            .unwrap()
            .expect("remove should write");
        assert!(out.contains("// keep"), "comment preserved:\n{out}");
        assert!(
            !out.contains("\"paneflow\""),
            "managed key must be removed:\n{out}"
        );
        let v = jsonc::parse(&out).unwrap();
        assert!(v["mcp"].get("paneflow").is_none());
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
        assert!(
            remove_json_family_entry(Path::new("o.jsonc"), &out, "mcp", "paneflow")
                .unwrap()
                .is_none()
        );
    }
}
