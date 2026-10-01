use std::fmt;
use std::ops::Range;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsoncError(String);

impl JsoncError {
    fn invalid(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for JsoncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for JsoncError {}

/// Parse JSONC while accepting comments and trailing commas.
pub fn parse(input: &str) -> Result<Value, JsoncError> {
    Parser::new(input).parse_document()?;
    let without_comments = strip_comments(input)?;
    let normalized = remove_trailing_commas(&without_comments)?;
    serde_json::from_str(&normalized)
        .map_err(|error| JsoncError::invalid(format!("invalid JSONC: {error}")))
}

/// Remove one nested object member without reserializing the surrounding file.
pub fn remove_entry(
    input: &str,
    container_key: &str,
    entry_key: &str,
) -> Result<Option<String>, JsoncError> {
    remove_at(input, &[container_key], entry_key)
}

/// Remove `entry_key` from the object at `container_path`.
///
/// An empty path is the root object. `Ok(None)` when the container or the
/// entry is already absent.
pub fn remove_at(
    input: &str,
    container_path: &[&str],
    entry_key: &str,
) -> Result<Option<String>, JsoncError> {
    let semantic = parse(input)?;
    let Some(container) = walk_optional(&semantic, container_path)? else {
        return Ok(None);
    };
    let container = container.as_object().ok_or_else(|| {
        JsoncError::invalid(not_object_message(container_path, container_path.len()))
    })?;
    if !container.contains_key(entry_key) {
        return Ok(None);
    }

    let document = Parser::new(input).parse_document()?;
    let container_node = resolve(&document, container_path)?;
    let container = container_node.object.as_ref().ok_or_else(|| {
        JsoncError::invalid(not_object_message(container_path, container_path.len()))
    })?;
    let entry_index = member_index(container, entry_key)
        .ok_or_else(|| JsoncError::invalid(format!("could not locate entry `{entry_key}`")))?;
    let updated = remove_member(input, container, entry_index);
    let got = parse(&updated)?;
    let mut expected = semantic.clone();
    let removed = walk_mut(&mut expected, container_path)?
        .as_object_mut()
        .is_some_and(|container| container.remove(entry_key).is_some());
    debug_assert!(removed);
    if got != expected {
        return Err(JsoncError::invalid(format!(
            "internal error: jsonc splice changed more than `{}`",
            display_path(container_path, entry_key)
        )));
    }
    Ok(Some(updated))
}

fn remove_member(input: &str, object: &ObjectNode, index: usize) -> String {
    let member = &object.members[index];
    let mut output = input.to_string();
    if let Some(comma) = &member.comma {
        // First or middle member (or a trailing comma): the member and its
        // own comma go together.
        output.replace_range(whole_lines(input, member.start..comma.end), "");
    } else if let Some(previous_comma) = index
        .checked_sub(1)
        .and_then(|previous| object.members[previous].comma.as_ref())
    {
        // Last member: drop the member and the comma that separated it from
        // the previous sibling, as two cuts. Whatever sits between them -
        // the sibling's trailing `//` comment, a commented-out sibling line -
        // stays. Back to front, so the first cut does not shift the second.
        output.replace_range(whole_lines(input, member.start..member.value.end), "");
        output.replace_range(previous_comma.clone(), "");
    } else {
        // The only member.
        output.replace_range(whole_lines(input, member.start..member.value.end), "");
    }
    output
}

/// Widen `range` to the whole lines it spans when nothing else shares them,
/// so a member that had lines of its own leaves no blank line behind.
/// A member sharing a line with a sibling, a brace, or a comment keeps the
/// narrow range, and so every other byte.
fn whole_lines(input: &str, range: Range<usize>) -> Range<usize> {
    let line_start = input[..range.start].rfind('\n').map_or(0, |i| i + 1);
    if !input[line_start..range.start]
        .bytes()
        .all(|b| matches!(b, b' ' | b'\t'))
    {
        return range;
    }
    let Some(newline) = input[range.end..].find('\n') else {
        return range;
    };
    let line_end = range.end + newline + 1;
    if !input[range.end..line_end]
        .bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return range;
    }
    line_start..line_end
}

fn member_index(object: &ObjectNode, key: &str) -> Option<usize> {
    object.members.iter().position(|member| member.key == key)
}

fn walk_optional<'a>(value: &'a Value, path: &[&str]) -> Result<Option<&'a Value>, JsoncError> {
    let mut cursor = value;
    for (depth, key) in path.iter().enumerate() {
        let Some(object) = cursor.as_object() else {
            return Err(JsoncError::invalid(not_object_message(path, depth)));
        };
        let Some(next) = object.get(*key) else {
            return Ok(None);
        };
        cursor = next;
    }
    Ok(Some(cursor))
}

