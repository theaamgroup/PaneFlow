//! Silent Sparkle 2 bootstrap for signed `.app` bundles.
//!
//! Cargo-built binaries outside an application bundle deliberately skip this:
//! only `scripts/bundle-macos.sh` installs `Sparkle.framework` and stamps the
//! feed/signing configuration into `Info.plist`. The framework is loaded at
//! runtime so ordinary `cargo build` and `cargo test` stay self-contained.

use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

thread_local! {
    // Sparkle is main-thread-only; never send its objects to a worker.
    static UPDATER: Cell<*mut Object> = const { Cell::new(std::ptr::null_mut()) };
}

static NOTICE: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

pub(crate) fn take_notice() -> Option<String> {
    NOTICE.lock().take()
}

static STATUS: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());

pub(crate) fn status() -> String {
    let status = STATUS.lock();
    if status.is_empty() {
        "Updates available in the installed app".into()
    } else {
        status.clone()
    }
}

fn set_status(message: impl Into<String>) {
    let message = message.into();
    log::info!("Sparkle: {message}");
    let mut status = STATUS.lock();
    if *status != message
        && (message.contains("downloaded")
            || message.contains("failed")
            || message.contains("unavailable")
            || message.contains("could not start"))
    {
        *NOTICE.lock() = Some(message.clone());
    }
    *status = message;
}

/// User-initiated checks use Sparkle's standard UI, including retry/error UI.
/// Background checks continue to download silently and never force a restart.
pub(crate) fn check_for_updates() -> Result<(), String> {
    UPDATER.with(|slot| {
        let updater = slot.get();
        if updater.is_null() {
            return Err(status());
        }
        // SAFETY: the retained updater is accessed on AppKit's main thread.
        unsafe {
            let allowed: BOOL = msg_send![updater, canCheckForUpdates];
            if allowed != YES {
                return Err(format!(
                    "An update check is already in progress. {}",
                    status()
                ));
            }
            set_status("Checking for updates…");
            let _: () = msg_send![updater, checkForUpdates];
        }
        Ok(())
    })
}

use objc::declare::ClassDecl;
use objc::runtime::{BOOL, Class, NO, Object, Protocol, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};

static START: Once = Once::new();

/// Start Sparkle exactly once when running from a packaged PaneFlow.app.
///
/// The controller, delegate, and `dlopen` handle intentionally live until
/// process exit. Sparkle weakly references both delegates and owns background
/// update scheduling for the application's full lifetime.
pub(crate) fn start_if_bundled() {
    START.call_once(|| {
        if matches!(
            std::env::var("PANEFLOW_DISABLE_SPARKLE").ok().as_deref(),
            Some("1")
        ) {
            log::info!("Sparkle updater disabled by PANEFLOW_DISABLE_SPARKLE=1");
            set_status("Automatic updates disabled by PANEFLOW_DISABLE_SPARKLE");
            return;
        }

        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => {
                log::warn!("Sparkle updater: cannot resolve current executable: {error}");
                set_status(format!("Updates unavailable: {error}"));
                return;
            }
        };
        let Some(framework) = bundled_framework_binary(&executable) else {
            log::debug!("Sparkle updater: not running from a PaneFlow.app bundle");
            return;
        };
        if !framework.is_file() {
            set_status("Updates unavailable: Sparkle framework is missing. Reinstall PaneFlow.");
            log::warn!(
                "Sparkle updater: packaged framework is missing at {}",
                framework.display()
            );
            return;
        }

        // SAFETY: called from PaneFlowApp::new on GPUI's AppKit main thread.
        // The framework path is inside the current signed bundle. The loaded
        // Objective-C objects are deliberately leaked for process lifetime so
        // Sparkle's weak delegate references and scheduler remain valid.
        unsafe {
            let Some(_framework_handle) = load_framework(&framework) else {
                set_status("Updates unavailable: Sparkle could not load. Reinstall PaneFlow.");
                return;
            };
            let Some(controller_class) = Class::get("SPUStandardUpdaterController") else {
                set_status("Updates unavailable: Sparkle controller is missing");
                log::error!("Sparkle updater: SPUStandardUpdaterController class is unavailable");
                return;
            };
            let delegate_class = sparkle_delegate_class();
            let delegate: *mut Object = msg_send![delegate_class, new];
            if delegate.is_null() {
                set_status("Updates unavailable: could not initialize Sparkle delegate");
                log::error!("Sparkle updater: failed to allocate delegate");
                return;
            }

            let controller: *mut Object = msg_send![controller_class, alloc];
            let controller: *mut Object = msg_send![controller,
                initWithStartingUpdater: NO
                updaterDelegate: delegate
                userDriverDelegate: delegate
            ];
            if controller.is_null() {
                set_status("Updates unavailable: could not initialize Sparkle controller");
                log::error!("Sparkle updater: failed to create updater controller");
                return;
            }

            let updater: *mut Object = msg_send![controller, updater];
            let mut error: *mut Object = std::ptr::null_mut();
            let started: BOOL = msg_send![updater, startUpdater: &mut error];
            if started != YES {
                set_status(format!(
                    "Updates could not start: {}",
                    error_description(error)
                ));
                return;
            }
            UPDATER.with(|slot| slot.set(updater));
            let automatic: BOOL = msg_send![updater, automaticallyChecksForUpdates];
            set_status(if automatic == YES {
                "Automatic updates enabled · installs when you quit"
            } else {
                "Automatic checks are disabled · use Check for Updates"
            });
            // Run the initial check now rather than waiting another hour in
            // a long-lived app. Sparkle owns all subsequent retry scheduling.
            if automatic == YES {
                let _: () = msg_send![updater, checkForUpdatesInBackground];
            }
        }
    });
}

