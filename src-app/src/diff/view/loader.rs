use super::*;

impl DiffView {
    pub(super) fn start_loading(&mut self, cx: &mut Context<Self>) {
        let base = self.base_ref.clone();
        let mode = self.last_effective_mode;
        let theme = crate::theme::active_theme();
        let theme_generation = crate::theme::theme_generation();
        let col = &mut self.column;
        col.generation = col.generation.wrapping_add(1);
        col.loading_mode = None;
        col.loading_theme_generation = Some(theme_generation);
        let generation = col.generation;
        let path = col.path.clone();
        let branch = col.branch.clone();
        log::debug!("diff: start_loading base={base:?} ({branch})");
        if base.is_empty() {
            col.state = ColumnState::Failed("Select a base branch".to_string());
            cx.notify();
            return;
        }
        log::debug!("diff: ({branch}) task SPAWNED (gen={generation})");
        cx.spawn(async move |this, cx| {
            log::debug!("diff: ({branch}) task STARTED (polled)");
            let bc = branch.clone();
            let built = smol::unblock(move || {
                // US-016: snapshot the fingerprint BEFORE reading the tree, so a
                // commit landing mid-build makes the stored fingerprint LAG the
                // rows - `revalidate` then sees HEAD moved and reloads (a harmless
                // extra reload) rather than matching a stale fingerprint and
                // showing pre-commit rows as current (the unsafe direction).
                // Issue #309: fingerprint, diff, and file stats run under ONE git
                // budget with a shared toplevel + merge-base, so a wedged git fails
                // the column once, not once per pipeline. Upstream a8d55f74 runs
                // these as three independent pipelines; this fork must not, and
                // `column_load_runs_under_one_git_budget` greps this file to say so.
                let t0 = Instant::now();
                let super::super::git::ColumnLoad {
                    fingerprint,
                    diff,
                    file_stats,
                } = super::super::git::load_column(&path, &base);
                let file_stats = match file_stats {
                    Ok(stats) => stats,
                    Err(e) => {
                        // Counters fall back to the diff's own hunk counts
                        // below; never an empty map read as "no changes".
                        log::warn!("diff: ({bc}) file stats unavailable, using hunk counts: {e}");
                        std::collections::HashMap::new()
                    }
                };
                log::debug!(
                    "diff: ({bc}) computed {} files in {:?} (error={:?})",
                    diff.files.len(),
                    t0.elapsed(),
                    diff.error
                );
                if let Some(e) = diff.error {
                    return Built::Failed(e);
                }
                let t1 = Instant::now();
                let syntax = SYNTAX_HIGHLIGHT_ENABLED
                    .then(|| super::super::syntax::DiffSyntax::from_theme(&theme));
                let row_caches = build_file_row_caches(&diff.files, syntax.as_ref());
                let mut rows = build_rows_for_mode_with_caches(&diff.files, mode, &row_caches);
                let mut files: Vec<FileEntry> = diff
                    .files
                    .iter()
                    .map(|f| {
                        let (added, removed) = file_stats
                            .get(&(f.change, f.path.clone()))
                            .map(|stat| (stat.added, stat.removed))
                            .unwrap_or_else(|| f.line_counts());
                        FileEntry {
                            path: f.path.clone(),
                            change: f.change,
                            old_path: f.old_path.clone(),
                            added,
                            removed,
                            is_binary: f.is_binary,
                        }
                    })
                    .collect();
                if diff.truncated {
                    let notice = super::super::git::truncation_notice();
                    append_truncation_notice(&mut rows, &notice);
                    files.push(truncation_file_entry(notice));
                }
                log::debug!(
                    "diff: ({bc}) built {} rows for {} in {:?}",
                    match &rows {
                        BuiltModeRows::Unified { rows, .. } => rows.len(),
                        BuiltModeRows::Split { rows, .. } => rows.len(),
                    },
                    mode.label(),
                    t1.elapsed()
                );
                // Issue #1094: session attribution is not part of the build.
                // It runs vendor CLIs with a 30 s budget, so it starts after
                // these rows land (`request_attribution`), never before. Until
                // that run lands, the header may briefly show the previous
                // generation's attribution for this same column and branch.
                Built::Loaded {
                    rows,
                    file_count: files.len(),
                    files,
                    files_full: diff.files,
                    row_caches,
                    theme_generation,
                    fingerprint: Box::new(fingerprint),
                }
            })
            .await;
            log::debug!("diff: ({branch}) off-thread done, applying on main thread");
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx| {
                    let col = &mut view.column;
                    if col.generation != generation {
                        log::debug!(
                            "diff: ({branch}) superseded - task gen={generation} != gen={}",
                            col.generation
                        );
                        return;
                    }
                    let new_state = match built {
                        Built::Failed(e) => {
                            log::warn!("diff: ({branch}) FAILED: {e}");
                            // Drop the stored fingerprint so the next revalidate
                            // cannot match a stale complete read and leave the
                            // column failed (issue #891).
                            col.fingerprint = None;
                            col.loading_mode = None;
                            col.loading_theme_generation = None;
                            ColumnState::Failed(e)
                        }
                        Built::Loaded {
                            rows,
                            file_count,
                            files,
                            files_full,
                            row_caches,
                            theme_generation,
                            fingerprint,
                        } => {
                            log::debug!("diff: ({branch}) LOADED ({file_count} files)");
                            col.fingerprint = Some(*fingerprint);
                            col.loading_mode = None;
                            col.loading_theme_generation = None;
                            match rows {
                                BuiltModeRows::Unified { rows, anchors } => ColumnState::Loaded {
                                    unified: Some(Rc::new(rows)),
                                    split: None,
                                    file_count,
                                    files: Rc::new(files),
                                    anchors_unified: Some(Rc::new(anchors)),
                                    anchors_split: None,
                                    files_full: Arc::new(files_full),
                                    row_caches: Arc::new(row_caches),
                                    theme_generation,
                                },
                                BuiltModeRows::Split { rows, anchors } => ColumnState::Loaded {
                                    unified: None,
                                    split: Some(Rc::new(rows)),
                                    file_count,
                                    files: Rc::new(files),
                                    anchors_unified: None,
                                    anchors_split: Some(Rc::new(anchors)),
                                    files_full: Arc::new(files_full),
                                    row_caches: Arc::new(row_caches),
                                    theme_generation,
                                },
                            }
                        }
                    };
                    let loaded = matches!(new_state, ColumnState::Loaded { .. });
                    col.state = new_state;
                    col.recompute_display_for(mode);
                    col.clear_display_mode(mode.other());
                    view.body_menu = None;
                    view.schedule_mode_build(mode.other(), cx);
                    if loaded {
                        view.request_attribution(cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn ensure_mode_loaded(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        let col = &mut self.column;
        if col.has_rows_for_mode(mode) {
            if col.loading_mode == Some(mode) {
                col.loading_mode = None;
            }
            if !col.has_display_for_mode(mode) {
                col.recompute_display_for(mode);
            }
            return;
        }
        self.schedule_mode_build(mode, cx);
    }

    fn schedule_mode_build(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        let Some(build) = self.column.begin_mode_build(mode) else {
            return;
        };
        log::debug!(
            "diff: scheduling lazy {} row build (gen={})",
            mode.label(),
            build.generation
        );
        let truncation_notice = build.truncation_notice.clone();
        cx.spawn(async move |this, cx| {
            let files = build.files.clone();
            let row_caches = build.row_caches.clone();
            let rows = smol::unblock(move || {
                let mut rows =
                    build_rows_for_mode_with_caches(files.as_ref(), mode, row_caches.as_ref());
                if let Some(notice) = truncation_notice {
                    append_truncation_notice(&mut rows, &notice);
                }
                rows
            })
            .await;
            let _ = cx.update(|cx| {
                this.update(cx, |view: &mut Self, cx| {
                    if view.column.finish_mode_build(&build, rows) {
                        cx.notify();
                    }
                })
            });
        })
        .detach();
    }
}

/// The inputs one lazy mode build read. The task keeps the `Arc`s alive, so
/// a later load can never reuse their addresses and pass as the same state.
struct LazyModeBuild {
    mode: ViewMode,
    generation: u64,
    files: Arc<Vec<super::super::git::FileDiff>>,
    row_caches: Arc<Vec<FileRowCache>>,
    /// Sentence shown when the file list was capped. Not one of `files`.
    truncation_notice: Option<String>,
}

/// What a finished lazy mode build may do with its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LazyBuildVerdict {
    /// Built from the state now shown: install the rows.
    Install,
    /// Superseded. Another build owns `loading_mode`, so leave it set.
    Discard,
    /// The column holds no loaded rows any more: release `loading_mode`.
    Release,
}

impl Column {
    /// Claim `loading_mode` for `mode` and snapshot the loaded inputs, or
    /// `None` when the rows exist, a build is already running, or nothing is
    /// loaded.
    fn begin_mode_build(&mut self, mode: ViewMode) -> Option<LazyModeBuild> {
        if self.has_rows_for_mode(mode) || self.loading_mode.is_some() {
            return None;
        }
        let ColumnState::Loaded {
            files_full,
            row_caches,
            files,
            ..
        } = &self.state
        else {
            return None;
        };
        let build = LazyModeBuild {
            mode,
            generation: self.generation,
            files: files_full.clone(),
            row_caches: row_caches.clone(),
            truncation_notice: listed_truncation_notice(files),
        };
        self.loading_mode = Some(mode);
        Some(build)
    }

    /// Issue #736: `start_loading` bumps `generation` but keeps the old
    /// `Loaded` state on screen, so a mode switch mid-reload builds from the
    /// OLD `files_full` under the NEW generation. Generation and
    /// `loading_mode` alone then accept those rows into the fresh state.
    /// Matching the exact `Arc`s the build read rejects them.
    fn lazy_build_verdict(&self, build: &LazyModeBuild) -> LazyBuildVerdict {
        if self.generation != build.generation || self.loading_mode != Some(build.mode) {
            return LazyBuildVerdict::Discard;
        }
        match &self.state {
            ColumnState::Loaded {
                files_full,
                row_caches,
                ..
            } => {
                if Arc::ptr_eq(files_full, &build.files)
                    && Arc::ptr_eq(row_caches, &build.row_caches)
                {
                    LazyBuildVerdict::Install
                } else {
                    LazyBuildVerdict::Discard
                }
            }
            _ => LazyBuildVerdict::Release,
        }
    }

    /// Apply a finished lazy build. Returns whether rows were installed.
    fn finish_mode_build(&mut self, build: &LazyModeBuild, rows: BuiltModeRows) -> bool {
        match self.lazy_build_verdict(build) {
            LazyBuildVerdict::Discard => false,
            LazyBuildVerdict::Release => {
                self.loading_mode = None;
                false
            }
            LazyBuildVerdict::Install => {
                self.loading_mode = None;
                self.insert_mode_rows(rows);
                self.recompute_display_for(build.mode);
                true
            }
        }
    }
}

/// The capped-list sentence, painted as its own header. The anchor path is not
/// a [`super::super::git::FileDiff`], so copying that row does not emit a diff.
fn append_truncation_notice(rows: &mut BuiltModeRows, notice: &str) {
    let header = super::super::rows::HeaderParts {
        dir_prefix: "".into(),
        basename: notice.into(),
        added: 0,
        removed: 0,
    };
    match rows {
        BuiltModeRows::Unified { rows, anchors } => {
            anchors.push((notice.to_string(), rows.len()));
            rows.push(DisplayRow {
                kind: RowKind::FileHeader,
                text: format!("{notice}   +0 -0").into(),
                old_no: None,
                new_no: None,
                syntax_runs: Vec::new(),
                header: Some(header),
                fold_key: None,
                fold_base_start: None,
                fold_new_start: None,
                fold_count: 0,
                folded_rows: Vec::new(),
            });
        }
        BuiltModeRows::Split { rows, anchors } => {
            anchors.push((notice.to_string(), rows.len()));
            rows.push(SplitRow::Header(header));
        }
    }
}

fn truncation_file_entry(notice: String) -> FileEntry {
    FileEntry {
        path: notice,
        change: super::super::git::FileChange::Modified,
        old_path: None,
        added: 0,
        removed: 0,
        is_binary: true,
    }
}

fn listed_truncation_notice(files: &[FileEntry]) -> Option<String> {
    let notice = super::super::git::truncation_notice();
    files
        .iter()
        .any(|entry| entry.path == notice)
        .then_some(notice)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_at(path: &str) -> super::super::super::git::FileDiff {
        let base = "alpha\nold\nomega\n".to_string();
        let new = "alpha\nnew\nomega\n".to_string();
        super::super::super::git::FileDiff {
            path: path.into(),
            change: super::super::super::git::FileChange::Modified,
            old_path: None,
            hunks: super::super::super::engine::compute_hunks(&base, &new),
            base_text: base,
            new_text: new,
            is_binary: false,
        }
    }

    /// A `Loaded` state holding unified rows only, as `start_loading` leaves it
    /// when the active mode is unified.
    fn loaded_unified(path: &str) -> ColumnState {
        let files = vec![file_at(path)];
        let row_caches = build_file_row_caches(&files, None);
        let BuiltModeRows::Unified { rows, anchors } =
            build_rows_for_mode_with_caches(&files, ViewMode::Unified, &row_caches)
        else {
            unreachable!("requested unified rows");
        };
        ColumnState::Loaded {
            unified: Some(Rc::new(rows)),
            split: None,
            file_count: 1,
            files: Rc::new(Vec::new()),
            anchors_unified: Some(Rc::new(anchors)),
            anchors_split: None,
            files_full: Arc::new(files),
            row_caches: Arc::new(row_caches),
            theme_generation: crate::theme::theme_generation(),
        }
    }

    fn rows_for(build: &LazyModeBuild) -> BuiltModeRows {
        build_rows_for_mode_with_caches(&build.files, build.mode, &build.row_caches)
    }

    #[test]
    fn stale_lazy_mode_build_is_discarded_after_reload() {
        let mut col = Column::new_loading("feature".into(), PathBuf::from("."), None);
        col.state = loaded_unified("src/before.rs");

        // A reload starts: `start_loading` bumps the generation and keeps the
        // old rows on screen until the fresh load lands.
        col.generation = col.generation.wrapping_add(1);
        col.loading_mode = None;

        // A switch to split mid-reload snapshots the OLD files under the NEW
        // generation.
        let stale = col
            .begin_mode_build(ViewMode::Split)
            .expect("mid-reload split build scheduled");

        // The reload lands (same generation): fresh state, `loading_mode`
        // cleared, and the other mode scheduled from the fresh files.
        col.loading_mode = None;
        col.state = loaded_unified("src/after.rs");
        let fresh = col
            .begin_mode_build(ViewMode::Split)
            .expect("post-reload split build scheduled");

        // The stale build finishes first. Generation and mode both match, so
        // only the source identity can reject it. It must not install and
        // must not release the fresh build's claim on `loading_mode`.
        assert_eq!(col.lazy_build_verdict(&stale), LazyBuildVerdict::Discard);
        assert!(!col.finish_mode_build(&stale, rows_for(&stale)));
        assert!(!col.has_rows_for_mode(ViewMode::Split));
        assert!(col.loading_mode == Some(ViewMode::Split));

        // The fresh build still lands, with rows from the fresh files.
        assert_eq!(col.lazy_build_verdict(&fresh), LazyBuildVerdict::Install);
        assert!(col.finish_mode_build(&fresh, rows_for(&fresh)));
        assert!(col.loading_mode.is_none());
        let paths: Vec<&str> = col
            .disp_anchors_split
            .iter()
            .map(|(path, _)| path.as_str())
            .collect();
        assert_eq!(paths, ["src/after.rs"]);
    }

    #[test]
    fn lazy_mode_build_releases_the_claim_when_the_load_failed() {
        let mut col = Column::new_loading("feature".into(), PathBuf::from("."), None);
        col.state = loaded_unified("src/lib.rs");
        let build = col
            .begin_mode_build(ViewMode::Split)
            .expect("split build scheduled");
        col.state = ColumnState::Failed("boom".into());

        assert_eq!(col.lazy_build_verdict(&build), LazyBuildVerdict::Release);
        assert!(!col.finish_mode_build(&build, rows_for(&build)));
        assert!(col.loading_mode.is_none());
    }

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let out = crate::workspace::worktree::git_command()
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn session(agent: crate::agent_sessions::SessionAgent) -> SessionMeta {
        SessionMeta {
            agent,
            session_id: "s".into(),
            timestamp: "2026-06-01T10:00:00Z".into(),
            cwd: "/repo".into(),
            git_branch: "main".into(),
            summary: None,
        }
    }

    /// Run the diff load started last until its rows land and its lazy
    /// other-mode build finishes. Git runs on the blocking pool, so poll.
    fn settle_load(view: &gpui::Entity<DiffView>, cx: &mut gpui::TestAppContext) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            let settled = view.read_with(cx, |view, _| {
                let col = &view.column;
                matches!(col.state, ColumnState::Loaded { .. })
                    && col.loading_theme_generation.is_none()
                    && col.loading_mode.is_none()
            });
            if settled {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "diff rows never landed while session attribution was pending"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Issue #1094: rows land while attribution is still pending, a reload
    /// does not stack a second run, and a superseded run still applies before
    /// the newest generation's rerun replaces it.
    #[gpui::test]
    fn start_loading_publishes_rows_before_session_attribution_completes(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::agent_sessions::SessionAgent;
        use gpui::AppContext as _;

        super::super::attribution::hold_attribution();
        let held = super::super::attribution::take_held_attribution;
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().to_path_buf();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &["config", "user.email", "paneflow-tests@example.invalid"],
        );
        git(&repo, &["config", "user.name", "PaneFlow Tests"]);
        std::fs::write(repo.join("lib.rs"), "alpha\nold\nomega\n").expect("tracked file");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "fixture"]);
        std::fs::write(repo.join("lib.rs"), "alpha\nnew\nomega\n").expect("edit");

        let subject = ReviewSubject {
            repo_root: repo.clone(),
            worktree: DiffWorktree {
                path: repo,
                branch: "main".into(),
                workspace_id: None,
            },
        };
        let view = cx.new(|cx| {
            let mut view = DiffView::for_test(subject, cx);
            view.base_ref = "HEAD".into();
            view
        });
        // The git load finishes on the blocking pool and wakes the
        // foreground task from that thread.
        cx.executor().allow_parking();

        view.update(cx, |view, cx| view.start_loading(cx));
        settle_load(&view, cx);
        let first = held();
        assert_eq!(first.len(), 1, "landing rows starts one attribution run");
        view.read_with(cx, |view, _| {
            let col = &view.column;
            assert!(col.has_rows_for_mode(ViewMode::Unified));
            let paths: Vec<&str> = col
                .disp_anchors_unified
                .iter()
                .map(|(path, _)| path.as_str())
                .collect();
            assert_eq!(paths, ["lib.rs"], "rows are on screen before attribution");
            assert!(col.attribution.is_empty(), "attribution is still pending");
        });

        // A refresh supersedes that generation while its run is pending. The
        // rows land again, and no second run starts beside the hung one.
        view.update(cx, |view, cx| view.start_loading(cx));
        settle_load(&view, cx);
        assert!(
            held().is_empty(),
            "a reload must not stack attribution runs"
        );

        // The superseded run finishes: nothing newer is shown, so it applies
        // (same column and branch), and the newer generation gets a rerun.
        for tx in first {
            tx.send(vec![session(SessionAgent::Codex)])
                .expect("run awaiting");
        }
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let agents: Vec<SessionAgent> =
                view.column.attribution.iter().map(|s| s.agent).collect();
            assert_eq!(
                agents,
                [SessionAgent::Codex],
                "a superseded run must still apply, or reloads starve the header"
            );
        });
        let second = held();
        assert_eq!(second.len(), 1, "the current generation reruns attribution");

        for tx in second {
            tx.send(vec![session(SessionAgent::Claude)])
                .expect("run awaiting");
        }
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let agents: Vec<SessionAgent> =
                view.column.attribution.iter().map(|s| s.agent).collect();
            assert_eq!(agents, [SessionAgent::Claude]);
        });
    }
}
