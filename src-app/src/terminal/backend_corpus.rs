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
            events.extend(normalize_alacritty_event(event));
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

fn normalize_alacritty_event(event: AlacEvent) -> Vec<String> {
    match event {
        AlacEvent::Wakeup | AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => {
            Vec::new()
        }
        AlacEvent::PtyWrite(text) => normalize_pty_write(&text),
        AlacEvent::ClipboardStore(_, text) => vec![format!("ClipboardStore({text:?})")],
        AlacEvent::Bell => vec!["Bell".to_owned()],
        AlacEvent::Title(title) => vec![format!("Title({title:?})")],
        other => vec![format!("{other:?}")],
    }
}

/// C0 escape, and the two string terminators the reply grammar below needs.
const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Normalize one PTY write into one entry per reply it carries.
///
/// A backend may batch adjacent replies into one write; only the byte stream is
/// observable, so the corpus compares replies in order. A reply whose bytes
/// actually differ still fails, because each reply is normalized on its own.
fn normalize_pty_write(text: &str) -> Vec<String> {
    split_pty_replies(text)
        .iter()
        .map(|reply| normalize_pty_reply(reply))
        .collect()
}

/// Device attributes are an emulator's own identity, so the two engines answer
/// with legitimately different parameters. Every other reply is compared
/// byte-for-byte.
fn normalize_pty_reply(reply: &str) -> String {
    if reply.starts_with("\x1b[?") && reply.ends_with('c') {
        "PtyWrite(PrimaryDeviceAttributes)".to_owned()
    } else if reply.starts_with("\x1b[>") && reply.ends_with('c') {
        "PtyWrite(SecondaryDeviceAttributes)".to_owned()
    } else {
        format!("PtyWrite({reply:?})")
    }
}

