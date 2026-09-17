//! Startup trace probe (#519, upstream `df375ba5` part 3).
//!
//! When `PANEFLOW_STARTUP_TRACE` names a file, `main`, `mount_paneflow_app`,
//! `PaneFlowApp::new`, and `PaneFlowApp::render` record a mark at each stage
//! of the launch, and the app writes the timeline as JSON and quits once the
//! frame it was launched to measure has been presented:
//!
//! - with no session to restore (a fresh launch, or an empty `session.json`),
//!   that is the first presented frame, which already shows the default
//!   workspace and its shell;
//! - with a saved session, the fork restores workspaces one batch per frame
//!   after the first frame (issue #156), so the trace runs on until the frame
//!   after the last batch (`restored_frame`), which is the first frame that
//!   shows every restored pane.
//!
//! The probe ships in release builds so the shipping profile is what gets
//! measured; without the variable every entry point is one `OnceLock` read.
//! `startup_bench.rs` launches the release binary with it set and turns the
//! marks into metrics. Unset, the probe records nothing and never quits.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use gpui::{App, Window};

pub(crate) const OUTPUT_PATH_ENV: &str = "PANEFLOW_STARTUP_TRACE";
const SCHEMA_VERSION: u32 = 1;

static ORIGIN: OnceLock<Instant> = OnceLock::new();
static MARKS: Mutex<Vec<Mark>> = Mutex::new(Vec::new());
static FIRST_RENDER_SEEN: AtomicBool = AtomicBool::new(false);
static FIRST_RENDER_BUILT: AtomicBool = AtomicBool::new(false);
static FIRST_FRAME_SEEN: AtomicBool = AtomicBool::new(false);
static RESTORE_PENDING: AtomicBool = AtomicBool::new(false);
static RESTORED_FRAME_SEEN: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) name: &'static str,
    pub(crate) at_us: u64,
}

/// Fix the origin every mark is measured from. The first line of `main`.
pub(crate) fn begin() {
    ORIGIN.get_or_init(Instant::now);
}

/// Record `name` at the current offset from the origin, when tracing.
pub(crate) fn mark(name: &'static str) {
    if output_path().is_some() {
        record(name);
    }
}

fn record(name: &'static str) {
    let Some(origin) = ORIGIN.get() else {
        return;
    };
    let at_us = origin.elapsed().as_micros().min(u64::MAX as u128) as u64;
    log::debug!("startup trace: {name} at {at_us} us");
    MARKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Mark { name, at_us });
}

/// The trace file named by `PANEFLOW_STARTUP_TRACE`, read once.
pub(crate) fn output_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os(OUTPUT_PATH_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
    .as_ref()
}

/// `PaneFlowApp::new` found a session to restore in stages: the trace must
/// outlive the first frame and end at the frame after the last batch.
pub(crate) fn expect_session_restore() {
    if output_path().is_some() {
        RESTORE_PENDING.store(true, Ordering::SeqCst);
        mark("session_restore_scheduled");
    }
}

/// Top of `PaneFlowApp::render`: marks the first render, starts the frame
/// pump, and arms the first-frame callback that may end the trace.
pub(crate) fn on_app_render(window: &mut Window, cx: &mut App) {
    if output_path().is_none() || FIRST_RENDER_SEEN.swap(true, Ordering::SeqCst) {
        return;
    }
    mark("first_render");
    window.on_next_frame(|_, cx| {
        mark("first_frame");
        FIRST_FRAME_SEEN.store(true, Ordering::SeqCst);
        try_finish(cx);
    });
    pump_frames(window, cx);
}

/// How often the pump asks for a frame while tracing.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

/// Keep frames coming until the trace is written.
///
/// GPUI requests frames from a per-window display link, and on macOS that
/// link only starts once the window is key or its occlusion state changes.
/// A benchmark launches the app from a process that is not the active
/// application, and macOS refuses the activation, so the window gets the one
/// frame AppKit's initial layer display requests and never another: a staged
/// restore, which runs one batch per frame (issue #156), would stall on its
/// first batch. Sending `displayLayer:` to the GPUI view reaches the path
/// that first frame came through, which runs the pending next-frame
/// callbacks, draws, and presents with a transaction, so AppKit paces it at
/// the display refresh. A foreground task sends it every [`PUMP_INTERVAL`]
/// from the run loop, outside any GPUI update (sending it from inside one
/// would re-enter the app), until the trace has been written. Without
/// `PANEFLOW_STARTUP_TRACE` none of this runs.
fn pump_frames(window: &Window, cx: &mut App) {
    use cocoa::base::{id, nil};
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let native_view = handle.ns_view.as_ptr() as usize;
    cx.spawn(async move |cx| {
        while !FINISHED.load(Ordering::SeqCst) {
            cx.background_executor().timer(PUMP_INTERVAL).await;
            if FINISHED.load(Ordering::SeqCst) {
                break;
            }
            let native_view = native_view as id;
            // SAFETY: this task runs on AppKit's main thread through GPUI's
            // foreground executor, the view outlives the window the trace
            // quits with, and `layer` / `displayLayer:` are plain AppKit
            // messages the view already answers for its own display cycle.
            unsafe {
                let layer: id = msg_send![native_view, layer];
                if layer != nil {
                    let _: () = msg_send![native_view, displayLayer: layer];
                }
            }
        }
    })
    .detach();
}