fn walk_mut<'a>(value: &'a mut Value, path: &[&str]) -> Result<&'a mut Value, JsoncError> {
    let mut cursor = value;
    for (depth, key) in path.iter().enumerate() {
        cursor = cursor
            .as_object_mut()
            .ok_or_else(|| JsoncError::invalid(not_object_message(path, depth)))?
            .get_mut(*key)
            .ok_or_else(|| JsoncError::invalid(format!("could not locate config key `{key}`")))?;
    }
    Ok(cursor)
}

fn resolve<'a>(root: &'a Node, path: &[&str]) -> Result<&'a Node, JsoncError> {
    let mut node = root;
    for key in path {
        let object = node
            .object
            .as_ref()
            .ok_or_else(|| JsoncError::invalid(format!("config key `{key}` must be an object")))?;
        let index = member_index(object, key)
            .ok_or_else(|| JsoncError::invalid(format!("could not locate config key `{key}`")))?;
        node = &object.members[index].value;
    }
    Ok(node)
}

fn not_object_message(path: &[&str], depth: usize) -> String {
    if depth == 0 {
        "config root must be a JSON object".to_string()
    } else {
        format!("config key `{}` must be an object", path[depth - 1])
    }
}

fn display_path(container_path: &[&str], entry_key: &str) -> String {
    if container_path.is_empty() {
        entry_key.to_string()
    } else {
        format!("{}.{entry_key}", container_path.join("."))
    }
}

#[derive(Debug)]
struct Node {
    end: usize,
    object: Option<ObjectNode>,
}

#[derive(Debug)]
struct ObjectNode {
    members: Vec<Member>,
}

#[derive(Debug)]
struct Member {
    key: String,
    start: usize,
    value: Node,
    comma: Option<Range<usize>>,
}

