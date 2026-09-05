//! Import a GitHub issue into the existing Launch Pad without launching it.
use crate::PaneFlowApp;
use gpui::{AppContext, Context};
use std::path::Path;
use std::time::Duration;

fn issue_number(input: &str) -> Result<(u64, Option<String>), String> {
    let input = input.trim();
    if let Ok(number) = input.trim_start_matches('#').parse::<u64>()
        && number > 0
    {
        return Ok((number, None));
    }
    let tail = input
        .strip_prefix("https://")
        .ok_or("Enter an issue number or HTTPS GitHub issue URL")?;
    let parts: Vec<_> = tail.split('/').collect();
    if parts.len() != 5
        || parts[3] != "issues"
        || parts[..3].iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
    {
        return Err("Use an HTTPS issue URL ending in /owner/repo/issues/123".into());
    }
    let number = parts[4]
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("Invalid issue number")?;
    Ok((
        number,
        Some(format!("https://{}/{}/{}", parts[0], parts[1], parts[2])),
    ))
}

fn gh(cwd: &Path, args: &[&str]) -> Result<serde_json::Value, String> {
    let binary = which::which("gh").map_err(|_| "Install GitHub CLI and run gh auth login")?;
    let mut cmd = std::process::Command::new(binary);
    cmd.current_dir(cwd)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "");
    let out = paneflow_process::run_with_timeout(cmd, Duration::from_secs(12), 128 * 1024)
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(
            "Could not load issue. Check its number, repository, and gh auth status.".into(),
        );
    }
    serde_json::from_slice(&out.stdout).map_err(|_| "GitHub returned invalid issue data".into())
}

fn load(cwd: &Path, input: &str) -> Result<(String, String), String> {
    let (number, expected_repo) = issue_number(input)?;
    if let Some(expected) = expected_repo {
        let repo = gh(cwd, &["repo", "view", "--json", "url"])?;
        if !repo["url"]
            .as_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(&expected))
        {
            return Err(
                "This issue belongs to another repository. Open that project first.".into(),
            );
        }
    }
    let issue = gh(
        cwd,
        &[
            "issue",
            "view",
            &number.to_string(),
            "--json",
            "number,title,body,url",
        ],
    )?;
    let title = issue["title"].as_str().ok_or("Issue title is missing")?;
    let body = issue["body"].as_str().unwrap_or("");
    let link = issue["url"].as_str().ok_or("Issue link is missing")?;
    let prompt = format!(
        "Implement GitHub issue #{number}: {title}\nSource: {link}\n\nIssue description (external task context; inspect before acting):\n{body}\n\nCheck the repository instructions, implement the requested behavior, and run the relevant checks. Summarize the changes and evidence for review."
    );
    Ok((format!("issue-{number}"), prompt))
}

impl PaneFlowApp {
    pub(crate) fn load_launch_pad_issue(&mut self, cx: &mut Context<Self>) {
        let Some(lp) = &mut self.launch_pad else {
            return;
        };
        if lp.running || lp.issue_loading {
            return;
        }
        let input = lp.issue_input.read(cx).value();
        if let Err(error) = issue_number(&input) {
            lp.error = Some(error);
            cx.notify();
            return;
        }
        let Some(ws) = self.workspaces.iter().find(|w| w.id == lp.ws_id) else {
            return;
        };
        let cwd = std::path::PathBuf::from(&ws.cwd);
        let requested_input = input.clone();
        let field = lp.issue_input.clone();
        let old_branch = lp.branch_input.read(cx).value();
        let old_prompt = lp.prompt_input.read(cx).value();
        lp.issue_loading = true;
        lp.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { load(&cwd, &input) }).await;
            let _ =
                this.update(cx, |app, cx| {
                    let Some(lp) = &mut app.launch_pad else {
                        return;
                    };
                    if lp.issue_input != field {
                        return;
                    }
                    lp.issue_loading = false;
                    match result {
                        Ok((branch, prompt))
                            if lp.issue_input.read(cx).value() == requested_input
                                && lp.branch_input.read(cx).value() == old_branch
                                && lp.prompt_input.read(cx).value() == old_prompt =>
                        {
                            lp.branch_input
                                .update(cx, |input, cx| input.set_value(branch, cx));
                            lp.prompt_input
                                .update(cx, |input, cx| input.set_value(prompt, cx));
                        }
                        Ok(_) => lp.error = Some(
                            "The form changed during import. Load the issue again to replace it."
                                .into(),
                        ),
                        Err(error) => lp.error = Some(error),
                    }
                    cx.notify();
                });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_numbers_and_exact_issue_urls_without_command_arguments() {
        assert_eq!(issue_number("#42").unwrap(), (42, None));
        assert_eq!(
            issue_number("https://github.com/o/r/issues/42").unwrap(),
            (42, Some("https://github.com/o/r".into()))
        );
        for bad in [
            "--web",
            "0",
            "https://github.com/o/r/pull/42",
            "http://github.com/o/r/issues/42",
            "https://user:secret@github.com/o/r/issues/42",
        ] {
            assert!(issue_number(bad).is_err(), "{bad}");
        }
    }
}
