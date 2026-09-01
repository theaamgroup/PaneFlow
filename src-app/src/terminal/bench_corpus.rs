//! Deterministic VT streams and process counters for the terminal benchmarks.
//!
//! The streams are generated, never recorded, so a benchmark run is
//! reproducible from `CORPUS_SEED` alone and no fixture file has to be kept in
//! sync with the engine. Nothing here touches a terminal backend: the same
//! bytes feed the Ghostty stress scenarios and the GPUI input-to-frame probe.

use std::time::Duration;

pub(crate) const CORPUS_SEED: u64 = 0x5041_4e45_464c_4f57;
const CORPUS_FAMILIES: usize = 27;
const CORPUS_VARIANTS: usize = 5;
const CORPUS_SIZE: usize = CORPUS_FAMILIES * CORPUS_VARIANTS;

/// The full stream set, one entry per family/variant pair.
pub(crate) fn deterministic_streams() -> Vec<Vec<u8>> {
    let mut streams = Vec::with_capacity(CORPUS_SIZE);
    for index in 0..CORPUS_SIZE {
        let variant = index / CORPUS_FAMILIES;
        let family = index % CORPUS_FAMILIES;
        let bytes = match family {
            0 => format!("plain-ascii-{variant}\r\n").into_bytes(),
            1 => format!("unicode-{variant}: café Καλημέρα हिन्दी 🦀\r\n").into_bytes(),
            2 => format!("grapheme-{variant}: e\u{301} n\u{303} 👨‍👩‍👧‍👦\r\n").into_bytes(),
            3 => format!("wide-{variant}: 中文 日本語 한글\r\n").into_bytes(),
            4 => format!("\x1b[1;3;4;9mstyled-{variant}\x1b[0m\r\n").into_bytes(),
            5 => format!(
                "\x1b[38;2;{};{};{}mtruecolor-{variant}\x1b[0m",
                20 + variant,
                80 + variant,
                140 + variant
            )
            .into_bytes(),
            6 => format!(
                "origin\x1b[{};{}Hcursor-{variant}\x1b[2A\x1b[3C",
                2 + variant,
                3 + variant
            )
            .into_bytes(),
            7 => (format!("wrap-{variant}-") + &"x".repeat(180 + variant)).into_bytes(),
            8 => (format!("reflow-{variant}-") + &"0123456789".repeat(24)).into_bytes(),
            9 => format!("before\x1b[?1049halt-{variant}\x1b[?1049lafter").into_bytes(),
            10 => (0..40)
                .map(|line| format!("scroll-{variant}-{line}\r\n"))
                .collect::<String>()
                .into_bytes(),
            11 => format!("\x1b[?1h\x1b[?1000h\x1b[?1006hmode-{variant}").into_bytes(),
            12 => format!("\x1b]2;synthetic-title-{variant}\x07title-body").into_bytes(),
            13 => format!("query-{variant}\x1b[5n\x1b[6n\x1b[c\x1b[>c").into_bytes(),
            14 => format!("malformed-{variant}\x1b[999999999999999999999;?;mend").into_bytes(),
            15 => {
                format!("truncated-{variant}\x1b]8;;https://synthetic.invalid/unterminated")
                    .into_bytes()
            }
            16 => format!("erase-{variant}\x1b[2J\x1b[Hredrawn-{variant}").into_bytes(),
            17 => format!(
                "\x1b]8;id=synthetic-{variant};https://example.invalid/{variant}\x07link\x1b]8;;\x07"
            )
            .into_bytes(),
            18 => format!(
                "\x1b]133;A\x07prompt-{variant}\x1b]133;B\x07command\x1b]133;C\x07output\x1b]133;D;0\x07"
            )
            .into_bytes(),
            19 => format!("\x1b]52;c;c3ludGhldGljLWNsaXBib2FyZC0{variant}=\x07").into_bytes(),
            20 => format!(
                "\x1b[{};{}mansi16-{variant}\x1b[0m",
                30 + variant,
                40 + ((variant + 2) % 6)
            )
            .into_bytes(),
            21 => format!(
                "\x1b[38;5;{};48;5;{}mindexed256-{variant}\x1b[0m",
                16 + variant * 17,
                231 - variant * 11
            )
            .into_bytes(),
            22 => format!("\x1b[2;7mdim-inverse-{variant}\x1b[0m").into_bytes(),
            23 => format!("\x1b[{} qcursor-shape-{variant}", variant + 1).into_bytes(),
            24 => {
                let mut bytes = format!("invalid-utf8-{variant}:").into_bytes();
                bytes.extend_from_slice(&[0xf0, 0x28, 0x8c, 0x28, b'\r', b'\n']);
                bytes
            }
            25 => format!("tabs-{variant}:\talpha\t中\tomega\r\n").into_bytes(),
            26 => format!("selection-{variant}-target").into_bytes(),
            _ => unreachable!(),
        };
        streams.push(bytes);
    }
    streams
}

pub(crate) fn percentile_duration(values: &[Duration], percentile: usize) -> Duration {
    let index = values.len().saturating_sub(1).saturating_mul(percentile) / 100;
    values.get(index).copied().unwrap_or_default()
}

pub(crate) fn percentile_us(values: &[Duration], percentile: usize) -> u128 {
    percentile_duration(values, percentile).as_micros()
}

fn task_all_info() -> Option<libproc::libproc::task_info::TaskAllInfo> {
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::libproc::task_info::TaskAllInfo;
    pidinfo::<TaskAllInfo>(std::process::id() as i32, 0).ok()
}

pub(crate) fn resident_set_bytes() -> u64 {
    task_all_info()
        .map(|info| info.ptinfo.pti_resident_size)
        .unwrap_or(0)
}

pub(crate) fn process_cpu_time() -> Duration {
    task_all_info()
        .map(|info| {
            duration_from_mach_ticks(
                info.ptinfo
                    .pti_total_user
                    .saturating_add(info.ptinfo.pti_total_system),
            )
        })
        .unwrap_or_default()
}

/// `pti_total_user` / `pti_total_system` are Mach absolute-time ticks, not
/// nanoseconds. Convert with the kernel timebase (observed 125/3 on arm64).
fn duration_from_mach_ticks(ticks: u64) -> Duration {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: `info` is a local C-layout struct; the syscall only writes it.
    let kr = unsafe { mach_timebase_info(&mut info) };
    if kr != 0 || info.denom == 0 {
        return Duration::ZERO;
    }
    let nanos = u64::try_from(u128::from(ticks) * u128::from(info.numer) / u128::from(info.denom))
        .unwrap_or(u64::MAX);
    Duration::from_nanos(nanos)
}

pub(crate) fn cpu_model() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[test]
fn resident_set_bytes_samples_the_live_process() {
    assert!(
        resident_set_bytes() > 0,
        "live process RSS must be greater than zero"
    );
}

#[test]
fn process_cpu_time_samples_the_live_process() {
    // Burn a little user time so a freshly spawned test process is not at zero.
    let mut acc = 0u64;
    for i in 0..50_000u64 {
        acc = acc.wrapping_add(i);
    }
    std::hint::black_box(acc);
    assert!(
        process_cpu_time() > Duration::ZERO,
        "live process CPU time must be greater than zero"
    );
}
