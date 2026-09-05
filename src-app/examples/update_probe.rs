//! Native Sparkle bootstrap smoke, run inside an isolated .app bundle.
//! Exercises the real framework and main run loop without opening terminals.
//! Build with `cargo build -p paneflow-app --example update_probe`.
#[path = "../src/sparkle.rs"]
#[allow(dead_code)]
mod sparkle;

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};
use std::time::{Duration, Instant};

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

fn main() {
    env_logger::init();
    let install_fixture = std::env::args().any(|arg| arg == "--install-fixture");
    // SAFETY: this executable owns its main thread and AppKit run loop.
    unsafe {
        let _: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        sparkle::start_if_bundled();
        let run_loop: *mut Object = msg_send![class!(NSRunLoop), currentRunLoop];
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            let until: *mut Object =
                msg_send![class!(NSDate), dateWithTimeIntervalSinceNow: 0.05f64];
            let _: () = msg_send![run_loop, runUntilDate: until];
            let status = sparkle::status();
            if install_fixture && status.contains("downloaded") {
                println!("{status}");
                let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![app, terminate: std::ptr::null_mut::<Object>()];
            }
            if status == "PaneFlow is up to date" {
                println!("{status}");
                if install_fixture {
                    std::process::exit(3);
                }
                return;
            }
            if status.contains("failed")
                || status.contains("unavailable")
                || status.contains("could not start")
            {
                eprintln!("{status}");
                std::process::exit(1);
            }
        }
    }
    eprintln!("Update probe timed out: {}", sparkle::status());
    std::process::exit(2);
}