/// Bottom of `PaneFlowApp::render`: the element tree exists, layout and
/// paint have not run yet.
pub(crate) fn on_app_render_built() {
    if output_path().is_none() || FIRST_RENDER_BUILT.swap(true, Ordering::SeqCst) {
        return;
    }
    mark("first_render_built");
}

/// `finish_session_restore` ran: every workspace exists, the next frame is
/// the first one that shows them all.
pub(crate) fn on_session_restored(window: &mut Window) {
    if output_path().is_none() || RESTORED_FRAME_SEEN.load(Ordering::SeqCst) {
        return;
    }
    mark("session_restored");
    window.on_next_frame(|_, cx| {
        if RESTORED_FRAME_SEEN.swap(true, Ordering::SeqCst) {
            return;
        }
        mark("restored_frame");
        try_finish(cx);
    });
}

/// Write the timeline and quit once every frame the launch was measuring has
/// been presented. Both frame callbacks call this; whichever runs last wins.
fn try_finish(cx: &mut App) {
    if !trace_complete(
        FIRST_FRAME_SEEN.load(Ordering::SeqCst),
        RESTORE_PENDING.load(Ordering::SeqCst),
        RESTORED_FRAME_SEEN.load(Ordering::SeqCst),
    ) || FINISHED.swap(true, Ordering::SeqCst)
    {
        return;
    }
    if let Some(path) = output_path() {
        let marks = MARKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Err(error) = std::fs::write(path, report_json(&marks)) {
            log::error!("startup trace: failed to write {}: {error}", path.display());
        }
    }
    cx.quit();
}

/// Pure form of the end condition: the first frame is always required, and a
/// staged restore additionally requires the frame after its last batch.
fn trace_complete(first_frame: bool, restore_pending: bool, restored_frame: bool) -> bool {
    first_frame && (!restore_pending || restored_frame)
}

pub(crate) fn report_json(marks: &[Mark]) -> String {
    let mut previous_us = 0;
    let steps: Vec<serde_json::Value> = marks
        .iter()
        .map(|mark| {
            let step_us = mark.at_us.saturating_sub(previous_us);
            previous_us = mark.at_us;
            serde_json::json!({
                "name": mark.name,
                "at_us": mark.at_us,
                "step_us": step_us,
            })
        })
        .collect();
    let total_us = marks.last().map(|mark| mark.at_us).unwrap_or(0);
    let document = serde_json::json!({
        "schema": SCHEMA_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "os": std::env::consts::OS,
        "total_us": total_us,
        "marks": steps,
    });
    serde_json::to_string_pretty(&document).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Mark, begin, record, report_json, trace_complete};

    #[test]
    fn marks_are_monotonic_from_the_origin() {
        begin();
        record("first");
        std::thread::sleep(std::time::Duration::from_millis(2));
        record("second");
        let marks = super::MARKS.lock().unwrap().clone();
        let first = marks.iter().position(|m| m.name == "first").unwrap();
        let second = marks.iter().position(|m| m.name == "second").unwrap();
        assert!(first < second);
        assert!(marks[second].at_us >= marks[first].at_us + 2_000);
    }

    #[test]
    fn report_carries_absolute_and_step_durations() {
        let marks = [
            Mark {
                name: "login_shell_env_loaded",
                at_us: 1_500,
            },
            Mark {
                name: "first_frame",
                at_us: 9_000,
            },
        ];
        let document: serde_json::Value = serde_json::from_str(&report_json(&marks)).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["total_us"], 9_000);
        assert_eq!(document["marks"][0]["step_us"], 1_500);
        assert_eq!(document["marks"][1]["name"], "first_frame");
        assert_eq!(document["marks"][1]["step_us"], 7_500);
        assert!(document["profile"].is_string());
        assert_eq!(document["os"], "macos");
    }

    #[test]
    fn empty_report_is_still_valid_json() {
        let document: serde_json::Value = serde_json::from_str(&report_json(&[])).unwrap();
        assert_eq!(document["total_us"], 0);
        assert_eq!(document["marks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_fresh_launch_ends_at_the_first_frame_and_a_restore_waits_for_its_frame() {
        assert!(!trace_complete(false, false, false));
        assert!(trace_complete(true, false, false));
        assert!(!trace_complete(true, true, false));
        assert!(trace_complete(true, true, true));
        // The restored frame alone is not enough: the first frame must have
        // been presented too, whichever order the callbacks ran in.
        assert!(!trace_complete(false, true, true));
    }

    #[test]
    fn the_probe_is_inert_without_the_environment_variable() {
        // The test binary never sets PANEFLOW_STARTUP_TRACE, so the path is
        // absent and every entry point short-circuits before recording.
        assert!(super::output_path().is_none());
        super::mark("never_recorded");
        super::expect_session_restore();
        assert!(
            !super::MARKS
                .lock()
                .unwrap()
                .iter()
                .any(|mark| mark.name == "never_recorded")
        );
        assert!(!super::RESTORE_PENDING.load(std::sync::atomic::Ordering::SeqCst));
    }
}
