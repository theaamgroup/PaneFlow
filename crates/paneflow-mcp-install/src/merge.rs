//! No-clobber config-merge primitives (EP-002 US-006).
//!
//! Three formats, three rules:
//! - **JSON** (Claude Code, Gemini, opencode `.json`) is merged via
//!   `serde_json::Value` - never a typed-struct round-trip - so unknown
//!   keys and sibling MCP servers are preserved byte-for-meaning. Only the
//!   `paneflow` entry under the agent's container key is inserted/updated.
//! - **JSONC** (opencode `.jsonc`) is parsed to `Value` for the semantic
//!   merge, then only `container.entry` is spliced back into the original
//!   text so comments, trailing commas, and sibling keys stay. Do not
//!   round-trip JSONC through `json_to_bytes`.
//! - **TOML** (Codex) is edited via `toml_edit::DocumentMut`, which
//!   preserves comments and key order. Only `command` / `args` under
//!   `[<table>.paneflow]` are upserted; unknown keys in that table stay.
//!
//! Both `read_*_or_default` helpers treat a **missing** file as an empty
//! skeleton (so a fresh install creates it) but a **present-but-invalid**
//! file as an error (so we never overwrite a config we could not parse -
//! the user repairs it by hand). This is the no-clobber guarantee.

use std::path::Path;

use anyhow::{bail, Context, Result};

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Read + parse a JSON config. Missing file → empty object skeleton.
/// Present but unparseable → `Err` (caller must abort, never clobber).
pub fn read_json_or_default(path: &Path) -> Result<serde_json::Value> {
    match std::fs::read(path) {
        Ok(bytes) => parse_json_or_jsonc(path, &bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::Value::Object(serde_json::Map::new()))
        }
        Err(e) => Err(e).with_context(|| format!("read {} failed", path.display())),
    }
}

/// True when `path` is a JSONC config (`.jsonc`). Writes to these files
/// must go through [`upsert_jsonc_entry`] / [`remove_jsonc_entry`] so
/// comments are not stripped by a `Value` round-trip.
#[must_use]
pub fn is_jsonc_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonc")
}

fn parse_json_or_jsonc(path: &Path, bytes: &[u8]) -> Result<serde_json::Value> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Ok(value),
        Err(_json_error) if is_jsonc_path(path) => {
            let text = std::str::from_utf8(bytes).with_context(|| {
                format!(
                    "{} is not valid UTF-8 JSONC - refusing to overwrite it; fix or remove it, then re-run",
                    path.display()
                )
            })?;
            parse_jsonc_text(text).with_context(|| {
                format!(
                    "{} is not valid JSONC - refusing to overwrite it; fix or remove it, then re-run",
                    path.display()
                )
            })
        }
        Err(json_error) => Err(json_error).with_context(|| {
            format!(
                "{} is not valid JSON - refusing to overwrite it; \
                 fix or remove it, then re-run",
                path.display()
            )
        }),
    }
}

/// Parse JSON or JSONC text (comments + trailing commas). Used by the
/// JSONC splicer and by tests that must not go through `serde_json` on
/// the raw file.
pub fn parse_jsonc_text(text: &str) -> Result<serde_json::Value> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(_) => serde_json::from_str(&normalize_jsonc(text)).context("not valid JSONC"),
    }
}

fn normalize_jsonc(input: &str) -> String {
    remove_trailing_commas(&strip_jsonc_comments(input))
}

fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for comment_ch in chars.by_ref() {
                        if comment_ch == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for comment_ch in chars.by_ref() {
                        if comment_ch == '\n' {
                            out.push('\n');
                        }
                        if prev == '*' && comment_ch == '/' {
                            break;
                        }
                        prev = comment_ch;
                    }
                    continue;
                }
                _ => {}
            }
        }

        out.push(ch);
    }

    out
}

fn remove_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;

    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += 1;
            continue;
        }

        if ch == ',' {
            let next = chars[i + 1..].iter().copied().find(|c| !c.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                i += 1;
                continue;
            }
        }

        out.push(ch);
        i += 1;
    }

    out
}

/// Upsert `root[container_key][entry_name] = entry_value`, creating the
/// container object if needed. Returns `true` iff the document changed
/// (the entry was absent or differed); `false` is a no-op (idempotent).
///
/// Sibling entries under `container_key`, and every other top-level key,
/// are left untouched. Errors only if `root` (or an existing
/// `container_key`) is present but not a JSON object - overwriting a
/// non-object there would be a clobber.
pub fn merge_json_entry(
    root: &mut serde_json::Value,
    container_key: &str,
    entry_name: &str,
    entry_value: serde_json::Value,
) -> Result<bool> {
    let obj = root
        .as_object_mut()
        .context("config root is not a JSON object - refusing to overwrite")?;

    let container = obj
        .entry(container_key)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let container = container.as_object_mut().with_context(|| {
        format!("config key `{container_key}` is not an object - refusing to overwrite")
    })?;

    if container.get(entry_name) == Some(&entry_value) {
        return Ok(false);
    }
    container.insert(entry_name.to_string(), entry_value);
    Ok(true)
}

