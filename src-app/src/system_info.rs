//! The report behind Help > System Info… (#184 Phase 4, upstream #37).
//!
//! A bug report's environment section: version, install format, OS, CPU,
//! GPU, renderer, terminal engine. It carries no project path, no repository
//! name and no environment dump. The one thing it takes from the process is
//! `current_exe()`, and only to *classify* the install format (app bundle or
//! bare binary); the path itself is never printed.
//!
//! Collection is split in two so the render thread never blocks:
//!
//! - [`SystemInfoProbe::capture`] runs on the render thread and takes only
//!   what needs `&Window` (the GPU adapter, if GPUI's window reports one).
//! - [`SystemInfoProbe::resolve`] does the blocking probes (`sysctl`, Metal
//!   enumeration, the install-format file check) and must be called from a
//!   background task.
//!
//! This module is pure collection and formatting. The modal that shows the
//! result, and the button that copies it, live in
//! [`crate::app::system_info_dialog`].
//!
//! Ported from upstream v0.10.0 and stripped to macOS: the Linux
//! (`os-release`, `cpuinfo`, compositor) and Windows (registry) probes are
//! gone, and so is the Rosetta "emulated on" note - `src-app/build.rs`
//! refuses every target but `aarch64-apple-darwin`, and an arm64 binary is
//! never translated.

use std::fmt::{self, Display};
use std::path::Path;

use gpui::{GpuSpecs, SharedString, Window};

/// The renderer GPUI drives on this target. GPUI exposes the adapter
/// (`GpuSpecs`) but not the graphics API behind it; on macOS it is Metal.
const RENDERER: &str = "Metal (GPUI)";

/// Placeholder for a probe that came back empty. Kept as one constant so a
/// reader of an issue can tell "we asked and the system did not say" apart
/// from a field we never collect.
const UNKNOWN: &str = "unknown";

/// Install-format labels. `Sparkle available` means the packaged framework is
/// where `sparkle::start_if_bundled` looks for it, so self-update can run.
const INSTALL_APP_BUNDLE: &str = "app bundle (Sparkle available)";
const INSTALL_APP_BUNDLE_NO_SPARKLE: &str = "app bundle (Sparkle framework missing)";
const INSTALL_BARE_BINARY: &str = "bare binary (cargo run / development)";

/// The render-thread half of the collection. Holds the values that cannot be
/// read from a background thread, and nothing that blocks.
pub(crate) struct SystemInfoProbe {
    /// `None` at the pinned GPUI: `gpui_macos`'s window returns no specs, and
    /// [`SystemInfoProbe::resolve`] falls back to Metal enumeration. Kept so
    /// a GPUI that starts reporting the adapter is picked up without a code
    /// change here.
    gpu: Option<GpuSpecs>,
}

/// A finished, formattable report. Plain data: every field is already a
/// string, so [`Display`] is pure and unit-testable without a window, a GPU
/// or a host to probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SystemInfo {
    version: &'static str,
    target_triple: &'static str,
    install: &'static str,
    os: String,
    cpu: String,
    gpu: String,
    renderer: &'static str,
    terminal_engine: String,
}

impl SystemInfoProbe {
    /// Take the render-thread half of the report. Cheap: `Window::gpu_specs`
    /// reads adapter information the renderer already holds.
    pub(crate) fn capture(window: &Window) -> Self {
        Self {
            gpu: window.gpu_specs(),
        }
    }

    /// Finish the report. **Blocking**: reads `sysctl`, enumerates Metal
    /// devices and stats the Sparkle framework. Call from
    /// `cx.background_spawn`, never from the render thread.
    pub(crate) fn resolve(self) -> SystemInfo {
        SystemInfo {
            version: env!("CARGO_PKG_VERSION"),
            target_triple: env!("PANEFLOW_TARGET_TRIPLE"),
            install: install_format(),
            os: os_description(),
            cpu: cpu_description(),
            gpu: gpu_description(self.gpu.as_ref()),
            renderer: RENDERER,
            terminal_engine: terminal_engine_description(),
        }
    }
}

impl SystemInfo {
    /// The report as label / value pairs, in reading order.
    ///
    /// One source of truth for both renderings: the System Info modal lays
    /// these out as a two-column table, and [`Display`] turns the same pairs
    /// into the Markdown bullets the Copy button puts on the clipboard. What
    /// the user reads and what they paste therefore cannot drift.
    pub(crate) fn rows(&self) -> Vec<(&'static str, SharedString)> {
        vec![
            (
                "PaneFlow",
                format!("{} ({})", self.version, self.target_triple).into(),
            ),
            ("Install", self.install.into()),
            ("OS", self.os.clone().into()),
            ("CPU", self.cpu.clone().into()),
            ("GPU", self.gpu.clone().into()),
            ("Renderer", self.renderer.into()),
            ("Terminal engine", self.terminal_engine.clone().into()),
        ]
    }
}

