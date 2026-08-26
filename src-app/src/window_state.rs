//! Durable main-window sizing across PaneFlow launches.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use gpui::{App, Bounds, Pixels, Size, Window, px, size};
use serde::{Deserialize, Serialize};

const DEFAULT_WINDOW_WIDTH: f32 = 1200.;
const DEFAULT_WINDOW_HEIGHT: f32 = 800.;
const MIN_WINDOW_WIDTH: f32 = 800.;
const MIN_WINDOW_HEIGHT: f32 = 500.;
const FALLBACK_MAX_WINDOW_WIDTH: f32 = 3840.;
const FALLBACK_MAX_WINDOW_HEIGHT: f32 = 2160.;
const MAX_WINDOW_STATE_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct PersistedWindowSize {
    width: f32,
    height: f32,
}

static LAST_WINDOWED_SIZE: Mutex<Option<PersistedWindowSize>> = Mutex::new(None);

pub(crate) fn initial_bounds(cx: &App) -> Bounds<Pixels> {
    let persisted = load().unwrap_or(PersistedWindowSize {
        width: DEFAULT_WINDOW_WIDTH,
        height: DEFAULT_WINDOW_HEIGHT,
    });
    *last_windowed_size_guard() = Some(persisted);
    let visible_bounds = cx.primary_display().map(|display| display.visible_bounds());
    let display_size = visible_bounds.map(|bounds| bounds.size).unwrap_or_else(|| {
        size(
            px(FALLBACK_MAX_WINDOW_WIDTH),
            px(FALLBACK_MAX_WINDOW_HEIGHT),
        )
    });
    let max_width = f32::from(display_size.width).max(MIN_WINDOW_WIDTH);
    let max_height = f32::from(display_size.height).max(MIN_WINDOW_HEIGHT);
    let restored_size = size(
        px(persisted.width.clamp(MIN_WINDOW_WIDTH, max_width)),
        px(persisted.height.clamp(MIN_WINDOW_HEIGHT, max_height)),
    );

    visible_bounds
        .map(|bounds| Bounds::centered_at(bounds.center(), restored_size))
        .unwrap_or_else(|| Bounds::centered(None, restored_size, cx))
}

pub(crate) fn minimum_size() -> Size<Pixels> {
    size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))
}

pub(crate) fn record_windowed_size(window: &Window) {
    // macOS and X11 can expose the current display-sized bounds while maximized,
    // so only record geometry while the window is genuinely windowed.
    if window.is_maximized() || window.is_fullscreen() {
        return;
    }
    let size = window.window_bounds().get_bounds().size;
    let state = PersistedWindowSize {
        width: size.width.into(),
        height: size.height.into(),
    };
    if !is_valid_size(state) {
        log::warn!(
            "window state: refusing to record invalid size {}x{}",
            state.width,
            state.height
        );
        return;
    }
    *last_windowed_size_guard() = Some(state);
}

pub(crate) fn save() {
    let Some(state) = *last_windowed_size_guard() else {
        return;
    };

    let Some(path) = state_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        log::warn!("window state: failed to create directory: {error}");
        return;
    }

    let mut temporary = match tempfile::NamedTempFile::new_in(parent) {
        Ok(file) => file,
        Err(error) => {
            log::warn!("window state: failed to create temporary file: {error}");
            return;
        }
    };
    if let Err(error) = serde_json::to_writer_pretty(&mut temporary, &state)
        .and_then(|_| temporary.write_all(b"\n").map_err(serde_json::Error::io))
    {
        log::warn!("window state: failed to serialize: {error}");
        return;
    }
    if let Err(error) = temporary.as_file().sync_all() {
        log::warn!("window state: failed to sync temporary file: {error}");
        return;
    }
    if let Err(error) = temporary.persist(&path) {
        log::warn!("window state: failed to persist: {error}");
    }
}

fn load() -> Option<PersistedWindowSize> {
    let path = state_path()?;
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!(
                "window state: failed to inspect {}: {error}",
                path.display()
            );
            return None;
        }
    };
    if !metadata.is_file() || metadata.len() > MAX_WINDOW_STATE_BYTES {
        log::warn!("window state: rejected invalid file at {}", path.display());
        return None;
    }

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            log::warn!("window state: failed to read {}: {error}", path.display());
            return None;
        }
    };
    match serde_json::from_str::<PersistedWindowSize>(&contents) {
        Ok(state) if state.width.is_finite() && state.height.is_finite() => Some(state),
        Ok(_) => {
            log::warn!(
                "window state: rejected non-finite size at {}",
                path.display()
            );
            None
        }
        Err(error) => {
            log::warn!("window state: invalid JSON at {}: {error}", path.display());
            None
        }
    }
}

pub(crate) fn state_path() -> Option<PathBuf> {
    // Same APP_SUBDIR as paneflow.json so a debug `cargo run` never
    // overwrites the installed app's window size.
    dirs::config_dir().map(|directory| {
        directory
            .join(paneflow_config::loader::APP_SUBDIR)
            .join("window-state.json")
    })
}

fn is_valid_size(state: PersistedWindowSize) -> bool {
    state.width.is_finite()
        && state.height.is_finite()
        && state.width >= MIN_WINDOW_WIDTH
        && state.height >= MIN_WINDOW_HEIGHT
}

fn last_windowed_size_guard() -> MutexGuard<'static, Option<PersistedWindowSize>> {
    LAST_WINDOWED_SIZE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use paneflow_config::loader::APP_SUBDIR;

    #[test]
    fn state_path_uses_config_app_subdir() {
        let path = state_path().expect("config dir must resolve on macOS");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("window-state.json")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some(APP_SUBDIR),
            "window-state.json parent must be APP_SUBDIR, not a hardcoded paneflow/ sibling of the release file"
        );
        if cfg!(debug_assertions) {
            assert_eq!(APP_SUBDIR, "paneflow-dev");
            assert!(
                path.to_string_lossy().contains("paneflow-dev"),
                "debug window-state must live under paneflow-dev, got {path:?}"
            );
            assert!(
                !path.ends_with("paneflow/window-state.json"),
                "debug window-state must not be a sibling of the release file, got {path:?}"
            );
        }
    }
}
