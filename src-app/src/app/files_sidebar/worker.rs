use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use smol::channel::{Receiver, Sender};

use crate::app::files_tree::{FilesTreeState, ListingAuthority, read_dir_listing};

const DEBOUNCE: Duration = Duration::from_millis(40);
const MAX_LATENCY: Duration = Duration::from_millis(160);
const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

pub(super) struct TreeUpdate {
    pub revision: u64,
    pub tree: FilesTreeState,
    pub watcher_available: bool,
}

pub(super) struct FilesWorker {
    requests: Sender<Request>,
}

enum Request {
    Expanded(u64, Vec<PathBuf>),
    Changed(notify::Result<Event>),
}

impl FilesWorker {
    pub(super) fn start(root: PathBuf, expanded: Vec<PathBuf>) -> (Self, Receiver<TreeUpdate>) {
        let (requests, incoming) = smol::channel::unbounded();
        let (updates, receiver) = smol::channel::unbounded();
        let events = requests.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("files-tree".into())
            .spawn(move || {
                let scanner = Scanner {
                    tree: FilesTreeState::root_shell(root),
                    watcher: None,
                    watched: HashSet::new(),
                    dirty: HashSet::new(),
                    authority: HashMap::new(),
                };
                smol::block_on(run(scanner, expanded, incoming, updates, events));
            })
        {
            log::error!("files worker unavailable: {error}");
        }
        (Self { requests }, receiver)
    }

    pub(super) fn set_expanded(&self, revision: u64, expanded: Vec<PathBuf>) {
        let _ = self
            .requests
            .try_send(Request::Expanded(revision, expanded));
    }
}

impl Drop for FilesWorker {
    fn drop(&mut self) {
        self.requests.close();
    }
}

struct Scanner {
    tree: FilesTreeState,
    watcher: Option<RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    dirty: HashSet<PathBuf>,
    /// How far each cached listing can be trusted about absent children.
    authority: HashMap<PathBuf, ListingAuthority>,
}

impl Scanner {
    fn set_expanded(&mut self, expanded: Vec<PathBuf>) {
        self.tree.expanded = expanded
            .into_iter()
            .filter(|path| path.starts_with(&self.tree.root))
            .collect();
        self.tree.expanded.insert(self.tree.root.clone());
    }

    fn scan(&mut self, cancelled: impl Fn() -> bool) -> bool {
        let root = &self.tree.root;
        let mut pending = vec![(root.clone(), true)];
        let mut active = HashSet::new();
        while let Some((dir, expanded_ancestors)) = pending.pop() {
            if cancelled() {
                return false;
            }
            active.insert(dir.clone());
            if !self.watched.contains(&dir)
                && let Some(watcher) = self.watcher.as_mut()
                && watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok()
            {
                self.watched.insert(dir.clone());
                self.dirty.insert(dir.clone());
            }
            // Issue #238: `read_dir_listing` caps the raw `read_dir` walk
            // before the ignore filter, so a pathological directory bounds
            // this scan the same way it bounded the old on-expand read.
            if self.dirty.remove(&dir) || !self.tree.children.contains_key(&dir) {
                let (nodes, authority) = read_dir_listing(root, &dir);
                self.tree.children.insert(dir.clone(), nodes);
                self.authority.insert(dir.clone(), authority);
            }
            if let Some(children) = self.tree.children.get(&dir) {
                for node in children {
                    let expanded = expanded_ancestors && self.tree.expanded.contains(&node.path);
                    if node.is_dir && (expanded || self.tree.children.contains_key(&node.path)) {
                        pending.push((node.path.clone(), expanded));
                    }
                }
            }
            // A truncated or failed listing says nothing about the children
            // it does not name: keep every already-loaded child directory
            // active so its listing and watch are not evicted as deleted.
            if !self.listing_is_complete(&dir) {
                let unlisted: Vec<PathBuf> = self
                    .tree
                    .children
                    .keys()
                    .filter(|child| child.parent() == Some(dir.as_path()))
                    .filter(|child| {
                        !self.tree.children[&dir]
                            .iter()
                            .any(|node| node.is_dir && node.path == **child)
                    })
                    .cloned()
                    .collect();
                for child in unlisted {
                    let expanded = expanded_ancestors && self.tree.expanded.contains(&child);
                    pending.push((child, expanded));
                }
            }
        }
        self.tree.expanded.retain(|path| {
            path.ancestors()
                .take_while(|path| *path != root)
                .all(|path| {
                    let Some(parent) = path.parent() else {
                        return true;
                    };
                    if self
                        .authority
                        .get(parent)
                        .is_some_and(|authority| *authority != ListingAuthority::Complete)
                    {
                        return true;
                    }
                    self.tree.children.get(parent).is_none_or(|nodes| {
                        nodes.iter().any(|node| node.is_dir && node.path == path)
                    })
                })
        });
        self.tree.children.retain(|dir, _| active.contains(dir));
        self.authority.retain(|dir, _| active.contains(dir));
        self.watched.retain(|dir| {
            if active.contains(dir) {
                return true;
            }
            if let Some(watcher) = self.watcher.as_mut() {
                let _ = watcher.unwatch(dir);
            }
            false
        });
        self.dirty.clear();
        !cancelled()
    }

