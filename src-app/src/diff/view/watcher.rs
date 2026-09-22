use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::Either;
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::agent_sessions::SessionMeta;

use super::{DiffView, REFRESH_COOLDOWN, REFRESH_DEBOUNCE};

const WATCH_IGNORE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".jj",
    ".hg",
    ".svn",
    "dist",
    "build",
    ".next",
    ".cache",
    ".venv",
    "venv",
    "vendor",
];

enum Revalidation {
    Unchanged,
    Changed,
    Attribution(Vec<SessionMeta>),
}

pub(super) fn event_relevant(res: &notify::Result<Event>) -> bool {
    let Ok(event) = res else {
        return false;
    };
    match event.kind {
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => return false,
        _ => {}
    }
    event.paths.iter().any(|path| !is_noise_path(path))
}

fn component_eq(component: &OsStr, expected: &str) -> bool {
    component == OsStr::new(expected)
}

fn has_component(components: &[&OsStr], expected: &str) -> bool {
    components.iter().any(|part| component_eq(part, expected))
}

fn has_component_pair(components: &[&OsStr], first: &str, second: &str) -> bool {
    components
        .windows(2)
        .any(|pair| component_eq(pair[0], first) && component_eq(pair[1], second))
}

fn ignored_watch_dir(name: &OsStr) -> bool {
    WATCH_IGNORE_DIRS
        .iter()
        .any(|expected| component_eq(name, expected))
}

fn is_noise_path(path: &Path) -> bool {
    let components: Vec<&OsStr> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();

    has_component(&components, "target")
        || has_component(&components, "node_modules")
        || has_component_pair(&components, ".git", "objects")
        || has_component_pair(&components, ".git", "logs")
        || has_component_pair(&components, ".git", "index.lock")
        || ["FETCH_HEAD", "ORIG_HEAD", "COMMIT_EDITMSG", "MERGE_HEAD"]
            .iter()
            .any(|name| has_component_pair(&components, ".git", name))
        || path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(super::super::git::is_skipped_name)
}

pub(super) fn build(
    tx: mpsc::UnboundedSender<notify::Result<Event>>,
    worktree: PathBuf,
    repo_root: PathBuf,
) -> Option<RecommendedWatcher> {
    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let _ = tx.unbounded_send(res);
        },
        Config::default(),
    ) {
        Ok(watcher) => watcher,
        Err(e) => {
            log::warn!("diff watcher: failed to create: {e}");
            return None;
        }
    };

    let mut targets: Vec<(PathBuf, RecursiveMode)> = Vec::new();
    targets.push((worktree.clone(), RecursiveMode::NonRecursive));
    if let Ok(entries) = std::fs::read_dir(&worktree) {
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            let path = entry.path();
            let ignored = path.file_name().is_some_and(ignored_watch_dir);
            if !ignored {
                targets.push((path, RecursiveMode::Recursive));
            }
        }
    }

    let git_common = repo_root.join(".git");
    if git_common.is_dir() {
        targets.push((
            git_common.join("refs").join("heads"),
            RecursiveMode::Recursive,
        ));
        targets.push((git_common.join("packed-refs"), RecursiveMode::NonRecursive));
        targets.push((git_common.join("HEAD"), RecursiveMode::NonRecursive));
    }

    let mut registered = 0usize;
    for (path, mode) in &targets {
        match watcher.watch(path, *mode) {
            Ok(()) => registered += 1,
            Err(e) => log::debug!("diff watcher: skip {}: {e}", path.display()),
        }
    }
    log::debug!(
        "diff: watcher registered {registered}/{} paths for {}",
        targets.len(),
        worktree.display()
    );
    Some(watcher)
}

/// How many notify events one wake may take before yielding the frame.
/// The rest stay queued so a hot stream cannot stall the GPUI poll (issue #678).
const MAX_EVENTS_PER_WAKE: usize = 32;

