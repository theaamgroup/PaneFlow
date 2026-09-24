use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
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

/// What the watcher covers, kept beside the [`RecommendedWatcher`] so the
/// event loop can judge paths against it.
///
/// Issue #734: FSEvents reports absolute paths, so an ANCESTOR of the
/// worktree named `target` or `node_modules` made every event look like
/// noise. Noise is judged only below the longest matching root. FSEvents
/// reports the real path (`/private/var/...` for a watch on `/var/...`,
/// symlinks resolved), so each root is kept as given and canonicalised. A
/// path under neither form is judged whole, as before.
#[derive(Default)]
struct WatchScope {
    /// The worktree root, as given and canonicalised.
    worktree: Vec<PathBuf>,
    /// The repository root, whose `.git` holds the watched refs. Kept above
    /// `.git` so the `.git/<name>` noise pairs still match.
    repo: Vec<PathBuf>,
    /// Names of the top-level directories with a recursive watch (issue #735).
    watched: HashSet<OsString>,
}

impl WatchScope {
    fn new(worktree: &Path, repo_root: &Path) -> Self {
        Self {
            worktree: root_forms(worktree),
            repo: root_forms(repo_root),
            watched: HashSet::new(),
        }
    }

    /// `path` below its longest matching root, or `path` itself.
    fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        self.worktree
            .iter()
            .chain(&self.repo)
            .filter_map(|root| path.strip_prefix(root).ok())
            .min_by_key(|rest| rest.components().count())
            .unwrap_or(path)
    }

    fn event_relevant(&self, res: &notify::Result<Event>) -> bool {
        let Ok(event) = res else {
            return false;
        };
        match event.kind {
            EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => return false,
            _ => {}
        }
        event
            .paths
            .iter()
            .any(|path| !is_noise_path(self.relative(path)))
    }

    /// Issue #735: `build` watches the root non-recursively and adds a
    /// recursive watch only for the top-level directories that existed then.
    /// A directory created (or renamed into place) later is queued on
    /// `pending` so the loop can watch it; otherwise edits inside it never
    /// arrive.
    fn note_new_dirs(&self, res: &notify::Result<Event>, pending: &mut Vec<PathBuf>) {
        let Ok(event) = res else {
            return;
        };
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
        ) {
            return;
        }
        for path in &event.paths {
            let Some(name) = path.file_name() else {
                continue;
            };
            let top_level = path
                .parent()
                .is_some_and(|parent| self.worktree.iter().any(|root| root == parent));
            if !top_level
                || ignored_watch_dir(name)
                || self.watched.contains(name)
                || pending
                    .iter()
                    .any(|queued| queued.file_name() == Some(name))
                || !path.is_dir()
            {
                continue;
            }
            pending.push(path.clone());
        }
    }

    fn mark_watched(&mut self, dirs: &[PathBuf]) {
        self.watched.extend(
            dirs.iter()
                .filter_map(|dir| dir.file_name())
                .map(OsStr::to_owned),
        );
    }
}

