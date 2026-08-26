//! In-app self-update dispatcher - routes clicks on the update pill to the
//! right installer branch (AppBundle DMG /
//! legacy `.run`) based on the detected
//! [`crate::update::install_method::InstallMethod`].
//!
//! Extracted from `main.rs` per US-028 of the src-app refactor PRD.

use gpui::{ClipboardItem, Context, Window};

use crate::{DismissUpdate, PaneFlowApp, StartSelfUpdate, TOAST_HOLD_MS, ToastAction, update};

/// App-level backstop for a wedged `Downloading` state (EP-002,
/// U-002/U-015). Every installer worker is spawned + detached and the only
/// transitions out of `Downloading` live inside those workers' match arms, so
/// a worker whose future never resolves would pin the pill busy forever. The
/// per-attempt watchdog routes through `record_update_failure` after this
/// deadline. Sized generously so a slow link fetching a full `.dmg` is never
/// mistaken for a hung install.
const DOWNLOAD_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// One-line summary of the install method for log messages - used by the
/// auto-kickoff gate to keep diagnostic noise low when the running binary
/// is not auto-updatable.
fn install_method_label(method: &update::install_method::InstallMethod) -> &'static str {
    match method {
        update::install_method::InstallMethod::AppBundle { .. } => "app-bundle",
        update::install_method::InstallMethod::ExternallyManaged { .. } => "externally-managed",
        update::install_method::InstallMethod::Unknown => "unknown",
    }
}

impl PaneFlowApp {
    /// Current update CTA state, shared by every surface that renders it
    /// (Diff title-bar pill, Cli/Agents sidebar banner). `None` when no
    /// update is available (or the pill was dismissed for this launch).
    ///
    /// Pill state for the in-app installer flow (AppBundle DMG) is shared
    /// with the catch-all so both reflect the live install state machine.
    pub(crate) fn update_pill_info(&self) -> Option<crate::window_chrome::title_bar::UpdateInfo> {
        use crate::window_chrome::title_bar;
        let in_app_state = match &self.self_update.self_update_status {
            update::SelfUpdateStatus::Idle => title_bar::SelfUpdatePillState::Idle,
            update::SelfUpdateStatus::Downloading => title_bar::SelfUpdatePillState::Downloading,
            update::SelfUpdateStatus::ReadyToRestart => {
                title_bar::SelfUpdatePillState::ReadyToRestart
            }
            update::SelfUpdateStatus::Errored(_) => title_bar::SelfUpdatePillState::Errored,
        };
        match &self.self_update.update_status {
            Some(update::checker::UpdateStatus::Available { version, .. }) => {
                let kind = match &self.self_update.install_method {
                    // Flatpak / Snap / `PANEFLOW_UPDATE_EXPLANATION` -
                    // packager owns updates, render the same generic
                    // SystemHint pill. The explanation copy is surfaced
                    // by the click handler below.
                    update::install_method::InstallMethod::ExternallyManaged { .. } => {
                        title_bar::UpdatePillKind::SystemManaged(
                            title_bar::SystemPackageKind::Other,
                        )
                    }
                    _ => title_bar::UpdatePillKind::InApp(in_app_state),
                };
                Some(title_bar::UpdateInfo {
                    version: version.clone(),
                    kind,
                })
            }
            _ => None,
        }
    }

    /// Action entry point. Stays a thin wrapper around
    /// [`PaneFlowApp::kickoff_self_update_install`] so that auto-kickoff
    /// from the polling loop can share the exact same logic without
    /// having to forge a `Window`.
    pub(crate) fn handle_start_self_update(
        &mut self,
        _: &StartSelfUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.kickoff_self_update_install(cx);
    }

    /// Dismiss the update pill for the current launch.
    /// Clears `update_status` so the title-bar pill disappears and forces
    /// a re-render. Intentionally NOT persisted - the next paneflow launch
    /// will re-detect the update and re-show the pill (we don't want a
    /// user accidentally sticking on an old version because the
    /// preference outlived their interest).
    pub(crate) fn handle_dismiss_update(
        &mut self,
        _: &DismissUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.self_update.update_status = None;
        cx.notify();
    }

    /// US-017: shared completion for every pre-installed update path. Flips to
    /// `ReadyToRestart` and persists the session (blocking - the next event is
    /// a process-replacing restart). Dedups the six identical blocks that
    /// previously inlined this.
    fn on_preinstall_success(&mut self, cx: &mut Context<Self>) {
        self.self_update.self_update_status = update::SelfUpdateStatus::ReadyToRestart;
        self.save_session_blocking(cx);
        cx.notify();
    }