/// Debounce/cooldown driver for the watcher task, extracted so tests can drive
/// it with an injected event stream and timer instead of a real
/// [`RecommendedWatcher`] and `smol::Timer` (issue #209).
///
/// `revalidate` is called at most once per [`REFRESH_COOLDOWN`] period and
/// returns whether the view is still alive (false stops the loop). A relevant
/// event arriving during the cooldown sets a dirty bit; when the cooldown
/// expires dirty, the loop revalidates immediately and enters a fresh cooldown,
/// so the trailing edge is never dropped while reload churn still costs at most
/// one deferred refresh per period, never a tight loop.
///
/// Each deadline is polled before the event stream. A ready timer wins even
/// when events are queued, and a wake drains at most [`MAX_EVENTS_PER_WAKE`]
/// of them before yielding (issue #678).
async fn drive_refresh_loop<S, T, TF, R>(mut events: S, mut make_timer: T, mut revalidate: R)
where
    S: futures::Stream<Item = notify::Result<Event>> + Unpin,
    T: FnMut(Duration) -> TF,
    TF: Future + Unpin,
    R: FnMut() -> bool,
{
    let mut relevant_events = 0u64;
    loop {
        // Idle: block until the next relevant event.
        loop {
            let Some(result) = events.next().await else {
                return;
            };
            if event_relevant(&result) {
                relevant_events += 1;
                if let Ok(event) = &result {
                    log::debug!(
                        "diff: watcher relevant event #{relevant_events} ({:?} {:?}) -> debounce",
                        event.kind,
                        event.paths.first()
                    );
                }
                break;
            }
        }

        // Debounce: a fixed window that coalesces the burst into one refresh.
        // The deadline is polled first, so queued events cannot hold it off.
        if await_deadline(&mut events, make_timer(REFRESH_DEBOUNCE), |_| {}).await {
            return;
        }

        // Revalidate, then cool down. A relevant event arriving during the
        // cooldown sets `dirty`; a dirty expiry revalidates immediately and
        // enters a fresh cooldown (trailing edge, still bounded to one refresh
        // per cooldown period). Irrelevant events do not set `dirty`.
        loop {
            if !revalidate() {
                return;
            }
            let mut dirty = false;
            let stream_ended =
                await_deadline(&mut events, make_timer(REFRESH_COOLDOWN), |result| {
                    if event_relevant(&result) {
                        dirty = true;
                    }
                })
                .await;
            if stream_ended {
                return;
            }
            if !dirty {
                break;
            }
            log::debug!("diff: watcher dirty during cooldown -> trailing revalidate");
        }
    }
}

/// Poll `timer` before the stream. Returns `true` when the stream ended.
///
/// `select` polls its first future first and, if that future is ready, does
/// not poll the second. A ready deadline therefore breaks without taking a
/// queued event. After [`MAX_EVENTS_PER_WAKE`] takes, yield so one wake cannot
/// empty the channel; leftover events stay queued and the dirty/debounce path
/// still refreshes.
async fn await_deadline<S, TF>(
    events: &mut S,
    mut timer: TF,
    mut on_event: impl FnMut(notify::Result<Event>),
) -> bool
where
    S: futures::Stream<Item = notify::Result<Event>> + Unpin,
    TF: Future + Unpin,
{
    let mut drained = 0usize;
    loop {
        match futures::future::select(timer, events.next()).await {
            Either::Left((_, unread)) => {
                // Timer won. `unread` was not polled, so the stream is not advanced.
                drop(unread);
                return false;
            }
            Either::Right((None, _)) => return true,
            Either::Right((Some(result), rest)) => {
                timer = rest;
                on_event(result);
                drained += 1;
                if drained >= MAX_EVENTS_PER_WAKE {
                    smol::future::yield_now().await;
                    drained = 0;
                }
            }
        }
    }
}