impl Display for SystemInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (label, value)) in self.rows().into_iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            write!(formatter, "- **{label}**: {value}")?;
        }
        Ok(())
    }
}

// ── Install format ───────────────────────────────────────────────────────

/// How this binary was installed, judged from where it runs.
fn install_format() -> &'static str {
    match std::env::current_exe() {
        Ok(executable) => install_format_for(&executable),
        Err(_) => UNKNOWN,
    }
}

/// Three answers on macOS: a packaged `PaneFlow.app` with its Sparkle
/// framework in place; a bundle whose framework is missing (a hand-assembled
/// or stripped bundle, where self-update silently does nothing); and a bare
/// binary (`cargo run`, a `target/` build, an executable copied out of the
/// bundle).
///
/// The locator is [`crate::sparkle::bundled_framework_binary`], the same one
/// `sparkle::start_if_bundled` uses, so the report cannot disagree with the
/// updater about whether the framework is there. No shelling out, no
/// `Info.plist` parse: the framework file either exists or it does not.
fn install_format_for(executable: &Path) -> &'static str {
    match crate::sparkle::bundled_framework_binary(executable) {
        Some(framework) if framework.is_file() => INSTALL_APP_BUNDLE,
        Some(_) => INSTALL_APP_BUNDLE_NO_SPARKLE,
        None => INSTALL_BARE_BINARY,
    }
}

// ── GPU ──────────────────────────────────────────────────────────────────

/// Render `GpuSpecs` as `<device> - <driver> <driver info>`, with the
/// software-rasterizer flag spelled out. That flag is the highest-signal bit
/// in the whole report: a user on a software rasterizer complaining about
/// frame rate is diagnosed by this line alone.
fn format_gpu_specs(specs: &GpuSpecs) -> String {
    let device = specs.device_name.trim();
    let mut out = if device.is_empty() {
        UNKNOWN.to_string()
    } else {
        device.to_string()
    };

    let driver = [specs.driver_name.trim(), specs.driver_info.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !driver.is_empty() {
        out.push_str(" - ");
        out.push_str(&driver);
    }

    if specs.is_software_emulated {
        out.push_str(" (software emulated)");
    }
    out
}

/// GPUI's AppKit window yields no `GpuSpecs` at the pinned revision, so the
/// adapter is read straight from Metal unless GPUI starts answering.
fn gpu_description(specs: Option<&GpuSpecs>) -> String {
    if let Some(specs) = specs {
        return format_gpu_specs(specs);
    }
    metal_device_names().unwrap_or_else(|| UNKNOWN.to_string())
}

/// Every Metal device in the machine, in Metal's own order.
///
/// `MTLCopyAllDevices` is used rather than `MTLCreateSystemDefaultDevice`
/// deliberately: on a graphics-switching Mac the latter forces the system onto
/// the high-power GPU (Apple documents this on the function itself), which
/// would make "copy my system info" cost the user battery life.
/// `MTLCopyAllDevices` has no such effect and reports both GPUs of a dual-GPU
/// Intel MacBook Pro instead of just one.
///
/// Returns `None` when the machine reports no Metal device at all, which the
/// caller renders as `unknown` rather than an empty field.
fn metal_device_names() -> Option<String> {
    use objc2_metal::{MTLCopyAllDevices, MTLDevice};

    let devices = MTLCopyAllDevices();
    // `NSArray::iter` sits behind objc2-foundation's `NSEnumerator` feature,
    // which nothing in our dependency set is required to turn on. Indexing
    // needs only `NSArray` itself, which `objc2-metal/MTLDevice` already
    // enables.
    let names: Vec<String> = (0..devices.count())
        .map(|index| devices.objectAtIndex(index).name().to_string())
        .filter(|name| !name.trim().is_empty())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

// ── OS name and version ──────────────────────────────────────────────────

fn os_description() -> String {
    let version = sysctl_string(c"kern.osproductversion");
    let build = sysctl_string(c"kern.osversion");
    match (version, build) {
        (Some(version), Some(build)) => format!("macOS {version} (build {build})"),
        (Some(version), None) => format!("macOS {version}"),
        // `hw.model` ("MacBookPro18,3") is not a version, but it still tells a
        // triager which machine class the report came from.
        (None, _) => match sysctl_string(c"hw.model") {
            Some(model) => format!("macOS ({UNKNOWN} version, {model})"),
            None => format!("macOS ({UNKNOWN} version)"),
        },
    }
}

/// Read a string-valued `sysctl` by name. Two calls: one to size the buffer,
/// one to fill it. Returns `None` for a missing key or a value that is not
/// valid UTF-8.
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut size: usize = 0;
    // SAFETY: standard `sysctlbyname` FFI. `name` is a valid NUL-terminated C
    // string, the output pointer is null so the call only reports the size it
    // would write into `size`, and the new-value pointer is null (read-only
    // query).
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }

    let mut buffer = vec![0u8; size];
    // SAFETY: same call, now with a buffer of exactly the size the kernel just
    // asked for; `size` is passed by pointer and updated with the bytes
    // actually written.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }

    buffer.truncate(size);
    let value = String::from_utf8(buffer).ok()?;
    let value = value.trim_end_matches('\0').trim();
    (!value.is_empty()).then(|| value.to_string())
}

