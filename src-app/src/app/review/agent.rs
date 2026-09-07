//! Human-reviewed prompt prefill in ordinary workspace agent tabs.
use crate::PaneFlowApp;
use crate::diff::ReviewSubject;
use crate::diff::review_terminal::{ReviewCli, build_cli_review_prompt};
use gpui::{ClipboardItem, Context};

fn review_prompts(
    branch: &str,
    base: &str,
    picks: &[ReviewCli],
) -> Result<Vec<(ReviewCli, String)>, &'static str> {
    if picks.is_empty() {
        return Err("Select at least one CLI");
    }
    if base.is_empty() {
        return Err("Wait for the review base to load");
    }
    Ok(picks
        .iter()
        .enumerate()
        .map(|(rank, &cli)| (cli, build_cli_review_prompt(branch, base, rank > 0)))
        .collect())
}

/// Shared dispatch loop: a rejected tab stops the launch batch and the caller
/// only prefills terminals it actually opened. This matters at the tab cap.
fn dispatch_reviews<T>(
    prompts: Vec<(ReviewCli, String)>,
    mut launch: impl FnMut(ReviewCli) -> Option<T>,
) -> Vec<(T, String)> {
    let mut opened = Vec::new();
    for (cli, prompt) in prompts {
        let Some(terminal) = launch(cli) else {
            break;
        };
        opened.push((terminal, prompt));
    }
    opened
}

/// Resolve by repository identity even when the subject's originating
/// workspace closed or its checkout is a linked worktree.
fn review_workspace<'a>(
    subject: &ReviewSubject,
    workspaces: impl Iterator<Item = (u64, Option<&'a std::path::Path>)>,
) -> Option<usize> {
    let mut fallback = None;
    for (index, (id, root)) in workspaces.enumerate() {
        if root != Some(subject.repo_root.as_path()) {
            continue;
        }
        if Some(id) == subject.worktree.workspace_id {
            return Some(index);
        }
        fallback.get_or_insert(index);
    }
    fallback
}

