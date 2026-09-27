//! Finds a top-level `hooks` key in a Hermes `config.yaml` without a YAML
//! parser (the shim is size-capped).
//!
//! YAML can spell that key as `hooks:`, `"hooks":`, `'hooks':`, `hooks :`,
//! `? hooks`, with an anchor or tag in front, or inside a flow mapping root,
//! and any of them turns an appended `hooks:` block into a duplicate key
//! that silently overrides the user's hooks (#1056). The scan is line based
//! and conservative: whatever it cannot rule out counts as a match, so the
//! caller refuses rather than appends.

/// True when `content` defines, or may define, a top-level `hooks` key.
pub(super) fn yaml_may_have_top_level_hooks(content: &str) -> bool {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut root_indent: Option<usize> = None;
    let mut flow_root = false;
    for line in content.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let body = line.trim_start_matches(' ');
        let indent = line.len() - body.len();
        if indent == 0 {
            if body.starts_with('%') {
                continue;
            }
            if let Some(rest) = document_marker(body) {
                root_indent = None;
                flow_root = false;
                let node = strip_properties(rest.trim_start());
                if starts_flow(node) {
                    flow_root = true;
                }
                if node_may_be_hooks(node) {
                    return true;
                }
                continue;
            }
        }
        if flow_root {
            // Keys of a flow mapping root sit at any indentation.
            if line.contains("hooks") {
                return true;
            }
            continue;
        }
        let node = strip_properties(body.trim_start());
        if node.is_empty() || node.starts_with('#') {
            continue;
        }
        match root_indent {
            None => {
                root_indent = Some(indent);
                flow_root = starts_flow(node);
            }
            // Deeper lines belong to a nested node: a child mapping, a
            // block scalar, or the continuation of a value.
            Some(root) if indent > root => continue,
            Some(_) => {}
        }
        if node_may_be_hooks(node) {
            return true;
        }
    }
    false
}

/// `---` or `...` followed by nothing or whitespace; returns the rest.
fn document_marker(line: &str) -> Option<&str> {
    let rest = line
        .strip_prefix("---")
        .or_else(|| line.strip_prefix("..."))?;
    (rest.is_empty() || rest.starts_with([' ', '\t'])).then_some(rest)
}

fn starts_flow(node: &str) -> bool {
    node.starts_with(['{', '['])
}

/// Drops leading `&anchor` and `!tag` node properties.
fn strip_properties(mut node: &str) -> &str {
    while node.starts_with(['&', '!']) {
        let end = node.find([' ', '\t']).unwrap_or(node.len());
        node = node[end..].trim_start();
    }
    node
}

/// Whether one line, starting at a top-level node, may be a `hooks` key.
fn node_may_be_hooks(node: &str) -> bool {
    let indicator_then_space = |indicator: char| {
        node.strip_prefix(indicator)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    };
    if node.is_empty() || node.starts_with('#') {
        return false;
    }
    if indicator_then_space('?') {
        return explicit_key_may_be_hooks(node[1..].trim_start());
    }
    // A `: value` line of an explicit key, or a sequence entry.
    if indicator_then_space(':') || indicator_then_space('-') {
        return false;
    }
    match node.as_bytes()[0] {
        b'"' | b'\'' => match quoted(node) {
            Some((key, rest)) => key == "hooks" && rest.trim_start().starts_with(':'),
            None => true,
        },
        // An alias key may name `hooks`, and a merge key can pull it in.
        b'*' => true,
        b'<' if node.starts_with("<<") => true,
        b'{' | b'[' => node.contains("hooks"),
        // A root block scalar or a reserved indicator is not a key.
        b'|' | b'>' | b'@' | b'`' => false,
        _ => node.starts_with("hooks:") || plain_key(node) == Some("hooks"),
    }
}

fn explicit_key_may_be_hooks(key: &str) -> bool {
    if key.is_empty() || key.starts_with('#') {
        // The key itself sits on the following lines.
        return true;
    }
    let key = strip_properties(key);
    match key.as_bytes().first() {
        Some(b'"' | b'\'') => quoted(key).is_none_or(|(key, _)| key == "hooks"),
        Some(b'|' | b'>' | b'{' | b'[' | b'*') | None => true,
        Some(_) => {
            let end = comment_start(key).unwrap_or(key.len());
            key[..end].trim_end() == "hooks"
        }
    }
}

/// The key of a plain-scalar `key: value` line, if the line has one.
fn plain_key(node: &str) -> Option<&str> {
    let bytes = node.as_bytes();
    let limit = comment_start(node).unwrap_or(node.len());
    (0..limit)
        .find(|&index| {
            bytes[index] == b':' && matches!(bytes.get(index + 1), None | Some(b' ' | b'\t'))
        })
        .map(|index| node[..index].trim_end())
}

/// Byte offset of a ` #` comment, which ends a plain scalar.
fn comment_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    (1..bytes.len()).find(|&index| bytes[index] == b'#' && matches!(bytes[index - 1], b' ' | b'\t'))
}

/// Decodes a single-line quoted scalar at the start of `node` and returns
/// it with the text after the closing quote. `None` when the scalar does
/// not close on this line or holds an escape this scan does not decode.
fn quoted(node: &str) -> Option<(String, &str)> {
    let quote = node.chars().next()?;
    let mut out = String::new();
    let mut chars = node.char_indices().skip(1);
    while let Some((index, c)) = chars.next() {
        if quote == '\'' && c == '\'' {
            if node[index + 1..].starts_with('\'') {
                out.push('\'');
                chars.next();
                continue;
            }
            return Some((out, &node[index + 1..]));
        }
        if quote == '"' && c == '"' {
            return Some((out, &node[index + 1..]));
        }
        if quote == '"' && c == '\\' {
            let (_, escape) = chars.next()?;
            let simple = match escape {
                '0' => Some('\0'),
                'a' => Some('\u{7}'),
                'b' => Some('\u{8}'),
                't' | '\t' => Some('\t'),
                'n' => Some('\n'),
                'v' => Some('\u{b}'),
                'f' => Some('\u{c}'),
                'r' => Some('\r'),
                'e' => Some('\u{1b}'),
                ' ' => Some(' '),
                '"' => Some('"'),
                '/' => Some('/'),
                '\\' => Some('\\'),
                'N' => Some('\u{85}'),
                '_' => Some('\u{a0}'),
                'L' => Some('\u{2028}'),
                'P' => Some('\u{2029}'),
                _ => None,
            };
            if let Some(simple) = simple {
                out.push(simple);
                continue;
            }
            let digits = match escape {
                'x' => 2,
                'u' => 4,
                'U' => 8,
                _ => return None,
            };
            let mut code = 0u32;
            for _ in 0..digits {
                let (_, digit) = chars.next()?;
                code = code * 16 + digit.to_digit(16)?;
            }
            out.push(char::from_u32(code)?);
            continue;
        }
        out.push(c);
    }
    None
}
