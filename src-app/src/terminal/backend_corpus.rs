use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::Event as AlacEvent;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Side as AlacSide;
use alacritty_terminal::selection::{Selection as AlacSelection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};

use super::listener::{SpikeTermSize, ZedListener};
use super::types::{Content, Modes, Point, SelectionRange, content_from_term};

pub(crate) const CORPUS_SEED: u64 = 0x5041_4e45_464c_4f57;
const CORPUS_FAMILIES: usize = 27;
const CORPUS_VARIANTS: usize = 5;
const CORPUS_SIZE: usize = CORPUS_FAMILIES * CORPUS_VARIANTS;

struct CorpusCase {
    name: String,
    bytes: Vec<u8>,
    resize_after_feed: Option<(usize, usize)>,
    selection_after_feed: Option<SelectionRange>,
    search_after_feed: Option<&'static str>,
}


struct Harness {
    term: Arc<FairMutex<Term<ZedListener>>>,
    events: UnboundedReceiver<AlacEvent>,
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedSnapshot {
    content: String,
    logical_text: String,
    modes: String,
    events: Vec<String>,
    search: Option<SearchObservation>,
    resize_damage: Option<ResizeDamageObservation>,
    history_size: usize,
    cell_count: usize,
    absolute_cursor_line: i64,
    cursor_column: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct SearchObservation {
    matches: Vec<(i32, usize, i32, usize)>,
    regex_error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ResizeDamageObservation {
    before_dimensions: (usize, usize),
    after_dimensions: (usize, usize),
    before_cell_count: usize,
    after_cell_count: usize,
    snapshot_changed: bool,
}

struct ResizeBefore {
    dimensions: (usize, usize),
    cell_count: usize,
    content: String,
}

impl ResizeBefore {
    fn capture(content: &Content) -> Self {
        Self {
            dimensions: (content.cols, content.rows),
            cell_count: content.cells.len(),
            content: normalize_content(content.clone()),
        }
    }

    fn complete(self, content: &Content) -> ResizeDamageObservation {
        ResizeDamageObservation {
            before_dimensions: self.dimensions,
            after_dimensions: (content.cols, content.rows),
            before_cell_count: self.cell_count,
            after_cell_count: content.cells.len(),
            snapshot_changed: self.content != normalize_content(content.clone()),
        }
    }
}

impl Harness {
    fn new() -> Self {
        let (events_tx, events) = unbounded();
        let listener = ZedListener::new(events_tx);
        let dimensions = SpikeTermSize {
            columns: 80,
            screen_lines: 24,
        };
        let config = TermConfig {
            scrolling_history: 10_000,
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &dimensions, listener)));
        Self { term, events }
    }

    fn replay(mut self, case: &CorpusCase, chunks: &[usize]) -> NormalizedSnapshot {
        let mut processor = Processor::<StdSyncHandler>::new();
        let mut offset = 0;
        for &size in chunks {
            if offset >= case.bytes.len() {
                break;
            }
            let end = offset.saturating_add(size).min(case.bytes.len());
            processor.advance(&mut *self.term.lock(), &case.bytes[offset..end]);
            offset = end;
        }
        if offset < case.bytes.len() {
            processor.advance(&mut *self.term.lock(), &case.bytes[offset..]);
        }

        let resize_before = if let Some((columns, screen_lines)) = case.resize_after_feed {
            let before = {
                let term = self.term.lock_unfair();
                ResizeBefore::capture(&content_from_term(&term))
            };
            self.term.lock().resize(SpikeTermSize {
                columns,
                screen_lines,
            });
            Some(before)
        } else {
            None
        };
        if let Some(range) = case.selection_after_feed {
            let mut selection =
                AlacSelection::new(SelectionType::Simple, range.start.into(), AlacSide::Left);
            selection.update(range.end.into(), AlacSide::Right);
            self.term.lock().selection = Some(selection);
        }

        let search = case.search_after_feed.map(|query| {
            normalize_alacritty_search(crate::search::search_term(&self.term, query, false))
        });

        let (content, logical_text, modes, history_size, cell_count) = {
            let term = self.term.lock_unfair();
            let content = content_from_term(&term);
            let history_size = content.history_size;
            let cell_count = content.cells.len();
            (
                content,
                normalize_alacritty_grid(&term),
                normalize_modes(*term.mode()),
                history_size,
                cell_count,
            )
        };
        let resize_damage = resize_before.map(|before| before.complete(&content));
        let absolute_cursor_line = history_size as i64 + i64::from(content.cursor.point.line.0);
        let cursor_column = content.cursor.point.column.0;
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let Some(normalized) = normalize_alacritty_event(event) {
                events.push(normalized);
            }
        }
        NormalizedSnapshot {
            content: normalize_content(content),
            logical_text,
            modes,
            events,
            search,
            resize_damage,
            history_size,
            cell_count,
            absolute_cursor_line,
            cursor_column,
        }
    }
}

