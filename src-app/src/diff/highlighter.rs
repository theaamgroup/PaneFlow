//! Tree-sitter syntax highlighting for diff lines.
//!
//! Engine introduced by `prd-diff-syntax-highlight-2026-Q3.md`; language
//! coverage + the Markdown inline pass added by
//! `prd-diff-syntax-palette-2026-Q3.md` (EP-002).
//!
//! The same engine Zed uses. Unlike the old syntect pass (0.3-2.8 s/file → the
//! reason highlighting shipped gated), a tree-sitter parse is ms-scale, so we
//! highlight each side once at build time (off the GPUI thread, inside
//! `view.rs`'s `smol::unblock`) and bucket the captures into per-line runs.
//! Very large sides skip parsing and render monochrome; unknown extensions /
//! parse failures do the same.
//!
//! Grammars bridge through `tree-sitter-language` 0.1 (`LANGUAGE: LanguageFn`);
//! core `tree-sitter` 0.27 is a direct dependency (#223). Fifteen grammars
//! (issue #433, upstream `1f5fde23`) compile Zed's own `highlights.scm`,
//! vendored byte for byte under `queries/<lang>/` and pinned by
//! `queries/MANIFEST.toml` (Zed revision + one sha256 per file, verified by
//! `manifest_hashes_match_the_vendored_queries`); TOML, HTML, Java and Ruby
//! keep their crate's stock query. JavaScript runs Zed's JavaScript query on
//! the TSX grammar. Markdown runs TWO passes over the same text - the block
//! query (headings / fences / list markers) and the inline query (emphasis /
//! links / inline code) - merged by `resolve_runs`, which follows Zed's
//! last-active-capture rule so nested inline captures keep their colors.
//!
//! **Reuse contract (prd-file-editor-2026-Q3, US-004).** The file editor's
//! incremental driver (`app/diff_dock/code/highlight.rs`) must color a file
//! exactly like this module colors its diff, so it consumes the same grammars
//! ([`grammar_for_ext`], [`markdown_inline_grammar`]), the same size cutoff
//! ([`MAX_HIGHLIGHT_BYTES`]) and the same overlap resolution
//! ([`resolve_runs`]) instead of holding a second copy of the grammar table.
//! Those five items are `pub(crate)` for that reason alone - the parse driven
//! here is still the diff's own, and the editor never calls [`highlight_lines`]
//! outside its parity test. Nothing in this module's behavior may change to
//! suit the editor: a divergence between the two surfaces is the one failure
//! US-004 does not tolerate.

use std::ops::Range;
use std::sync::OnceLock;

use gpui::Hsla;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

use super::syntax::DiffSyntax;

/// Full-file tree-sitter parsing above this size is more likely to hurt Review
/// responsiveness than help readability. The diff still renders normally.
///
/// Set by measurement, not by guess (#427): a file at its cap must hold less
/// than 128 MiB of tree-sitter tree, and `tree_memory_probe`
/// (`app/diff_dock/code/perf_bench.rs`) asserts it. The densest single-pass
/// grammar in the corpus, minified JSON, sets the number; Rust alone would
/// allow twice as much.
pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 2_000_000;

/// Markdown's own cap: the inline injection parses the whole document a
/// second time, so its tree costs about four times Rust's per source byte.
/// Read through [`highlight_cap`] by the editor and the diff view alike.
pub(crate) const MAX_MARKDOWN_HIGHLIGHT_BYTES: usize = 1_000_000;

/// Upper bound on the captures one row feeds into [`resolve_runs`]; anything
/// past it is dropped in capture order. A pathological minified line cannot
/// turn the stack walk into an unbounded amount of work per frame.
pub(crate) const MAX_CAPTURES_PER_ROW: usize = 4_096;

pub(crate) fn is_markdown(ext: &str) -> bool {
    matches!(ext, "md" | "markdown" | "mdx")
}

