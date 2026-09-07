//! Public, GPUI-light data model exposed by the diff view.

use std::path::PathBuf;
use std::rc::Rc;

/// One worktree column seed: its working-tree root and current branch name.
///
/// `Hash`/`Eq` since #438: a review subject is a map key and a drag payload,
/// so the seed has to compare and hash by value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// Open workspace this worktree belongs to, or `None` for an on-disk
    /// worktree with no open workspace.
    pub workspace_id: Option<u64>,
}

/// What one Review pane is pointed at: a repository and one checkout inside
/// it (#438). Review used to be a scope over many columns; it is now a grid of
/// panes that each hold exactly one of these.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewSubject {
    pub repo_root: PathBuf,
    pub worktree: DiffWorktree,
}

impl ReviewSubject {
    /// Last path component of the repository root, falling back to the whole
    /// path when there is none (a filesystem root).
    pub fn repo_name(&self) -> String {
        self.repo_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repo_root.display().to_string())
    }

    /// Branch name, or the worktree's directory label when the checkout is
    /// detached. Shared with the sidebar so one checkout reads the same in
    /// both places.
    pub fn branch_label(&self) -> String {
        crate::workspace::worktree::checkout_label(
            Some(&self.worktree.branch),
            &self.worktree.path,
            &self.repo_root,
        )
    }

    /// `Project · branch`, or just the project when the branch label is empty.
    /// This is the pane header title and the drag label.
    pub fn label(&self) -> String {
        let branch = self.branch_label();
        if branch.is_empty() {
            self.repo_name()
        } else {
            format!("{} · {branch}", self.repo_name())
        }
    }

    /// Whether both subjects name the same checkout of the same repository.
    /// Deliberately ignores `workspace_id`, which is resolved late and is
    /// `None` on a restored subject until its workspace is matched.
    pub fn same_worktree(&self, other: &ReviewSubject) -> bool {
        self.repo_root == other.repo_root && self.worktree.path == other.worktree.path
    }
}

/// Lightweight per-file summary rendered by the Changes rail.
#[derive(Clone)]
pub struct FileEntry {
    pub path: String,
    pub change: super::super::git::FileChange,
    pub old_path: Option<String>,
    pub added: u32,
    pub removed: u32,
    pub is_binary: bool,
}

/// Changed-files list state consumed by the Changes rail.
#[derive(Clone)]
pub enum FileListState {
    Loading,
    Loaded(Rc<Vec<FileEntry>>),
    Failed(String),
}
