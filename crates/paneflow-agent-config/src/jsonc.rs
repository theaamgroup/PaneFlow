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
/// entry is already absent. This is the inverse of [`insert_entry`] for a
/// member that function just added.
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

/// Insert `entry_key` as the last member of the object at `container_path`.
///
/// An empty path is the root object. Comments, trailing commas, and every
/// byte outside the new member stay put. Removing the inserted member with
/// [`remove_at`] restores the previous bytes.
pub fn insert_entry(
    input: &str,
    container_path: &[&str],
    entry_key: &str,
    value: &Value,
) -> Result<String, JsoncError> {
    let semantic = parse(input)?;
    let container = walk_required(&semantic, container_path)?;
    let container = container.as_object().ok_or_else(|| {
        JsoncError::invalid(not_object_message(container_path, container_path.len()))
    })?;
    if container.contains_key(entry_key) {
        return Err(JsoncError::invalid(format!(
            "entry `{entry_key}` already exists"
        )));
    }

    let document = Parser::new(input).parse_document()?;
    let container_node = resolve(&document, container_path)?;
    let object = container_node.object.as_ref().ok_or_else(|| {
        JsoncError::invalid(not_object_message(container_path, container_path.len()))
    })?;
    if member_index(object, entry_key).is_some() {
        return Err(JsoncError::invalid(format!(
            "entry `{entry_key}` already exists"
        )));
    }
    let updated = insert_span(
        input,
        container_node,
        b'}',
        &object_spans(object),
        &render_member(entry_key, value)?,
    )?;
    let got = parse(&updated)?;
    let mut expected = semantic;
    insert_semantic(&mut expected, container_path, entry_key, value.clone())?;
    if got != expected {
        return Err(JsoncError::invalid(format!(
            "internal error: jsonc splice changed more than `{}`",
            display_path(container_path, entry_key)
        )));
    }
    Ok(updated)
}

/// Append `value` to the array at `array_path` without reserializing it.
pub fn append_array_element(
    input: &str,
    array_path: &[&str],
    value: &Value,
) -> Result<String, JsoncError> {
    let semantic = parse(input)?;
    let array = walk_required(&semantic, array_path)?;
    if !array.is_array() {
        return Err(JsoncError::invalid(format!(
            "config key `{}` must be an array",
            array_path.last().copied().unwrap_or("root")
        )));
    }

    let document = Parser::new(input).parse_document()?;
    let array_node = resolve(&document, array_path)?;
    let elements = array_node.array.as_ref().ok_or_else(|| {
        JsoncError::invalid(format!(
            "config key `{}` must be an array",
            array_path.last().copied().unwrap_or("root")
        ))
    })?;
    let updated = insert_span(
        input,
        array_node,
        b']',
        &array_spans(elements),
        &render_value(value)?,
    )?;
    let got = parse(&updated)?;
    let mut expected = semantic;
    let appended = walk_mut(&mut expected, array_path)?
        .as_array_mut()
        .is_some_and(|array| {
            array.push(value.clone());
            true
        });
    if !appended || got != expected {
        return Err(JsoncError::invalid(format!(
            "internal error: jsonc splice changed more than `{}`",
            array_path.join(".")
        )));
    }
    Ok(updated)
}

/// Remove array elements for which `predicate` is true.
///
/// `Ok(None)` when the path is absent, is not an array, or contains no match.
/// A match that cannot be removed without touching other bytes is an error,
/// and the returned text is the only candidate a caller should write.
pub fn remove_array_elements(
    input: &str,
    array_path: &[&str],
    predicate: impl Fn(&Value) -> bool,
) -> Result<Option<String>, JsoncError> {
    remove_matching(input, array_path, |semantic| {
        let array = walk_lenient(semantic, array_path)?.as_array()?;
        array
            .iter()
            .position(&predicate)
            .map(|index| CutTarget::Array { index })
    })
}