/// Remove `root[container_key][entry_name]`. Returns `true` iff something
/// was removed. Leaves siblings and the container itself in place.
pub fn remove_json_entry(
    root: &mut serde_json::Value,
    container_key: &str,
    entry_name: &str,
) -> bool {
    root.as_object_mut()
        .and_then(|obj| obj.get_mut(container_key))
        .and_then(serde_json::Value::as_object_mut)
        .is_some_and(|container| container.remove(entry_name).is_some())
}

/// Serialize a JSON config back to bytes: pretty-printed, trailing newline
/// (matches what editors and `claude mcp add` leave behind).
///
/// US-038: returns `Result` and propagates a serialization error instead of
/// the old `unwrap_or_else(|_| "{}")` fallback, which would have silently
/// written an empty object over the user's real MCP servers (a no-clobber
/// violation) if a parsed `Value` ever failed to re-serialize.
pub fn json_to_bytes(root: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    let mut s = serde_json::to_string_pretty(root)?;
    s.push('\n');
    Ok(s.into_bytes())
}

/// Upsert `root[container_key][entry_name] = entry_value` by splicing the
/// original JSONC text. Comments, trailing commas, key order, and sibling
/// entries are left in place. Returns `Ok(None)` when the document already
/// has that exact entry (idempotent no-op).
///
/// The spliced text is parsed and compared to a `Value` merge of the same
/// edit; a mismatch is an error (never a silent fallback to pretty JSON).
pub fn upsert_jsonc_entry(
    source: &str,
    container_key: &str,
    entry_name: &str,
    entry_value: &serde_json::Value,
) -> Result<Option<String>> {
    let mut expected = parse_jsonc_text(source).context(
        "config is not valid JSONC - refusing to overwrite it; fix or remove it, then re-run",
    )?;
    let changed = merge_json_entry(
        &mut expected,
        container_key,
        entry_name,
        entry_value.clone(),
    )?;
    if !changed {
        return Ok(None);
    }
    let spliced = splice_jsonc_upsert(source, container_key, entry_name, entry_value)?;
    let got = parse_jsonc_text(&spliced)
        .context("internal error: jsonc splice produced invalid JSONC")?;
    if got != expected {
        bail!("internal error: jsonc splice changed more than `{container_key}.{entry_name}`");
    }
    Ok(Some(spliced))
}

/// Remove `root[container_key][entry_name]` by splicing the original JSONC
/// text. Returns `Ok(None)` when the entry is already absent.
pub fn remove_jsonc_entry(
    source: &str,
    container_key: &str,
    entry_name: &str,
) -> Result<Option<String>> {
    let mut expected = parse_jsonc_text(source).context(
        "config is not valid JSONC - refusing to overwrite it; fix or remove it, then re-run",
    )?;
    if !remove_json_entry(&mut expected, container_key, entry_name) {
        return Ok(None);
    }
    let spliced = splice_jsonc_remove(source, container_key, entry_name)?;
    let got = parse_jsonc_text(&spliced)
        .context("internal error: jsonc splice produced invalid JSONC")?;
    if got != expected {
        bail!("internal error: jsonc splice changed more than `{container_key}.{entry_name}`");
    }
    Ok(Some(spliced))
}

fn splice_jsonc_upsert(
    source: &str,
    container_key: &str,
    entry_name: &str,
    entry_value: &serde_json::Value,
) -> Result<String> {
    let (root_open, root_props, root_close) = scan_root_object(source)?;
    if let Some(container) = last_matching(&root_props, container_key) {
        let mut inner = Scan::new_at(source, container.value_start);
        inner.skip_trivia()?;
        if inner.peek() != Some('{') {
            bail!("config key `{container_key}` is not an object - refusing to overwrite");
        }
        let inner_open = inner.i;
        let (inner_props, inner_close) = object_props(&mut inner)?;
        if let Some(existing) = last_matching(&inner_props, entry_name) {
            let pretty = source[existing.value_start..existing.value_end].contains('\n');
            let key_indent = line_indent_at(source, existing.key_start);
            let formatted = format_json_value(entry_value, pretty, &key_indent, newline(source))?;
            return Ok(replace_span(
                source,
                existing.value_start,
                existing.value_end,
                &formatted,
            ));
        }
        insert_property(
            source,
            inner_open,
            inner_close,
            &inner_props,
            entry_name,
            entry_value,
        )
    } else {
        let mut map = serde_json::Map::new();
        map.insert(entry_name.to_string(), entry_value.clone());
        insert_property(
            source,
            root_open,
            root_close,
            &root_props,
            container_key,
            &serde_json::Value::Object(map),
        )
    }
}

