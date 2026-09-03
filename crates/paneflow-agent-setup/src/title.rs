//! The one-line title a file row carries: frontmatter `name:` (then
//! `description:`), else the first markdown heading, else the file stem.
//!
//! Only the head of a file is ever read ([`TITLE_SNIFF_BYTES`]): a title is a
//! label, not a reason to pull a 10 MB instruction file into memory.

use std::io::Read;
use std::path::Path;

/// How far into a file the title sniff reads.
pub const TITLE_SNIFF_BYTES: usize = 8 * 1024;

/// Longest title kept, so a pathological heading cannot widen a row.
const MAX_TITLE_CHARS: usize = 120;

/// Read the first [`TITLE_SNIFF_BYTES`] of `path` and derive its title. Any
/// read failure falls back to the stem: the row still lists, it just says
/// less.
pub fn read_title(path: &Path) -> String {
    let stem = file_stem(path);
    let mut head = Vec::with_capacity(TITLE_SNIFF_BYTES);
    let read = std::fs::File::open(path)
        .and_then(|file| file.take(TITLE_SNIFF_BYTES as u64).read_to_end(&mut head));
    if read.is_err() {
        return stem;
    }
    title_from_head(&head, &stem)
}

/// The file stem used as the last-resort title.
pub fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Pure core: the precedence rule over the bytes already read.
pub fn title_from_head(head: &[u8], stem: &str) -> String {
    let text = String::from_utf8_lossy(head);
    frontmatter_title(&text)
        .or_else(|| first_heading(&text))
        .map(|title| clip(&title))
        .unwrap_or_else(|| stem.to_string())
}

/// `name:` (then `description:`) out of a leading `---` YAML block. Only
/// top-level scalar keys are read; a nested or multi-line value yields nothing
/// and the heading rule takes over.
fn frontmatter_title(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed.trim() == "---" || trimmed.trim() == "..." {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" if name.is_none() => name = Some(value.to_string()),
            "description" if description.is_none() => description = Some(value.to_string()),
            _ => {}
        }
    }
    name.or(description)
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// The first ATX heading (`# Title`), skipping fenced code blocks so a `#`
/// comment inside a shell snippet is not mistaken for one.
fn first_heading(text: &str) -> Option<String> {
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        if hashes == 0 || hashes > 6 {
            continue;
        }
        let rest = &trimmed[hashes..];
        if !rest.starts_with(' ') && !rest.starts_with('\t') {
            continue;
        }
        let heading = rest.trim().trim_end_matches('#').trim();
        if !heading.is_empty() {
            return Some(heading.to_string());
        }
    }
    None
}

fn clip(title: &str) -> String {
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_TITLE_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(MAX_TITLE_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_name_wins_over_a_heading() {
        let head = b"---\nname: deploy-checklist\ndescription: How to ship\n---\n# Deploy\n";
        assert_eq!(title_from_head(head, "SKILL"), "deploy-checklist");
    }

    #[test]
    fn frontmatter_description_stands_in_for_a_missing_name() {
        let head = b"---\ndescription: \"Rules for the API layer\"\nglobs: src/**\n---\n";
        assert_eq!(title_from_head(head, "api"), "Rules for the API layer");
    }

    #[test]
    fn the_first_heading_is_used_when_there_is_no_frontmatter() {
        let head = b"Some preamble\n\n```sh\n# not a heading\n```\n## Project rules ##\n";
        assert_eq!(title_from_head(head, "AGENTS"), "Project rules");
    }

    #[test]
    fn the_stem_is_the_last_resort() {
        assert_eq!(title_from_head(b"plain text only\n", "CLAUDE"), "CLAUDE");
        assert_eq!(title_from_head(b"", "CLAUDE"), "CLAUDE");
        // A `#` with no following space is not a heading.
        assert_eq!(title_from_head(b"#hashtag\n", "notes"), "notes");
    }

    #[test]
    fn a_runaway_heading_is_clipped() {
        let long = format!("# {}\n", "x".repeat(500));
        let title = title_from_head(long.as_bytes(), "s");
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn read_title_only_looks_at_the_head_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.md");
        let mut body = "x".repeat(TITLE_SNIFF_BYTES * 4);
        body.push_str("\n# Late heading\n");
        std::fs::write(&path, body).unwrap();
        // The heading sits past the sniff window, so the stem is used.
        assert_eq!(read_title(&path), "big");
        assert_eq!(read_title(&dir.path().join("missing.md")), "missing");
    }
}
