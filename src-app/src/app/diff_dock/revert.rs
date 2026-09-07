use std::path::{Path, PathBuf};

use gpui::{Context, Pixels, Point, point, px};

use super::code::save::{FileStamp, SaveError, save_regular_blocking};
use super::git::{normalized_working_text, read_regular_snapshot};
use super::model::{DiffDockTab, DiffHover};
use crate::PaneFlowApp;
use crate::diff::{
    CellKind, DiffHunk, DisplayRow, FileChange, FileDiff, HeadFile, RowKind, SplitRow,
    hunk_for_base_line, hunk_for_new_line, revert_chip_bounds, row_at_offset, show_revision_file,
};

pub(super) const DIRTY_TAB_MESSAGE: &str = "Save or discard the editor changes first";
pub(super) const STALE_FILE_MESSAGE: &str = "File changed on disk, refresh first";

/// Recover the exact HEAD bytes discarded by display normalization. Also
/// refuse type changes: a pathname blob is never a regular source-file base.
fn load_revert_base(
    top: &Path,
    relative: &str,
    sha: &str,
    displayed: &str,
) -> Result<String, String> {
    let (bytes, symlink) = match show_revision_file(top, sha, relative)? {
        HeadFile::Content(bytes) => (bytes, false),
        HeadFile::Symlink(bytes) => (bytes, true),
        HeadFile::Missing => return Err(STALE_FILE_MESSAGE.into()),
    };
    let raw = String::from_utf8(bytes).map_err(|_| STALE_FILE_MESSAGE.to_string())?;
    let meta = std::fs::symlink_metadata(top.join(relative))
        .map_err(|_| STALE_FILE_MESSAGE.to_string())?;
    if meta.file_type().is_symlink() != symlink
        || (!symlink && !meta.is_file())
        || normalized_working_text(&raw) != displayed
    {
        return Err(STALE_FILE_MESSAGE.into());
    }
    Ok(raw)
}

/// Lines with their original terminators still attached, so a single-hunk
/// Revert does not rewrite untouched LF/CRLF mix.
fn line_slices(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..=index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                let end = if bytes.get(index + 1) == Some(&b'\n') {
                    index + 2
                } else {
                    index + 1
                };
                lines.push(&text[start..end]);
                index = end;
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

pub(crate) fn splice_base_lines(new_text: &str, base_text: &str, hunk: &DiffHunk) -> String {
    let new_lines = line_slices(new_text);
    let base_lines = line_slices(base_text);
    let start = (hunk.new_row_range.start as usize).min(new_lines.len());
    let end = (hunk.new_row_range.end as usize).clamp(start, new_lines.len());
    let base_start = (hunk.base_row_range.start as usize).min(base_lines.len());
    let base_end = (hunk.base_row_range.end as usize).clamp(base_start, base_lines.len());

    let mut out = String::new();
    for line in new_lines[..start]
        .iter()
        .chain(&base_lines[base_start..base_end])
        .chain(&new_lines[end..])
    {
        out.push_str(line);
    }
    out
}

pub(super) fn revert_hunk_blocking(
    path: &Path,
    base_text: &str,
    hunk: &DiffHunk,
    recorded: Option<FileStamp>,
    expected_working: &str,
) -> Result<FileStamp, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| STALE_FILE_MESSAGE.to_string())?;
    if meta.file_type().is_symlink() {
        return revert_symlink_blocking(path, base_text, hunk, expected_working);
    }
    let (new_text, current) =
        read_regular_snapshot(path).ok_or_else(|| STALE_FILE_MESSAGE.to_string())?;
    match (recorded, Some(current)) {
        (Some(recorded), Some(current)) if !recorded.differs(&current) => {}
        _ => return Err(STALE_FILE_MESSAGE.to_string()),
    }
    if normalized_working_text(&new_text) != expected_working {
        return Err(STALE_FILE_MESSAGE.to_string());
    }
    let text = splice_base_lines(&new_text, base_text, hunk);
    save_regular_blocking(path, &text, recorded).map_err(|err| match err {
        SaveError::Conflict => STALE_FILE_MESSAGE.to_string(),
        SaveError::Write(message) => message,
    })
}