/// The highlight cap for a file extension: Markdown's two-pass budget, or
/// [`MAX_HIGHLIGHT_BYTES`] for everything else.
pub(crate) fn highlight_cap(ext: &str) -> usize {
    if is_markdown(ext) {
        MAX_MARKDOWN_HIGHLIGHT_BYTES
    } else {
        MAX_HIGHLIGHT_BYTES
    }
}

/// A resolved grammar: its `Language` + parsed highlights `Query`, interned
/// once per process (`Query::new` is not cheap).
pub(crate) struct Grammar {
    pub(crate) language: Language,
    pub(crate) query: Query,
}

/// Resolve + intern the grammar for a file extension; `None` for unknown
/// extensions (→ monochrome fallback).
pub(crate) fn grammar_for_ext(ext: &str) -> Option<&'static Grammar> {
    macro_rules! grammar {
        ($cell:ident, $lang:expr, $query:expr) => {{
            static $cell: OnceLock<Option<Grammar>> = OnceLock::new();
            $cell
                .get_or_init(|| {
                    let language: Language = $lang.into();
                    let query = Query::new(&language, $query).ok()?;
                    Some(Grammar { language, query })
                })
                .as_ref()
        }};
    }
    match ext {
        "rs" => grammar!(
            RUST,
            tree_sitter_rust::LANGUAGE,
            include_str!("queries/rust/highlights.scm")
        ),
        // Zed keeps separate JSON and JSONC queries (the latter adds comments),
        // so the two extensions intern distinct grammars over one language.
        "json" => grammar!(
            JSON,
            tree_sitter_json::LANGUAGE,
            include_str!("queries/json/highlights.scm")
        ),
        "jsonc" => grammar!(
            JSONC,
            tree_sitter_json::LANGUAGE,
            include_str!("queries/jsonc/highlights.scm")
        ),
        "sh" | "bash" | "zsh" => grammar!(
            BASH,
            tree_sitter_bash::LANGUAGE,
            include_str!("queries/bash/highlights.scm")
        ),
        "py" | "pyi" => grammar!(
            PY,
            tree_sitter_python::LANGUAGE,
            include_str!("queries/python/highlights.scm")
        ),
        "ts" | "mts" | "cts" => grammar!(
            TS,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            include_str!("queries/typescript/highlights.scm")
        ),
        "tsx" => grammar!(
            TSX,
            tree_sitter_typescript::LANGUAGE_TSX,
            include_str!("queries/tsx/highlights.scm")
        ),
        // No tree-sitter-javascript crate: Zed's JavaScript query runs on the
        // TSX grammar (recorded as a deviation in `queries/MANIFEST.toml`).
        "jsx" | "js" | "mjs" | "cjs" => grammar!(
            JS,
            tree_sitter_typescript::LANGUAGE_TSX,
            include_str!("queries/javascript/highlights.scm")
        ),
        "toml" => grammar!(
            TOML,
            tree_sitter_toml_ng::LANGUAGE,
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY
        ),
        "md" | "markdown" | "mdx" => grammar!(
            MD,
            tree_sitter_md::LANGUAGE,
            include_str!("queries/markdown/highlights.scm")
        ),
        // EP-002 / US-003 (P1): Go, YAML, CSS, HTML. HTML stays on the crate's
        // stock query (no Zed import, see `fixtures/README.md`).
        "go" => grammar!(
            GO,
            tree_sitter_go::LANGUAGE,
            include_str!("queries/go/highlights.scm")
        ),
        "yaml" | "yml" => grammar!(
            YAML,
            tree_sitter_yaml::LANGUAGE,
            include_str!("queries/yaml/highlights.scm")
        ),
        "css" => grammar!(
            CSS,
            tree_sitter_css::LANGUAGE,
            include_str!("queries/css/highlights.scm")
        ),
        "html" | "htm" => grammar!(
            HTML,
            tree_sitter_html::LANGUAGE,
            tree_sitter_html::HIGHLIGHTS_QUERY
        ),
        // EP-002 / US-005 (P2): C, C++, Java, Ruby. Java and Ruby keep their
        // crate's stock query (`HIGHLIGHTS_QUERY`).
        "c" | "h" => grammar!(
            C,
            tree_sitter_c::LANGUAGE,
            include_str!("queries/c/highlights.scm")
        ),
        // Zed's C++ query is self-contained (no `; inherits: c` overlay to
        // layer under), and it names the module-syntax nodes that only exist
        // past the crates.io 0.23.4 grammar - hence the git pin in Cargo.toml.
        // The stock overlay survives as test data in `fixtures/`.
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => grammar!(
            CPP,
            tree_sitter_cpp::LANGUAGE,
            include_str!("queries/cpp/highlights.scm")
        ),
        "java" => grammar!(
            JAVA,
            tree_sitter_java::LANGUAGE,
            tree_sitter_java::HIGHLIGHTS_QUERY
        ),
        "rb" => grammar!(
            RUBY,
            tree_sitter_ruby::LANGUAGE,
            tree_sitter_ruby::HIGHLIGHTS_QUERY
        ),
        _ => None,
    }
}