fn splice_jsonc_remove(source: &str, container_key: &str, entry_name: &str) -> Result<String> {
    let (_root_open, root_props, _root_close) = scan_root_object(source)?;
    let Some(container) = last_matching(&root_props, container_key) else {
        bail!("internal error: jsonc splice could not find `{container_key}`");
    };
    let mut inner = Scan::new_at(source, container.value_start);
    inner.skip_trivia()?;
    if inner.peek() != Some('{') {
        bail!("config key `{container_key}` is not an object - refusing to overwrite");
    }
    let inner_open = inner.i;
    let (inner_props, _inner_close) = object_props(&mut inner)?;
    let matches: Vec<&JsoncProp> = inner_props.iter().filter(|p| p.key == entry_name).collect();
    if matches.is_empty() {
        bail!("internal error: jsonc splice could not find `{container_key}.{entry_name}`");
    }
    let mut spans: Vec<(usize, usize)> = matches
        .iter()
        .map(|p| removal_span(source, p, inner_open))
        .collect();
    spans.sort_by_key(|(start, _)| *start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end) in spans {
        if start < cursor {
            bail!("internal error: overlapping jsonc removal spans");
        }
        out.push_str(&source[cursor..start]);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    Ok(out)
}

fn scan_root_object(source: &str) -> Result<(usize, Vec<JsoncProp>, usize)> {
    let mut scan = Scan::new(source);
    scan.skip_trivia()?;
    if scan.peek() != Some('{') {
        bail!("config root is not a JSON object - refusing to overwrite");
    }
    let open = scan.i;
    let (props, close) = object_props(&mut scan)?;
    Ok((open, props, close))
}

fn last_matching<'a>(props: &'a [JsoncProp], key: &str) -> Option<&'a JsoncProp> {
    props.iter().rev().find(|p| p.key == key)
}

fn insert_property(
    source: &str,
    open_brace: usize,
    close_brace: usize,
    props: &[JsoncProp],
    name: &str,
    value: &serde_json::Value,
) -> Result<String> {
    let pretty = source[open_brace..=close_brace].contains('\n');
    let nl = newline(source);
    let key = serde_json::to_string(name).context("serialize jsonc key failed")?;
    if pretty {
        let key_indent = match props.first() {
            Some(first) => line_indent_at(source, first.key_start),
            None => {
                let parent = line_indent_at(source, close_brace);
                format!("{parent}{}", indent_unit_for(&parent))
            }
        };
        let formatted = format_json_value(value, true, &key_indent, nl)?;
        let insert_at = start_of_close_line(source, close_brace);
        let comma_at = props.last().and_then(|last| {
            if last.comma_end.is_none() {
                Some(last.value_end)
            } else {
                None
            }
        });
        let prop_text = format!("{key_indent}{key}: {formatted}{nl}");
        Ok(apply_edits(source, comma_at, insert_at, &prop_text))
    } else {
        let formatted = format_json_value(value, false, "", nl)?;
        let mut prefix = String::new();
        if let Some(last) = props.last() {
            if last.comma_end.is_none() {
                prefix.push(',');
            }
        }
        prefix.push_str(&key);
        prefix.push_str(": ");
        prefix.push_str(&formatted);
        Ok(replace_span(source, close_brace, close_brace, &prefix))
    }
}

fn apply_edits(source: &str, comma_at: Option<usize>, insert_at: usize, prop_text: &str) -> String {
    match comma_at {
        Some(comma_at) if comma_at <= insert_at => {
            let mut out = String::with_capacity(source.len() + prop_text.len() + 1);
            out.push_str(&source[..comma_at]);
            out.push(',');
            out.push_str(&source[comma_at..insert_at]);
            out.push_str(prop_text);
            out.push_str(&source[insert_at..]);
            out
        }
        _ => replace_span(source, insert_at, insert_at, prop_text),
    }
}

fn replace_span(source: &str, start: usize, end: usize, new: &str) -> String {
    let mut out = String::with_capacity(source.len() - (end - start) + new.len());
    out.push_str(&source[..start]);
    out.push_str(new);
    out.push_str(&source[end..]);
    out
}

fn format_json_value(
    value: &serde_json::Value,
    pretty: bool,
    key_indent: &str,
    nl: &str,
) -> Result<String> {
    if pretty {
        let pretty_s =
            serde_json::to_string_pretty(value).context("serialize jsonc value failed")?;
        Ok(reindent_pretty(&pretty_s, key_indent, nl))
    } else {
        serde_json::to_string(value).context("serialize jsonc value failed")
    }
}