impl DiffView {
    pub(super) fn start_watchers(&mut self, cx: &mut gpui::Context<Self>) {
        let worktree = self.column.path.clone();
        let repo_root = self.repo_root.clone();
        let epoch = self.watch_epoch;
        let (tx, rx) = mpsc::unbounded::<notify::Result<Event>>();

        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                log::debug!("diff: start_watchers building watcher off-thread");
                let watcher = smol::unblock(move || build(tx, worktree, repo_root)).await;
                let Some(watcher) = watcher else {
                    log::warn!("diff: watcher build returned None");
                    return;
                };
                let installed = cx.update(|cx| {
                    this.update(cx, |view: &mut Self, _| {
                        if view.watch_epoch != epoch {
                            return false;
                        }
                        view._watchers.push(watcher);
                        true
                    })
                    .unwrap_or(false)
                });
                if !installed {
                    log::debug!("diff: watcher build superseded (epoch advanced) - dropped");
                    return;
                }

                drive_refresh_loop(rx, smol::Timer::after, move || {
                    cx.update(|cx| {
                        this.update(cx, |view: &mut Self, cx| {
                            if view.watch_epoch != epoch {
                                return false;
                            }
                            view.revalidate(cx);
                            true
                        })
                        .unwrap_or(false)
                    })
                })
                .await;
            },
        )
        .detach();
    }

    pub fn suspend(&mut self, _cx: &mut gpui::Context<Self>) {
        if self.suspended {
            return;
        }
        self.suspended = true;
        self.watch_epoch = self.watch_epoch.wrapping_add(1);
        self._watchers.clear();
    }

    pub fn resume(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.suspended {
            return;
        }
        self.suspended = false;
        if !self.bootstrapped {
            return;
        }
        self.start_watchers(cx);
        if !self.base_ref.is_empty() {
            self.revalidate(cx);
        }
    }

    fn revalidate(&mut self, cx: &mut gpui::Context<Self>) {
        let base = self.base_ref.clone();
        let path = self.column.path.clone();
        let branch = self.column.branch.clone();
        let generation = self.column.generation;
        let stored = self.column.fingerprint.clone();
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(move || {
                let fresh = super::super::git::column_fingerprint(&path, &base);
                if stored.as_ref() != Some(&fresh) {
                    return Revalidation::Changed;
                }
                let cwd = path.to_string_lossy();
                let sessions = crate::agent_sessions::attribution_for_column(&cwd, &branch);
                if sessions.is_empty() {
                    Revalidation::Unchanged
                } else {
                    Revalidation::Attribution(sessions)
                }
            })
            .await;
            if matches!(outcome, Revalidation::Unchanged) {
                return;
            }
            let _ = cx.update(|cx| {
                this.update(cx, |view: &mut Self, cx| {
                    if view.suspended {
                        return;
                    }
                    match outcome {
                        Revalidation::Changed => view.start_loading(cx),
                        Revalidation::Attribution(sessions) => {
                            if view.column.generation == generation {
                                view.column.attribution = sessions;
                                cx.notify();
                            }
                        }
                        Revalidation::Unchanged => {}
                    }
                })
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(path: PathBuf) -> notify::Result<Event> {
        Ok(Event {
            kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            paths: vec![path],
            attrs: Default::default(),
        })
    }

    #[test]
    fn ignores_noise_directories_using_native_components() {
        assert!(!event_relevant(&event(
            ["repo", "target", "debug", "paneflow"].iter().collect()
        )));
        assert!(!event_relevant(&event(
            ["repo", "node_modules", "pkg", "index.js"].iter().collect()
        )));
        assert!(!event_relevant(&event(
            ["repo", ".git", "objects", "ab", "hash"].iter().collect()
        )));
    }

    #[test]
    fn ignores_git_transient_files_and_lockfiles() {
        assert!(!event_relevant(&event(
            ["repo", ".git", "FETCH_HEAD"].iter().collect()
        )));
        assert!(!event_relevant(&event(
            ["repo", "Cargo.lock"].iter().collect()
        )));
    }

    #[test]
    fn accepts_source_and_ref_changes() {
        assert!(event_relevant(&event(
            ["repo", "src", "main.rs"].iter().collect()
        )));
        assert!(event_relevant(&event(
            ["repo", ".git", "refs", "heads", "main"].iter().collect()
        )));
    }
    use std::cell::Cell;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use futures::Stream;

    /// Manually released fake timer: ticket `n` completes once the harness has
    /// released more than `n` timers, so the test controls exactly when each
    /// debounce/cooldown window "expires".
    struct ManualTimer {
        ticket: usize,
        released: Rc<Cell<usize>>,
    }

    impl Future for ManualTimer {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            if self.ticket < self.released.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    /// Counts items the driver actually pulled. Dropping an unpolled `next`
    /// future must not increment this.
    struct CountingStream<S> {
        inner: S,
        taken: Rc<Cell<usize>>,
    }

    impl<S> Stream for CountingStream<S>
    where
        S: Stream + Unpin,
    {
        type Item = S::Item;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    this.taken.set(this.taken.get() + 1);
                    Poll::Ready(Some(item))
                }
                other => other,
            }
        }
    }

    struct Harness {
        tx: Option<mpsc::UnboundedSender<notify::Result<Event>>>,
        fut: Pin<Box<dyn Future<Output = ()>>>,
        released: Rc<Cell<usize>>,
        revalidations: Rc<Cell<usize>>,
        events_taken: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let (tx, rx) = mpsc::unbounded();
            let released = Rc::new(Cell::new(0usize));
            let created = Rc::new(Cell::new(0usize));
            let revalidations = Rc::new(Cell::new(0usize));
            let events_taken = Rc::new(Cell::new(0usize));
            let make_timer = {
                let released = released.clone();
                move |_duration: Duration| {
                    let ticket = created.get();
                    created.set(ticket + 1);
                    ManualTimer {
                        ticket,
                        released: released.clone(),
                    }
                }
            };
            let revalidate = {
                let revalidations = revalidations.clone();
                move || {
                    revalidations.set(revalidations.get() + 1);
                    true
                }
            };
            let events = CountingStream {
                inner: rx,
                taken: events_taken.clone(),
            };
            Self {
                tx: Some(tx),
                fut: Box::pin(drive_refresh_loop(events, make_timer, revalidate)),
                released,
                revalidations,
                events_taken,
            }
        }

        fn send(&self, path: PathBuf) {
            self.tx
                .as_ref()
                .expect("sender dropped")
                .unbounded_send(event(path))
                .expect("send event");
        }

        /// Expire the next outstanding fake timer.
        fn expire_timer(&self) {
            self.released.set(self.released.get() + 1);
        }

        fn poll(&mut self) -> Poll<()> {
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            self.fut.as_mut().poll(&mut cx)
        }

        fn revalidations(&self) -> usize {
            self.revalidations.get()
        }

        fn events_taken(&self) -> usize {
            self.events_taken.get()
        }
    }

    fn relevant_path() -> PathBuf {
        ["repo", "src", "main.rs"].iter().collect()
    }

    #[test]
    fn event_during_cooldown_triggers_exactly_one_trailing_revalidate() {
        let mut harness = Harness::new();

        // Leading event -> debounce window opens (timer #0).
        harness.send(relevant_path());
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 0);

        // Debounce expires -> first revalidate, cooldown (timer #1) starts.
        harness.expire_timer();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);

        // A relevant event lands during the cooldown: consumed, no refresh yet.
        harness.send(["repo", "src", "lib.rs"].iter().collect());
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);

        // Cooldown expires dirty -> exactly one trailing revalidate, and a
        // fresh cooldown (timer #2) starts.
        harness.expire_timer();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 2);

        // The fresh cooldown expires clean -> back to idle, no extra refresh.
        harness.expire_timer();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 2);

        // Stream end terminates the task with no further revalidation.
        harness.tx = None;
        assert!(harness.poll().is_ready());
        assert_eq!(harness.revalidations(), 2);
    }

    #[test]
    fn clean_cooldown_returns_to_idle_without_extra_revalidate() {
        let mut harness = Harness::new();

        harness.send(relevant_path());
        assert!(harness.poll().is_pending());
        harness.expire_timer(); // debounce expires
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);

        // No events during the cooldown: expiry returns to idle silently.
        harness.expire_timer();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);

        // A later leading event still starts a fresh debounce cycle.
        harness.send(relevant_path());
        assert!(harness.poll().is_pending());
        harness.expire_timer(); // debounce expires
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 2);

        harness.tx = None;
        assert!(harness.poll().is_ready());
        assert_eq!(harness.revalidations(), 2);
    }

    #[test]
    fn irrelevant_event_during_cooldown_does_not_mark_dirty() {
        let mut harness = Harness::new();

        harness.send(relevant_path());
        assert!(harness.poll().is_pending());
        harness.expire_timer(); // debounce expires
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);

        // Noise (build churn under target/) during the cooldown must not
        // schedule a trailing revalidate.
        harness.send(["repo", "target", "debug", "paneflow"].iter().collect());
        assert!(harness.poll().is_pending());
        harness.expire_timer(); // cooldown expires clean
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);

        harness.tx = None;
        assert!(harness.poll().is_ready());
        assert_eq!(harness.revalidations(), 1);
    }

    #[test]
    fn debounce_deadline_is_polled_while_events_are_waiting() {
        let mut harness = Harness::new();

        // Leading event opens the debounce window (timer #0) and is consumed.
        harness.send(relevant_path());
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 0);
        let taken_idle = harness.events_taken();
        assert_eq!(taken_idle, 1);

        // Expire that deadline before the next poll, with a backlog well above
        // one wake's drain cap already sitting in the channel.
        harness.expire_timer();
        let queued = 96usize;
        for _ in 0..queued {
            harness.send(relevant_path());
        }

        // One poll must refresh because the deadline is ready, and must return
        // Pending while events are still queued. A stream that stays Ready
        // would hang the old loop; this finite backlog is enough when the
        // timer is polled first and a wake stops after 32 takes.
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);
        let drained = harness.events_taken() - taken_idle;
        assert!(
            drained <= 32,
            "one wake drained {drained} events, cap is 32"
        );
        assert!(
            drained < queued,
            "deadline poll consumed the whole backlog ({drained})"
        );

        // No new expiry: the loop must not spin through the rest or refresh again.
        let taken_mid = harness.events_taken();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);
        let drained_again = harness.events_taken() - taken_mid;
        assert!(
            drained_again <= 32,
            "follow-up wake drained {drained_again} events"
        );
        assert!(
            harness.events_taken() - taken_idle < queued,
            "follow-up wake emptied the backlog"
        );
    }
}