    /// Enter the `Downloading` state and arm a one-shot watchdog (EP-002,
    /// U-002/U-015). Replaces the bare `self_update_status = Downloading;
    /// cx.notify();` at every installer dispatch site so no path can sit in
    /// `Downloading` forever: if we are still in THIS attempt and still busy
    /// after [`DOWNLOAD_WATCHDOG`], the failure is routed through
    /// [`PaneFlowApp::record_update_failure`] (which leaves the busy state,
    /// bumps the 3-strikes counter, and surfaces the timeout toast), making the
    /// retry / circuit-breaker paths reachable again.
    fn enter_downloading(&mut self, label: &'static str, cx: &mut Context<Self>) {
        let generation = self.self_update.download_generation.wrapping_add(1);
        self.self_update.download_generation = generation;
        self.self_update.self_update_status = update::SelfUpdateStatus::Downloading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            smol::Timer::after(DOWNLOAD_WATCHDOG).await;
            let _ = this.update(cx, |app, cx| {
                // Fire only if THIS download is still the live one and still
                // busy; a completed / failed / superseded worker already moved
                // the state on, and the generation guard stops a stale watchdog
                // from clobbering a newer attempt.
                if app.self_update.download_generation == generation
                    && app.self_update.self_update_status.is_busy()
                {
                    log::warn!(
                        "self-update/{label}: watchdog fired after {DOWNLOAD_WATCHDOG:?} - \
                         worker wedged in {:?}; resetting via record_update_failure",
                        app.self_update.self_update_status,
                    );
                    app.record_update_failure(
                        label,
                        &anyhow::Error::new(update::UpdateError::Timeout),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Kick off the in-app self-update flow. See the module-level doc for the
    /// branch matrix; on any failure a toast surfaces and the update pill
    /// returns to "Update failed".
    pub(crate) fn kickoff_self_update_install(&mut self, cx: &mut Context<Self>) {
        // Fast path: the new binary is already on disk and
        // `set_restart_path` has been wired ahead of time by the
        // background pre-installer (see `try_auto_kickoff_install`
        // below). The click handler does ZERO I/O - just hand control
        // to GPUI's relauncher script. This is what makes the
        // user-perceived restart latency drop from "vachement long"
        // (download + install + analytics flush) to GPUI's
        // ~100 ms `kill -0` polling interval.
        if matches!(
            self.self_update.self_update_status,
            update::SelfUpdateStatus::ReadyToRestart
        ) {
            log::info!("self-update: ReadyToRestart click - invoking cx.restart()");
            cx.restart();
            return;
        }

        // Externally managed runtime (Flatpak / Snap / packager-baked
        // `PANEFLOW_UPDATE_EXPLANATION`). The in-app updater is disabled
        // by design - surface the packager's explanation copy and copy
        // the upgrade command to the clipboard so the user has a one-click
        // path forward. Mirrors how Zed handles `ZED_UPDATE_EXPLANATION`.
        if let update::install_method::InstallMethod::ExternallyManaged { explanation } =
            &self.self_update.install_method
        {
            cx.write_to_clipboard(ClipboardItem::new_string(explanation.clone()));
            self.push_toast(explanation.clone(), Vec::new(), TOAST_HOLD_MS * 4, cx);
            return;
        }

        if self.self_update.self_update_status.is_busy() {
            return;
        }

        // After 3 consecutive failures, the 4th click stops re-trying and
        // points the user at the releases page (US-013). Skipping the
        // network here is important - repeated fast retries against a
        // flaky mirror are never the right answer.
        if self.self_update.update_attempt_count >= 3 {
            let releases_url = match &self.self_update.update_status {
                Some(update::checker::UpdateStatus::Available { url, .. }) if !url.is_empty() => {
                    Some(url.clone())
                }
                _ => None,
            };
            let actions = match releases_url {
                Some(url) => vec![ToastAction::OpenReleasesPage(url)],
                None => Vec::new(),
            };
            self.push_toast(
                "Update keeps failing. Self-update is not hosted for this build.".to_string(),
                actions,
                TOAST_HOLD_MS * 4,
                cx,
            );
            return;
        }

        let asset_url = match &self.self_update.update_status {
            Some(update::checker::UpdateStatus::Available {
                asset_url: Some(url),
                ..
            }) => url.clone(),
            Some(update::checker::UpdateStatus::Available { url, .. }) => {
                // No Linux asset on this release (edge case: draft, mis-tagged).
                // Fall back to opening the release page so the user can grab it.
                if let Err(err) = crate::external_open::open_http_url(url) {
                    log::warn!("self-update: open release page failed: {err}");
                }
                return;
            }
            _ => return,
        };

        // No trust anchor baked into this build (a dev build, or a release cut
        // before the US-002 signing keys were provisioned). Refuse to start ANY
        // installer before touching disk. `fetch_and_verify` already fails
        // closed on a keyless build (signature.rs), but for the bundle that
        // rejection only fires *after* `appimageupdatetool -O` has rewritten the
        // live binary in place - mutating a binary we can never verify. Bailing
        // here keeps every install path verify-before-side-effect and shows a
        // clear message instead of a silently corrupted bundle.
        if !update::signature::has_embedded_key() {
            self.push_toast(
                "This build can't self-update (unsigned). There is no hosted releases page yet."
                    .to_string(),
                Vec::new(),
                TOAST_HOLD_MS * 4,
                cx,
            );
            return;
        }

        // Use the cached install method. The install location never changes
        // at runtime, so one probe at startup is enough.
        let method = self.self_update.install_method.clone();

        // US-009: macOS `.app` bundle - mount the DMG, swap the bundle
        // atomically, then restart through the promoted `.app` path.
        // Dispatch is an `if let` (not a cfg guard) so the code remains
        // a single compile-closure across all targets; the
        // `InstallMethod::AppBundle` variant is only produced on macOS
        // by `install_method::detect()`, so on Linux / Windows this
        // branch is runtime-dead without needing a `#[cfg(target_os)]`.
        if let update::install_method::InstallMethod::AppBundle { bundle_path } = &method {
            let url = asset_url.clone();
            // US-004: replace the bundle at its detected location, not a
            // hardcoded /Applications path.
            let bundle = bundle_path.clone();
            // EP-002 AC2: `dmg::install` runs `hdiutil attach/detach` + `cp` on
            // the ALREADY-downloaded local `.dmg` (network fetch separately
            // bounded by the 15-minute DMG body timeout, matching this
            // watchdog). Killing a mounted-volume operation mid-flight risks
            // leaking a mount / corrupting the swap, so these local tools are
            // NOT wrapped in `run_with_timeout`; the worker watchdog armed
            // below bounds a wedged install.
            self.enter_downloading("dmg", cx);

            cx.spawn(async move |this, cx| {
                let result =
                    smol::unblock(move || update::macos::dmg::install(&url, &bundle)).await;
                match result {
                    Ok(restart_path) => {
                        let _ = this.update(cx, |app, cx| {
                            app.on_preinstall_success(cx);
                        });
                        cx.update(|cx| {
                            log::info!(
                                "self-update/dmg: pre-installed - restart pending at {}",
                                restart_path.display()
                            );
                            cx.set_restart_path(restart_path);
                        });
                    }
                    Err(err) => {
                        let _ = this.update(cx, |app, cx| {
                            app.record_update_failure("dmg", &err, cx);
                        });
                    }
                }
            })
            .detach();
            return;
        }

        // All supported install methods must have returned above. Do not fall
        // back to the retired `.run` executor: current releases do not ship it,
        // and keeping a downloader plus chmod plus exec path around would be an
        // avoidable update-chain risk.
        let msg = anyhow::anyhow!(
            "Self-update dispatch did not handle install method {:?}. Download the new release manually from {asset_url}",
            method
        );
        self.record_update_failure("unsupported-dispatch", &msg, cx);
    }

    /// Best-effort background pre-install. Called once per polling cycle
    /// after `update_status` transitions to `Available`. By the time
    /// the user actually clicks the pill, the new binary is already on
    /// disk and `set_restart_path` is wired - `cx.restart()` is the
    /// only thing left to do, dropping click→restart latency from
    /// download-time + 2 s analytics flush to GPUI's `kill -0` watcher
    /// interval (~100 ms). Mirrors Zed's silent auto-update worker
    /// (`crates/auto_update/src/auto_update.rs::poll`).
    ///
    /// Gating, in order:
    /// - `update_status` is `Available`.
    /// - `self_update_status` is `Idle` - never re-kick a flow that's
    ///   already downloading, installed, or errored.
    /// - `update_attempt_count < 3` - reuse the 3-strikes circuit
    ///   breaker so a flaky mirror doesn't burn user bandwidth every
    ///   poll cycle.
    /// - `install_method` is auto-installable without exiting the app, which
    ///   means `AppBundle` and nothing else. `Unknown` is a non-bundle launch
    ///   and is intentionally not auto-kicked; `ExternallyManaged` defers to
    ///   whichever packager claimed the install.
    pub(crate) fn try_auto_kickoff_install(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            self.self_update.update_status,
            Some(update::checker::UpdateStatus::Available { .. })
        ) {
            return;
        }
        if !matches!(
            self.self_update.self_update_status,
            update::SelfUpdateStatus::Idle
        ) {
            return;
        }
        if self.self_update.update_attempt_count >= 3 {
            return;
        }
        let auto_eligible = matches!(
            self.self_update.install_method,
            update::install_method::InstallMethod::AppBundle { .. }
        );
        if !auto_eligible {
            log::debug!(
                "self-update/auto-kickoff: skipped (install_method={})",
                install_method_label(&self.self_update.install_method)
            );
            return;
        }

        log::info!(
            "self-update/auto-kickoff: starting background pre-install (install_method={})",
            install_method_label(&self.self_update.install_method)
        );
        self.kickoff_self_update_install(cx);
    }
}