fn normalize_alacritty_grid(term: &Term<ZedListener>) -> String {
    let mut lines = Vec::new();
    let mut row = term.topmost_line().0;
    let bottom = term.bottommost_line().0;
    while row <= bottom {
        let text = term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(row),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(row),
                term.last_column(),
            ),
        );
        lines.push(text.trim_end().to_owned());
        row += 1;
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn normalize_alacritty_event(event: AlacEvent) -> Option<String> {
    match event {
        AlacEvent::Wakeup | AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => None,
        AlacEvent::PtyWrite(text) => Some(normalize_pty_write(&text)),
        AlacEvent::ClipboardStore(_, text) => Some(format!("ClipboardStore({text:?})")),
        AlacEvent::Bell => Some("Bell".to_owned()),
        AlacEvent::Title(title) => Some(format!("Title({title:?})")),
        other => Some(format!("{other:?}")),
    }
}

fn normalize_pty_write(text: &str) -> String {
    if text.starts_with("\x1b[?") && text.ends_with('c') {
        "PtyWrite(PrimaryDeviceAttributes)".to_owned()
    } else if text.starts_with("\x1b[>") && text.ends_with('c') {
        "PtyWrite(SecondaryDeviceAttributes)".to_owned()
    } else {
        format!("PtyWrite({text:?})")
    }
}

fn normalize_alacritty_search(result: crate::search::SearchResult) -> SearchObservation {
    SearchObservation {
        matches: result
            .matches
            .into_iter()
            .map(|found| {
                (
                    found.start.line.0,
                    found.start.column.0,
                    found.end.line.0,
                    found.end.column.0,
                )
            })
            .collect(),
        regex_error: result.regex_error,
    }
}






fn normalize_content(content: Content) -> String {
    let mut cells = String::new();
    for cell in content.cells.iter() {
        use std::fmt::Write as _;
        let _ = write!(
            cells,
            "{}:{}:{:?}:{:?}:{:?}:{:?}:{:?}:{}|",
            cell.point.line.0,
            cell.point.column.0,
            cell.c,
            cell.fg,
            cell.bg,
            cell.flags,
            cell.zerowidth,
            cell.hyperlink
        );
    }
    format!(
        "history={};offset={};cursor={:?};selection={:?};cells={cells}",
        content.history_size, content.display_offset, content.cursor, content.selection
    )
}

fn normalize_modes(mode: TermMode) -> String {
    format!("{:?}", Modes::from(mode))
}

fn fixed_chunks(len: usize, size: usize) -> Vec<usize> {
    vec![size; len.div_ceil(size)]
}

fn seeded_chunks(len: usize, seed: u64) -> Vec<usize> {
    let mut state = seed;
    let mut remaining = len;
    let mut chunks = Vec::new();
    while remaining > 0 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let size = 1 + (state as usize % 257);
        chunks.push(size.min(remaining));
        remaining = remaining.saturating_sub(size);
    }
    chunks
}

