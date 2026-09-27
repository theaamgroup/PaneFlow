//! Reads how a Codex `config.toml` defines its root `features` table.
//!
//! TOML can spell that table as a `[features]` header, an inline table
//! (`features = { hooks = true }`), or dotted keys (`features.hooks = true`),
//! and any of them makes an appended `[features]` block a duplicate
//! declaration (#1053). This is a statement scanner, not a full TOML parser:
//! it only tracks table headers, key paths, and enough of each value
//! (strings, nested brackets, comments) to find where a statement ends, so a
//! string or multi-line value can never pass for a header or a key.

/// What the root `features` table already says about Codex hooks.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CodexFeatures {
    /// No root `features` definition; appending `[features]` is safe.
    Absent,
    /// `features` already sets `hooks = true` (or `codex_hooks = true`).
    HooksEnabled,
    /// `features` is defined without an enabled hooks flag, so a new
    /// `[features]` table would be a duplicate declaration.
    Unsupported,
}

/// `None` when the content is not TOML the scanner can follow.
pub(super) fn codex_features(content: &str) -> Option<CodexFeatures> {
    let mut scanner = Scanner::new(content);
    let mut at_root = true;
    let mut in_features_table = false;
    let mut defined = false;
    let mut enabled = false;
    loop {
        scanner.skip_trivia();
        let Some(byte) = scanner.peek() else {
            break;
        };
        if byte == b'[' {
            let array = scanner.eat("[[");
            if !array {
                scanner.pos += 1;
            }
            let path = scanner.key_path()?;
            scanner.skip_blank();
            if !scanner.eat(if array { "]]" } else { "]" }) {
                return None;
            }
            scanner.end_of_line()?;
            // `[features.sub]` only defines features implicitly, and TOML
            // still allows a later `[features]` header after it.
            let is_features = path == ["features"];
            defined |= is_features;
            in_features_table = is_features && !array;
            at_root = false;
            continue;
        }

        let path = scanner.key_path()?;
        scanner.skip_blank();
        if !scanner.eat("=") {
            return None;
        }
        scanner.skip_blank();
        let value = scanner.skip_value()?;
        scanner.end_of_line()?;
        if in_features_table {
            enabled |= enables_hooks(&path, value);
        } else if at_root {
            if let Some((first, rest)) = path.split_first() {
                if first == "features" {
                    defined = true;
                    enabled |= if rest.is_empty() {
                        inline_table_enables_hooks(value)?
                    } else {
                        enables_hooks(rest, value)
                    };
                }
            }
        }
    }
    Some(if enabled {
        CodexFeatures::HooksEnabled
    } else if defined {
        CodexFeatures::Unsupported
    } else {
        CodexFeatures::Absent
    })
}

fn enables_hooks(key: &[String], value: &str) -> bool {
    matches!(key, [name] if name == "hooks" || name == "codex_hooks") && value == "true"
}

/// `Some(false)` for a value that is not an inline table.
fn inline_table_enables_hooks(value: &str) -> Option<bool> {
    let mut scanner = Scanner::new(value);
    if !scanner.eat("{") {
        return Some(false);
    }
    let mut enabled = false;
    loop {
        scanner.skip_trivia();
        if scanner.eat("}") {
            break;
        }
        let path = scanner.key_path()?;
        scanner.skip_blank();
        if !scanner.eat("=") {
            return None;
        }
        scanner.skip_blank();
        let entry = scanner.skip_value()?;
        enabled |= enables_hooks(&path, entry);
        scanner.skip_trivia();
        if !scanner.eat(",") && scanner.peek() != Some(b'}') {
            return None;
        }
    }
    Some(enabled)
}