/// Remove elements of the `inner_key` array inside elements of `outer_path`.
///
/// Used when a matcher group mixes a managed handler with a user's handler:
/// the managed command goes, and the group stays. `Ok(None)` when nothing
/// matches or the path is not an array of objects.
pub fn remove_nested_array_elements(
    input: &str,
    outer_path: &[&str],
    inner_key: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Option<String>, JsoncError> {
    remove_matching(input, outer_path, |semantic| {
        let outer = walk_lenient(semantic, outer_path)?.as_array()?;
        outer.iter().enumerate().find_map(|(outer_index, group)| {
            let inner = group.get(inner_key)?.as_array()?;
            let inner_index = inner.iter().position(&predicate)?;
            Some(CutTarget::Nested {
                outer_index,
                inner_key: inner_key.to_string(),
                inner_index,
            })
        })
    })
}

/// Whether the value at `path` contains a `//` or `/* */` comment outside a string.
///
/// Missing paths are `false`. A span that cannot be scanned is `true`, so a
/// caller that deletes the value only when this is false keeps the comment.
pub fn value_contains_comment(input: &str, path: &[&str]) -> Result<bool, JsoncError> {
    let semantic = parse(input)?;
    if walk_optional(&semantic, path)?.is_none() {
        return Ok(false);
    }
    let document = Parser::new(input).parse_document()?;
    let node = resolve(&document, path)?;
    if node.end < node.start || node.end > input.len() {
        return Ok(true);
    }
    let slice = &input[node.start..node.end];
    match strip_comments(slice) {
        Ok(stripped) => Ok(stripped != slice),
        Err(_) => Ok(true),
    }
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

fn object_spans(object: &ObjectNode) -> Vec<ExistingSpan> {
    object
        .members
        .iter()
        .map(|member| ExistingSpan {
            start: member.start,
            value_end: member.value.end,
            has_comma: member.comma.is_some(),
        })
        .collect()
}

fn array_spans(array: &ArrayNode) -> Vec<ExistingSpan> {
    array
        .elements
        .iter()
        .map(|element| ExistingSpan {
            start: element.start,
            value_end: element.value.end,
            has_comma: element.comma.is_some(),
        })
        .collect()
}

struct ExistingSpan {
    start: usize,
    value_end: usize,
    has_comma: bool,
}

struct Insertion {
    comma_at: Option<usize>,
    at: usize,
    text: String,
}

enum CutTarget {
    Array {
        index: usize,
    },
    Nested {
        outer_index: usize,
        inner_key: String,
        inner_index: usize,
    },
}

fn remove_matching(
    input: &str,
    array_path: &[&str],
    mut find: impl FnMut(&Value) -> Option<CutTarget>,
) -> Result<Option<String>, JsoncError> {
    let mut current = input.to_string();
    let mut changed = false;
    for _ in 0..10_000 {
        let semantic = parse(&current)?;
        let Some(target) = find(&semantic) else {
            if changed {
                return Ok(Some(current));
            }
            return Ok(None);
        };
        let document = Parser::new(&current).parse_document()?;
        let node = resolve(&document, array_path)?;
        let updated = cut_array_element(&current, node, &target)?;
        let got = parse(&updated)?;
        let mut expected = semantic;
        forget_array_element(&mut expected, array_path, &target)?;
        if got != expected {
            return Err(JsoncError::invalid(
                "internal error: jsonc splice changed more than the removed array element",
            ));
        }
        current = updated;
        changed = true;
    }
    Err(JsoncError::invalid(
        "internal error: jsonc array removal did not finish",
    ))
}

fn cut_array_element(input: &str, node: &Node, target: &CutTarget) -> Result<String, JsoncError> {
    let array = node.array.as_ref().ok_or_else(|| {
        JsoncError::invalid("internal error: jsonc array disappeared during splice")
    })?;
    match target {
        CutTarget::Array { index } => {
            if *index >= array.elements.len() {
                return Err(JsoncError::invalid(
                    "internal error: jsonc array index does not match the parsed value",
                ));
            }
            Ok(remove_element(input, array, *index))
        }
        CutTarget::Nested {
            outer_index,
            inner_key,
            inner_index,
        } => {
            let element = array.elements.get(*outer_index).ok_or_else(|| {
                JsoncError::invalid(
                    "internal error: jsonc array index does not match the parsed value",
                )
            })?;
            let object = element.value.object.as_ref().ok_or_else(|| {
                JsoncError::invalid("internal error: jsonc element is not an object")
            })?;
            let member = member_index(object, inner_key).ok_or_else(|| {
                JsoncError::invalid(format!("could not locate entry `{inner_key}`"))
            })?;
            let inner =
                object.members[member].value.array.as_ref().ok_or_else(|| {
                    JsoncError::invalid(format!("`{inner_key}` must be an array"))
                })?;
            if *inner_index >= inner.elements.len() {
                return Err(JsoncError::invalid(
                    "internal error: jsonc array index does not match the parsed value",
                ));
            }
            Ok(remove_element(input, inner, *inner_index))
        }
    }
}

fn forget_array_element(
    expected: &mut Value,
    array_path: &[&str],
    target: &CutTarget,
) -> Result<(), JsoncError> {
    match target {
        CutTarget::Array { index } => {
            let array = walk_mut(expected, array_path)?
                .as_array_mut()
                .ok_or_else(|| {
                    JsoncError::invalid("internal error: jsonc array disappeared during splice")
                })?;
            if *index >= array.len() {
                return Err(JsoncError::invalid(
                    "internal error: jsonc array index does not match the parsed value",
                ));
            }
            array.remove(*index);
            Ok(())
        }
        CutTarget::Nested {
            outer_index,
            inner_key,
            inner_index,
        } => {
            let group = walk_mut(expected, array_path)?
                .as_array_mut()
                .and_then(|groups| groups.get_mut(*outer_index))
                .ok_or_else(|| {
                    JsoncError::invalid(
                        "internal error: jsonc array index does not match the parsed value",
                    )
                })?;
            let inner = group
                .get_mut(inner_key)
                .and_then(Value::as_array_mut)
                .ok_or_else(|| JsoncError::invalid(format!("`{inner_key}` must be an array")))?;
            if *inner_index >= inner.len() {
                return Err(JsoncError::invalid(
                    "internal error: jsonc array index does not match the parsed value",
                ));
            }
            inner.remove(*inner_index);
            Ok(())
        }
    }
}

fn insert_span(
    input: &str,
    container: &Node,
    closing: u8,
    items: &[ExistingSpan],
    body: &str,
) -> Result<String, JsoncError> {
    if container.end == 0 || container.end > input.len() {
        return Err(JsoncError::invalid(
            "internal error: jsonc container span is out of range",
        ));
    }
    let close = container.end - 1;
    if input.as_bytes().get(close) != Some(&closing) {
        return Err(JsoncError::invalid(
            "internal error: jsonc container is not closed",
        ));
    }
    if body.is_empty() {
        return Err(JsoncError::invalid(
            "internal error: jsonc insertion is empty",
        ));
    }
    let plan = plan_insertion(input, container.start, close, items, body);
    apply_insertion(input, plan)
}

fn plan_insertion(
    input: &str,
    open: usize,
    close: usize,
    items: &[ExistingSpan],
    body: &str,
) -> Insertion {
    let newline = if input.contains("\r\n") { "\r\n" } else { "\n" };
    if items.is_empty() {
        let interior = &input[open + 1..close];
        if interior.contains('\n') {
            Insertion {
                comma_at: None,
                at: line_start(input, close),
                text: format!("  {body}{newline}"),
            }
        } else {
            Insertion {
                comma_at: None,
                at: close,
                text: body.to_string(),
            }
        }
    } else {
        let last = &items[items.len() - 1];
        if on_its_own_line(input, close) {
            let indent = item_indent(input, last.start);
            let at = line_start(input, close);
            if last.has_comma {
                Insertion {
                    comma_at: None,
                    at,
                    text: format!("{indent}{body},{newline}"),
                }
            } else {
                Insertion {
                    comma_at: Some(last.value_end),
                    at,
                    text: format!("{indent}{body}{newline}"),
                }
            }
        } else if last.has_comma {
            Insertion {
                comma_at: None,
                at: close,
                text: format!("{body},"),
            }
        } else {
            Insertion {
                comma_at: Some(last.value_end),
                at: close,
                text: body.to_string(),
            }
        }
    }
}

fn apply_insertion(input: &str, plan: Insertion) -> Result<String, JsoncError> {
    if plan.at > input.len()
        || !input.is_char_boundary(plan.at)
        || plan
            .comma_at
            .is_some_and(|index| index > plan.at || !input.is_char_boundary(index))
    {
        return Err(JsoncError::invalid(
            "internal error: jsonc insertion point is not a char boundary",
        ));
    }
    let mut output = input.to_string();
    output.insert_str(plan.at, &plan.text);
    if let Some(comma_at) = plan.comma_at {
        output.insert(comma_at, ',');
    }
    Ok(output)
}

fn line_start(input: &str, index: usize) -> usize {
    input[..index].rfind('\n').map_or(0, |newline| newline + 1)
}

fn on_its_own_line(input: &str, close: usize) -> bool {
    let start = line_start(input, close);
    input[start..close]
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

fn item_indent(input: &str, item_start: usize) -> String {
    let start = line_start(input, item_start);
    let prefix = &input[start..item_start];
    if !prefix.is_empty() && prefix.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
        prefix.to_string()
    } else {
        "  ".to_string()
    }
}

fn render_member(key: &str, value: &Value) -> Result<String, JsoncError> {
    let key = serde_json::to_string(key)
        .map_err(|error| JsoncError::invalid(format!("could not render JSON key: {error}")))?;
    Ok(format!("{key}:{}", render_value(value)?))
}

fn render_value(value: &Value) -> Result<String, JsoncError> {
    serde_json::to_string(value)
        .map_err(|error| JsoncError::invalid(format!("could not render JSON value: {error}")))
}

fn insert_semantic(
    root: &mut Value,
    path: &[&str],
    key: &str,
    value: Value,
) -> Result<(), JsoncError> {
    let container = walk_mut(root, path)?;
    let object = container
        .as_object_mut()
        .ok_or_else(|| JsoncError::invalid(not_object_message(path, path.len())))?;
    object.insert(key.to_string(), value);
    Ok(())
}

fn walk_required<'a>(value: &'a Value, path: &[&str]) -> Result<&'a Value, JsoncError> {
    walk_optional(value, path)?.ok_or_else(|| JsoncError::invalid(missing_message(path)))
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

fn walk_lenient<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    walk_optional(value, path).ok().flatten()
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

fn missing_message(path: &[&str]) -> String {
    match path.last() {
        Some(key) => format!("could not locate config key `{key}`"),
        None => "could not locate config root".to_string(),
    }
}

fn display_path(container_path: &[&str], entry_key: &str) -> String {
    if container_path.is_empty() {
        entry_key.to_string()
    } else {
        format!("{}.{entry_key}", container_path.join("."))
    }
}

fn remove_element(input: &str, array: &ArrayNode, index: usize) -> String {
    let element = &array.elements[index];
    let mut output = input.to_string();
    if let Some(comma) = &element.comma {
        output.replace_range(whole_lines(input, element.start..comma.end), "");
    } else if let Some(previous_comma) = index
        .checked_sub(1)
        .and_then(|previous| array.elements[previous].comma.as_ref())
    {
        output.replace_range(whole_lines(input, element.start..element.value.end), "");
        output.replace_range(previous_comma.clone(), "");
    } else {
        output.replace_range(whole_lines(input, element.start..element.value.end), "");
    }
    output
}

#[derive(Debug)]
struct Node {
    start: usize,
    end: usize,
    object: Option<ObjectNode>,
    array: Option<ArrayNode>,
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

#[derive(Debug)]
struct ArrayNode {
    elements: Vec<Element>,
}

#[derive(Debug)]
struct Element {
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
                let start = self.position;
                self.parse_string()?;
                Ok(Node {
                    start,
                    end: self.position,
                    object: None,
                    array: None,
                })
            }
            Some(_) => self.parse_primitive(),
            None => Err(self.error("expected a JSON value")),
        }
    }

    fn parse_object(&mut self) -> Result<Node, JsoncError> {
        let start = self.position;
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
                    start,
                    end: self.position,
                    object: Some(ObjectNode { members }),
                    array: None,
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
        let start = self.position;
        self.expect(b'[')?;
        let mut elements: Vec<Element> = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.peek() == Some(b']') {
                self.position += 1;
                return Ok(Node {
                    start,
                    end: self.position,
                    object: None,
                    array: Some(ArrayNode { elements }),
                });
            }
            let value = self.parse_value()?;
            let element_start = value.start;
            self.skip_trivia()?;
            let comma = if self.peek() == Some(b',') {
                let comma = self.position..self.position + 1;
                self.position += 1;
                Some(comma)
            } else {
                None
            };
            let missing_comma = comma.is_none();
            elements.push(Element {
                start: element_start,
                value,
                comma,
            });
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
            start,
            end: self.position,
            object: None,
            array: None,
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

    fn assert_insert_round_trip(source: &str, path: &[&str], key: &str, value: Value) {
        let inserted = insert_entry(source, path, key, &value).unwrap();
        assert_ne!(inserted, source);
        if source.contains("//") {
            assert!(inserted.contains("//"), "{inserted}");
        }
        if source.contains("/*") {
            assert!(inserted.contains("/*"), "{inserted}");
        }
        let removed = remove_at(&inserted, path, key).unwrap().unwrap();
        assert_eq!(removed, source, "inserted:\n{inserted}");
    }

    #[test]
    fn insert_entry_is_the_inverse_of_remove() {
        let value = serde_json::json!({"BeforeAgent":[{"matcher":"*","hooks":[{"command":"paneflow-ai-hook UserPromptSubmit","timeout":5000}]}]});
        assert_insert_round_trip(
            "{\n  // Gemini accepts comments in settings.json\n  \"theme\": \"Default\"\n}\n",
            &[],
            "hooks",
            value.clone(),
        );
        assert_insert_round_trip(
            "{\n  \"theme\": \"Default\",\n}\n",
            &[],
            "hooks",
            value.clone(),
        );
        assert_insert_round_trip("{ \"theme\": \"Default\" }", &[], "hooks", value.clone());
        assert_insert_round_trip("{}", &[], "hooks", value.clone());
        assert_insert_round_trip("{\n}\n", &[], "hooks", value.clone());
        assert_insert_round_trip(
            "{\r\n  \"theme\": \"Default\"\r\n}\r\n",
            &[],
            "hooks",
            value.clone(),
        );
        assert_insert_round_trip(
            "{\n  /* block */\n  \"theme\": \"Default\" // keep\n}\n",
            &[],
            "hooks",
            value.clone(),
        );
        assert_insert_round_trip(
            "{\n  \"hooks\": {\n    // reserved\n  }\n}\n",
            &["hooks"],
            "BeforeAgent",
            serde_json::json!([{"matcher":"*"}]),
        );
    }

    #[test]
    fn append_array_element_is_the_inverse_of_remove() {
        let cases = [
            "{\n  \"hooks\": {\n    \"BeforeAgent\": [\n      { \"command\": \"mine\" }\n    ]\n  }\n}\n",
            "{\n  \"hooks\": {\n    \"BeforeAgent\": [{ \"command\": \"mine\" },]\n  }\n}\n",
            "{\"hooks\":{\"BeforeAgent\":[{\"command\":\"mine\"}]}}",
            "{\n  \"hooks\": {\n    \"BeforeAgent\": []\n  }\n}\n",
            "{\n  \"hooks\": {\n    \"BeforeAgent\": [\n    ]\n  }\n}\n",
        ];
        let value = serde_json::json!({"command":"paneflow-ai-hook UserPromptSubmit"});
        for source in cases {
            let appended = append_array_element(source, &["hooks", "BeforeAgent"], &value).unwrap();
            assert!(appended.contains("paneflow-ai-hook"), "{appended}");
            assert!(
                source.contains("mine") == appended.contains("mine"),
                "{appended}"
            );
            let removed = remove_array_elements(&appended, &["hooks", "BeforeAgent"], |element| {
                element.get("command").and_then(Value::as_str)
                    == Some("paneflow-ai-hook UserPromptSubmit")
            })
            .unwrap()
            .unwrap();
            assert_eq!(removed, source, "appended:\n{appended}");
        }
    }
}