fn corpus() -> Vec<CorpusCase> {
    let mut cases = Vec::with_capacity(CORPUS_SIZE);
    for index in 0..CORPUS_SIZE {
        let variant = index / CORPUS_FAMILIES;
        let family = index % CORPUS_FAMILIES;
        let (bytes, resize_after_feed) = match family {
            0 => (format!("plain-ascii-{variant}\r\n").into_bytes(), None),
            1 => (format!("unicode-{variant}: café Καλημέρα हिन्दी 🦀\r\n").into_bytes(), None),
            2 => (format!("grapheme-{variant}: e\u{301} n\u{303} 👨‍👩‍👧‍👦\r\n").into_bytes(), None),
            3 => (format!("wide-{variant}: 中文 日本語 한글\r\n").into_bytes(), None),
            4 => (format!("\x1b[1;3;4;9mstyled-{variant}\x1b[0m\r\n").into_bytes(), None),
            5 => (format!("\x1b[38;2;{};{};{}mtruecolor-{variant}\x1b[0m", 20 + variant, 80 + variant, 140 + variant).into_bytes(), None),
            6 => (format!("origin\x1b[{};{}Hcursor-{variant}\x1b[2A\x1b[3C", 2 + variant, 3 + variant).into_bytes(), None),
            7 => ((format!("wrap-{variant}-") + &"x".repeat(180 + variant)).into_bytes(), None),
            8 => ((format!("reflow-{variant}-") + &"0123456789".repeat(24)).into_bytes(), Some((41 + variant, 18 + variant))),
            9 => (format!("before\x1b[?1049halt-{variant}\x1b[?1049lafter").into_bytes(), None),
            10 => ((0..40).map(|line| format!("scroll-{variant}-{line}\r\n")).collect::<String>().into_bytes(), None),
            11 => (format!("\x1b[?1h\x1b[?1000h\x1b[?1006hmode-{variant}").into_bytes(), None),
            12 => (format!("\x1b]2;synthetic-title-{variant}\x07title-body").into_bytes(), None),
            13 => (
                format!("query-{variant}\x1b[5n\x1b[6n\x1b[c\x1b[>c").into_bytes(),
                None,
            ),
            14 => (format!("malformed-{variant}\x1b[999999999999999999999;?;mend").into_bytes(), None),
            15 => (format!("truncated-{variant}\x1b]8;;https://synthetic.invalid/unterminated").into_bytes(), None),
            16 => (format!("erase-{variant}\x1b[2J\x1b[Hredrawn-{variant}").into_bytes(), None),
            17 => (format!("\x1b]8;id=synthetic-{variant};https://example.invalid/{variant}\x07link\x1b]8;;\x07").into_bytes(), None),
            18 => (format!("\x1b]133;A\x07prompt-{variant}\x1b]133;B\x07command\x1b]133;C\x07output\x1b]133;D;0\x07").into_bytes(), None),
            19 => (format!("\x1b]52;c;c3ludGhldGljLWNsaXBib2FyZC0{variant}=\x07").into_bytes(), None),
            20 => (format!("\x1b[{};{}mansi16-{variant}\x1b[0m", 30 + variant, 40 + ((variant + 2) % 6)).into_bytes(), None),
            21 => (format!("\x1b[38;5;{};48;5;{}mindexed256-{variant}\x1b[0m", 16 + variant * 17, 231 - variant * 11).into_bytes(), None),
            22 => (format!("\x1b[2;7mdim-inverse-{variant}\x1b[0m").into_bytes(), None),
            23 => (format!("\x1b[{} qcursor-shape-{variant}", variant + 1).into_bytes(), None),
            24 => {
                let mut bytes = format!("invalid-utf8-{variant}:").into_bytes();
                bytes.extend_from_slice(&[0xf0, 0x28, 0x8c, 0x28, b'\r', b'\n']);
                (bytes, None)
            }
            25 => (format!("tabs-{variant}:\talpha\t中\tomega\r\n").into_bytes(), None),
            26 => (format!("selection-{variant}-target").into_bytes(), None),
            _ => unreachable!(),
        };
        cases.push(CorpusCase {
            name: format!("family-{family:02}-variant-{variant}"),
            bytes,
            resize_after_feed,
            selection_after_feed: (family == 26).then_some(SelectionRange {
                start: Point::new(0, 0),
                end: Point::new(0, 8),
                is_block: false,
            }),
            search_after_feed: (family == 26).then_some("target"),
        });
    }
    cases
}

pub(crate) fn deterministic_streams() -> Vec<Vec<u8>> {
    corpus().into_iter().map(|case| case.bytes).collect()
}

#[test]
fn alacritty_corpus_is_chunk_invariant() {
    let corpus = corpus();
    assert_eq!(corpus.len(), CORPUS_SIZE);
    for (index, case) in corpus.iter().enumerate() {
        let baseline = Harness::new().replay(case, &[case.bytes.len().max(1)]);
        for (label, chunks) in [
            ("1", fixed_chunks(case.bytes.len(), 1)),
            ("7", fixed_chunks(case.bytes.len(), 7)),
            ("64", fixed_chunks(case.bytes.len(), 64)),
            ("4096", fixed_chunks(case.bytes.len(), 4096)),
            (
                "seeded",
                seeded_chunks(case.bytes.len(), CORPUS_SEED ^ index as u64),
            ),
        ] {
            assert_eq!(
                Harness::new().replay(case, &chunks),
                baseline,
                "chunk divergence in {} with {label}-byte plan",
                case.name
            );
        }
    }
}



#[test]
fn malformed_and_oversized_streams_are_deterministic() {
    let mut hostile = vec![b'A'; 1024 * 1024];
    hostile.extend_from_slice(b"\x1b]52;c;");
    hostile.extend(std::iter::repeat_n(b'B', 128 * 1024));
    hostile.extend_from_slice(b"\x1b\\\x1b[999999999999999999999999999999m");
    let case = CorpusCase {
        name: "hostile-bounded-fixture".to_owned(),
        bytes: hostile,
        resize_after_feed: None,
        selection_after_feed: None,
        search_after_feed: None,
    };
    let first = Harness::new().replay(&case, &fixed_chunks(case.bytes.len(), 4096));
    let second = Harness::new().replay(&case, &fixed_chunks(case.bytes.len(), 4096));
    assert_eq!(first, second);
    assert!(first.history_size <= 10_000, "scrollback cap was exceeded");
    assert!(
        first.cell_count <= 80 * 24,
        "snapshot escaped the viewport bound"
    );
}