/// The Markdown *inline* grammar (US-004) - a second pass over the same text
/// that colors emphasis / links / inline code the block grammar leaves grey.
/// Interned once; `None` if its query fails to compile (→ block-only fallback,
/// still graceful).
pub(crate) fn markdown_inline_grammar() -> Option<&'static Grammar> {
    static MD_INLINE: OnceLock<Option<Grammar>> = OnceLock::new();
    MD_INLINE
        .get_or_init(|| {
            let language: Language = tree_sitter_md::INLINE_LANGUAGE.into();
            let query = Query::new(
                &language,
                include_str!("queries/markdown-inline/highlights.scm"),
            )
            .ok()?;
            Some(Grammar { language, query })
        })
        .as_ref()
}

/// Per-line foreground runs (line-relative byte ranges), indexed like
/// `str::lines()` so the index lines up with the diff row builder. Empty inner
/// vecs for unknown grammars / parse failures.
pub fn highlight_lines(
    text: &str,
    ext: &str,
    syntax: &DiffSyntax,
) -> Vec<Vec<(Range<usize>, Hsla)>> {
    if text.len() > highlight_cap(ext) {
        return text.lines().map(|_| Vec::new()).collect();
    }

    // Byte range of each line, matching `str::lines()` exactly (the slices are
    // substrings of `text`, so pointer subtraction gives the offset; `len()`
    // excludes the trailing `\n` / `\r\n`).
    let line_ranges: Vec<Range<usize>> = text
        .lines()
        .map(|l| {
            let start = l.as_ptr() as usize - text.as_ptr() as usize;
            start..start + l.len()
        })
        .collect();
    let mut out: Vec<Vec<(Range<usize>, Hsla)>> = vec![Vec::new(); line_ranges.len()];

    let Some(grammar) = grammar_for_ext(ext) else {
        return out;
    };
    apply_grammar(grammar, text, syntax, &line_ranges, &mut out);

    // US-004: Markdown gets a second inline pass merged into the same runs.
    // `resolve_runs` (below) collapses block/inline overlaps: the inline
    // captures come later in capture order, so they paint over the block ones.
    if is_markdown(ext)
        && let Some(inline) = markdown_inline_grammar()
    {
        apply_grammar(inline, text, syntax, &line_ranges, &mut out);
    }

    for runs in &mut out {
        resolve_runs(runs);
    }
    out
}

