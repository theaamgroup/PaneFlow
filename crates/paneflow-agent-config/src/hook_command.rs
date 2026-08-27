use std::path::Path;

const HOOK_PROGRAM: &str = "paneflow-ai-hook";

pub fn display_hook_program(path: &Path) -> String {
    path.display().to_string()
}

pub fn shell_program_path(path: &Path) -> String {
    let rendered = display_hook_program(path);
    if rendered.chars().any(|character| {
        character.is_whitespace()
            || matches!(character, '\'' | '"' | '\\' | '$' | '`' | ';' | '&' | '|')
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