/// Open arrays and objects the span parser follows before it refuses.
///
/// The parser recurses once per level, so an unbounded file (100,000 `[`
/// is about 100 KiB) exhausted the native stack instead of failing (#1054).
/// Every entry point also runs `serde_json::from_str`, whose default
/// recursion limit of 128 rejects anything at or past this depth anyway, so
/// matching it refuses nothing a caller could have succeeded with.
const MAX_NESTING_DEPTH: usize = 128;

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            depth: 0,
        }
    }

    fn parse_document(mut self) -> Result<Node, JsoncError> {
        self.skip_trivia()?;
        let node = self.parse_value()?;
        self.skip_trivia()?;
        if self.position != self.bytes.len() {
            return Err(self.error("unexpected content after root value"));
        }
        Ok(node)
    }

    fn parse_value(&mut self) -> Result<Node, JsoncError> {
        self.skip_trivia()?;
        match self.peek() {
            Some(open @ (b'{' | b'[')) => {
                if self.depth >= MAX_NESTING_DEPTH {
                    return Err(
                        self.error(format!("nesting deeper than {MAX_NESTING_DEPTH} levels"))
                    );
                }
                self.depth += 1;
                let node = if open == b'{' {
                    self.parse_object()
                } else {
                    self.parse_array()
                };
                self.depth -= 1;
                node
            }
            Some(b'"') => {
                self.parse_string()?;
                Ok(Node {
                    end: self.position,
                    object: None,
                })
            }
            Some(_) => self.parse_primitive(),
            None => Err(self.error("expected a JSON value")),
        }
    }

    fn parse_object(&mut self) -> Result<Node, JsoncError> {
        self.expect(b'{')?;
        let mut members: Vec<Member> = Vec::new();
        // A set, not a scan of `members`: a `~/.claude.json` can hold
        // thousands of project keys in one object.
        let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            self.skip_trivia()?;
            if self.peek() == Some(b'}') {
                self.position += 1;
                return Ok(Node {
                    end: self.position,
                    object: Some(ObjectNode { members }),
                });
            }

            let member_start = self.position;
            let key_range = self.parse_string()?;
            let key: String = serde_json::from_str(&self.input[key_range.clone()])
                .map_err(|error| self.error(format!("invalid object key: {error}")))?;
            if !keys.insert(key.clone()) {
                return Err(self.error(format!("duplicate object key `{key}` is ambiguous")));
            }
            self.skip_trivia()?;
            self.expect(b':')?;
            let value = self.parse_value()?;
            self.skip_trivia()?;
            let comma = if self.peek() == Some(b',') {
                let comma = self.position..self.position + 1;
                self.position += 1;
                Some(comma)
            } else {
                None
            };
            members.push(Member {
                key,
                start: member_start,
                value,
                comma,
            });
            self.skip_trivia()?;
            if members.last().is_some_and(|member| member.comma.is_none())
                && self.peek() != Some(b'}')
            {
                return Err(self.error("expected `,` or `}` after object member"));
            }
        }
    }

    fn parse_array(&mut self) -> Result<Node, JsoncError> {
        self.expect(b'[')?;
        // Array elements are validated, not recorded: no splice targets them.
        loop {
            self.skip_trivia()?;
            if self.peek() == Some(b']') {
                self.position += 1;
                return Ok(Node {
                    end: self.position,
                    object: None,
                });
            }
            self.parse_value()?;
            self.skip_trivia()?;
            let missing_comma = if self.peek() == Some(b',') {
                self.position += 1;
                false
            } else {
                true
            };
            if missing_comma && self.peek() != Some(b']') {
                return Err(self.error("expected `,` or `]` after array value"));
            }
        }
    }

    fn parse_string(&mut self) -> Result<Range<usize>, JsoncError> {
        let start = self.position;
        self.expect(b'"')?;
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.position += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return Ok(start..self.position);
            }
        }
        Err(self.error("unterminated JSON string"))
    }

    fn parse_primitive(&mut self) -> Result<Node, JsoncError> {
        let start = self.position;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}') {
                break;
            }
            if byte == b'/' && matches!(self.bytes.get(self.position + 1), Some(b'/') | Some(b'*'))
            {
                break;
            }
            self.position += 1;
        }
        if self.position == start {
            return Err(self.error("expected a JSON value"));
        }
        Ok(Node {
            end: self.position,
            object: None,
        })
    }

    fn skip_trivia(&mut self) -> Result<(), JsoncError> {
        loop {
            while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
                self.position += 1;
            }
            if self.bytes.get(self.position..self.position + 2) == Some(b"//") {
                self.position += 2;
                while self.peek().is_some_and(|byte| byte != b'\n') {
                    self.position += 1;
                }
                continue;
            }
            if self.bytes.get(self.position..self.position + 2) == Some(b"/*") {
                self.position += 2;
                let mut closed = false;
                while self.position + 1 < self.bytes.len() {
                    if self.bytes.get(self.position..self.position + 2) == Some(b"*/") {
                        self.position += 2;
                        closed = true;
                        break;
                    }
                    self.position += 1;
                }
                if !closed {
                    return Err(self.error("unterminated block comment"));
                }
                continue;
            }
            return Ok(());
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), JsoncError> {
        if self.peek() == Some(expected) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(format!("expected `{}`", char::from(expected))))
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn error(&self, message: impl Into<String>) -> JsoncError {
        JsoncError::invalid(format!("{} at byte {}", message.into(), self.position))
    }
}

fn strip_comments(input: &str) -> Result<String, JsoncError> {
    let bytes = input.as_bytes();
    let mut output = bytes.to_vec();
    let mut position = 0;
    let mut in_string = false;
    let mut escaped = false;

    while position < bytes.len() {
        let byte = bytes[position];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            position += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            position += 1;
            continue;
        }
        if bytes.get(position..position + 2) == Some(b"//") {
            output[position] = b' ';
            output[position + 1] = b' ';
            position += 2;
            while position < bytes.len() && bytes[position] != b'\n' {
                output[position] = b' ';
                position += 1;
            }
            continue;
        }
        if bytes.get(position..position + 2) == Some(b"/*") {
            output[position] = b' ';
            output[position + 1] = b' ';
            position += 2;
            let mut closed = false;
            while position < bytes.len() {
                if bytes.get(position..position + 2) == Some(b"*/") {
                    output[position] = b' ';
                    output[position + 1] = b' ';
                    position += 2;
                    closed = true;
                    break;
                }
                if bytes[position] != b'\n' && bytes[position] != b'\r' {
                    output[position] = b' ';
                }
                position += 1;
            }
            if !closed {
                return Err(JsoncError::invalid("unterminated block comment"));
            }
            continue;
        }
        position += 1;
    }
    String::from_utf8(output)
        .map_err(|error| JsoncError::invalid(format!("JSONC must remain UTF-8: {error}")))
}