fn reindent_pretty(pretty: &str, key_indent: &str, nl: &str) -> String {
    let mut lines = pretty.split('\n');
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut out = String::from(first);
    for line in lines {
        out.push_str(nl);
        out.push_str(key_indent);
        out.push_str(line);
    }
    out
}

fn newline(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn indent_unit_for(parent_indent: &str) -> &'static str {
    if parent_indent.contains('\t') {
        "\t"
    } else {
        "  "
    }
}

fn line_indent_at(source: &str, pos: usize) -> String {
    let line_start = source[..pos].rfind('\n').map_or(0, |i| i + 1);
    source[line_start..pos]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn start_of_close_line(source: &str, close: usize) -> usize {
    match source[..close].rfind('\n') {
        Some(nl) => {
            let between = &source[nl + 1..close];
            if between.chars().all(|c| c == ' ' || c == '\t') {
                nl + 1
            } else {
                close
            }
        }
        None => close,
    }
}

fn removal_span(source: &str, prop: &JsoncProp, object_open: usize) -> (usize, usize) {
    let mut start = prop.key_start;
    let floor = object_open + 1;
    let bytes = source.as_bytes();
    while start > floor {
        match bytes[start - 1] {
            b' ' | b'\t' => start -= 1,
            b'\n' => {
                start -= 1;
                if start > floor && bytes[start - 1] == b'\r' {
                    start -= 1;
                }
                break;
            }
            _ => break,
        }
    }
    let mut end = prop.comma_end.unwrap_or(prop.value_end);
    while end < bytes.len() && matches!(bytes[end], b' ' | b'\t') {
        end += 1;
    }
    (start, end)
}

struct JsoncProp {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
    comma_end: Option<usize>,
}

struct Scan<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn new(s: &'a str) -> Self {
        let i = if s.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        Self { s, i }
    }

    fn new_at(s: &'a str, i: usize) -> Self {
        Self { s, i }
    }

    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.rest().starts_with(prefix)
    }

    fn bump(&mut self) -> Option<char> {
        let mut chars = self.rest().chars();
        let ch = chars.next()?;
        self.i += ch.len_utf8();
        Some(ch)
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.starts_with("//") => self.skip_line_comment(),
                Some('/') if self.starts_with("/*") => self.skip_block_comment()?,
                _ => return Ok(()),
            }
        }
    }

    fn skip_line_comment(&mut self) {
        self.i += 2;
        while let Some(ch) = self.bump() {
            if ch == '\n' {
                break;
            }
        }
    }

    fn skip_block_comment(&mut self) -> Result<()> {
        self.i += 2;
        loop {
            if self.rest().is_empty() {
                bail!("unclosed block comment");
            }
            if self.starts_with("*/") {
                self.i += 2;
                return Ok(());
            }
            self.bump();
        }
    }

    fn skip_string(&mut self) -> Result<()> {
        if self.bump() != Some('"') {
            bail!("expected string");
        }
        let mut escaped = false;
        loop {
            let Some(ch) = self.bump() else {
                bail!("unclosed string");
            };
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                return Ok(());
            }
        }
    }

    fn parse_string(&mut self) -> Result<String> {
        let start = self.i;
        self.skip_string()?;
        let raw = &self.s[start..self.i];
        unescape_json_string(raw)
    }

    fn skip_literal(&mut self, lit: &str) -> Result<()> {
        if !self.starts_with(lit) {
            bail!("expected `{lit}`");
        }
        self.i += lit.len();
        Ok(())
    }

    fn skip_number(&mut self) {
        if self.peek() == Some('-') {
            self.bump();
        }
        while matches!(self.peek(), Some('0'..='9')) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while matches!(self.peek(), Some('0'..='9')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            while matches!(self.peek(), Some('0'..='9')) {
                self.bump();
            }
        }
    }

    fn skip_balanced(&mut self, open: char, close: char) -> Result<()> {
        if self.bump() != Some(open) {
            bail!("expected `{open}`");
        }
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                None => bail!("unbalanced `{open}{close}`"),
                Some('"') => self.skip_string()?,
                Some('/') if self.starts_with("//") => self.skip_line_comment(),
                Some('/') if self.starts_with("/*") => self.skip_block_comment()?,
                Some(c) if c == open => {
                    self.bump();
                    depth += 1;
                }
                Some(c) if c == close => {
                    self.bump();
                    depth -= 1;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
        Ok(())
    }

    fn skip_value(&mut self) -> Result<()> {
        match self.peek() {
            Some('{') => self.skip_balanced('{', '}'),
            Some('[') => self.skip_balanced('[', ']'),
            Some('"') => self.skip_string(),
            Some('t') => self.skip_literal("true"),
            Some('f') => self.skip_literal("false"),
            Some('n') => self.skip_literal("null"),
            Some('-' | '0'..='9') => {
                self.skip_number();
                Ok(())
            }
            other => bail!("unexpected jsonc value starting with {other:?}"),
        }
    }
}