    fn listing_is_complete(&self, dir: &std::path::Path) -> bool {
        self.authority
            .get(dir)
            .is_none_or(|authority| *authority == ListingAuthority::Complete)
    }

    fn watcher_available(&self) -> bool {
        self.tree
            .children
            .keys()
            .all(|dir| self.watched.contains(dir))
    }

    fn rescan(&mut self) {
        self.dirty.extend(self.tree.children.keys().cloned());
    }

    /// Mark every directory whose last listing failed or skipped an entry
    /// for a re-read, and report whether there was one. A watch does not
    /// cover recovery from a read error: an entry skipped mid-scan, a type
    /// that could not be read, or a directory that was unreadable for a
    /// moment produces no filesystem event once it is readable again, so
    /// the fallback timer retries those listings even while the watcher is
    /// available. A `Capped` listing is left alone: re-reading a directory
    /// that is over `MAX_DIRECTORY_ENTRIES` cannot make it whole, and
    /// would publish a fresh snapshot every interval for nothing.
    fn retry_incomplete_listings(&mut self) -> bool {
        let incomplete: Vec<PathBuf> = self
            .authority
            .iter()
            .filter(|(_, authority)| {
                matches!(
                    authority,
                    ListingAuthority::Truncated | ListingAuthority::Failed
                )
            })
            .map(|(dir, _)| dir.clone())
            .collect();
        let any = !incomplete.is_empty();
        self.dirty.extend(incomplete);
        any
    }

    fn invalidate_subtree(&mut self, path: &std::path::Path) {
        self.dirty.extend(
            self.tree
                .children
                .keys()
                .filter(|dir| dir.starts_with(path))
                .cloned(),
        );
        self.watched.retain(|dir| {
            if !dir.starts_with(path) {
                return true;
            }
            if let Some(watcher) = self.watcher.as_mut() {
                let _ = watcher.unwatch(dir);
            }
            false
        });
    }

    fn changed(&mut self, event: notify::Result<Event>) -> bool {
        let event = match event {
            Ok(event) if !event.need_rescan() && !event.paths.is_empty() => event,
            Err(error) => {
                log::warn!("files watcher failed: {error}");
                self.watcher = None;
                self.watched.clear();
                self.rescan();
                return true;
            }
            _ => {
                self.rescan();
                return true;
            }
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        for path in event.paths {
            if matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) {
                self.invalidate_subtree(&path);
            }
            if self.tree.children.contains_key(&path) {
                self.dirty.insert(path.clone());
            }
            if let Some(parent) = path.parent() {
                if self.tree.children.contains_key(parent) {
                    self.dirty.insert(parent.to_path_buf());
                }
                if path.file_name().is_some_and(|name| name == ".gitignore") {
                    self.dirty.extend(
                        self.tree
                            .children
                            .keys()
                            .filter(|dir| dir.starts_with(parent))
                            .cloned(),
                    );
                }
            }
        }
        !self.dirty.is_empty()
    }
}

async fn run(
    mut scanner: Scanner,
    expanded: Vec<PathBuf>,
    requests: Receiver<Request>,
    updates: Sender<TreeUpdate>,
    events: Sender<Request>,
) {
    scanner.set_expanded(expanded);
    let mut revision = 0;
    let mut ready = true;
    let mut first_event: Option<Instant> = None;
    let mut deadline = Instant::now() + FALLBACK_INTERVAL;
    while !requests.is_closed() && !updates.is_closed() {
        if ready {
            if scanner.watcher.is_none() {
                let events = events.clone();
                scanner.watcher = notify::recommended_watcher(move |event| {
                    let _ = events.try_send(Request::Changed(event));
                })
                .inspect_err(|error| log::warn!("files watcher unavailable: {error}"))
                .ok();
            }
            if !scanner.scan(|| requests.is_closed() || updates.is_closed()) {
                break;
            }
            if updates
                .try_send(TreeUpdate {
                    revision,
                    tree: scanner.tree.clone(),
                    watcher_available: scanner.watcher_available(),
                })
                .is_err()
            {
                break;
            }
            ready = false;
            first_event = None;
            deadline = Instant::now() + FALLBACK_INTERVAL;
        }
        let mut next = smol::future::or(async { requests.recv().await.ok() }, async {
            smol::Timer::at(deadline).await;
            None
        })
        .await;
        if next.is_some() {
            for index in 0..1024 {
                let request = if index == 0 {
                    next.take()
                } else {
                    requests.try_recv().ok()
                };
                match request {
                    Some(Request::Expanded(next_revision, expanded))
                        if next_revision >= revision =>
                    {
                        revision = next_revision;
                        scanner.set_expanded(expanded);
                        ready = true;
                    }
                    Some(Request::Changed(event)) => {
                        if scanner.changed(event) {
                            let now = Instant::now();
                            let first = *first_event.get_or_insert(now);
                            deadline = (now + DEBOUNCE).min(first + MAX_LATENCY);
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
        }
        if Instant::now() >= deadline {
            if first_event.is_some() || !scanner.watcher_available() {
                if first_event.is_none() {
                    scanner.rescan();
                }
                ready = true;
            } else if scanner.retry_incomplete_listings() {
                ready = true;
            } else {
                deadline = Instant::now() + FALLBACK_INTERVAL;
            }
        }
    }
}

#[cfg(test)]
mod tests;
