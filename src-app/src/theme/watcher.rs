//! Theme cache for the `theme` field of `paneflow.json`.
//!
//! The cache is a process-global `Mutex<Option<TerminalTheme>>`. The first
//! [`active_theme`] call resolves it from the file on disk. After that it is
//! replaced by [`set_active_theme_from`], which resolves the theme from a
//! config already in memory and never re-reads the file: the config watcher's
//! reload path (`process_config_changes`) passes the config it just applied,
//! and the Settings theme writes pass the config they just saved. A save that
//! is not valid JSON landing between those two steps therefore cannot turn
//! into a cached PaneFlow Dark. Themes are built in, so `paneflow.json` is the
//! only file a theme change touches.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use paneflow_config::schema::PaneFlowConfig;
use parking_lot::Mutex;

use super::builtin::{paneflow_dark, theme_by_name};
use super::model::{TerminalTheme, apply_surface_overrides};

/// The resolved theme plus a generation bumped whenever it may have changed.
struct ThemeCache {
    theme: Mutex<Option<TerminalTheme>>,
    generation: AtomicU64,
}

impl ThemeCache {
    const fn new() -> Self {
        Self {
            theme: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// The cached theme, calling `resolve` only while the cache is empty.
    fn get_or_resolve(&self, resolve: impl FnOnce() -> TerminalTheme) -> TerminalTheme {
        // parking_lot::Mutex: ~2x faster than std::sync::Mutex under
        // contention, no poisoning. Called several times per render frame
        // from the agents UI; std lock dominates that path under streaming.
        *self.theme.lock().get_or_insert_with(resolve)
    }

    /// Replace the cached theme with the one `config` names.
    fn set_from_config(&self, config: &PaneFlowConfig) {
        let theme = theme_for_config(config);
        *self.theme.lock() = Some(theme);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

static THEME_CACHE: ThemeCache = ThemeCache::new();

/// Resolve the theme `config` names, falling back to PaneFlow Dark when it
/// names none or an unknown one. Reads nothing from disk.
fn theme_for_config(config: &PaneFlowConfig) -> TerminalTheme {
    if let Some(name) = config.theme.as_deref() {
        if let Some(theme) = theme_by_name(name) {
            return apply_surface_overrides(theme);
        }
        log::warn!("Unknown theme '{}', using default", name);
    }
    apply_surface_overrides(paneflow_dark())
}

/// Resolve the theme from the config file on disk. Only the first
/// [`active_theme`] call uses this; a missing or corrupted file resolves to
/// PaneFlow Dark.
fn resolve_theme() -> TerminalTheme {
    theme_for_config(&paneflow_config::loader::load_config())
}

/// Cache the theme `config` names and bump [`theme_generation`]. Callers pass
/// the config they just applied or saved, so the cache never depends on a
/// second read of `paneflow.json`.
pub fn set_active_theme_from(config: &PaneFlowConfig) {
    THEME_CACHE.set_from_config(config);
}

/// Monotonic generation bumped whenever the active theme may have changed.
pub fn theme_generation() -> u64 {
    THEME_CACHE.generation()
}

/// Get the config file modification time for change detection.
pub fn config_mtime() -> Option<SystemTime> {
    let config_path = paneflow_config::loader::config_path()?;
    std::fs::metadata(config_path).ok()?.modified().ok()
}

/// Get the active theme: the cached one, resolved from `paneflow.json` on the
/// first call.
pub fn active_theme() -> TerminalTheme {
    THEME_CACHE.get_or_resolve(resolve_theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_theme(name: Option<&str>) -> PaneFlowConfig {
        PaneFlowConfig {
            theme: name.map(str::to_string),
            ..Default::default()
        }
    }

    fn same_palette(a: &TerminalTheme, b: &TerminalTheme) -> bool {
        a.background == b.background
            && a.foreground == b.foreground
            && a.cursor == b.cursor
            && a.red == b.red
            && a.blue == b.blue
    }

    /// Issue #850: an applied reload caches the theme of the config it
    /// applied. The disk resolver must never run afterwards, so an invalid
    /// save landing between the reload and the next render cannot cache
    /// PaneFlow Dark in its place.
    #[test]
    fn a_theme_set_from_a_config_is_served_without_reading_disk() {
        let cache = ThemeCache::new();
        let before = cache.generation();

        cache.set_from_config(&config_with_theme(Some("Claude Light")));

        let got =
            cache.get_or_resolve(|| panic!("an applied config must not be re-read from disk"));
        let want = apply_surface_overrides(theme_by_name("Claude Light").expect("bundled theme"));
        assert!(same_palette(&got, &want), "the passed config's theme wins");
        assert!(
            !same_palette(&got, &apply_surface_overrides(paneflow_dark())),
            "Claude Light must not collapse to the PaneFlow Dark fallback"
        );
        assert!(cache.generation() > before, "a set bumps the generation");
    }

    #[test]
    fn a_config_without_a_known_theme_resolves_to_paneflow_dark() {
        let dark = apply_surface_overrides(paneflow_dark());
        for name in [None, Some("No Such Theme")] {
            let theme = theme_for_config(&config_with_theme(name));
            assert!(same_palette(&theme, &dark), "{name:?}");
        }
    }

    #[test]
    fn an_empty_cache_resolves_once() {
        let cache = ThemeCache::new();
        let dark = apply_surface_overrides(paneflow_dark());
        let first = cache.get_or_resolve(|| dark);
        let second = cache.get_or_resolve(|| panic!("a filled cache must not resolve again"));
        assert!(same_palette(&first, &second));
    }
}
