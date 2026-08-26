//! Markdown viewer pane (US-020 - prd-cmux-port-2026-q2.md, EP-006).
//!
//! Three-layer split:
//! - `parser` - pulldown-cmark event walker → owned `MdNode` AST. Pure Rust,
//!   no GPUI deps; unit-tested in isolation.
//! - `theme`  - semantic palette derived from the active `TerminalTheme`. The
//!   markdown viewer never owns colors; it borrows the terminal palette so a
//!   user theme switch repaints everything consistently.
//! - `view`   - `MarkdownView: Render` GPUI entity that walks the AST and
//!   emits a nested `div` element tree.
//!
//! Out of scope here: live reload (US-021), scroll-state persistence (US-022),
//! syntax highlighting (P2 follow-up via `syntect`).

mod parser;
// Image-ref helpers in this module are still unused (no image load path).
// `validate_link_url` is live: Help-menu URLs go through `open_http_url`.
// Markdown click hit-testing is not wired yet; when it is, use the
// same function before `open::that`.
pub(crate) mod security;
mod state;
mod theme;
mod view;

// Public surface: only `MarkdownView` is consumed outside this module today,
// but the parser primitives are re-exported for upcoming stories (US-021
// live reload re-runs `parse_with_limit`; US-022 walks `MdNode` for search).
#[allow(unused_imports)]
pub use parser::{MAX_INPUT_BYTES, MdNode, ParseError, Span, SpanStyle, parse_with_limit};
// US-016/US-020 (orchestration-v2): the agent-question display path reuses
// the same bidi/zero-width strip as rendered markdown (one sanitizer, every
// untrusted display surface).
pub(crate) use parser::strip_bidi_zero_width;
pub use view::MarkdownView;
