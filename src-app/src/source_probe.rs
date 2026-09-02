//! Test-only helper for the source-pinning tests (issue #219).
//!
//! A number of tests read production source through `include_str!` and
//! assert a property of ONE region of it: the menu tree handed to
//! `cx.set_menus`, a single `fn` body, one `impl` block. The hazard is the
//! unbounded slice - `src.split(start).nth(1)` or
//! `src.split_once(start).map(|(_, rest)| rest)` with no end anchor - which
//! runs to end-of-file. That remainder includes the test module itself, so
//! the assertion either matches its own needle or quietly covers code it
//! never meant to (fail-open). A silently missing anchor is as bad:
//! `.split(renamed).next()` hands back the whole input and the test keeps
//! passing against the wrong region.
//!
//! Policy: a source-slice test MUST bound its region with both a start and an
//! end anchor through [`source_slice`]. The helper panics, naming the anchor,
//! when the start is absent or when no end follows it, so a renamed or
//! reordered anchor fails LOUDLY instead of matching nothing or everything.
//! Compose an anchor at runtime (`format!`) when the test's own literal would
//! otherwise be the first match in the file; a column-0 `"\n}\n"` is a
//! structural end anchor for a top-level item, since rustfmt indents every
//! nested closing brace.

/// The text strictly between the first `start` in `src` and the first `end`
/// that follows it. Neither anchor is part of the result.
///
/// Panics when `start` is missing, when `end` does not occur after `start`
/// (absent, or only earlier in the file), or when either anchor is empty.
#[track_caller]
pub(crate) fn source_slice<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    assert!(
        !start.is_empty(),
        "source-slice start anchor must not be empty"
    );
    assert!(!end.is_empty(), "source-slice end anchor must not be empty");
    let start_at = src
        .find(start)
        .unwrap_or_else(|| panic!("source-slice start anchor {start:?} is missing"));
    let body_at = start_at + start.len();
    let len = src[body_at..].find(end).unwrap_or_else(|| {
        panic!("source-slice end anchor {end:?} does not follow start anchor {start:?}")
    });
    &src[body_at..body_at + len]
}

#[cfg(test)]
mod tests {
    use super::source_slice;

    const SRC: &str = "head\nfn one() {\n    a;\n}\n\nfn two() {\n    b;\n}\n";

    #[test]
    fn returns_the_region_strictly_between_the_anchors() {
        assert_eq!(source_slice(SRC, "fn one() {", "\n}\n"), "\n    a;");
        assert_eq!(source_slice(SRC, "fn two() {", "\n}\n"), "\n    b;");
    }

    #[test]
    #[should_panic(expected = "start anchor \"fn three() {\" is missing")]
    fn a_missing_start_anchor_panics() {
        source_slice(SRC, "fn three() {", "\n}\n");
    }

    #[test]
    #[should_panic(
        expected = "end anchor \"fn three()\" does not follow start anchor \"fn two() {\""
    )]
    fn a_missing_end_anchor_panics_instead_of_scanning_to_eof() {
        source_slice(SRC, "fn two() {", "fn three()");
    }

    #[test]
    #[should_panic(
        expected = "end anchor \"fn one()\" does not follow start anchor \"fn two() {\""
    )]
    fn an_end_anchor_before_the_start_anchor_panics() {
        source_slice(SRC, "fn two() {", "fn one()");
    }

    #[test]
    #[should_panic(expected = "end anchor must not be empty")]
    fn an_empty_anchor_panics() {
        source_slice(SRC, "fn two() {", "");
    }

    /// Documents the pre-#219 defect on a real site. `main.rs` slices the
    /// `impl Render for PaneFlowApp` block; the unbounded remainder used to
    /// run past the end of that `impl` through every `#[cfg(test)]` module
    /// that follows it, so the assertion covered the test source too. The
    /// bounded slice stops at the impl's column-0 closing brace.
    #[test]
    fn the_unbounded_remainder_reaches_the_test_modules_and_the_bounded_slice_does_not() {
        let src = include_str!("main.rs");
        let anchor = format!("impl Render for {} {{", "PaneFlowApp");
        let unbounded = src
            .split_once(anchor.as_str())
            .map(|(_, rest)| rest)
            .expect("the impl exists");
        assert!(
            unbounded.contains("#[cfg(test)]"),
            "the unbounded remainder runs into the test modules after the impl"
        );
        let bounded = source_slice(src, &anchor, "\n}\n");
        assert!(
            bounded.contains("fn render("),
            "the impl body is inside the slice"
        );
        assert!(
            !bounded.contains("#[cfg(test)]"),
            "the bounded slice ends at the impl and never reaches a test module"
        );
    }
}
