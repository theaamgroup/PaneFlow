//! Silent Sparkle 2 bootstrap for signed `.app` bundles.
//!
//! Cargo-built binaries outside an application bundle deliberately skip this:
//! only `scripts/bundle-macos.sh` installs `Sparkle.framework` and stamps the
//! feed/signing configuration into `Info.plist`. The framework is loaded at
//! runtime so ordinary `cargo build` and `cargo test` stay self-contained.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

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
            return;
        }

        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => {
                log::warn!("Sparkle updater: cannot resolve current executable: {error}");
                return;
            }
        };
        let Some(framework) = bundled_framework_binary(&executable) else {
            log::debug!("Sparkle updater: not running from a PaneFlow.app bundle");
            return;
        };
        if !framework.is_file() {
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
                return;
            };
            let Some(controller_class) = Class::get("SPUStandardUpdaterController") else {
                log::error!("Sparkle updater: SPUStandardUpdaterController class is unavailable");
                return;
            };
            let delegate_class = sparkle_delegate_class();
            let delegate: *mut Object = msg_send![delegate_class, new];
            if delegate.is_null() {
                log::error!("Sparkle updater: failed to allocate delegate");
                return;
            }

            let controller: *mut Object = msg_send![controller_class, alloc];
            let controller: *mut Object = msg_send![controller,
                initWithStartingUpdater: YES
                updaterDelegate: delegate
                userDriverDelegate: delegate
            ];
            if controller.is_null() {
                log::error!("Sparkle updater: failed to create updater controller");
                return;
            }

            log::info!("Sparkle updater started (hourly checks, silent install on quit)");
        }
    });
}

fn bundled_framework_binary(executable: &Path) -> Option<PathBuf> {
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
    _item: *mut Object,
    _immediate_installation: *mut Object,
) -> BOOL {
    // Returning YES takes responsibility for the immediate-install block.
    // Deliberately never invoking that block suppresses impatient reminders;
    // Sparkle still guarantees installation when the app terminates.
    YES
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
    _update: *mut Object,
    _state: *mut Object,
) {
    // PaneFlow deliberately owns gentle scheduled reminders and presents none.
    // The downloaded update remains staged for Sparkle's install-on-quit path.
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
            YES
        );
        assert_eq!(
            prevent_relaunch(object, sel!(description), ptr::null_mut()),
            NO
        );
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