impl PaneFlowApp {
    pub(crate) fn launch_review_agents(
        &mut self,
        subject: &ReviewSubject,
        base: &str,
        picks: &[ReviewCli],
        cx: &mut Context<Self>,
    ) {
        let prompts = match review_prompts(&subject.worktree.branch, base, picks) {
            Ok(prompts) => prompts,
            Err(message) => {
                self.show_toast(message, cx);
                return;
            }
        };
        // Keep the synchronous fallback even if placement fails; no terminal
        // receives a prompt until open_agent_tab_at_cwd has accepted the tab.
        cx.write_to_clipboard(ClipboardItem::new_string(prompts[0].1.clone()));
        let ws_idx = review_workspace(
            subject,
            self.workspaces
                .iter()
                .map(|ws| (ws.id, ws.repo_root.as_deref())),
        );
        let Some(ws_idx) = ws_idx else {
            self.show_toast("Open this repository before starting a review", cx);
            return;
        };
        if crate::workspace::path_is_in_retiring_worktree(&subject.worktree.path) {
            self.show_toast("Worktree is still being retired", cx);
            return;
        }
        if !subject.worktree.path.is_dir() {
            self.show_toast("Review checkout no longer exists", cx);
            return;
        }
        let config = self.cached_config.clone();
        let delay = config.resolved_review_prefill_delay_ms();
        let opened = dispatch_reviews(prompts, |cli| {
            let declared = match cli {
                ReviewCli::ClaudeCode => crate::agent_launcher::TerminalAgent::ClaudeCode,
                ReviewCli::Codex => crate::agent_launcher::TerminalAgent::Codex,
                ReviewCli::OpenCode => crate::agent_launcher::TerminalAgent::OpenCode,
                ReviewCli::Pi => crate::agent_launcher::TerminalAgent::Pi,
            };
            self.open_agent_tab_at_cwd(
                ws_idx,
                subject.worktree.path.clone(),
                Some(cli.launch_command(&config)),
                Some(declared),
                cx,
            )
        });
        if opened.is_empty() {
            return;
        }
        if self.active_idx != ws_idx {
            self.activate_workspace_without_window(ws_idx, cx);
        }
        self.review_leave_without_window(cx);
        for (terminal, prompt) in opened {
            let weak = terminal.downgrade();
            cx.spawn(async move |_, cx: &mut gpui::AsyncApp| {
                smol::Timer::after(std::time::Duration::from_millis(delay)).await;
                cx.update(|cx| {
                    if let Some(terminal) = weak.upgrade() {
                        // Deliberately no Enter: the human reviews and submits.
                        terminal.read(cx).send_text(&prompt);
                    }
                });
            })
            .detach();
        }
        self.show_toast("Review prompt ready · ⌘V to paste · Enter to submit", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_checkout_review_resolves_repo_despite_stale_workspace_id() {
        use std::path::{Path, PathBuf};
        let subject = ReviewSubject {
            repo_root: PathBuf::from("/repo"),
            worktree: crate::diff::DiffWorktree {
                path: PathBuf::from("/linked-checkout"),
                branch: "feature".into(),
                workspace_id: Some(11),
            },
        };
        assert_eq!(
            review_workspace(&subject, [(22, Some(Path::new("/repo")))].into_iter()),
            Some(0)
        );
        assert_eq!(
            review_workspace(
                &subject,
                [(11, Some(Path::new("/different-repo")))].into_iter()
            ),
            None
        );
        let producer = include_str!("../work_review/mod.rs");
        assert!(producer.contains("self.workspaces[w]"));
        assert!(producer.contains("path: checkout.root.clone()"));
        assert!(!producer.contains("repo_root: checkout.root"));
    }

    #[test]
    fn review_dispatch_preserves_choice_order_and_second_opinion_prompts() {
        let prompts =
            review_prompts("feature", "HEAD~1", &[ReviewCli::Codex, ReviewCli::Pi]).unwrap();
        let opened = dispatch_reviews(prompts, Some);
        assert_eq!(
            opened[0],
            (
                ReviewCli::Codex,
                build_cli_review_prompt("feature", "HEAD~1", false)
            )
        );
        assert_eq!(
            opened[1],
            (
                ReviewCli::Pi,
                build_cli_review_prompt("feature", "HEAD~1", true)
            )
        );
        assert!(
            opened
                .iter()
                .all(|(_, prompt)| !prompt.ends_with(['\n', '\r']))
        );
    }

    #[test]
    fn rejected_tab_never_receives_prefill_and_stops_the_batch() {
        let prompts = review_prompts("feature", "main^", &ReviewCli::all()).unwrap();
        let mut attempts = 0;
        let opened = dispatch_reviews(prompts, |cli| {
            attempts += 1;
            (attempts == 1).then_some(cli)
        });
        assert_eq!(attempts, 2);
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].0, ReviewCli::ClaudeCode);
        assert!(
            dispatch_reviews(
                review_prompts("feature", "main", &[ReviewCli::Pi]).unwrap(),
                |_| None::<()>
            )
            .is_empty()
        );
    }

    #[test]
    fn review_requires_a_choice_and_resolved_base() {
        assert_eq!(
            review_prompts("feature", "main", &[]).unwrap_err(),
            "Select at least one CLI"
        );
        assert_eq!(
            review_prompts("feature", "", &[ReviewCli::Codex]).unwrap_err(),
            "Wait for the review base to load"
        );
    }
}

#[cfg(test)]
mod integration_contract_tests {
    // PaneFlowApp binds a real socket. Pin its boundary to the independently
    // tested #334 placement helper without booting a second live application.
    #[test]
    fn header_dispatch_prefills_only_opened_agent_tabs_without_submission() {
        let src = include_str!("agent.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(src.contains("self.open_agent_tab_at_cwd("));
        assert!(src.contains("config.resolved_review_prefill_delay_ms()"));
        assert!(src.contains("Duration::from_millis(delay)"));
        assert!(src.contains("terminal.downgrade()"));
        assert!(src.contains("terminal.read(cx).send_text(&prompt)"));
        assert!(!src.contains("send_command("));
        assert!(!src.contains("send_bytes("));
        assert!(
            src.find("write_to_clipboard").unwrap()
                < src.find("let opened = dispatch_reviews").unwrap()
        );
        let pane = include_str!("../../pane/review.rs");
        assert!(pane.contains("cx.emit(PaneEvent::ReviewWithAgent"));
        let events = include_str!("../event_handlers.rs");
        assert!(events.contains("self.launch_review_agents(subject, base, picks, cx)"));
    }
}