/// Parse `text` with `grammar`, resolve each capture to a palette color, and
/// bucket the colored spans into per-line runs. A `set_language` / parse
/// failure is a graceful no-op (leaves `out` as-is → monochrome).
fn apply_grammar(
    grammar: &Grammar,
    text: &str,
    syntax: &DiffSyntax,
    line_ranges: &[Range<usize>],
    out: &mut [Vec<(Range<usize>, Hsla)>],
) {
    let mut parser = Parser::new();
    if parser.set_language(&grammar.language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(text, None) else {
        return;
    };

    let names = grammar.query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut caps = cursor.captures(&grammar.query, tree.root_node(), text.as_bytes());
    // `QueryCursor::captures` is a StreamingIterator in tree-sitter >= 0.25.
    while let Some((mat, idx)) = caps.next() {
        let cap = mat.captures()[*idx];
        let name = names[cap.index as usize];
        let Some(color) = syntax.color_for_capture(name) else {
            continue;
        };
        bucket_capture(
            cap.node.start_byte(),
            cap.node.end_byte(),
            color,
            line_ranges,
            out,
        );
    }
}

/// Split a capture's byte span across the lines it covers, pushing
/// line-relative runs. Binary-searches for the first overlapping line; most
/// captures touch a single line.
fn bucket_capture(
    cstart: usize,
    cend: usize,
    color: Hsla,
    line_ranges: &[Range<usize>],
    out: &mut [Vec<(Range<usize>, Hsla)>],
) {
    if cend <= cstart || line_ranges.is_empty() {
        return;
    }
    let mut li = line_ranges.partition_point(|r| r.end <= cstart);
    while li < line_ranges.len() {
        let lr = &line_ranges[li];
        if lr.start >= cend {
            break;
        }
        let s = cstart.max(lr.start) - lr.start;
        let e = cend.min(lr.end) - lr.start;
        if e > s {
            out[li].push((s..e, color));
        }
        li += 1;
    }
}

/// Sort + de-overlap one line's runs into the ascending, non-overlapping list
/// `element.rs::text_runs` expects, following Zed's last-active-capture rule
/// (issue #433, upstream `1f5fde23`; `zed:crates/language/src/syntax_map.rs`).
/// Captures are ordered by byte start, preserving capture order at equal
/// starts, and pushed onto a stack as the offset reaches them; the capture on
/// top paints until its own end or the next capture's start, even when it is
/// wider than one pushed earlier. Empty ranges are dropped, input is capped at
/// [`MAX_CAPTURES_PER_ROW`], and adjacent bytes owned by one capture merge
/// into a single run. This is also what merges the Markdown block + inline
/// passes; `parity_tests.rs` holds the independent byte oracle.
pub(crate) fn resolve_runs<T: Copy>(runs: &mut Vec<(Range<usize>, T)>) {
    let mut captures: Vec<_> = runs
        .drain(..)
        .take(MAX_CAPTURES_PER_ROW)
        .filter(|(range, _)| range.start < range.end)
        .collect();
    // Stable: captures starting together keep their query order, so the later
    // one lands on top of the stack.
    captures.sort_by_key(|(range, _)| range.start);
    let mut stack: Vec<usize> = Vec::with_capacity(captures.len());
    let mut next = 0;
    let mut offset = captures.first().map_or(0, |(range, _)| range.start);
    let mut last_capture = None;
    while next < captures.len() || !stack.is_empty() {
        while stack
            .last()
            .is_some_and(|&index| captures[index].0.end <= offset)
        {
            stack.pop();
        }
        while next < captures.len() && captures[next].0.start <= offset {
            stack.push(next);
            next += 1;
        }
        let next_start = captures
            .get(next)
            .map_or(usize::MAX, |(range, _)| range.start);
        if let Some(&index) = stack.last() {
            let end = captures[index].0.end.min(next_start);
            if last_capture == Some(index)
                && let Some((range, _)) = runs.last_mut()
            {
                range.end = end;
            } else {
                runs.push((offset..end, captures[index].1));
            }
            last_capture = Some(index);
            offset = end;
        } else {
            offset = next_start;
            last_capture = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::paneflow_dark;

    #[test]
    fn highlights_rust_keyword() {
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let lines = highlight_lines("fn main() {}", "rs", &syn);
        assert_eq!(lines.len(), 1, "one run-list per input line");
        assert!(
            !lines[0].is_empty(),
            "expected colored runs for recognized rust code"
        );
        // Runs are byte-ranged within the line, sorted, non-overlapping.
        for w in lines[0].windows(2) {
            assert!(w[0].0.end <= w[1].0.start);
        }
    }

    #[test]
    fn the_diff_view_caps_markdown_at_its_own_two_pass_budget() {
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let mut text = String::with_capacity(MAX_MARKDOWN_HIGHLIGHT_BYTES + 128);
        while text.len() <= MAX_MARKDOWN_HIGHLIGHT_BYTES {
            text.push_str("# Heading with `code`, *emphasis* and [a link](https://paneflow.dev)\n");
        }
        assert!(
            text.len() < MAX_HIGHLIGHT_BYTES,
            "the markdown cap must be the lower of the two, or this test proves nothing"
        );

        let lines = highlight_lines(&text, "md", &syn);
        assert_eq!(lines.len(), text.lines().count(), "one run-list per line");
        assert!(
            lines.iter().all(Vec::is_empty),
            "a markdown side past its own cap must not build two trees for the diff view"
        );

        let under = "# Title\n\nSome `code` here.\n";
        assert!(
            highlight_lines(under, "md", &syn)
                .iter()
                .any(|runs| !runs.is_empty()),
            "markdown under the cap still colors"
        );
    }

    #[test]
    fn line_count_matches_input() {
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let lines = highlight_lines("let a = 1;\nlet b = 2;\n", "rs", &syn);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn unknown_extension_returns_empty_runs_without_panic() {
        // US-006 AC #1: unknown ext → one empty run-list per line (monochrome).
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let lines = highlight_lines("plain text line\nsecond", "xyz", &syn);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|r| r.is_empty()));
    }

    /// True if at least one line carries a colored run.
    fn has_color(lines: &[Vec<(Range<usize>, Hsla)>]) -> bool {
        lines.iter().any(|r| !r.is_empty())
    }

    /// Number of pairwise-distinct colors across all lines (`Hsla` is neither
    /// `Eq` nor `Hash`, so no `HashSet`).
    fn distinct_colors(lines: &[Vec<(Range<usize>, Hsla)>]) -> usize {
        let mut seen: Vec<Hsla> = Vec::new();
        for line in lines {
            for (_, c) in line {
                if !seen.contains(c) {
                    seen.push(*c);
                }
            }
        }
        seen.len()
    }

    #[test]
    fn new_p1_grammars_produce_colored_runs() {
        // US-003 AC #2: Go / YAML / CSS / HTML each color their core families.
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let cases: &[(&str, &str)] = &[
            (
                "go",
                "package main\n\nfunc main() {\n\tvar x string = \"hi\"\n\t_ = x\n}\n",
            ),
            ("yaml", "name: paneflow\nport: 8080\nenabled: true\n"),
            ("css", ".btn {\n  color: #ffffff;\n  margin: 0;\n}\n"),
            ("html", "<div class=\"x\">\n  <p>hello</p>\n</div>\n"),
        ];
        for (ext, src) in cases {
            let lines = highlight_lines(src, ext, &syn);
            assert!(has_color(&lines), "expected colored runs for {ext} snippet");
        }
    }

    #[test]
    fn new_p2_grammars_produce_colored_runs() {
        // US-005 AC #2: C / C++ / Java / Ruby each color keyword/type/fn/string.
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let cases: &[(&str, &str)] = &[
            (
                "c",
                "#include <stdio.h>\nint main(void) {\n  return 0;\n}\n",
            ),
            (
                "cpp",
                "#include <vector>\nint main() {\n  std::vector<int> v;\n  return 0;\n}\n",
            ),
            (
                "java",
                "class A {\n  void f() {\n    String s = \"x\";\n  }\n}\n",
            ),
            ("rb", "def foo\n  x = \"bar\"\n  puts x\nend\n"),
        ];
        for (ext, src) in cases {
            let lines = highlight_lines(src, ext, &syn);
            assert!(has_color(&lines), "expected colored runs for {ext} snippet");
        }
    }

    #[test]
    fn markdown_block_and_inline_passes_color_richly() {
        // US-004 AC #2/#4: heading + fenced code + inline link + list marker
        // each colored; the inline pass adds emphasis/link color the block
        // grammar leaves grey, so the doc shows several distinct colors.
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let doc = "# Heading\n\nSome **bold** text and a [link](https://github.com/theaamgroup/paneflow).\n\n- first item\n- second item\n\n```rust\nfn x() {}\n```\n";
        let lines = highlight_lines(doc, "md", &syn);
        assert!(has_color(&lines), "expected colored markdown runs");
        assert!(
            distinct_colors(&lines) >= 3,
            "expected ≥3 distinct markdown colors (heading/code/link/marker), got {}",
            distinct_colors(&lines)
        );
        // Runs stay sorted + non-overlapping after the block+inline merge
        // (US-004 AC #3: no double-coloring / artifact from overlap).
        for line in &lines {
            for w in line.windows(2) {
                assert!(
                    w[0].0.end <= w[1].0.start,
                    "merged markdown runs must be non-overlapping"
                );
            }
        }
    }

    #[test]
    fn resolve_runs_preserves_nested_specific_captures() {
        let palette = paneflow_dark().syntax;
        let mut runs = vec![
            (0..10, palette.text_literal),
            (2..5, palette.emphasis_strong),
            (7..9, palette.link_text),
        ];

        resolve_runs(&mut runs);

        let ranges: Vec<Range<usize>> = runs.iter().map(|(range, _)| range.clone()).collect();
        assert_eq!(ranges, vec![0..2, 2..5, 5..7, 7..9, 9..10]);
        assert_eq!(runs[1].1, palette.emphasis_strong);
        assert_eq!(runs[3].1, palette.link_text);
    }

    #[test]
    fn resolve_runs_uses_capture_order_even_when_the_last_capture_is_wider() {
        // Zed's rule: the later capture paints, wider or not, and the earlier
        // one resumes once it ends (the old narrowest-wins rule gave 0..3 to 1).
        let mut runs = vec![(0..3, 1), (0..8, 2), (2..5, 3), (2..5, 4)];
        resolve_runs(&mut runs);
        assert_eq!(runs, vec![(0..2, 2), (2..5, 4), (5..8, 2)]);
    }

    #[test]
    fn resolve_runs_caps_a_row_at_the_capture_budget() {
        let mut runs: Vec<_> = (0..MAX_CAPTURES_PER_ROW + 512)
            .map(|index| (index * 2..index * 2 + 1, index))
            .collect();
        resolve_runs(&mut runs);
        assert_eq!(runs.len(), MAX_CAPTURES_PER_ROW);
        assert_eq!(
            runs.last().map(|(_, index)| *index),
            Some(MAX_CAPTURES_PER_ROW - 1)
        );
    }

    #[test]
    fn malformed_and_empty_inputs_never_panic() {
        // US-003 AC #4 / US-006: empty + garbage input of every supported new
        // type yields no panic (and 0 or N empty run-lists).
        let syn = DiffSyntax::from_theme(&paneflow_dark());
        let exts = [
            "go", "yaml", "yml", "css", "html", "c", "cpp", "java", "rb", "md",
        ];
        for ext in exts {
            let _ = highlight_lines("", ext, &syn);
            let _ = highlight_lines(">>>;;;@@@ \0 not valid {[(", ext, &syn);
            let _ = highlight_lines("\n\n\n", ext, &syn);
        }
    }

    #[test]
    fn malformed_query_compiles_to_none_not_panic() {
        // US-006 AC #2 (simulated query-compile failure): the interning step
        // turns a failed `Query::new` into a `None` grammar via `.ok()?`, which
        // `highlight_lines` already treats as monochrome (see
        // `unknown_extension_returns_empty_runs_without_panic`). We can't inject
        // a bad query into the static table, so we lock the contract on the
        // same fallible call directly.
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let bad = Query::new(&language, "(this is not a valid query");
        assert!(
            bad.is_err(),
            "a malformed query must Err so `.ok()?` degrades to monochrome"
        );
    }
}