fn remove_trailing_commas(input: &str) -> Result<String, JsoncError> {
    let bytes = input.as_bytes();
    let mut output = bytes.to_vec();
    let mut position = 0;
    let mut in_string = false;
    let mut escaped = false;
    while position < bytes.len() {
        let byte = bytes[position];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            position += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            position += 1;
            continue;
        }
        if byte == b',' {
            let next = bytes[position + 1..]
                .iter()
                .copied()
                .find(|candidate| !candidate.is_ascii_whitespace());
            if matches!(next, Some(b'}') | Some(b']')) {
                output[position] = b' ';
                position += 1;
                continue;
            }
        }
        position += 1;
    }
    String::from_utf8(output)
        .map_err(|error| JsoncError::invalid(format!("JSONC must remain UTF-8: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"{
  // this comment must survive
  "$schema": "https://example.test/schema.json",
  "mcp": {
    "weather": { "command": ["weather-mcp"] },
    // Paneflow entry comment
    "paneflow": { "command": ["/old"] },
  },
}"#;

    #[test]
    fn remove_preserves_comments_siblings_and_trailing_commas() {
        let removed = remove_entry(SOURCE, "mcp", "paneflow").unwrap().unwrap();
        assert_eq!(
            removed,
            r#"{
  // this comment must survive
  "$schema": "https://example.test/schema.json",
  "mcp": {
    "weather": { "command": ["weather-mcp"] },
    // Paneflow entry comment
  },
}"#
        );
        assert_eq!(remove_entry(&removed, "mcp", "paneflow").unwrap(), None);
    }

    #[test]
    fn remove_drops_the_members_own_lines_and_nothing_else() {
        let first = "{\r\n\t\"mcp\": {\r\n\t\t\"paneflow\": {\r\n\t\t\t\"command\": [\"/p\"]\r\n\t\t},\r\n\t\t\"w\": 1\r\n\t}\r\n}\r\n";
        assert_eq!(
            remove_entry(first, "mcp", "paneflow").unwrap().unwrap(),
            "{\r\n\t\"mcp\": {\r\n\t\t\"w\": 1\r\n\t}\r\n}\r\n"
        );
        let last = "{\n  \"mcp\": {\n    \"w\": 1,\n    \"paneflow\": 2\n  }\n}\n";
        assert_eq!(
            remove_entry(last, "mcp", "paneflow").unwrap().unwrap(),
            "{\n  \"mcp\": {\n    \"w\": 1\n  }\n}\n"
        );
        let only = "{\n  \"mcp\": {\n    \"paneflow\": 2\n  }\n}\n";
        assert_eq!(
            remove_entry(only, "mcp", "paneflow").unwrap().unwrap(),
            "{\n  \"mcp\": {\n  }\n}\n"
        );
        let shared = "{\"mcp\": {\"paneflow\": 2, \"w\": 1}, \"x\": 0}";
        assert_eq!(
            remove_entry(shared, "mcp", "paneflow").unwrap().unwrap(),
            "{\"mcp\": { \"w\": 1}, \"x\": 0}"
        );
        let trailing_comment =
            "{\n  \"mcp\": {\n    \"paneflow\": 2, // ours\n    \"w\": 1\n  }\n}\n";
        assert_eq!(
            remove_entry(trailing_comment, "mcp", "paneflow")
                .unwrap()
                .unwrap(),
            "{\n  \"mcp\": {\n     // ours\n    \"w\": 1\n  }\n}\n"
        );
    }

    /// The layout the old `paneflow mcp install` splice produced: it added
    /// the separating comma right after the previous sibling's value (before
    /// that sibling's trailing comment) and put the entry on its own lines
    /// just above the closing brace, after any commented-out sibling line.
    #[test]
    fn removing_the_last_member_keeps_the_siblings_comments() {
        let installed = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {\n    // \"github\": { \"type\": \"local\", \"command\": [\"gh\"] },\n    \"weather\": { \"type\": \"local\", \"command\": [\"weather-mcp\"] }, // mine\n    // \"old\": { \"command\": [\"x\"] },\n    \"paneflow\": {\n        \"type\": \"local\",\n        \"command\": [\n            \"/x/paneflow-mcp\"\n        ],\n        \"enabled\": true\n    }\n  }\n}\n";
        let removed = remove_entry(installed, "mcp", "paneflow").unwrap().unwrap();
        assert_eq!(
            removed,
            "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {\n    // \"github\": { \"type\": \"local\", \"command\": [\"gh\"] },\n    \"weather\": { \"type\": \"local\", \"command\": [\"weather-mcp\"] } // mine\n    // \"old\": { \"command\": [\"x\"] },\n  }\n}\n"
        );
    }

    #[test]
    fn strict_json_stays_strict_json_wherever_the_member_sits() {
        let entry = "\"paneflow\": {\"command\": \"/x/paneflow-mcp\"}";
        for (source, expected) in [
            (
                format!("{{\"mcp\": {{{entry}, \"a\": 1, \"b\": 2}}}}"),
                "{\"mcp\": {\"a\": 1, \"b\": 2}}",
            ),
            (
                format!("{{\"mcp\": {{\"a\": 1, {entry}, \"b\": 2}}}}"),
                "{\"mcp\": {\"a\": 1, \"b\": 2}}",
            ),
            (
                format!("{{\"mcp\": {{\"a\": 1, \"b\": 2, {entry}}}}}"),
                "{\"mcp\": {\"a\": 1, \"b\": 2}}",
            ),
            (format!("{{\"mcp\": {{{entry}}}}}"), "{\"mcp\": {}}"),
            (
                format!("{{\n  \"mcp\": {{\n    \"a\": 1,\n    {entry}\n  }}\n}}\n"),
                "{\n  \"mcp\": {\n    \"a\": 1\n  }\n}\n",
            ),
            (
                format!("{{\n  \"mcp\": {{\n    {entry},\n    \"a\": 1\n  }}\n}}\n"),
                "{\n  \"mcp\": {\n    \"a\": 1\n  }\n}\n",
            ),
        ] {
            let removed = remove_entry(&source, "mcp", "paneflow").unwrap().unwrap();
            let strict = serde_json::from_str::<Value>(&removed);
            assert!(strict.is_ok(), "not strict JSON ({strict:?}): {removed}");
            let strict = strict.unwrap();
            let expected_value: Value = serde_json::from_str(expected).unwrap();
            assert_eq!(strict, expected_value, "{source}");
            // A one-line object keeps the blanks around the cut; a member on
            // lines of its own leaves none.
            if source.contains('\n') {
                assert_eq!(removed, expected, "{source}");
            } else {
                let compact = |text: &str| text.split_whitespace().collect::<String>();
                assert_eq!(compact(&removed), compact(expected), "{source}");
            }
        }
    }

    #[test]
    fn remove_of_an_absent_entry_or_container_is_a_noop() {
        assert_eq!(remove_entry(SOURCE, "mcp", "missing").unwrap(), None);
        assert_eq!(remove_entry(SOURCE, "servers", "paneflow").unwrap(), None);
    }

    #[test]
    fn invalid_boundaries_and_duplicate_keys_are_refused() {
        assert!(remove_entry("[]", "mcp", "paneflow").is_err());
        assert!(remove_entry("{\"mcp\": 1}", "mcp", "paneflow").is_err());
        let duplicate_current =
            r#"{"mcp":{"paneflow":{"command":["/old"]},"paneflow":{"command":["/old"]}}}"#;
        assert!(parse(duplicate_current).is_err());
        assert!(remove_entry(duplicate_current, "mcp", "paneflow").is_err());
        assert!(remove_entry(r#"{"mcp":{},"mcp":{}}"#, "mcp", "paneflow").is_err());
        assert!(parse("{/* unterminated").is_err());
    }

    /// `levels` nested arrays or objects around `1`.
    fn nested(levels: usize, object: bool) -> String {
        let (open, close) = if object { ("{\"a\":", "}") } else { ("[", "]") };
        format!("{}1{}", open.repeat(levels), close.repeat(levels))
    }

    #[test]
    fn excessive_nesting_is_an_error_not_a_stack_overflow() {
        // Issue #1054: the span parser recursed once per level with no budget,
        // so a small but deep settings file exhausted the native stack.
        for object in [false, true] {
            let deep = nested(100_000, object);
            let inside = format!("{{\"mcp\":{{\"paneflow\":{deep}}}}}");
            for input in [&deep, &inside] {
                let parsed = parse(input).unwrap_err();
                assert!(parsed.to_string().contains("nesting"), "{parsed}");
                let removed = remove_entry(input, "mcp", "paneflow").unwrap_err();
                assert!(removed.to_string().contains("nesting"), "{removed}");
            }
        }
    }

    #[test]
    fn nesting_budget_accepts_what_serde_json_accepts() {
        for object in [false, true] {
            // serde_json's default recursion limit admits 127 levels, and
            // the span parser must not refuse anything below that.
            assert!(parse(&nested(127, object)).is_ok());
            let inside = format!("{{\"mcp\":{{\"paneflow\":{}}}}}", nested(125, object));
            assert!(remove_entry(&inside, "mcp", "paneflow").unwrap().is_some());
            assert!(parse(&nested(128, object)).is_err());
        }
    }
}