struct Scanner<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        Self { text, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        self.text.get(self.pos..).unwrap_or_default()
    }

    fn peek(&self) -> Option<u8> {
        self.rest().bytes().next()
    }

    /// Advances past one whole character, never into the middle of one.
    fn bump(&mut self) {
        self.pos += self.rest().chars().next().map_or(1, char::len_utf8);
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.rest().starts_with(token);
        if found {
            self.pos += token.len();
        }
        found
    }

    fn skip_blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some(b'#') {
            let line = self.rest();
            self.pos += line.find(['\n', '\r']).unwrap_or(line.len());
        }
    }

    /// Whitespace, newlines, and comments between statements.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'#') => self.skip_comment(),
                _ => return,
            }
        }
    }

    fn end_of_line(&mut self) -> Option<()> {
        self.skip_blank();
        self.skip_comment();
        if self.peek().is_none() || self.eat("\n") || self.eat("\r\n") {
            Some(())
        } else {
            None
        }
    }

    fn key_path(&mut self) -> Option<Vec<String>> {
        let mut path = Vec::new();
        loop {
            self.skip_blank();
            path.push(self.simple_key()?);
            self.skip_blank();
            if !self.eat(".") {
                return Some(path);
            }
        }
    }

    fn simple_key(&mut self) -> Option<String> {
        match self.peek()? {
            b'"' if !self.rest().starts_with("\"\"\"") => self.basic_string(),
            b'\'' if !self.rest().starts_with("'''") => self.literal_string(),
            _ => {
                let rest = self.rest();
                let len = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    .unwrap_or(rest.len());
                if len == 0 {
                    return None;
                }
                self.pos += len;
                rest.get(..len).map(str::to_owned)
            }
        }
    }

    /// A single-line basic string, decoded; `pos` sits on the opening quote.
    fn basic_string(&mut self) -> Option<String> {
        self.pos += 1;
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((index, c)) = chars.next() {
            match c {
                '"' => {
                    self.pos += index + 1;
                    return Some(out);
                }
                '\\' => {
                    let (_, escape) = chars.next()?;
                    let simple = match escape {
                        'b' => Some('\u{8}'),
                        't' => Some('\t'),
                        'n' => Some('\n'),
                        'f' => Some('\u{c}'),
                        'r' => Some('\r'),
                        'e' => Some('\u{1b}'),
                        '"' => Some('"'),
                        '\\' => Some('\\'),
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
                }
                '\n' | '\r' => return None,
                c => out.push(c),
            }
        }
        None
    }

    /// A single-line literal string; `pos` sits on the opening quote.
    fn literal_string(&mut self) -> Option<String> {
        self.pos += 1;
        let rest = self.rest();
        let end = rest.find(['\'', '\n', '\r'])?;
        if rest.as_bytes().get(end) != Some(&b'\'') {
            return None;
        }
        self.pos += end + 1;
        rest.get(..end).map(str::to_owned)
    }

    /// Skips a multi-line string whose opening delimiter is at `pos`. Up to
    /// two extra quotes may sit just inside the closing delimiter.
    fn skip_multiline_string(&mut self, delimiter: &str, escapes: bool) -> Option<()> {
        self.pos += delimiter.len();
        let quote = delimiter.as_bytes().first().copied()?;
        loop {
            let rest = self.rest();
            let next = rest.find(|c: char| c == char::from(quote) || (escapes && c == '\\'))?;
            self.pos += next;
            if escapes && self.peek() == Some(b'\\') {
                self.pos += 1;
                self.bump();
                continue;
            }
            if self.eat(delimiter) {
                for _ in 0..2 {
                    if self.peek() == Some(quote) {
                        self.pos += 1;
                    }
                }
                return Some(());
            }
            self.pos += 1;
        }
    }

    /// Skips one value and returns its text. The value ends at a newline or
    /// comment outside brackets, or at a `,`, `]`, or `}` that closes an
    /// enclosing inline table or array.
    fn skip_value(&mut self) -> Option<&'a str> {
        let start = self.pos;
        let mut depth = 0usize;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' if self.rest().starts_with("\"\"\"") => {
                    self.skip_multiline_string("\"\"\"", true)?;
                }
                b'\'' if self.rest().starts_with("'''") => {
                    self.skip_multiline_string("'''", false)?;
                }
                b'"' => {
                    self.basic_string()?;
                }
                b'\'' => {
                    self.literal_string()?;
                }
                b'[' | b'{' => {
                    depth += 1;
                    self.pos += 1;
                }
                b']' | b'}' | b',' | b'#' | b'\n' | b'\r' if depth == 0 => break,
                b']' | b'}' => {
                    depth -= 1;
                    self.pos += 1;
                }
                b'#' => self.skip_comment(),
                _ => self.bump(),
            }
        }
        if depth != 0 {
            return None;
        }
        let value = self
            .text
            .get(start..self.pos)?
            .trim_end_matches([' ', '\t']);
        (!value.is_empty()).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_follows_values_that_span_lines_or_hide_delimiters() {
        let cases = [
            ("", CodexFeatures::Absent),
            (
                "model = \"gpt-5\"\r\nfeatures.hooks = true\r\n",
                CodexFeatures::HooksEnabled,
            ),
            (
                "\u{feff}features = { hooks = true }",
                CodexFeatures::HooksEnabled,
            ),
            (
                "\"\\u0066eatures\".hooks = true\n",
                CodexFeatures::HooksEnabled,
            ),
            (
                "list = [\n  \"[features]\", # ]\n  { a = '}' },\n]\nfeatures.hooks = true\n",
                CodexFeatures::HooksEnabled,
            ),
            (
                "s = '''\n[x]'''''\nfeatures = { hooks = true }\n",
                CodexFeatures::HooksEnabled,
            ),
            (
                "s = \"\"\"\\\"\"\"\"\nfeatures.hooks = true\n",
                CodexFeatures::HooksEnabled,
            ),
            ("features = true\n", CodexFeatures::Unsupported),
            (
                "features = { hooks = \"true\" }\n",
                CodexFeatures::Unsupported,
            ),
            ("features.hooks.x = true\n", CodexFeatures::Unsupported),
            ("[features.sub]\nhooks = true\n", CodexFeatures::Absent),
            (
                "[other]\nfeatures = { hooks = true }\n",
                CodexFeatures::Absent,
            ),
        ];
        for (content, expected) in cases {
            assert_eq!(codex_features(content), Some(expected), "{content:?}");
        }
    }

    #[test]
    fn scanner_rejects_content_it_cannot_follow() {
        for content in [
            "features",
            "features = ",
            "features = [1, 2\n",
            "features = \"open\n",
            "[features\n",
            "= 1\n",
        ] {
            assert_eq!(codex_features(content), None, "{content:?}");
        }
    }
}