fn root_forms(root: &Path) -> Vec<PathBuf> {
    let mut forms = vec![root.to_path_buf()];
    if let Ok(real) = std::fs::canonicalize(root)
        && real != root
    {
        forms.push(real);
    }
    forms
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

fn build(
    tx: mpsc::UnboundedSender<notify::Result<Event>>,
    worktree: PathBuf,
    repo_root: PathBuf,
) -> Option<(RecommendedWatcher, WatchScope)> {
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

    let mut scope = WatchScope::new(&worktree, &repo_root);
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
                scope.watched.insert(entry.file_name());
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
    Some((watcher, scope))
}

/// Add a recursive watch for each new top-level directory and return the
/// ones that took. On FSEvents every `watch` restarts the stream, so this
/// runs off the main thread, like [`build`].
fn watch_new_dirs(watcher: &mut RecommendedWatcher, dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .filter(|dir| match watcher.watch(dir, RecursiveMode::Recursive) {
            Ok(()) => {
                log::debug!("diff watcher: watching new directory {}", dir.display());
                true
            }
            Err(e) => {
                log::debug!("diff watcher: skip new {}: {e}", dir.display());
                false
            }
        })
        .cloned()
        .collect()
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
///
/// New top-level directories are queued as events arrive and handed to
/// `watch_dirs` right before each revalidate (issue #735). `watch_dirs`
/// returns the directories it watched, or `None` when the view is gone. An
/// edit inside a new directory that lands before its watch is still caught:
/// the revalidate that follows the watch reads the whole worktree.
async fn drive_refresh_loop<S, T, TF, W, WF, R>(
    mut events: S,
    mut scope: WatchScope,
    mut make_timer: T,
    mut watch_dirs: W,
    mut revalidate: R,
) where
    S: futures::Stream<Item = notify::Result<Event>> + Unpin,
    T: FnMut(Duration) -> TF,
    TF: Future + Unpin,
    W: FnMut(Vec<PathBuf>) -> WF,
    WF: Future<Output = Option<Vec<PathBuf>>>,
    R: FnMut() -> bool,
{
    let mut relevant_events = 0u64;
    let mut pending: Vec<PathBuf> = Vec::new();
    loop {
        // Idle: block until the next relevant event.
        loop {
            let Some(result) = events.next().await else {
                return;
            };
            scope.note_new_dirs(&result, &mut pending);
            if scope.event_relevant(&result) {
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
        let stream_ended = await_deadline(&mut events, make_timer(REFRESH_DEBOUNCE), |result| {
            scope.note_new_dirs(&result, &mut pending);
        })
        .await;
        if stream_ended {
            return;
        }

        // Revalidate, then cool down. A relevant event arriving during the
        // cooldown sets `dirty`; a dirty expiry revalidates immediately and
        // enters a fresh cooldown (trailing edge, still bounded to one refresh
        // per cooldown period). Irrelevant events do not set `dirty`.
        loop {
            if !pending.is_empty() {
                let Some(watched) = watch_dirs(std::mem::take(&mut pending)).await else {
                    return;
                };
                scope.mark_watched(&watched);
            }
            if !revalidate() {
                return;
            }
            let mut dirty = false;
            let stream_ended =
                await_deadline(&mut events, make_timer(REFRESH_COOLDOWN), |result| {
                    scope.note_new_dirs(&result, &mut pending);
                    if scope.event_relevant(&result) {
                        dirty = true;
                    }
                })
                .await;
            if stream_ended {
                return;
            }
            if !dirty && pending.is_empty() {
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

/// Watch new top-level directories on this view's watcher (issue #735).
///
/// The watcher is taken off the view so the `watch` calls run off the main
/// thread, then put back. `None` means the view is gone or its watch epoch
/// moved on (suspend), which ends the refresh loop; a watcher taken back
/// after a suspend is dropped, as `suspend` would have done.
async fn add_recursive_watches(
    this: gpui::WeakEntity<DiffView>,
    cx: gpui::AsyncApp,
    epoch: u64,
    dirs: Vec<PathBuf>,
) -> Option<Vec<PathBuf>> {
    let taken = cx.update(|cx| {
        this.update(cx, |view: &mut DiffView, _| {
            (view.watch_epoch == epoch).then(|| view._watchers.pop())
        })
        .ok()
        .flatten()
    })?;
    let Some(mut watcher) = taken else {
        return Some(Vec::new());
    };
    let (watcher, watched) = smol::unblock(move || {
        let watched = watch_new_dirs(&mut watcher, &dirs);
        (watcher, watched)
    })
    .await;
    let restored = cx.update(|cx| {
        this.update(cx, |view: &mut DiffView, _| {
            if view.watch_epoch != epoch {
                return false;
            }
            view._watchers.push(watcher);
            true
        })
        .unwrap_or(false)
    });
    restored.then_some(watched)
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
                let built = smol::unblock(move || build(tx, worktree, repo_root)).await;
                let Some((watcher, scope)) = built else {
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

                let watch_this = this.clone();
                let watch_cx = cx.clone();
                let watch_dirs = move |dirs: Vec<PathBuf>| {
                    add_recursive_watches(watch_this.clone(), watch_cx.clone(), epoch, dirs)
                };
                drive_refresh_loop(rx, scope, smol::Timer::after, watch_dirs, move || {
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

    fn created_dir(path: PathBuf) -> notify::Result<Event> {
        Ok(Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![path],
            attrs: Default::default(),
        })
    }

    /// Judged with no roots: the whole path, as the relative-path tests expect.
    fn event_relevant(res: &notify::Result<Event>) -> bool {
        WatchScope::default().event_relevant(res)
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
    fn accepts_source_changes_in_a_worktree_under_a_target_ancestor() {
        let worktree = Path::new("/Users/dev/work/target/myrepo");
        let scope = WatchScope::new(worktree, worktree);
        let relevant = |rel: &str| scope.event_relevant(&event(worktree.join(rel)));

        // The ancestor `target` is outside the worktree: not noise.
        assert!(relevant("src/main.rs"));
        assert!(relevant(".git/refs/heads/main"));
        // Noise inside the worktree is still noise.
        assert!(!relevant("target/debug/paneflow"));
        assert!(!relevant("src/target/generated.rs"));
        assert!(!relevant("web/node_modules/pkg/index.js"));
        assert!(!relevant(".git/objects/ab/hash"));
        assert!(!relevant("Cargo.lock"));

        // A linked worktree whose repository sits under `node_modules`: the
        // refs under the repository root are not noise either.
        let repo = Path::new("/Users/dev/node_modules/repo");
        let linked = WatchScope::new(Path::new("/Users/dev/wt"), repo);
        assert!(linked.event_relevant(&event(repo.join(".git/refs/heads/topic"))));
        assert!(!linked.event_relevant(&event(repo.join(".git/objects/ab/hash"))));

        // FSEvents reports the real path. The temp dir is `/var/...`, a
        // symlink to `/private/var/...`, so the canonical form must match too.
        let tmp = tempfile::tempdir().expect("tempdir");
        let given = tmp.path().join("target").join("myrepo");
        std::fs::create_dir_all(&given).expect("create worktree");
        let real = std::fs::canonicalize(&given).expect("canonicalize");
        let scope = WatchScope::new(&given, &given);
        assert!(scope.event_relevant(&event(given.join("src/main.rs"))));
        assert!(scope.event_relevant(&event(real.join("src/main.rs"))));
        assert!(!scope.event_relevant(&event(real.join("target/debug/paneflow"))));
    }

    /// Wait up to `timeout` for an event that satisfies `accept`.
    fn wait_for_event(
        rx: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
        timeout: Duration,
        mut accept: impl FnMut(&notify::Result<Event>) -> bool,
    ) -> bool {
        smol::block_on(async {
            let deadline = smol::Timer::after(timeout);
            futures::pin_mut!(deadline);
            loop {
                match futures::future::select(rx.next(), deadline.as_mut()).await {
                    Either::Left((Some(result), _)) => {
                        if accept(&result) {
                            return true;
                        }
                    }
                    Either::Left((None, _)) | Either::Right(_) => return false,
                }
            }
        })
    }

    /// Drives a real FSEvents watcher from [`build`]: a directory created after
    /// the build is queued by [`WatchScope::note_new_dirs`], watched by
    /// [`watch_new_dirs`], and a write inside it then arrives as a relevant
    /// event. Without the new watch notify drops that write (the root watch is
    /// non-recursive), so the final wait times out. FSEvents delivery took
    /// several seconds under parallel build load, hence the generous timeout.
    #[test]
    fn new_top_level_directory_is_watched() {
        const TIMEOUT: Duration = Duration::from_secs(30);
        let tmp = tempfile::tempdir().expect("tempdir");
        let worktree = tmp.path().join("repo");
        std::fs::create_dir_all(worktree.join("src")).expect("create worktree");
        let (tx, mut rx) = mpsc::unbounded();
        let (mut watcher, mut scope) =
            build(tx, worktree.clone(), worktree.clone()).expect("build watcher");
        assert!(scope.watched.contains(OsStr::new("src")));

        let fresh = worktree.join("fresh");
        std::fs::create_dir(&fresh).expect("create new top-level dir");
        let mut pending = Vec::new();
        assert!(
            wait_for_event(&mut rx, TIMEOUT, |result| {
                scope.note_new_dirs(result, &mut pending);
                !pending.is_empty()
            }),
            "no create event for the new directory"
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file_name(), Some(OsStr::new("fresh")));

        let watched = watch_new_dirs(&mut watcher, &pending);
        assert_eq!(watched.len(), 1, "watch was not added");
        scope.mark_watched(&watched);

        // Queued once: a later event for the same directory is not re-queued.
        let mut again = Vec::new();
        scope.note_new_dirs(&created_dir(watched[0].clone()), &mut again);
        assert!(again.is_empty());

        std::fs::write(fresh.join("lib.rs"), "pub fn f() {}\n").expect("write file");
        assert!(
            wait_for_event(&mut rx, TIMEOUT, |result| {
                scope.event_relevant(result)
                    && result.as_ref().is_ok_and(|event| {
                        event
                            .paths
                            .iter()
                            .any(|path| path.ends_with("fresh/lib.rs"))
                    })
            }),
            "no event for a file written inside the new directory"
        );
        drop(watcher);
    }

    #[test]
    fn noise_and_existing_directories_are_not_queued() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let worktree = tmp.path().join("repo");
        for dir in ["src", "src/nested", "target", "node_modules", "fresh"] {
            std::fs::create_dir_all(worktree.join(dir)).expect("create dir");
        }
        std::fs::write(worktree.join("README.md"), "").expect("write file");
        let mut scope = WatchScope::new(&worktree, &worktree);
        scope.mark_watched(&[worktree.join("src")]);

        let mut pending = Vec::new();
        for path in [
            worktree.join("src"),
            worktree.join("src/nested"),
            worktree.join("target"),
            worktree.join("node_modules"),
            worktree.join("README.md"),
        ] {
            scope.note_new_dirs(&created_dir(path), &mut pending);
        }
        assert!(pending.is_empty(), "queued {pending:?}");

        // A rename into place counts; an edit to the directory does not.
        scope.note_new_dirs(&event(worktree.join("fresh")), &mut pending);
        assert!(pending.is_empty());
        let renamed = Ok(Event {
            kind: EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To)),
            paths: vec![worktree.join("fresh")],
            attrs: Default::default(),
        });
        scope.note_new_dirs(&renamed, &mut pending);
        scope.note_new_dirs(&renamed, &mut pending);
        assert_eq!(pending, [worktree.join("fresh")]);
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

    /// Each `watch_dirs` call, with the revalidate count at that moment.
    type WatchCalls = Rc<std::cell::RefCell<Vec<(usize, Vec<PathBuf>)>>>;

    struct Harness {
        tx: Option<mpsc::UnboundedSender<notify::Result<Event>>>,
        fut: Pin<Box<dyn Future<Output = ()>>>,
        released: Rc<Cell<usize>>,
        revalidations: Rc<Cell<usize>>,
        events_taken: Rc<Cell<usize>>,
        watch_calls: WatchCalls,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_scope(WatchScope::default())
        }

        fn with_scope(scope: WatchScope) -> Self {
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
            let watch_calls: WatchCalls = Rc::default();
            let watch_dirs = {
                let watch_calls = watch_calls.clone();
                let revalidations = revalidations.clone();
                move |dirs: Vec<PathBuf>| {
                    watch_calls
                        .borrow_mut()
                        .push((revalidations.get(), dirs.clone()));
                    futures::future::ready(Some(dirs))
                }
            };
            let events = CountingStream {
                inner: rx,
                taken: events_taken.clone(),
            };
            Self {
                tx: Some(tx),
                fut: Box::pin(drive_refresh_loop(
                    events, scope, make_timer, watch_dirs, revalidate,
                )),
                released,
                revalidations,
                events_taken,
                watch_calls,
            }
        }

        fn send_event(&self, result: notify::Result<Event>) {
            self.tx
                .as_ref()
                .expect("sender dropped")
                .unbounded_send(result)
                .expect("send event");
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

    #[test]
    fn new_directory_is_watched_before_the_revalidate_that_follows_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let worktree = tmp.path().join("repo");
        std::fs::create_dir_all(worktree.join("fresh")).expect("create dir");
        std::fs::create_dir_all(worktree.join("later")).expect("create dir");
        let mut harness = Harness::with_scope(WatchScope::new(&worktree, &worktree));

        // The create opens the debounce; the watch lands before the refresh.
        harness.send_event(created_dir(worktree.join("fresh")));
        assert!(harness.poll().is_pending());
        assert!(harness.watch_calls.borrow().is_empty());
        harness.expire_timer();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 1);
        assert_eq!(
            *harness.watch_calls.borrow(),
            [(0, vec![worktree.join("fresh")])]
        );

        // A directory created during the cooldown is watched before the
        // trailing refresh. The first one is not watched twice.
        harness.send_event(created_dir(worktree.join("fresh")));
        harness.send_event(created_dir(worktree.join("later")));
        assert!(harness.poll().is_pending());
        harness.expire_timer();
        assert!(harness.poll().is_pending());
        assert_eq!(harness.revalidations(), 2);
        assert_eq!(
            *harness.watch_calls.borrow(),
            [
                (0, vec![worktree.join("fresh")]),
                (1, vec![worktree.join("later")]),
            ]
        );

        harness.tx = None;
        assert!(harness.poll().is_ready());
    }
}