/// Where the packaged Sparkle framework binary sits for `executable`, or
/// `None` when the executable is not inside a `.app` bundle at all. Shared
/// with `crate::system_info`, so the install format the System Info report
/// prints cannot disagree with what the updater bootstrap actually checks.
pub(crate) fn bundled_framework_binary(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.extension()? != "app" {
        return None;
    }
    Some(
        contents
            .join("Frameworks")
            .join("Sparkle.framework")
            .join("Sparkle"),
    )
}

unsafe fn load_framework(path: &Path) -> Option<*mut libc::c_void> {
    let path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(_) => {
            log::error!("Sparkle updater: framework path contains a NUL byte");
            return None;
        }
    };
    // SAFETY: `path` is a live NUL-terminated string and flags are supported
    // by macOS dyld. The returned handle is retained for process lifetime.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        // SAFETY: dlerror returns either null or a process-owned C string that
        // remains valid until the next dynamic-loader call on this thread.
        let message = unsafe {
            let error = libc::dlerror();
            if error.is_null() {
                "unknown dynamic-loader error".to_string()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            }
        };
        log::error!("Sparkle updater: failed to load framework: {message}");
        None
    } else {
        Some(handle)
    }
}

unsafe fn sparkle_delegate_class() -> &'static Class {
    if let Some(class) = Class::get("PaneFlowSparkleDelegate") {
        return class;
    }

    let mut declaration = ClassDecl::new("PaneFlowSparkleDelegate", class!(NSObject))
        .expect("PaneFlowSparkleDelegate is registered only once");
    if let Some(protocol) = Protocol::get("SPUUpdaterDelegate") {
        declaration.add_protocol(protocol);
    }
    if let Some(protocol) = Protocol::get("SPUStandardUserDriverDelegate") {
        declaration.add_protocol(protocol);
    }
    // SAFETY: selectors and ABIs match Sparkle 2.9's public Objective-C
    // protocols. Each callback ignores borrowed objects/blocks and completes
    // synchronously, so no Rust reference escapes the call.
    unsafe {
        declaration.add_method(
            sel!(updater:didAbortWithError:),
            update_failed as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
        );
        declaration.add_method(
            sel!(updaterDidNotFindUpdate:),
            no_update as extern "C" fn(&Object, Sel, *mut Object),
        );
        declaration.add_method(
            sel!(updater:didFindValidUpdate:),
            update_found as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
        );
        declaration.add_method(
            sel!(updater:willInstallUpdateOnQuit:immediateInstallationBlock:),
            hold_update_until_quit
                as extern "C" fn(&Object, Sel, *mut Object, *mut Object, *mut Object) -> BOOL,
        );
        declaration.add_method(
            sel!(updaterShouldRelaunchApplication:),
            prevent_relaunch as extern "C" fn(&Object, Sel, *mut Object) -> BOOL,
        );
        declaration.add_method(
            sel!(supportsGentleScheduledUpdateReminders),
            handles_scheduled_reminders as extern "C" fn(&Object, Sel) -> BOOL,
        );
        declaration.add_method(
            sel!(standardUserDriverShouldHandleShowingScheduledUpdate:andInImmediateFocus:),
            suppress_scheduled_update_ui as extern "C" fn(&Object, Sel, *mut Object, BOOL) -> BOOL,
        );
        declaration.add_method(
            sel!(standardUserDriverWillHandleShowingUpdate:forUpdate:state:),
            suppress_scheduled_update_notification
                as extern "C" fn(&Object, Sel, BOOL, *mut Object, *mut Object),
        );
    }
    declaration.register()
}

extern "C" fn hold_update_until_quit(
    _this: &Object,
    _command: Sel,
    _updater: *mut Object,
    item: *mut Object,
    _immediate_installation: *mut Object,
) -> BOOL {
    set_status(format!(
        "Update {} downloaded · quit PaneFlow to install",
        item_version(item)
    ));
    // NO preserves future update cycles. YES transfers ownership of the
    // installation handler and stalls checks until that handler is invoked.
    NO
}

