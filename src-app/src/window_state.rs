//! Durable main-window sizing across PaneFlow launches.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
    load_from_path(&path)
}

fn load_from_path(path: &Path) -> Option<PersistedWindowSize> {
    // `open(O_RDONLY)` on a FIFO with no writer blocks forever, so refuse
    // anything that is not a regular file before the open. The size cap and
    // the type check are applied again on the opened descriptor below, which
    // is what closes the stat-to-read swap window (issue #258).
    match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            log::warn!("window state: rejected invalid file at {}", path.display());
            return None;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!(
                "window state: failed to inspect {}: {error}",
                path.display()
            );
            return None;
        }
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!("window state: failed to open {}: {error}", path.display());
            return None;
        }
    };
    let contents = read_capped(file, path)?;
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

/// Read at most `MAX_WINDOW_STATE_BYTES` from an already-open descriptor. The
/// type and size checks use the descriptor's own metadata, and the read is
/// bounded by `take`, so a file replaced or grown after the path was inspected
/// can neither be read unbounded nor block on a FIFO.
fn read_capped(file: std::fs::File, path: &Path) -> Option<String> {
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
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

    let mut contents = String::new();
    match file
        .take(MAX_WINDOW_STATE_BYTES + 1)
        .read_to_string(&mut contents)
    {
        Ok(_) if contents.len() as u64 <= MAX_WINDOW_STATE_BYTES => Some(contents),
        Ok(_) => {
            log::warn!("window state: rejected invalid file at {}", path.display());
            None
        }
        Err(error) => {
            log::warn!("window state: failed to read {}: {error}", path.display());
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

    fn padded_state_json(total_len: usize) -> String {
        let mut json = String::from("{\"width\": 1024.0, \"height\": 640.0}");
        while json.len() < total_len {
            json.push(' ');
        }
        json
    }

    #[test]
    fn load_from_path_accepts_regular_file_under_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("window-state.json");
        std::fs::write(&path, padded_state_json(0)).expect("write");
        let state = load_from_path(&path).expect("small regular file must load");
        assert_eq!(state.width, 1024.0);
        assert_eq!(state.height, 640.0);
    }

    #[test]
    fn load_from_path_rejects_regular_file_over_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("window-state.json");
        let oversize = usize::try_from(MAX_WINDOW_STATE_BYTES).expect("cap fits usize") + 1;
        std::fs::write(&path, padded_state_json(oversize)).expect("write");
        assert!(
            load_from_path(&path).is_none(),
            "a file over MAX_WINDOW_STATE_BYTES must be rejected"
        );
    }

    #[test]
    fn read_capped_rejects_file_that_grew_after_the_path_was_inspected() {
        // Issue #258: the cap must be enforced on the opened descriptor, not
        // on an earlier stat of the path. Open a small file, then grow it past
        // the cap before reading, which is what a replace-between-stat-and-read
        // looks like to the reader.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("window-state.json");
        std::fs::write(&path, padded_state_json(0)).expect("write");
        let file = std::fs::File::open(&path).expect("open");
        let oversize = usize::try_from(MAX_WINDOW_STATE_BYTES).expect("cap fits usize") + 1;
        std::fs::write(&path, padded_state_json(oversize)).expect("grow");
        assert!(
            read_capped(file, &path).is_none(),
            "an open descriptor whose file grew past the cap must be rejected"
        );
    }

    #[test]
    fn read_capped_accepts_file_exactly_at_cap() {
        // Boundary: `take(cap + 1)` must not turn a file of exactly `cap`
        // bytes into a rejection.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("window-state.json");
        let at_cap = usize::try_from(MAX_WINDOW_STATE_BYTES).expect("cap fits usize");
        std::fs::write(&path, padded_state_json(at_cap)).expect("write");
        let file = std::fs::File::open(&path).expect("open");
        let contents = read_capped(file, &path).expect("a file exactly at the cap must load");
        assert_eq!(contents.len(), at_cap);
    }
}
