use std::path::Path;

const HOOK_PROGRAM: &str = "paneflow-ai-hook";

pub fn display_hook_program(path: &Path) -> String {
    path.display().to_string()
}

pub fn shell_program_path(path: &Path) -> String {
    // Quote on whitespace or ANY POSIX shell metacharacter (issue #216).
    // Ordinary paths (letters, digits, `/`, `-`, `_`, `.`) must keep
    // rendering unquoted and byte-identical: claude_hooks.rs compares
    // stored commands against a canonical render_hook_command, so a
    // gratuitous quoting change would make previously-installed hooks
    // look unmanaged.
    let rendered = display_hook_program(path);
    if rendered.chars().any(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '\'' | '"'
                    | '\\'
                    | '$'
                    | '`'
                    | ';'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '!'
                    | '#'
                    | '~'
                    | '='
                    | '^'
            )
    }) {
        format!("'{}'", rendered.replace('\'', "'\\''"))
    } else {
        rendered
    }
}

pub fn render_hook_command(path: &Path, event: &str) -> String {
    format!("{} {event}", shell_program_path(path))
}

pub fn render_bare_hook_command(event: &str) -> String {
    render_hook_command(Path::new(HOOK_PROGRAM), event)
}

pub fn is_paneflow_hook_command(command: &str) -> bool {
    paneflow_hook_program_token(command).is_some()
}

pub fn paneflow_hook_program_token(command: &str) -> Option<String> {
    command_program_token(command).filter(|program| is_paneflow_hook_program(program))
}

fn is_paneflow_hook_program(program: &str) -> bool {
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    basename == HOOK_PROGRAM
}

pub fn command_program_token(command: &str) -> Option<String> {
    let mut output = String::new();
    let mut characters = command.trim_start().chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(character) = characters.next() {
        match quote {
            None => {
                if character.is_whitespace() {
                    break;
                }
                match character {
                    '\'' | '"' => quote = Some(character),
                    '\\' if characters.peek() == Some(&'\'') => {
                        let _ = characters.next();
                        output.push('\'');
                    }
                    _ => output.push(character),
                }
            }
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    output.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    match characters.peek().copied() {
                        Some(next @ ('"' | '\\' | '$' | '`')) => {
                            let _ = characters.next();
                            output.push(next);
                        }
                        _ => output.push(character),
                    }
                } else {
                    output.push(character);
                }
            }
            _ => {}
        }
    }

    (!output.is_empty()).then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_render_unquoted_and_byte_identical() {
        // Stored hook commands for ordinary install paths must not change
        // (issue #216): a rewrite here would make previously-installed
        // hooks look unmanaged until reconciled.
        let path = Path::new("/Users/user-name/Library/paneflow_1.2/bin/paneflow-ai-hook");
        assert_eq!(shell_program_path(path), display_hook_program(path));
    }

    #[test]
    fn metacharacter_paths_render_quoted_and_round_trip() {
        // Issue #216: paths containing POSIX shell metacharacters (not just
        // whitespace / quoting characters) must be single-quoted, and the
        // quoted form must round-trip through command_program_token.
        for raw in [
            "/tmp/backup(1)/paneflow-ai-hook",
            "/tmp/issue#216/paneflow-ai-hook",
            "/tmp/~archive/paneflow-ai-hook",
            "/tmp/a<b>c/paneflow-ai-hook",
            "/tmp/glob*?[x]{y}/paneflow-ai-hook",
            "/tmp/bang!eq=caret^/paneflow-ai-hook",
        ] {
            let path = Path::new(raw);
            assert_eq!(
                shell_program_path(path),
                format!("'{raw}'"),
                "path with shell metacharacter must be quoted: {raw}",
            );
            let command = render_hook_command(path, "Stop");
            assert_eq!(
                paneflow_hook_program_token(&command).as_deref(),
                Some(raw),
                "quoted command must round-trip: {command}",
            );
        }
    }
}
