//! Filesystem watcher construction and event filtering for [`super::DiffView`].
//!
//! Keep path decisions component-based: notify yields native paths, so matching
//! string literals containing `/` silently misses Windows events.

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

type AttributionRefresh = (usize, u64, Vec<SessionMeta>);
type RevalidationResult = (Vec<usize>, Vec<AttributionRefresh>);

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
    worktrees: Vec<PathBuf>,
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
    for worktree in &worktrees {
        targets.push((worktree.clone(), RecursiveMode::NonRecursive));
        let Ok(entries) = std::fs::read_dir(worktree) else {
            continue;
        };
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
        "diff: watcher registered {registered}/{} paths across {} worktrees",
        targets.len(),
        worktrees.len()
    );
    Some(watcher)
}

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
        let mut timer = make_timer(REFRESH_DEBOUNCE);
        loop {
            match futures::future::select(events.next(), timer).await {
                Either::Left((Some(_), rest)) => timer = rest,
                Either::Left((None, _)) => return,
                Either::Right(_) => break,
            }
        }

        // Revalidate, then cool down. A relevant event arriving during the
        // cooldown sets `dirty`; a dirty expiry revalidates immediately and
        // enters a fresh cooldown (trailing edge, still bounded to one refresh
        // per cooldown period).
        loop {
            if !revalidate() {
                return;
            }
            let mut dirty = false;
            let mut timer = make_timer(REFRESH_COOLDOWN);
            loop {
                match futures::future::select(events.next(), timer).await {
                    Either::Left((Some(result), rest)) => {
                        timer = rest;
                        if event_relevant(&result) {
                            dirty = true;
                        }
                    }
                    Either::Left((None, _)) => return,
                    Either::Right(_) => break,
                }
            }
            if !dirty {
                break;
            }
            log::debug!("diff: watcher dirty during cooldown -> trailing revalidate");
        }
    }
}

impl DiffView {
    pub(super) fn start_watchers(&mut self, cx: &mut gpui::Context<Self>) {
        let mut worktrees: Vec<PathBuf> = self
            .columns
            .iter()
            .filter(|column| column.visible)
            .map(|column| column.path.clone())
            .collect();
        worktrees.sort();
        worktrees.dedup();
        let repo_root = self.repo_root.clone();
        let epoch = self.watch_epoch;
        let (tx, rx) = mpsc::unbounded::<notify::Result<Event>>();

        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                log::debug!("diff: start_watchers building watcher off-thread");
                let watcher = smol::unblock(move || build(tx, worktrees, repo_root)).await;
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

    pub(super) fn restart_watchers(&mut self, cx: &mut gpui::Context<Self>) {
        if self.suspended || !self.bootstrapped {
            return;
        }
        self.watch_epoch = self.watch_epoch.wrapping_add(1);
        self._watchers.clear();
        self.start_watchers(cx);
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

    pub(crate) fn resume_with_base(&mut self, base: Option<String>, cx: &mut gpui::Context<Self>) {
        let base_changed = match base {
            Some(base) if !base.is_empty() && base != self.base_ref => {
                self.base_ref = base;
                self.base_picker_open = false;
                true
            }
            _ => false,
        };

        if !self.suspended {
            if base_changed {
                self.start_loading(cx);
            }
            return;
        }

        self.suspended = false;
        if !self.bootstrapped {
            return;
        }
        self.start_watchers(cx);
        if base_changed {
            self.start_loading(cx);
        } else if !self.base_ref.is_empty() {
            self.revalidate(cx);
        }
    }

    fn revalidate(&mut self, cx: &mut gpui::Context<Self>) {
        let shared_base = self.base_ref.clone();
        let probes: Vec<(
            usize,
            PathBuf,
            String,
            String,
            u64,
            Option<super::super::git::ColumnFingerprint>,
        )> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.visible)
            .map(|(index, column)| {
                (
                    index,
                    column.path.clone(),
                    column
                        .base_override
                        .clone()
                        .unwrap_or_else(|| shared_base.clone()),
                    column.branch.clone(),
                    column.generation,
                    column.fingerprint.clone(),
                )
            })
            .collect();
        if probes.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let (changed, attribution): RevalidationResult = smol::unblock(move || {
                let mut changed = Vec::new();
                let mut attribution = Vec::new();
                for (index, path, base, branch, generation, stored) in probes {
                    let fresh = super::super::git::column_fingerprint(&path, &base);
                    if stored.as_ref() != Some(&fresh) {
                        changed.push(index);
                    } else {
                        let cwd = path.to_string_lossy();
                        attribution.push((
                            index,
                            generation,
                            crate::agent_sessions::attribution_for_column(&cwd, &branch),
                        ));
                    }
                }
                (changed, attribution)
            })
            .await;
            if changed.is_empty() && attribution.is_empty() {
                return;
            }
            let _ = cx.update(|cx| {
                this.update(cx, |view: &mut Self, cx| {
                    if view.suspended {
                        return;
                    }
                    let mut attribution_refreshed = false;
                    for (index, generation, sessions) in attribution {
                        let Some(col) = view.columns.get_mut(index) else {
                            continue;
                        };
                        if !col.visible || col.generation != generation {
                            continue;
                        }
                        col.attribution = sessions;
                        attribution_refreshed = true;
                    }
                    if !changed.is_empty() {
                        view.start_loading_columns(&changed, cx);
                    } else if attribution_refreshed {
                        cx.notify();
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

    struct Harness {
        tx: Option<mpsc::UnboundedSender<notify::Result<Event>>>,
        fut: Pin<Box<dyn Future<Output = ()>>>,
        released: Rc<Cell<usize>>,
        revalidations: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let (tx, rx) = mpsc::unbounded();
            let released = Rc::new(Cell::new(0usize));
            let created = Rc::new(Cell::new(0usize));
            let revalidations = Rc::new(Cell::new(0usize));
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
            Self {
                tx: Some(tx),
                fut: Box::pin(drive_refresh_loop(rx, make_timer, revalidate)),
                released,
                revalidations,
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
}