// ── CPU ──────────────────────────────────────────────────────────────────

fn cpu_description() -> String {
    // Apple Silicon answers "Apple M2 Pro" here, Intel Macs their marketing
    // CPU string. `hw.model` is the machine identifier, a usable last resort.
    sysctl_string(c"machdep.cpu.brand_string")
        .or_else(|| sysctl_string(c"hw.model"))
        .unwrap_or_else(|| UNKNOWN.to_string())
}

// ── Terminal engine ──────────────────────────────────────────────────────

/// libghostty's version plus the ABI version we link against, from the same
/// pinned build identity `TerminalState::backend_diagnostics` reports. A
/// terminal bug that only reproduces on one release is usually an
/// engine-version story, and the vendored archive is invisible from the
/// outside otherwise.
fn terminal_engine_description() -> String {
    let identity = paneflow_terminal_ghostty::build_identity();
    format!(
        "libghostty {} (API {})",
        paneflow_terminal_ghostty::GHOSTTY_APP_VERSION,
        identity.api_version
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SystemInfo {
        SystemInfo {
            version: "0.1.3",
            target_triple: "aarch64-apple-darwin",
            install: INSTALL_APP_BUNDLE,
            os: "macOS 26.1 (build 25B77)".to_string(),
            cpu: "Apple M2 Pro".to_string(),
            gpu: "Apple M2 Pro".to_string(),
            renderer: "Metal (GPUI)",
            terminal_engine: "libghostty 1.2.0 (API 3)".to_string(),
        }
    }

    #[test]
    fn report_is_a_paste_ready_markdown_block() {
        assert_eq!(
            sample().to_string(),
            "- **PaneFlow**: 0.1.3 (aarch64-apple-darwin)\n\
             - **Install**: app bundle (Sparkle available)\n\
             - **OS**: macOS 26.1 (build 25B77)\n\
             - **CPU**: Apple M2 Pro\n\
             - **GPU**: Apple M2 Pro\n\
             - **Renderer**: Metal (GPUI)\n\
             - **Terminal engine**: libghostty 1.2.0 (API 3)"
        );
    }

    /// The modal renders `rows()` and the Copy button renders `Display`. This
    /// pins them to the same content, so a field added to one cannot go
    /// missing from the other.
    #[test]
    fn what_the_modal_shows_is_what_the_copy_button_writes() {
        let info = sample();
        let rendered: Vec<String> = info.to_string().lines().map(str::to_string).collect();
        let rows = info.rows();
        assert_eq!(rendered.len(), rows.len());
        for (line, (label, value)) in rendered.iter().zip(rows) {
            assert_eq!(line, &format!("- **{label}**: {value}"));
        }
    }

    #[test]
    fn report_has_no_trailing_newline_so_it_pastes_inside_a_template() {
        assert!(!sample().to_string().ends_with('\n'));
    }

    #[test]
    fn gpu_specs_render_device_then_driver() {
        let specs = GpuSpecs {
            is_software_emulated: false,
            device_name: "Apple M2 Pro".to_string(),
            driver_name: "Metal".to_string(),
            driver_info: "4.0".to_string(),
        };
        assert_eq!(format_gpu_specs(&specs), "Apple M2 Pro - Metal 4.0");
    }

    #[test]
    fn software_rasterizers_are_named_as_such() {
        let specs = GpuSpecs {
            is_software_emulated: true,
            device_name: "Apple Paravirtual device".to_string(),
            driver_name: "Metal".to_string(),
            driver_info: String::new(),
        };
        assert!(format_gpu_specs(&specs).ends_with("(software emulated)"));
    }

    #[test]
    fn gpu_specs_with_no_driver_strings_render_the_device_alone() {
        let specs = GpuSpecs {
            is_software_emulated: false,
            device_name: "Apple M3 Max".to_string(),
            driver_name: String::new(),
            driver_info: "  ".to_string(),
        };
        assert_eq!(format_gpu_specs(&specs), "Apple M3 Max");
    }

    #[test]
    fn an_empty_device_name_reads_as_unknown_not_as_an_empty_field() {
        let specs = GpuSpecs {
            is_software_emulated: false,
            device_name: "   ".to_string(),
            driver_name: "Metal".to_string(),
            driver_info: String::new(),
        };
        assert_eq!(format_gpu_specs(&specs), "unknown - Metal");
    }

    /// The only test that runs every probe for real, against the host the
    /// suite runs on. It cannot assert the values (they differ per machine),
    /// but it does assert the shape: no probe may return an empty field, and
    /// the block must stay one labelled bullet per line, which is what makes
    /// it paste cleanly into an issue.
    ///
    /// Run with `--nocapture` to eyeball the real report after adding a field.
    #[test]
    fn every_probe_yields_a_labelled_line_on_the_host() {
        let report = SystemInfoProbe { gpu: None }.resolve().to_string();
        println!("{report}");

        assert_eq!(report.lines().count(), 7, "{report}");

        for line in report.lines() {
            let (label, value) = line
                .strip_prefix("- **")
                .and_then(|rest| rest.split_once("**: "))
                .expect("every line is a `- **Label**: value` bullet");
            assert!(!label.is_empty(), "unlabelled line: {line}");
            assert!(!value.trim().is_empty(), "empty value for {label}");
        }
    }

    /// The install-format probe is a pure function of the executable path
    /// plus one file stat, so it is exercised against a scratch bundle rather
    /// than the test binary's own location (which is a bare `target/` build
    /// on every machine and would only ever cover one branch).
    #[test]
    fn install_format_tells_a_bare_binary_from_an_app_bundle() {
        assert_eq!(
            install_format_for(Path::new("/repo/target/debug/paneflow")),
            INSTALL_BARE_BINARY
        );
        assert_eq!(
            install_format_for(Path::new("/usr/local/bin/paneflow")),
            INSTALL_BARE_BINARY
        );

        let scratch = tempfile::tempdir().expect("scratch dir");
        let bundle = scratch.path().join("PaneFlow.app");
        let executable = bundle.join("Contents").join("MacOS").join("paneflow");
        assert_eq!(
            install_format_for(&executable),
            INSTALL_APP_BUNDLE_NO_SPARKLE,
            "a bundle without Frameworks/Sparkle.framework/Sparkle"
        );

        let framework = bundle
            .join("Contents")
            .join("Frameworks")
            .join("Sparkle.framework");
        std::fs::create_dir_all(&framework).expect("framework dir");
        std::fs::write(framework.join("Sparkle"), b"").expect("framework binary");
        assert_eq!(install_format_for(&executable), INSTALL_APP_BUNDLE);
    }

    /// The block goes into a public issue. Nothing in it may name where the
    /// reporter keeps their code or what their shell carries: no working
    /// directory, no executable path, no home directory, and no `KEY=value`
    /// line that would read as an environment dump.
    #[test]
    fn report_carries_no_working_directory_and_no_environment_dump() {
        let report = SystemInfoProbe { gpu: None }.resolve().to_string();

        let mut secrets: Vec<String> = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            secrets.push(cwd.display().to_string());
        }
        if let Ok(exe) = std::env::current_exe() {
            secrets.push(exe.display().to_string());
        }
        if let Some(home) = dirs::home_dir() {
            secrets.push(home.display().to_string());
        }
        assert!(
            !secrets.is_empty(),
            "the host must yield at least one path to check against"
        );
        for secret in &secrets {
            assert!(
                !report.contains(secret.as_str()),
                "report leaks a local path ({secret}):\n{report}"
            );
        }

        for line in report.lines() {
            let value = line
                .split_once("**: ")
                .map(|(_, value)| value)
                .unwrap_or(line);
            assert!(
                !value.contains("PATH=") && !value.contains("HOME="),
                "report carries a shell variable:\n{report}"
            );
            let looks_like_assignment = value.split_once('=').is_some_and(|(key, _)| {
                !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            });
            assert!(
                !looks_like_assignment,
                "report line reads as an environment dump: {line}"
            );
        }
    }

    /// Same guarantee at the source level: the collection half of this module
    /// never reads an environment variable, the working directory or the home
    /// directory. `current_exe` is the one process fact it may consult, and
    /// only to classify the install format.
    #[test]
    fn collection_never_reads_the_environment_or_the_working_directory() {
        let production = include_str!("system_info.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        for forbidden in [
            "env::var",
            "env::vars",
            "current_dir",
            "home_dir",
            "config_dir",
            "data_dir",
        ] {
            assert!(
                !production.contains(forbidden),
                "system_info.rs must not call `{forbidden}`: the report is pasted into public issues"
            );
        }
    }
}