fn object_props(scan: &mut Scan<'_>) -> Result<(Vec<JsoncProp>, usize)> {
    if scan.bump() != Some('{') {
        bail!("expected '{{'");
    }
    let mut props = Vec::new();
    loop {
        scan.skip_trivia()?;
        match scan.peek() {
            Some('}') => {
                let close = scan.i;
                scan.bump();
                return Ok((props, close));
            }
            Some(',') => {
                scan.bump();
            }
            Some('"') => {
                let key_start = scan.i;
                let key = scan.parse_string()?;
                scan.skip_trivia()?;
                if scan.bump() != Some(':') {
                    bail!("expected ':' after object key");
                }
                scan.skip_trivia()?;
                let value_start = scan.i;
                scan.skip_value()?;
                let value_end = scan.i;
                scan.skip_trivia()?;
                let comma_end = if scan.peek() == Some(',') {
                    scan.bump();
                    Some(scan.i)
                } else {
                    None
                };
                props.push(JsoncProp {
                    key,
                    key_start,
                    value_start,
                    value_end,
                    comma_end,
                });
            }
            other => bail!("unexpected token in jsonc object: {other:?}"),
        }
    }
}

fn unescape_json_string(quoted: &str) -> Result<String> {
    let inner = quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .context("jsonc key is not a quoted string")?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(esc) = chars.next() else {
            bail!("unterminated escape in jsonc string");
        };
        match esc {
            '"' | '\\' | '/' => out.push(esc),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let mut hex = String::with_capacity(4);
                for _ in 0..4 {
                    let Some(h) = chars.next() else {
                        bail!("truncated \\u escape in jsonc string");
                    };
                    hex.push(h);
                }
                let code =
                    u32::from_str_radix(&hex, 16).context("invalid \\u escape in jsonc string")?;
                let ch = char::from_u32(code).context("invalid \\u codepoint in jsonc string")?;
                out.push(ch);
            }
            other => bail!("invalid escape `\\{other}` in jsonc string"),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

/// Read + parse a TOML config. Missing file → empty document. Present but
/// unparseable → `Err` (no-clobber).
pub fn read_toml_or_default(path: &Path) -> Result<toml_edit::DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.parse::<toml_edit::DocumentMut>().with_context(|| {
            format!(
                "{} is not valid TOML - refusing to overwrite it; \
                 fix or remove it, then re-run",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml_edit::DocumentMut::new()),
        Err(e) => Err(e).with_context(|| format!("read {} failed", path.display())),
    }
}

/// Upsert `command` and `args` under `[<table_path>.<name>]`.
///
/// An existing table (or inline table) is updated in place so unknown keys
/// (`env`, `cwd`, timeouts, `enabled`, ...) and their comments stay. A
/// missing entry is created as a standard table with only `command` /
/// `args`. Sibling tables, parent comments, and key order are left
/// untouched. Returns `true` iff the serialized document changed.
pub fn upsert_toml_entry(
    doc: &mut toml_edit::DocumentMut,
    table_path: &str,
    name: &str,
    command: &str,
    args: &[&str],
) -> Result<bool> {
    use toml_edit::{Item, Table};

    let before = doc.to_string();

    // Auto-vivify the parent table only when absent; an existing parent is
    // reused untouched so we never strip a user's `[mcp_servers]` header or
    // its other entries.
    let parent = match doc.entry(table_path) {
        toml_edit::Entry::Vacant(v) => {
            let mut t = Table::new();
            // Render as `[mcp_servers.paneflow]` rather than emitting a
            // bare `[mcp_servers]` header for a freshly created parent.
            t.set_implicit(true);
            v.insert(Item::Table(t))
        }
        toml_edit::Entry::Occupied(o) => o.into_mut(),
    };
    let parent = parent
        .as_table_mut()
        .with_context(|| format!("`{table_path}` is not a TOML table - refusing to overwrite"))?;

    match parent.get_mut(name) {
        Some(existing) => {
            if let Some(table) = existing.as_table_like_mut() {
                write_toml_command_args(table, command, args);
            } else {
                // Managed key, but not a table: replace with the managed shape.
                *existing = Item::Table(new_toml_command_args_table(command, args));
            }
        }
        None => {
            parent[name] = Item::Table(new_toml_command_args_table(command, args));
        }
    }

    Ok(doc.to_string() != before)
}

fn new_toml_command_args_table(command: &str, args: &[&str]) -> toml_edit::Table {
    let mut entry = toml_edit::Table::new();
    write_toml_command_args(&mut entry, command, args);
    entry
}

/// Set `command` / `args` on an existing table without touching other keys.
/// Skips a key whose value already matches so user formatting is preserved
/// when the managed contract is already satisfied.
fn write_toml_command_args(table: &mut dyn toml_edit::TableLike, command: &str, args: &[&str]) {
    use toml_edit::{value, Array, Value};

    if table.get("command").and_then(|c| c.as_str()) != Some(command) {
        set_toml_item(table, "command", value(command));
    }
    let args_ok = table
        .get("args")
        .and_then(|a| a.as_array())
        .is_some_and(|arr| {
            arr.len() == args.len()
                && arr
                    .iter()
                    .zip(args)
                    .all(|(item, expected)| item.as_str() == Some(*expected))
        });
    if !args_ok {
        let mut arr = Array::new();
        for a in args {
            arr.push(Value::from(*a));
        }
        set_toml_item(table, "args", value(arr));
    }
}

fn set_toml_item(table: &mut dyn toml_edit::TableLike, key: &str, item: toml_edit::Item) {
    match table.entry(key) {
        toml_edit::Entry::Vacant(v) => {
            v.insert(item);
        }
        toml_edit::Entry::Occupied(mut o) => {
            *o.get_mut() = item;
        }
    }
}

/// Remove `[<table_path>.<name>]`. Returns `true` iff the document changed.
pub fn remove_toml_entry(doc: &mut toml_edit::DocumentMut, table_path: &str, name: &str) -> bool {
    let Some(parent) = doc
        .get_mut(table_path)
        .and_then(toml_edit::Item::as_table_mut)
    else {
        return false;
    };
    parent.remove(name).is_some()
}

/// Serialize a TOML document back to bytes.
#[must_use]
pub fn toml_to_bytes(doc: &toml_edit::DocumentMut) -> Vec<u8> {
    doc.to_string().into_bytes()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paneflow_entry() -> serde_json::Value {
        json!({ "command": "/data/bin/paneflow-mcp", "args": [] })
    }

    #[test]
    fn merge_json_inserts_without_touching_siblings() {
        let mut root = json!({
            "mcpServers": { "other": { "command": "x" } },
            "theme": "dark"
        });
        let changed =
            merge_json_entry(&mut root, "mcpServers", "paneflow", paneflow_entry()).unwrap();
        assert!(changed);
        // Sibling server and unrelated top-level key preserved.
        assert_eq!(root["mcpServers"]["other"]["command"], json!("x"));
        assert_eq!(root["theme"], json!("dark"));
        assert_eq!(root["mcpServers"]["paneflow"], paneflow_entry());
    }

    #[test]
    fn merge_json_is_noop_when_identical() {
        let mut root = json!({ "mcpServers": { "paneflow": paneflow_entry() } });
        let changed =
            merge_json_entry(&mut root, "mcpServers", "paneflow", paneflow_entry()).unwrap();
        assert!(!changed, "identical entry must be a no-op");
    }

    #[test]
    fn merge_json_creates_container_when_absent() {
        let mut root = json!({});
        let changed = merge_json_entry(&mut root, "mcp", "paneflow", paneflow_entry()).unwrap();
        assert!(changed);
        assert_eq!(root["mcp"]["paneflow"], paneflow_entry());
    }

    #[test]
    fn merge_json_errors_on_non_object_root() {
        let mut root = json!([1, 2, 3]);
        assert!(merge_json_entry(&mut root, "mcpServers", "paneflow", paneflow_entry()).is_err());
    }

    #[test]
    fn remove_json_only_removes_target() {
        let mut root = json!({
            "mcpServers": { "paneflow": paneflow_entry(), "other": { "command": "x" } }
        });
        assert!(remove_json_entry(&mut root, "mcpServers", "paneflow"));
        assert!(root["mcpServers"].get("paneflow").is_none());
        assert_eq!(root["mcpServers"]["other"]["command"], json!("x"));
        // Removing again is a no-op.
        assert!(!remove_json_entry(&mut root, "mcpServers", "paneflow"));
    }

    #[test]
    fn read_json_missing_is_empty_object() {
        let dir = tempfile::TempDir::new().unwrap();
        let v = read_json_or_default(&dir.path().join("nope.json")).unwrap();
        assert!(v.is_object() && v.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_json_invalid_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("broken.json");
        std::fs::write(&p, b"{ not json").unwrap();
        let err = read_json_or_default(&p).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn read_jsonc_allows_comments_and_trailing_commas() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("opencode.jsonc");
        std::fs::write(
            &p,
            br#"
{
  // user comment
  "mcp": {
    "paneflow": {
      "command": ["/p"], // trailing comment
    },
  },
  "url": "https://example.com/path//kept"
}
"#,
        )
        .unwrap();

        let v = read_json_or_default(&p).unwrap();
        assert_eq!(v["mcp"]["paneflow"]["command"], json!(["/p"]));
        assert_eq!(v["url"], json!("https://example.com/path//kept"));
    }

    #[test]
    fn upsert_toml_preserves_comments_and_siblings() {
        let input = "\
# top comment
[mcp_servers.existing]
command = \"keepme\"
args = []
";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        let changed = upsert_toml_entry(
            &mut doc,
            "mcp_servers",
            "paneflow",
            "/data/bin/paneflow-mcp",
            &[],
        )
        .unwrap();
        assert!(changed);
        let out = doc.to_string();
        assert!(out.contains("# top comment"), "comment preserved");
        assert!(out.contains("keepme"), "sibling entry preserved");
        assert!(out.contains("paneflow"), "new entry written");
        assert!(out.contains("/data/bin/paneflow-mcp"));
    }

    #[test]
    fn upsert_toml_is_noop_when_identical() {
        let mut doc = toml_edit::DocumentMut::new();
        upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/p", &[]).unwrap();
        let changed = upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/p", &[]).unwrap();
        assert!(!changed, "re-upsert of identical entry must be a no-op");
    }

    #[test]
    fn upsert_toml_updates_changed_path() {
        let mut doc = toml_edit::DocumentMut::new();
        upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/old", &[]).unwrap();
        let changed = upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/new", &[]).unwrap();
        assert!(changed);
        assert!(doc.to_string().contains("/new"));
        assert!(!doc.to_string().contains("/old"));
    }

    #[test]
    fn upsert_toml_preserves_unknown_keys_on_path_update() {
        let input = "\
[mcp_servers.paneflow]
command = \"/old\"
args = []
# keep this timeout
startup_timeout_sec = 60
env = { FOO = \"bar\" }
cwd = \"/tmp\"
tool_timeout_sec = 30
enabled = true
";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        let changed = upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/new", &[]).unwrap();
        assert!(changed);
        let out = doc.to_string();
        assert!(out.contains("command = \"/new\""));
        assert!(!out.contains("/old"));
        assert!(
            out.contains("# keep this timeout"),
            "comment on extra key preserved"
        );
        assert!(out.contains("startup_timeout_sec = 60"));
        assert!(out.contains("FOO"));
        assert!(out.contains("bar"));
        assert!(out.contains("cwd = \"/tmp\""));
        assert!(out.contains("tool_timeout_sec = 30"));
        assert!(out.contains("enabled = true"));
    }

    #[test]
    fn upsert_toml_is_noop_when_identical_with_unknown_keys() {
        let input = "\
[mcp_servers.paneflow]
command = \"/p\"
args = []
startup_timeout_sec = 60
env = { FOO = \"bar\" }
";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        let changed = upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/p", &[]).unwrap();
        assert!(!changed, "matching command/args must not rewrite extras");
        let out = doc.to_string();
        assert!(out.contains("startup_timeout_sec = 60"));
        assert!(out.contains("FOO"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn upsert_toml_leaves_enabled_false_in_place() {
        // Merge does not strip `enabled = false`. Status treats that as
        // NeedsRepair (the managed contract) without treating other extra
        // keys as a repair signal.
        let input = "\
[mcp_servers.paneflow]
command = \"/old\"
args = []
enabled = false
startup_timeout_sec = 60
";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/new", &[]).unwrap();
        let out = doc.to_string();
        assert!(out.contains("command = \"/new\""));
        assert!(out.contains("enabled = false"));
        assert!(out.contains("startup_timeout_sec = 60"));
    }

    #[test]
    fn upsert_toml_preserves_inline_table_unknown_keys() {
        let input = "\
[mcp_servers]
paneflow = { command = \"/old\", args = [], env = { FOO = \"bar\" }, startup_timeout_sec = 60 }
";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        let changed = upsert_toml_entry(&mut doc, "mcp_servers", "paneflow", "/new", &[]).unwrap();
        assert!(changed);
        let out = doc.to_string();
        assert!(out.contains("/new"));
        assert!(!out.contains("/old"));
        assert!(out.contains("FOO"));
        assert!(out.contains("bar"));
        assert!(out.contains("startup_timeout_sec"));
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
    fn read_toml_invalid_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("broken.toml");
        std::fs::write(&p, b"this = = invalid").unwrap();
        let err = read_toml_or_default(&p).unwrap_err();
        assert!(err.to_string().contains("not valid TOML"));
    }

    fn jsonc_entry() -> serde_json::Value {
        json!({ "type": "local", "command": ["/p"], "enabled": true })
    }

    #[test]
    fn is_jsonc_path_matches_jsonc_extension() {
        assert!(is_jsonc_path(Path::new("opencode.jsonc")));
        assert!(!is_jsonc_path(Path::new("opencode.json")));
        assert!(!is_jsonc_path(Path::new("config.toml")));
    }

    #[test]
    fn upsert_jsonc_preserves_comments_and_trailing_commas() {
        let input = r#"
{
  // keep this file selected
  "mcp": {
    /* sibling block */
    "weather": { "type": "local", "command": ["weather-mcp"], "enabled": true }, // trailing
  },
  "url": "https://example.com/path//kept"
}
"#;
        let out = upsert_jsonc_entry(input, "mcp", "paneflow", &jsonc_entry())
            .unwrap()
            .expect("insert should write");
        assert!(
            out.contains("// keep this file selected"),
            "top comment preserved:\n{out}"
        );
        assert!(
            out.contains("/* sibling block */"),
            "block comment preserved"
        );
        assert!(out.contains("// trailing"), "same-line comment preserved");
        assert!(
            serde_json::from_str::<serde_json::Value>(&out).is_err(),
            "must remain JSONC, not rewritten as JSON"
        );
        let v = parse_jsonc_text(&out).unwrap();
        assert_eq!(v["mcp"]["paneflow"], jsonc_entry());
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
        assert_eq!(v["url"], json!("https://example.com/path//kept"));
    }

    #[test]
    fn upsert_jsonc_is_noop_when_identical() {
        let input = r#"{
  // keep
  "mcp": {
    "paneflow": { "type": "local", "command": ["/p"], "enabled": true }
  }
}
"#;
        let out = upsert_jsonc_entry(input, "mcp", "paneflow", &jsonc_entry()).unwrap();
        assert!(out.is_none(), "identical entry must be a no-op");
    }

    #[test]
    fn upsert_jsonc_replaces_existing_value_without_touching_siblings() {
        let input = r#"{
  // top
  "mcp": {
    "paneflow": {
      // inside managed entry: may be rewritten
      "type": "local",
      "command": ["/old"],
      "enabled": true
    },
    "weather": { "type": "local", "command": ["weather-mcp"], "enabled": true }
  }
}
"#;
        let new_entry = json!({ "type": "local", "command": ["/new"], "enabled": true });
        let out = upsert_jsonc_entry(input, "mcp", "paneflow", &new_entry)
            .unwrap()
            .expect("update should write");
        assert!(out.contains("// top"), "sibling comment preserved:\n{out}");
        assert!(out.contains("weather-mcp"), "sibling entry preserved");
        assert!(!out.contains("/old"));
        let v = parse_jsonc_text(&out).unwrap();
        assert_eq!(v["mcp"]["paneflow"]["command"], json!(["/new"]));
    }

    #[test]
    fn upsert_jsonc_creates_container_when_absent() {
        let input = r#"{
  // schema comment
  "$schema": "https://opencode.ai/config.json",
}
"#;
        let out = upsert_jsonc_entry(input, "mcp", "paneflow", &jsonc_entry())
            .unwrap()
            .expect("insert should write");
        assert!(out.contains("// schema comment"));
        assert!(out.contains("$schema"));
        let v = parse_jsonc_text(&out).unwrap();
        assert_eq!(v["mcp"]["paneflow"], jsonc_entry());
        assert_eq!(v["$schema"], json!("https://opencode.ai/config.json"));
    }

    #[test]
    fn upsert_jsonc_inserts_into_compact_object() {
        let input = r#"{"mcp":{"weather":{"type":"local","command":["w"],"enabled":true}}}"#;
        let out = upsert_jsonc_entry(input, "mcp", "paneflow", &jsonc_entry())
            .unwrap()
            .expect("insert should write");
        let v = parse_jsonc_text(&out).unwrap();
        assert_eq!(v["mcp"]["paneflow"], jsonc_entry());
        assert_eq!(v["mcp"]["weather"]["command"], json!(["w"]));
    }

    #[test]
    fn upsert_jsonc_errors_on_non_object_container() {
        let input = r#"{ "mcp": [] }"#;
        assert!(upsert_jsonc_entry(input, "mcp", "paneflow", &jsonc_entry()).is_err());
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
        let out = remove_jsonc_entry(input, "mcp", "paneflow")
            .unwrap()
            .expect("remove should write");
        assert!(out.contains("// keep"), "comment preserved:\n{out}");
        assert!(
            !out.contains("\"paneflow\""),
            "managed key must be removed:\n{out}"
        );
        let v = parse_jsonc_text(&out).unwrap();
        assert!(v["mcp"].get("paneflow").is_none());
        assert_eq!(v["mcp"]["weather"]["command"], json!(["weather-mcp"]));
        assert!(remove_jsonc_entry(&out, "mcp", "paneflow")
            .unwrap()
            .is_none());
    }

    #[test]
    fn remove_jsonc_is_noop_when_absent() {
        let input = r#"{ "mcp": { "weather": {} } }"#;
        assert!(remove_jsonc_entry(input, "mcp", "paneflow")
            .unwrap()
            .is_none());
    }
}