fn item_version(item: *mut Object) -> String {
    if item.is_null() {
        return String::new();
    }
    // SAFETY: Sparkle lends a live SUAppcastItem for the callback.
    unsafe {
        let value: *mut Object = msg_send![item, displayVersionString];
        ns_string(value)
    }
}

unsafe fn ns_string(value: *mut Object) -> String {
    if value.is_null() {
        return String::new();
    }
    // SAFETY: callers supply a live NSString; copy before the callback ends.
    unsafe {
        let utf8: *const libc::c_char = msg_send![value, UTF8String];
        if utf8.is_null() {
            String::new()
        } else {
            CStr::from_ptr(utf8)
                .to_string_lossy()
                .chars()
                .take(512)
                .collect()
        }
    }
}

unsafe fn error_description(error: *mut Object) -> String {
    if error.is_null() {
        return "unknown Sparkle error".into();
    }
    // SAFETY: Sparkle supplies a live NSError.
    unsafe {
        let value: *mut Object = msg_send![error, localizedDescription];
        ns_string(value)
    }
}

extern "C" fn update_failed(_: &Object, _: Sel, _: *mut Object, error: *mut Object) {
    if !error.is_null() {
        // SAFETY: Sparkle lends an NSError. No-update is a normal outcome,
        // even though Sparkle also reports it through its abort callback.
        let (domain, code) = unsafe {
            let domain: *mut Object = msg_send![error, domain];
            let code: isize = msg_send![error, code];
            (ns_string(domain), code)
        };
        if domain == "SUSparkleErrorDomain" && code == 1001 {
            set_status("PaneFlow is up to date");
            return;
        }
        if domain == "SUSparkleErrorDomain" && code == 4007 {
            set_status("Update installation canceled · use Check for Updates to retry");
            return;
        }
    }
    // SAFETY: the callback parameter is a borrowed NSError.
    set_status(format!(
        "Update check failed: {} · use Check for Updates to retry",
        unsafe { error_description(error) }
    ));
}

extern "C" fn no_update(_: &Object, _: Sel, _: *mut Object) {
    set_status("PaneFlow is up to date");
}

extern "C" fn update_found(_: &Object, _: Sel, _: *mut Object, item: *mut Object) {
    set_status(format!("Update {} available", item_version(item)));
}

extern "C" fn prevent_relaunch(_this: &Object, _command: Sel, _updater: *mut Object) -> BOOL {
    NO
}

extern "C" fn handles_scheduled_reminders(_this: &Object, _command: Sel) -> BOOL {
    YES
}

extern "C" fn suppress_scheduled_update_ui(
    _this: &Object,
    _command: Sel,
    _update: *mut Object,
    _immediate_focus: BOOL,
) -> BOOL {
    NO
}

extern "C" fn suppress_scheduled_update_notification(
    _this: &Object,
    _command: Sel,
    _standard_driver_handles_update: BOOL,
    update: *mut Object,
    _state: *mut Object,
) {
    set_status(format!(
        "Update {} available · choose Check for Updates",
        item_version(update)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn resolves_framework_only_inside_an_app_bundle() {
        assert_eq!(
            bundled_framework_binary(Path::new(
                "/Applications/PaneFlow.app/Contents/MacOS/paneflow"
            )),
            Some(PathBuf::from(
                "/Applications/PaneFlow.app/Contents/Frameworks/Sparkle.framework/Sparkle"
            ))
        );
        assert_eq!(
            bundled_framework_binary(Path::new("/repo/target/debug/paneflow")),
            None
        );
        assert_eq!(
            bundled_framework_binary(Path::new("/tmp/not-an-app/Contents/MacOS/paneflow")),
            None
        );
    }

    #[test]
    fn delegate_policy_never_installs_immediately_or_relaunches() {
        let object = class!(NSObject);
        // The callbacks do not inspect any argument; a Class is also an
        // Objective-C object and is sufficient to exercise their policy.
        let object = unsafe { &*(object as *const Class as *const Object) };
        assert_eq!(
            hold_update_until_quit(
                object,
                sel!(description),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            NO
        );
        assert_eq!(
            prevent_relaunch(object, sel!(description), ptr::null_mut()),
            NO
        );
        assert_eq!(handles_scheduled_reminders(object, sel!(description)), YES);
        assert_eq!(
            suppress_scheduled_update_ui(object, sel!(description), ptr::null_mut(), YES,),
            NO
        );
    }

    #[test]
    fn sparkle_dist_curl_has_timeouts() {
        let src = include_str!("../../scripts/sparkle-dist.sh");
        let curl = src
            .lines()
            .find(|line| line.contains("curl ") && line.contains("$SPARKLE_URL"))
            .expect("sparkle-dist.sh must curl SPARKLE_URL");
        assert!(
            curl.contains("--connect-timeout") && curl.contains("--max-time"),
            "Sparkle fetch must fail a hung download instead of blocking packaging: {curl}"
        );
    }
}
