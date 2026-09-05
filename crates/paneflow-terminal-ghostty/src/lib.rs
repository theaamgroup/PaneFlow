//! Safe Rust surface over libghostty. The native FFI layer is always linked:
//! there is one engine and no fallback.
#![deny(unsafe_op_in_unsafe_fn)]

mod abi;
mod abi_layout;
mod batch;
mod callback_ffi;
mod callbacks;
mod color;
mod constructor;
mod encode;
#[cfg(test)]
mod encode_tests;
mod engine;
mod error;
mod formatter;
mod grid;
mod grid_ref;
mod handles;
mod input;
mod input_map;
mod input_options;
mod io;
mod kitty;
mod limits;
mod model;
mod modes;
mod navigation;
mod options;
mod osc;
mod osc7;
mod persistence;
mod render;
mod search;
mod selection;
mod selection_gesture;
mod sgr;
mod snapshot;
mod snapshot_cell;
mod snapshot_codec;
mod snapshot_ffi;
mod snapshot_state;
mod style;
mod sys;
mod terminal_ops;
mod tracked;
mod unicode;

pub use color::{
    PALETTE_LEN, PaletteMask, contrast, default_palette, encode_color_scheme_report,
    generate_palette, luminance, parse as parse_color, parse_palette_entry, parse_x11,
    perceived_luminance, x11_names,
};
pub use error::{GhosttyError, Result};
pub use formatter::{FormatterFormat, FormatterOptions, ScreenExtra, TerminalExtra};
pub use grid_ref::{CellContent, CellInfo, RowInfo, SemanticContent, SemanticPrompt};
pub use input::{
    FocusEvent, Key, KeyAction, KeyInput, Modifiers, MouseAction, MouseButton, MouseInput,
};
pub use input_options::{KeyEventState, MouseEventState, OptionAsAlt};
pub use kitty::{
    ImageCompression, ImageFormat, ImageInfo, KittyGraphics, KittyImage, Placement,
    PlacementCursor, PlacementLayer, PlacementRenderInfo, SourceRect,
};
pub use model::{
    BackendEvent, Cell, CellFlags, Color, ColorScheme, Content, Cursor, CursorShape, Hyperlink,
    Modes, Point, ProgressReport, ProgressState, Rgb, Scroll, SearchMatch, SearchResult,
    SelectionRange, TerminalAppearance, UnderlineStyle, WideCell, WindowSize,
};
pub use modes::{Mode, ModeReportState, encode_mode_report};
pub use osc::{OSC_TERMINATOR_BEL, OSC_TERMINATOR_ST, OscCommand, OscCommandType, OscParser};
pub use persistence::TranscriptWindow;
pub use search::{
    MAX_QUERY_LEN, MAX_SEARCH_CELLS, SEARCH_CHUNK_CELLS, SearchChunk, SearchEngine, SearchLine,
};
pub use selection::{SelectionAdjust, SelectionOrder};
pub use selection_gesture::{
    DragOptions, GestureAutoscroll, GestureBehavior, GestureBehaviors, GestureGeometry,
    GestureState, PressOptions,
};
pub use sgr::{SgrAttribute, SgrParser, SgrSeparator};
pub use snapshot_codec::{
    HistoryProgress, SnapshotDecoder, SnapshotRestore, SnapshotTerminal, TerminalScreen,
};
pub use style::Style;
pub use sys::{
    DecodedImage, LogLevel, LogSink, PngDecoder, SecureRandom, alloc, free, set_log_sink,
    set_log_to_stderr, set_png_decoder, set_secure_random,
};
pub use terminal_ops::{
    ClipboardLocation, CompressionMode, CompressionOutcome, GroundWrite, PasteRepresentation,
    PasteSource, SizeReportStyle,
};
pub use tracked::TrackedRef;
pub use unicode::{GraphemeCluster, codepoint_width, grapheme_width, text_width};

pub const GHOSTTY_APP_VERSION: &str = paneflow_libghostty_sys::GHOSTTY_APP_VERSION;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildIdentity {
    pub source_sha: &'static str,
    pub api_version: &'static str,
    pub zig_version: &'static str,
    pub optimization: &'static str,
    pub simd: &'static str,
}

pub fn build_identity() -> BuildIdentity {
    const MANIFEST: &str = include_str!("../../../native/libghostty/manifest.toml");

    fn value(key: &str) -> Option<&'static str> {
        let prefix = format!("{key} = \"");
        MANIFEST
            .lines()
            .find_map(|line| line.strip_prefix(&prefix)?.strip_suffix('"'))
    }

    BuildIdentity {
        source_sha: value("source_sha").unwrap_or("unknown"),
        api_version: paneflow_libghostty_sys::EXPECTED_API_VERSION,
        zig_version: value("zig_version").unwrap_or("unknown"),
        optimization: value("build_mode").unwrap_or("unknown"),
        simd: value("simd_profile").unwrap_or("unknown"),
    }
}

pub use engine::DisplayTerminal;

#[cfg(test)]
mod identity_tests {
    #[test]
    fn build_identity_is_derived_from_the_pinned_manifest() {
        let identity = super::build_identity();
        assert_eq!(identity.source_sha.len(), 40);
        assert_eq!(identity.api_version, "0.1.0");
        assert_eq!(identity.zig_version, "0.16.0");
        assert_eq!(identity.optimization, "ReleaseFast");
        assert_eq!(identity.simd, "upstream-default");
    }
}
