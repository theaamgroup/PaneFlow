//! Terminal theming with 36 color slots (see [`model::TerminalTheme`]),
//! compatible with Zed's terminal theme format.

mod builtin;
mod model;
mod watcher;

pub use builtin::{
    THEMES, ThemeEntry, claude, cursor, one_dark, paneflow_light, theme_by_name, vercel,
};
pub use model::{DiffColors, SyntaxPalette, TerminalTheme, UiColors, ui_colors, ui_colors_with};
pub use watcher::{
    ThemeWatcher, active_theme, config_mtime, invalidate_theme_cache, theme_generation,
};