/// Cut a PTY write payload into the individual replies it carries.
///
/// The grammar is only as wide as a terminal reply needs: a CSI runs to its
/// final byte (`0x40..=0x7e`), the string families (OSC, DCS, SOS, PM, APC)
/// run to BEL or ST, any other escape is two bytes, and a run of plain bytes
/// ends at the next escape. Every boundary falls on an ASCII byte, so the
/// slices are always on a character boundary.
fn split_pty_replies(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut replies = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if bytes[index] == ESC {
            index += 1;
            match bytes.get(index) {
                Some(b'[') => {
                    index += 1;
                    while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                        index += 1;
                    }
                    // The final byte belongs to the sequence, unless the write
                    // was truncated before one arrived.
                    index = index.saturating_add(1).min(bytes.len());
                }
                Some(b']' | b'P' | b'X' | b'^' | b'_') => {
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == BEL {
                            index += 1;
                            break;
                        }
                        if bytes[index] == ESC && bytes.get(index + 1) == Some(&b'\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                Some(_) => index += 1,
                None => {}
            }
        } else {
            while index < bytes.len() && bytes[index] != ESC {
                index += 1;
            }
        }
        replies.push(String::from_utf8_lossy(&bytes[start..index]).into_owned());
    }
    replies
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

/// The corpus compares reply streams, so the splitter is what makes the
/// comparison meaningful: a splitter that mangled its input symmetrically would
/// hide a real byte difference. Pin the grammar directly.
#[test]
fn a_coalesced_pty_write_splits_back_into_its_replies() {
    // A backend may batch adjacent replies into one write; only the byte stream
    // is observable, so the corpus compares replies in order.
    let coalesced = "\x1b[0n\x1b[1;8R\x1b[?62;22;52c\x1b[>1;10;0c";
    assert_eq!(
        normalize_pty_write(coalesced),
        vec![
            "PtyWrite(\"\\u{1b}[0n\")".to_owned(),
            "PtyWrite(\"\\u{1b}[1;8R\")".to_owned(),
            "PtyWrite(PrimaryDeviceAttributes)".to_owned(),
            "PtyWrite(SecondaryDeviceAttributes)".to_owned(),
        ]
    );

    // Split one reply at a time and the result is the same list, which is the
    // whole point: batching is not observable, bytes are.
    let separate: Vec<String> = ["\x1b[0n", "\x1b[1;8R", "\x1b[?62;22;52c", "\x1b[>1;10;0c"]
        .into_iter()
        .flat_map(normalize_pty_write)
        .collect();
    assert_eq!(normalize_pty_write(coalesced), separate);

    // Device attributes are the only payload the corpus lets diverge. A reply
    // whose bytes really differ still compares unequal.
    assert_ne!(
        normalize_pty_write("\x1b[0n"),
        normalize_pty_write("\x1b[3n")
    );
}

/// Every byte of the write must land in exactly one reply, whatever grammar the
/// payload uses, or the comparison silently drops part of the stream.
#[test]
fn splitting_a_pty_write_never_loses_a_byte() {
    for payload in [
        "",
        "\x1b[0n",
        // Truncated: no final byte ever arrives.
        "\x1b[38;2;1;2",
        // OSC terminated by BEL, then by ST.
        "\x1b]11;rgb:1111/2222/3333\x07",
        "\x1b]10;rgb:4444/5555/6666\x1b\\",
        // DCS carries an ST that is itself an escape; it must not split there.
        "\x1bP1$r0m\x1b\\\x1b[0n",
        // A two-byte escape, and plain text on both sides of one.
        "\x1bMtext\x1b[6n",
        "plain",
        // Non-ASCII text must not be cut mid-character.
        "café 🦀\x1b[0n",
    ] {
        let replies = split_pty_replies(payload);
        assert_eq!(
            replies.concat(),
            payload,
            "splitting {payload:?} changed the stream"
        );
        assert!(
            replies.iter().all(|reply| !reply.is_empty()),
            "empty reply in {payload:?}"
        );
    }

    // Spot-check the boundaries the concat property alone cannot pin.
    assert_eq!(
        split_pty_replies("\x1bP1$r0m\x1b\\\x1b[0n"),
        vec!["\x1bP1$r0m\x1b\\".to_owned(), "\x1b[0n".to_owned()]
    );
    assert_eq!(
        split_pty_replies("\x1bMtext\x1b[6n"),
        vec!["\x1bM".to_owned(), "text".to_owned(), "\x1b[6n".to_owned()]
    );
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

/// What one eight-pane corpus run measured. The numbers are machine-specific
/// and are printed for a human to read; the counts are structural and are
/// what [`alacritty_eight_pane_baseline`] asserts on.
struct EightPaneBaseline {
    frames: usize,
    total_bytes: usize,
    snapshot_cells: usize,
    json: String,
}

/// Feed the whole corpus into eight persistent panes and measure
/// parser-to-snapshot latency, throughput, lock hold time, CPU and RSS.
/// Measurement harness, not a threshold gate: the ceiling lives in
/// `layout::render::tests::eight_pane_gpui_input_to_paint_performance_gate`.
fn measure_eight_pane_baseline() -> EightPaneBaseline {
    let cases = corpus();
    let total_bytes = cases.iter().map(|case| case.bytes.len()).sum::<usize>() * 8;
    let wall_start = Instant::now();
    let cpu_start = process_cpu_time();
    let rss_start = resident_set_bytes();
    let mut frame_latencies = Vec::with_capacity(cases.len() * 8);
    let mut lock_durations = Vec::with_capacity(cases.len() * 8);
    let mut snapshot_cells: usize = 0;

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
            snapshot_cells += snapshot.cells.len();
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
    let json = format!(
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
    EightPaneBaseline {
        frames: frame_latencies.len(),
        total_bytes,
        snapshot_cells,
        json,
    }
}

/// Prints the baseline JSON for a human to record. Run with
/// `cargo test --release -p paneflow-app alacritty_eight_pane_baseline -- --ignored --nocapture`.
/// The assertions pin the shape of the run (every pane replayed every case,
/// the snapshots carried cells, the JSON has the keys a reader greps for),
/// not a machine-specific ceiling.
#[test]
#[ignore = "captures the machine-specific eight-pane performance baseline; run with --release -- --ignored --nocapture"]
fn alacritty_eight_pane_baseline() {
    let baseline = measure_eight_pane_baseline();
    println!("{}", baseline.json);
    assert_eq!(
        baseline.frames,
        CORPUS_SIZE * 8,
        "every pane must replay every corpus case"
    );
    assert!(baseline.total_bytes > 0, "corpus fed no bytes");
    assert!(baseline.snapshot_cells > 0, "snapshots carried no cells");
    for key in [
        "\"panes\":8",
        "\"streams_per_pane\":",
        "\"bytes\":",
        "\"throughput_mib_s\":",
        "\"input_to_snapshot_p50_us\":",
        "\"input_to_snapshot_p95_us\":",
        "\"lock_p95_us\":",
        "\"rss_end_bytes\":",
        "\"profile\":",
    ] {
        assert!(
            baseline.json.contains(key),
            "baseline JSON missing {key}: {}",
            baseline.json
        );
    }
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