/// The Changes tab diffs a symlink as Git does (`load_working_text`: the
/// target pathname, not the pointee). Restore the HEAD pathname onto the
/// link inode. Following the link and writing through `save_blocking` would
/// splice that pathname into the target file.
fn revert_symlink_blocking(
    path: &Path,
    base_text: &str,
    hunk: &DiffHunk,
    expected_working: &str,
) -> Result<FileStamp, String> {
    revert_symlink_with(path, base_text, hunk, expected_working, || {})
}

fn revert_symlink_with(
    path: &Path,
    base_text: &str,
    _hunk: &DiffHunk,
    expected_working: &str,
    before_persist: impl FnOnce(),
) -> Result<FileStamp, String> {
    let current = std::fs::read_link(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let Some(new_text) = current.to_str() else {
        return Err(STALE_FILE_MESSAGE.to_string());
    };
    if new_text != expected_working {
        return Err(STALE_FILE_MESSAGE.to_string());
    }
    // A symlink is one pathname, even when that pathname contains line
    // terminators. Restore it as a whole instead of line-splicing the name.
    let target = Path::new(base_text);
    if target.as_os_str().is_empty() {
        return Err(format!(
            "{}: reverted symlink target is empty",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("link");
    let tmp = unique_symlink_temp(parent, name, target)?;
    before_persist();
    if std::fs::read_link(path).ok().as_ref() != Some(&current) {
        let _ = std::fs::remove_file(&tmp);
        return Err(STALE_FILE_MESSAGE.to_string());
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {err}", path.display()));
    }
    Ok(FileStamp::read(path).unwrap_or_else(FileStamp::discarded))
}

fn unique_symlink_temp(parent: &Path, name: &str, target: &Path) -> Result<PathBuf, String> {
    let pid = std::process::id();
    for n in 0..1024u32 {
        let tmp = parent.join(format!(".{name}.paneflow-revert-{pid}-{n}"));
        match std::os::unix::fs::symlink(target, &tmp) {
            Ok(()) => return Ok(tmp),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("{}: {err}", tmp.display())),
        }
    }
    Err(format!(
        "could not allocate a temporary symlink in {}",
        parent.display()
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RevertTarget {
    pub(super) file: usize,
    pub(super) hunk: usize,
    pub(super) chip_row: usize,
}

fn revertable_file(anchors: &[(String, usize)], files: &[FileDiff], row: usize) -> Option<usize> {
    let (path, _) = anchors.iter().rev().find(|(_, header)| *header <= row)?;
    let index = files.iter().position(|file| file.path == *path)?;
    let file = &files[index];
    (file.change == FileChange::Modified && !file.is_binary).then_some(index)
}

fn hunk_index(hunks: &[DiffHunk], hunk: &DiffHunk) -> Option<usize> {
    hunks
        .iter()
        .position(|candidate| std::ptr::eq(candidate, hunk))
}

fn hunk_for_new_no(hunks: &[DiffHunk], no: Option<u32>) -> Option<usize> {
    let line = no?.checked_sub(1)?;
    hunk_index(hunks, hunk_for_new_line(hunks, line)?)
}

fn hunk_for_base_no(hunks: &[DiffHunk], no: Option<u32>) -> Option<usize> {
    let line = no?.checked_sub(1)?;
    hunk_index(hunks, hunk_for_base_line(hunks, line)?)
}

fn first_row_of_run(row: usize, is_changed: impl Fn(usize) -> bool) -> Option<usize> {
    (0..=row).rev().take_while(|r| is_changed(*r)).last()
}

pub(super) fn unified_revert_target(
    rows: &[DisplayRow],
    anchors: &[(String, usize)],
    files: &[FileDiff],
    row: usize,
) -> Option<RevertTarget> {
    let current = rows.get(row)?;
    let file = revertable_file(anchors, files, row)?;
    let hunks = &files[file].hunks;
    let hunk = match current.kind {
        RowKind::Added => hunk_for_new_no(hunks, current.new_no),
        RowKind::Removed => hunk_for_base_no(hunks, current.old_no),
        _ => None,
    }?;
    let chip_row = first_row_of_run(row, |r| {
        matches!(rows[r].kind, RowKind::Added | RowKind::Removed)
    })?;
    Some(RevertTarget {
        file,
        hunk,
        chip_row,
    })
}

fn is_changed_pair(row: &SplitRow) -> bool {
    matches!(
        row,
        SplitRow::Pair { left, right }
            if left.kind != CellKind::Context || right.kind != CellKind::Context
    )
}

pub(super) fn split_revert_target(
    rows: &[SplitRow],
    anchors: &[(String, usize)],
    files: &[FileDiff],
    row: usize,
) -> Option<RevertTarget> {
    let SplitRow::Pair { left, right } = rows.get(row)? else {
        return None;
    };
    let file = revertable_file(anchors, files, row)?;
    let hunks = &files[file].hunks;
    let hunk = match (left.kind, right.kind) {
        (_, CellKind::Added) => hunk_for_new_no(hunks, right.no),
        (CellKind::Removed, _) => hunk_for_base_no(hunks, left.no),
        _ => None,
    }?;
    let chip_row = first_row_of_run(row, |r| is_changed_pair(&rows[r]))?;
    Some(RevertTarget {
        file,
        hunk,
        chip_row,
    })
}

impl PaneFlowApp {
    fn diff_dock_revert_target_at(&self, position: Point<Pixels>) -> Option<DiffHover> {
        let active = self.diff_dock.diff_tabs.get(self.diff_dock.diff_active_tab);
        if !matches!(active, Some(DiffDockTab::Changes)) {
            return None;
        }
        let data = self.diff_dock.data.as_ref()?;
        let bounds = self.diff_dock.scroll.bounds();
        if !bounds.contains(&position) {
            return None;
        }
        let content_y =
            f32::from(position.y - bounds.top() - self.diff_dock.scroll.offset().y).max(0.0);
        let split = self.diff_dock.split;
        let target = if split {
            let row = row_at_offset(&data.disp_split_offsets, content_y)?;
            split_revert_target(
                &data.disp_split,
                &data.disp_anchors_split,
                &data.files_full,
                row,
            )
        } else {
            let row = row_at_offset(&data.disp_unified_offsets, content_y)?;
            unified_revert_target(
                &data.disp_unified,
                &data.disp_anchors_unified,
                &data.files_full,
                row,
            )
        }?;
        Some(DiffHover {
            split,
            path: data.files_full[target.file].path.clone(),
            hunk: target.hunk,
            chip_row: target.chip_row,
        })
    }

    pub(crate) fn update_diff_dock_hover(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let next = self.diff_dock_revert_target_at(position);
        if next != self.diff_dock.hover {
            self.diff_dock.hover = next;
            cx.notify();
        }
    }

    pub(super) fn handle_diff_dock_revert_click(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target) = self.diff_dock_revert_target_at(position) else {
            return false;
        };
        let inside_chip = {
            let Some(data) = self.diff_dock.data.as_ref() else {
                return false;
            };
            let offsets = if target.split {
                &data.disp_split_offsets
            } else {
                &data.disp_unified_offsets
            };
            let (Some(top), Some(bottom)) = (
                offsets.get(target.chip_row),
                offsets.get(target.chip_row + 1),
            ) else {
                return false;
            };
            let bounds = self.diff_dock.scroll.bounds();
            let origin = point(
                bounds.left(),
                bounds.top() + self.diff_dock.scroll.offset().y + px(*top),
            );
            revert_chip_bounds(origin, bounds.size.width, px(bottom - top)).contains(&position)
        };
        if !inside_chip {
            return false;
        }
        self.revert_diff_dock_hunk(target, cx);
        true
    }

    fn revert_diff_dock_hunk(&mut self, target: DiffHover, cx: &mut Context<Self>) {
        let Some(data) = self.diff_dock.data.as_ref() else {
            return;
        };
        let Some(toplevel) = data.toplevel.clone() else {
            return;
        };
        let Some(file) = data.files_full.iter().find(|file| file.path == target.path) else {
            return;
        };
        let Some(hunk) = file.hunks.get(target.hunk).cloned() else {
            return;
        };
        let path = toplevel.join(&file.path);
        let base_text = file.base_text.clone();
        let relative = file.path.clone();
        let Some(head_sha) = data.head_sha.clone() else {
            return;
        };
        let expected_working = file.new_text.clone();
        let recorded = data.stamps.get(&file.path).copied();
        let cwd = data.cwd.clone();
        let owner = self.diff_dock.owner;
        if self.dirty_file_tab_open(&path, cx) {
            self.show_diff_dock_error(DIRTY_TAB_MESSAGE, cx);
            return;
        }
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let result = smol::unblock(move || {
                    let base_text = load_revert_base(&toplevel, &relative, &head_sha, &base_text)?;
                    revert_hunk_blocking(&path, &base_text, &hunk, recorded, &expected_working)
                })
                .await;
                let _ = cx.update(|cx| {
                    this.update(cx, |app, cx| {
                        let same_cwd = app
                            .diff_dock
                            .data
                            .as_ref()
                            .is_some_and(|data| data.cwd == cwd);
                        match result {
                            Ok(_) => {
                                app.invalidate_parked_diff_docks_for_cwd(&cwd);
                                if same_cwd {
                                    app.refresh_diff_dock(cwd, cx);
                                }
                            }
                            Err(err) if same_cwd && app.diff_dock.owner == owner => {
                                app.show_diff_dock_error(&err, cx);
                            }
                            Err(_) => {}
                        }
                    })
                });
            },
        )
        .detach();
    }

    fn dirty_file_tab_open(&self, path: &Path, cx: &Context<Self>) -> bool {
        self.diff_dock.diff_tabs.iter().any(|tab| match tab {
            DiffDockTab::File(view) => {
                let view = view.read(cx);
                view.path() == path && view.is_dirty()
            }
            _ => false,
        })
    }

    fn show_diff_dock_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(data) = self.diff_dock.data.as_mut() {
            data.error = Some(message.to_string());
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::diff::{
        build_display_rows_with_caches, build_file_row_caches, build_split_rows_with_caches,
        compute_head_diff, head_sha,
    };

    fn hunks(base: &str, new: &str) -> Vec<DiffHunk> {
        crate::diff::compute_hunks(base, new)
    }

    fn revert(new: &str, base: &str, hunk: usize) -> String {
        let hunks = hunks(base, new);
        splice_base_lines(new, base, &hunks[hunk])
    }

    #[test]
    fn a_block_at_the_start_middle_or_end_is_replaced_by_the_base_lines() {
        let base = "one\ntwo\nthree\nfour\nfive\n";
        assert_eq!(
            revert("ONE\ntwo\nthree\nfour\nfive\n", base, 0),
            base,
            "start"
        );
        assert_eq!(
            revert("one\ntwo\nTHREE\nfour\nfive\n", base, 0),
            base,
            "middle"
        );
        assert_eq!(
            revert("one\ntwo\nthree\nfour\nFIVE\n", base, 0),
            base,
            "end"
        );
        assert_eq!(
            revert("one\ntwo\nthree\nfour\nfive\nsix\n", base, 0),
            base,
            "appended lines"
        );
        assert_eq!(revert("one\nfive\n", base, 0), base, "deleted lines");
    }

    #[test]
    fn reverting_one_of_two_blocks_leaves_the_other_change_in_place() {
        let base = "a\nb\nc\nd\ne\nf\n";
        let new = "A\nb\nc\nd\nE\nf\n";
        assert_eq!(revert(new, base, 0), "a\nb\nc\nd\nE\nf\n");
        assert_eq!(revert(new, base, 1), "A\nb\nc\nd\ne\nf\n");
    }

    #[test]
    fn crlf_files_keep_crlf_and_a_missing_final_newline_stays_missing() {
        let base = "a\nb\nc\n";
        let modified = hunks(base, "a\nB\nc\n");
        assert_eq!(
            splice_base_lines("a\r\nB\r\nc\r\n", base, &modified[0]),
            "a\r\nb\nc\r\n",
            "untouched lines keep CRLF; the restored line keeps HEAD's LF"
        );
        let unterminated = hunks("a\nb\nc", "a\nB\nc");
        assert_eq!(
            splice_base_lines("a\r\nB\r\nc", "a\nb\nc", &unterminated[0]),
            "a\r\nb\nc"
        );
        let last_line = hunks("a\nb\nc", "a\nb\nC");
        assert_eq!(
            splice_base_lines("a\nb\nC", "a\nb\nc", &last_line[0]),
            "a\nb\nc"
        );
    }

    #[test]
    fn a_mixed_ending_file_does_not_rewrite_untouched_lines() {
        let base = "keep\r\nchange\nend\r\n";
        let new = "keep\r\nCHANGE\nend\r\n";
        assert_eq!(
            revert(new, base, 0),
            "keep\r\nchange\nend\r\n",
            "Revert must not normalize the surrounding CRLF lines"
        );
    }

    #[test]
    fn revert_refuses_stale_text_even_when_the_recorded_stamp_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "agent\nnew\ntext\n").unwrap();
        let hunk = hunks("a\nb\nc\n", "a\nB\nc\n").remove(0);
        assert_eq!(
            revert_hunk_blocking(
                &path,
                "a\nb\nc\n",
                &hunk,
                FileStamp::read(&path),
                "a\nB\nc\n"
            ),
            Err(STALE_FILE_MESSAGE.into())
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "agent\nnew\ntext\n");
    }

    #[test]
    fn symlink_revert_preserves_newlines_in_the_target_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link");
        let base = "old\n";
        let new = "new\n";
        std::os::unix::fs::symlink(new, &path).unwrap();
        revert_hunk_blocking(&path, base, &hunks(base, new)[0], None, new).unwrap();
        assert_eq!(std::fs::read_link(path).unwrap(), Path::new(base));
    }

    #[test]
    fn symlink_revert_refuses_a_newline_only_target_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link");
        std::os::unix::fs::symlink("new\n", &path).unwrap();
        assert_eq!(
            revert_hunk_blocking(&path, "old", &hunks("old", "new")[0], None, "new"),
            Err(STALE_FILE_MESSAGE.into())
        );
        assert_eq!(std::fs::read_link(path).unwrap(), Path::new("new\n"));
    }

    #[test]
    fn symlink_revert_rechecks_the_target_before_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link");
        std::os::unix::fs::symlink("new", &path).unwrap();
        assert_eq!(
            revert_symlink_with(&path, "old", &hunks("old", "new")[0], "new", || {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink("agent", &path).unwrap();
            }),
            Err(STALE_FILE_MESSAGE.into())
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), Path::new("agent"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_block_at_the_end_takes_the_final_newline_from_the_base() {
        let lost_newline = hunks("a\nb\n", "a\nb");
        assert_eq!(
            splice_base_lines("a\nb", "a\nb\n", &lost_newline[0]),
            "a\nb\n"
        );
        let gained_newline = hunks("a\nb", "a\nb\n");
        assert_eq!(
            splice_base_lines("a\nb\n", "a\nb", &gained_newline[0]),
            "a\nb"
        );
        let tail = hunks("a\nb\nc\n", "a\nb\nX");
        assert_eq!(
            splice_base_lines("a\nb\nX", "a\nb\nc\n", &tail[0]),
            "a\nb\nc\n"
        );
    }

    fn file(path: &str, change: FileChange, base: &str, new: &str) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            change,
            old_path: None,
            base_text: base.to_string(),
            new_text: new.to_string(),
            hunks: hunks(base, new),
            is_binary: false,
        }
    }

    fn two_files() -> Vec<FileDiff> {
        vec![
            file(
                "src/a.rs",
                FileChange::Modified,
                "a\nb\nc\nd\ne\nf\ng\n",
                "a\nB\nc\nd\ne\nF\nG\n",
            ),
            file("src/new.rs", FileChange::Added, "", "x\ny\n"),
        ]
    }

    fn unified_anchors(rows: &[DisplayRow], files: &[FileDiff]) -> Vec<(String, usize)> {
        files
            .iter()
            .map(|f| f.path.clone())
            .zip(
                rows.iter()
                    .enumerate()
                    .filter(|(_, r)| r.kind == RowKind::FileHeader)
                    .map(|(i, _)| i),
            )
            .collect()
    }

    fn split_anchors(rows: &[SplitRow], files: &[FileDiff]) -> Vec<(String, usize)> {
        files
            .iter()
            .map(|f| f.path.clone())
            .zip(
                rows.iter()
                    .enumerate()
                    .filter(|(_, r)| matches!(r, SplitRow::Header(_)))
                    .map(|(i, _)| i),
            )
            .collect()
    }

    #[test]
    fn unified_rows_resolve_the_hunk_and_its_first_row_for_modified_files_only() {
        let files = two_files();
        let caches = build_file_row_caches(&files, None);
        let (rows, _) = build_display_rows_with_caches(&files, &caches);
        let anchors = unified_anchors(&rows, &files);

        let changed: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.kind, RowKind::Added | RowKind::Removed))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            changed.len(),
            2 + 4 + 2,
            "a two row block, a four row block, then the added file"
        );
        let first = unified_revert_target(&rows, &anchors, &files, changed[1]).expect("hunk 0");
        assert_eq!(
            first,
            RevertTarget {
                file: 0,
                hunk: 0,
                chip_row: changed[0]
            }
        );
        for row in [changed[2], changed[3], changed[5]] {
            let second = unified_revert_target(&rows, &anchors, &files, row).expect("hunk 1");
            assert_eq!(second.hunk, 1, "row {row}");
            assert_eq!(second.chip_row, changed[2], "row {row}");
        }
        assert!(
            unified_revert_target(&rows, &anchors, &files, changed[0] - 1).is_none(),
            "a context row shows no chip"
        );
        assert!(
            unified_revert_target(&rows, &anchors, &files, changed[6]).is_none(),
            "an added file shows no chip"
        );
    }

    #[test]
    fn split_rows_resolve_the_hunk_from_either_side() {
        let files = two_files();
        let caches = build_file_row_caches(&files, None);
        let (rows, _) = build_split_rows_with_caches(&files, &caches);
        let anchors = split_anchors(&rows, &files);
        let changed: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| is_changed_pair(r))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            changed.len(),
            1 + 2 + 2,
            "one paired row, two paired rows, the added file"
        );
        let first = split_revert_target(&rows, &anchors, &files, changed[0]).expect("hunk 0");
        assert_eq!(first.hunk, 0);
        assert_eq!(first.chip_row, changed[0]);
        let second = split_revert_target(&rows, &anchors, &files, changed[2]).expect("hunk 1");
        assert_eq!(second.hunk, 1);
        assert_eq!(second.chip_row, changed[1]);
        assert!(split_revert_target(&rows, &anchors, &files, changed[3]).is_none());
    }

    fn git(cwd: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn commit(root: &Path, message: &str) -> bool {
        git(root, &["add", "-A"])
            && git(
                root,
                &[
                    "-c",
                    "user.email=paneflow@example.com",
                    "-c",
                    "user.name=Paneflow",
                    "commit",
                    "-q",
                    "-m",
                    message,
                ],
            )
    }

    fn repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().expect("tempdir");
        if !git(dir.path(), &["init", "-q"]) {
            return None;
        }
        assert!(git(dir.path(), &["config", "core.autocrlf", "false"]));
        Some(dir)
    }

    #[test]
    fn reverting_the_first_of_two_blocks_writes_the_base_for_it_and_keeps_the_second() {
        let Some(dir) = repo() else {
            return;
        };
        let path = dir.path().join("notes.txt");
        let base = "one\ntwo\nthree\nfour\nfive\nsix\n";
        std::fs::write(&path, base).expect("write base");
        assert!(commit(dir.path(), "base"));
        let edited = "ONE\ntwo\nthree\nfour\nfive\nSIX\n";
        std::fs::write(&path, edited).expect("write edit");

        let diff = compute_head_diff(dir.path());
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(head_sha(dir.path()).is_some());
        let toplevel = diff.toplevel.clone().expect("toplevel");
        let file = diff
            .files
            .iter()
            .find(|f| f.path == "notes.txt")
            .expect("notes.txt");
        assert_eq!(file.change, FileChange::Modified);
        assert_eq!(file.hunks.len(), 2);
        let on_disk = toplevel.join(&file.path);
        let stamp = FileStamp::read(&on_disk);

        let stale =
            FileStamp::from_metadata(&std::fs::metadata(&on_disk).expect("meta")).expect("stamp");
        let wrong_len = {
            let mut copy = tempfile::NamedTempFile::new_in(dir.path()).expect("temp");
            use std::io::Write as _;
            copy.write_all(b"x").expect("write");
            FileStamp::read(copy.path()).expect("stamp")
        };
        assert!(stale.differs(&wrong_len));
        assert_eq!(
            revert_hunk_blocking(
                &on_disk,
                &file.base_text,
                &file.hunks[0],
                Some(wrong_len),
                edited,
            ),
            Err(STALE_FILE_MESSAGE.to_string())
        );
        assert_eq!(
            revert_hunk_blocking(&on_disk, &file.base_text, &file.hunks[0], None, edited),
            Err(STALE_FILE_MESSAGE.to_string())
        );
        assert_eq!(std::fs::read_to_string(&on_disk).expect("read"), edited);

        revert_hunk_blocking(&on_disk, &file.base_text, &file.hunks[0], stamp, edited)
            .expect("revert");
        assert_eq!(
            std::fs::read_to_string(&on_disk).expect("read"),
            "one\ntwo\nthree\nfour\nfive\nSIX\n"
        );
        let diff = compute_head_diff(dir.path());
        let file = diff
            .files
            .iter()
            .find(|f| f.path == "notes.txt")
            .expect("notes.txt");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].new_row_range, 5..6);
    }

    /// The chip diffs a symlink as Git does (pathname in, pathname out).
    /// Following the link would splice `v1.md` into `v2.md` and leave the
    /// link pointing at the corrupted file.
    #[cfg(unix)]
    #[test]
    fn reverting_a_tracked_symlink_restores_the_head_pathname_not_the_target() {
        let Some(dir) = repo() else {
            return;
        };
        let root = dir.path();
        let v1 = root.join("v1.md");
        let v2 = root.join("v2.md");
        let latest = root.join("latest");
        std::fs::write(&v1, "hello from v1\n").expect("v1");
        std::fs::write(&v2, "hello from v2\n").expect("v2");
        std::os::unix::fs::symlink("v1.md", &latest).expect("link");
        assert!(commit(root, "base"));
        std::fs::remove_file(&latest).expect("unlink");
        std::os::unix::fs::symlink("v2.md", &latest).expect("repoint");

        let diff = compute_head_diff(root);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        let file = diff
            .files
            .iter()
            .find(|f| f.path == "latest")
            .expect("latest");
        assert_eq!(file.change, FileChange::Modified);
        assert_eq!(file.base_text, "v1.md");
        assert_eq!(file.new_text, "v2.md");
        revert_hunk_blocking(
            &latest,
            &file.base_text,
            &file.hunks[0],
            FileStamp::read(&latest),
            &file.new_text,
        )
        .expect("revert");
        assert_eq!(
            std::fs::read_link(&latest).expect("readlink"),
            Path::new("v1.md"),
            "the link inode points at HEAD again"
        );
        assert!(
            std::fs::symlink_metadata(&latest)
                .expect("lstat")
                .file_type()
                .is_symlink(),
            "Revert must not replace the link with a regular file"
        );
        assert_eq!(std::fs::read_to_string(&v1).expect("v1"), "hello from v1\n");
        assert_eq!(
            std::fs::read_to_string(&v2).expect("v2"),
            "hello from v2\n",
            "the old target must not be rewritten with the pathname blob"
        );
    }

    #[test]
    fn revert_loads_exact_base_endings_and_refuses_file_type_changes() {
        let Some(dir) = repo() else { return };
        let root = dir.path();
        let path = root.join("file");
        let link = root.join("link");
        std::fs::write(&path, "old\r\n").unwrap();
        std::os::unix::fs::symlink("old\r\n", &link).unwrap();
        assert!(commit(root, "base"));
        let sha = head_sha(root).unwrap();
        assert_eq!(
            load_revert_base(root, "file", &sha, "old\n").unwrap(),
            "old\r\n"
        );
        assert_eq!(
            load_revert_base(root, "link", &sha, "old\n").unwrap(),
            "old\r\n"
        );
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "regular\n").unwrap();
        assert!(load_revert_base(root, "link", &sha, "old\n").is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("new", &path).unwrap();
        assert!(load_revert_base(root, "file", &sha, "old\n").is_err());
    }
}
