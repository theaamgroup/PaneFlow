//! Per-terminal branch identity, independent of workspace git/diff tracking.

use std::collections::{HashMap, HashSet};

use gpui::Context;

use crate::PaneFlowApp;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TerminalBranch {
    pub branch: String,
    pub repo_root: Option<std::path::PathBuf>,
}

fn probe_branches(cwds: HashSet<String>) -> HashMap<String, TerminalBranch> {
    cwds.into_iter()
        .map(|cwd| {
            let (branch, _) = crate::workspace::detect_branch(&cwd);
            let repo_root = crate::workspace::find_git_dir(&cwd)
                .and_then(|dir| crate::workspace::resolve_repo_root(&dir).0);
            (cwd, TerminalBranch { branch, repo_root })
        })
        .collect()
}

impl PaneFlowApp {
    pub(crate) fn poll_terminal_branches(cx: &mut Context<Self>) {
        cx.spawn(async |this, cx| {
            loop {
                let cwds = this.update(cx, |app, cx| {
                    app.workspaces
                        .iter()
                        .flat_map(|ws| ws.collect_panes())
                        .filter_map(|pane| {
                            let pane = pane.read(cx);
                            let terminal = pane.active_terminal_opt()?.read(cx);
                            terminal.terminal.current_cwd.clone()
                        })
                        .collect::<HashSet<_>>()
                });
                let Ok(cwds) = cwds else { break };
                // Only small git metadata files are read; no git subprocess or filesystem
                // work runs during render. Shared CWDs are probed once per pass.
                let branches = smol::unblock(move || probe_branches(cwds)).await;
                if this
                    .update(cx, |app, cx| {
                        if app.terminal_branches != branches {
                            app.terminal_branches = branches;
                            app.refresh_pull_requests(cx);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                // Branch switches need a refresh even when the terminal never cd's.
                smol::Timer::after(std::time::Duration::from_secs(2)).await;
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_cwds_keep_worktree_branches_independent_and_refresh_head() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("worktree");
        let plain = root.path().join("plain");
        let git = main.join(".git");
        let linked_git = git.join("worktrees/feature");
        std::fs::create_dir_all(&linked_git).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(linked_git.join("HEAD"), "ref: refs/heads/feature/one\n").unwrap();
        std::fs::write(linked_git.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", linked_git.display()),
        )
        .unwrap();
        let main = main.to_string_lossy().into_owned();
        let worktree = worktree.join("src").to_string_lossy().into_owned();
        let plain = plain.to_string_lossy().into_owned();
        let cwds = HashSet::from([main.clone(), worktree.clone(), plain.clone()]);
        let branches = probe_branches(cwds.clone());
        assert_eq!(branches[&main].branch, "main");
        assert_eq!(branches[&worktree].branch, "feature/one");
        assert_eq!(branches[&plain].branch, "");
        assert_eq!(branches[&main].repo_root, branches[&worktree].repo_root);
        assert!(branches[&plain].repo_root.is_none());
        std::fs::write(linked_git.join("HEAD"), "ref: refs/heads/feature/two\n").unwrap();
        let branches = probe_branches(cwds);
        assert_eq!(branches[&main].branch, "main");
        assert_eq!(branches[&worktree].branch, "feature/two");
        assert!(!probe_branches(HashSet::from([plain])).contains_key(&worktree));
    }
}