#[test]
#[ignore = "captures the machine-specific EP-001 performance baseline"]
fn alacritty_eight_pane_baseline() {
    let cases = corpus();
    let total_bytes = cases.iter().map(|case| case.bytes.len()).sum::<usize>() * 8;
    let wall_start = Instant::now();
    let cpu_start = process_cpu_time();
    let rss_start = resident_set_bytes();
    let mut frame_latencies = Vec::with_capacity(cases.len() * 8);
    let mut lock_durations = Vec::with_capacity(cases.len() * 8);

    let mut panes = (0..8).map(|_| Harness::new()).collect::<Vec<_>>();
    for (index, case) in cases.iter().enumerate() {
        for (pane, harness) in panes.iter_mut().enumerate() {
            let feed_start = Instant::now();
            let chunks = seeded_chunks(case.bytes.len(), CORPUS_SEED ^ pane as u64 ^ index as u64);
            let mut processor = Processor::<StdSyncHandler>::new();
            let mut offset: usize = 0;
            for size in chunks {
                let end = offset.saturating_add(size).min(case.bytes.len());
                processor.advance(&mut *harness.term.lock(), &case.bytes[offset..end]);
                offset = end;
            }
            if let Some((columns, screen_lines)) = case.resize_after_feed {
                harness.term.lock().resize(SpikeTermSize {
                    columns,
                    screen_lines,
                });
            }
            let lock_start = Instant::now();
            let snapshot = {
                let term = harness.term.lock_unfair();
                content_from_term(&term)
            };
            lock_durations.push(lock_start.elapsed());
            std::hint::black_box(snapshot);
            frame_latencies.push(feed_start.elapsed());
        }
    }

    let wall = wall_start.elapsed();
    let cpu = process_cpu_time().saturating_sub(cpu_start);
    let rss_end = resident_set_bytes();
    frame_latencies.sort_unstable();
    lock_durations.sort_unstable();
    let throughput = total_bytes as f64 / wall.as_secs_f64() / (1024.0 * 1024.0);
    println!(
        "{{\"seed\":\"0x{CORPUS_SEED:016x}\",\"panes\":8,\"streams_per_pane\":{},\"bytes\":{total_bytes},\"throughput_mib_s\":{throughput:.3},\"input_to_snapshot_p50_us\":{},\"input_to_snapshot_p95_us\":{},\"lock_p95_us\":{},\"wall_ms\":{},\"cpu_ms\":{},\"rss_start_bytes\":{},\"rss_end_bytes\":{},\"cpu_model\":{:?},\"profile\":{:?},\"measurement_scope\":\"persistent-eight-pane-parser-to-neutral-snapshot\"}}",
        cases.len(),
        percentile_us(&frame_latencies, 50),
        percentile_us(&frame_latencies, 95),
        percentile_us(&lock_durations, 95),
        wall.as_millis(),
        cpu.as_millis(),
        rss_start,
        rss_end,
        cpu_model(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
}







pub(crate) fn percentile_duration(values: &[Duration], percentile: usize) -> Duration {
    let index = values.len().saturating_sub(1).saturating_mul(percentile) / 100;
    values.get(index).copied().unwrap_or_default()
}


pub(crate) fn percentile_us(values: &[Duration], percentile: usize) -> u128 {
    percentile_duration(values, percentile).as_micros()
}

#[cfg(target_os = "linux")]
pub(crate) fn resident_set_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let resident_pages = statm
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
    resident_pages.saturating_mul(page_size)
}

#[cfg(target_os = "windows")]
pub(crate) fn resident_set_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: zeroed C POD with its byte size set before the current-process query.
    let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the current-process pseudo handle and writable counter buffer are valid.
    let result = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut memory,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if result == 0 {
        return 0;
    }
    u64::try_from(memory.WorkingSetSize).unwrap_or(u64::MAX)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn resident_set_bytes() -> u64 {
    0
}

#[cfg(target_os = "linux")]
pub(crate) fn process_cpu_time() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields)
        .unwrap_or("");
    let mut values = fields.split_whitespace();
    let user_ticks = values
        .nth(11)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let system_ticks = values
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Duration::from_secs_f64((user_ticks + system_ticks) as f64 / ticks_per_second as f64)
}

#[cfg(target_os = "windows")]
pub(crate) fn process_cpu_time() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    // SAFETY: FILETIME is a C POD and all four buffers are initialized before use.
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: the current-process pseudo handle and writable FILETIME buffers are valid.
    let result = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if result == 0 {
        return Duration::ZERO;
    }
    let ticks =
        |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    Duration::from_nanos(
        ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn process_cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(target_os = "linux")]
pub(crate) fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unknown")
        .to_owned()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cpu_model() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
