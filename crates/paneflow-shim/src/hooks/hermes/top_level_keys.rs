//! Finds a top-level `hooks` key in a Hermes `config.yaml` without a YAML
//! parser (the shim is size-capped).
//!
//! YAML can spell that key as `hooks:`, `"hooks":`, `'hooks':`, `hooks :`,
//! `? hooks`, or with an anchor or tag in front, and any of them turns an
//! appended `hooks:` block into a duplicate key that silently overrides the
//! user's hooks (#1056). The scan is line based and conservative: PaneFlow
//! only appends to a single document whose root is a column-0 block
//! mapping, and whatever the scan cannot rule out is `Unsure`.

/// What a Hermes config says about a top-level `hooks` key.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TopLevelHooks {
    /// An empty file, comments only, or a column-0 block mapping without
    /// `hooks`: appending the managed block is safe.
    Absent,
    /// The root mapping already has a `hooks` key.
    Present,
    /// The scan cannot tell, or appending a column-0 `hooks:` block would
    /// not extend the root mapping: a flow, sequence, scalar, or indented
    /// root, several documents, line breaks other than `\n`/`\r\n`, alias
    /// or merge keys, and lines that are not keys.
    Unsure,
}

pub(super) fn top_level_hooks(content: &str) -> TopLevelHooks {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if has_other_line_breaks(content) {
        return TopLevelHooks::Unsure;
    }
    let mut root_seen = false;
    let mut unsure = false;
    for line in content.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let body = line.trim_start_matches(' ');
        let indent = line.len() - body.len();
        if indent == 0 {
            if body.starts_with('%') {
                continue;
            }
            if let Some((marker, rest)) = document_marker(body) {
                // Only a leading `---` with nothing after it keeps a single
                // document whose root the appended block can extend.
                let node = strip_properties(rest.trim_start());
                let root_on_marker = !(node.is_empty() || node.starts_with('#'));
                unsure |= root_seen || marker == "..." || root_on_marker;
                // A root that starts on the marker line owns the lines below.
                root_seen |= root_on_marker;
                continue;
            }
        }
        let node = strip_properties(body.trim_start());
        if node.is_empty() || node.starts_with('#') {
            continue;
        }
        let kind = classify(node);
        if !root_seen {
            root_seen = true;
            if indent != 0 || !matches!(kind, Line::Key(_)) {
                // A flow, sequence, scalar, or indented root.
                unsure = true;
            }
        } else if indent > 0 {
            // A nested node: a child mapping, a block scalar, or the
            // continuation of a value.
            continue;
        }
        match kind {
            Line::Key(TopLevelHooks::Present) => return TopLevelHooks::Present,
            Line::Key(TopLevelHooks::Unsure) | Line::Other => unsure = true,
            Line::Key(TopLevelHooks::Absent) | Line::Entry => {}
        }
    }
    if unsure {
        TopLevelHooks::Unsure
    } else {
        TopLevelHooks::Absent
    }
}

/// PyYAML also breaks lines at a lone `\r`, U+0085, U+2028, and U+2029,
/// which `str::lines` does not.
fn has_other_line_breaks(content: &str) -> bool {
    content.contains(['\u{85}', '\u{2028}', '\u{2029}'])
        || content
            .match_indices('\r')
            .any(|(index, _)| content.as_bytes().get(index + 1) != Some(&b'\n'))
}

/// `---` or `...` followed by nothing or whitespace: the marker and the rest.
fn document_marker(line: &str) -> Option<(&str, &str)> {
    let marker = line
        .get(..3)
        .filter(|marker| matches!(*marker, "---" | "..."))?;
    let rest = &line[3..];
    (rest.is_empty() || rest.starts_with([' ', '\t'])).then_some((marker, rest))
}

/// Drops leading `&anchor` and `!tag` node properties.
fn strip_properties(mut node: &str) -> &str {
    while node.starts_with(['&', '!']) {
        let end = node.find([' ', '\t']).unwrap_or(node.len());
        node = node[end..].trim_start();
    }
    node
}

/// One line at the root's indentation.
enum Line {
    /// A mapping key, and whether it is `hooks`.
    Key(TopLevelHooks),
    /// A `- ` sequence entry or the `: ` value of an explicit key.
    Entry,
    /// Anything else: a scalar, a flow collection, or a continuation.
    Other,
}

fn classify(node: &str) -> Line {
    let indicator_then_space = |indicator: char| {
        node.strip_prefix(indicator)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    };
    if indicator_then_space('?') {
        return Line::Key(explicit_key(node[1..].trim_start()));
    }
    if indicator_then_space(':') || indicator_then_space('-') {
        return Line::Entry;
    }
    let key = |name: &str| {
        Line::Key(if name == "hooks" {
            TopLevelHooks::Present
        } else {
            TopLevelHooks::Absent
        })
    };
    match node.as_bytes().first() {
        Some(b'"' | b'\'') => match quoted(node) {
            Some((name, rest)) if rest.trim_start().starts_with(':') => key(&name),
            // A scalar, or a quoted key that does not close on this line.
            _ => Line::Other,
        },
        // An alias key may name `hooks`, and a merge key can pull it in.
        Some(b'*') => Line::Key(TopLevelHooks::Unsure),
        Some(b'<') if node.starts_with("<<") => Line::Key(TopLevelHooks::Unsure),
        Some(b'{' | b'[' | b'|' | b'>' | b'@' | b'`') | None => Line::Other,
        Some(_) => plain_key(node).map_or(Line::Other, key),
    }
}

fn explicit_key(key: &str) -> TopLevelHooks {
    let key = strip_properties(key);
    if key.is_empty() || key.starts_with('#') {
        // The key itself sits on the following lines.
        return TopLevelHooks::Unsure;
    }
    let name = match key.as_bytes().first() {
        Some(b'"' | b'\'') => match quoted(key) {
            Some((name, _)) => name,
            None => return TopLevelHooks::Unsure,
        },
        Some(b'|' | b'>' | b'{' | b'[' | b'*') => return TopLevelHooks::Unsure,
        _ => {
            let end = comment_start(key).unwrap_or(key.len());
            key[..end].trim_end().to_owned()
        }
    };
    if name == "hooks" {
        TopLevelHooks::Present
    } else {
        TopLevelHooks::Absent
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
