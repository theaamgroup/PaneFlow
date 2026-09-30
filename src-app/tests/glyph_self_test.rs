//! Issue #1116: `paneflow self-test glyphs` must pass on this build.
//!
//! The test runs the real `paneflow` binary Cargo built for this package, so
//! it carries the same `gpui_platform` features as the release binary. A build
//! that drops the `font-kit` feature gets GPUI's `NoopTextSystem`, every glyph
//! bitmap comes back empty, the self-test exits 1, and this test fails. The
//! render smoke lane runs the same command against the bundled release binary.

use std::process::Command;

#[test]
fn self_test_glyphs_rasterizes_ink_in_this_build() {
    let output = Command::new(env!("CARGO_BIN_EXE_paneflow"))
        .args(["self-test", "glyphs"])
        .env("RUST_LOG", "off")
        .output()
        .expect("spawn paneflow self-test glyphs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "paneflow self-test glyphs exited {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("paneflow self-test glyphs: ok (18 glyphs"),
        "unexpected self-test output:\n{stdout}\nstderr:\n{stderr}"
    );
}
