//! Clone repository modal (issue #524, upstream `9aa03d09` part 5): the
//! command palette's shell shaped as a quick pick. A typed URL becomes a
//! `Clone <url>` row; `Clone from GitHub` lists what `gh repo list` returns,
//! filtered as you type; Enter asks for a destination folder and clones into
//! `<folder>/<repo name>`, showing git's phase, a weighted percentage, and
//! throughput while it runs. The clone lands as a new workspace through
//! `open_workspace_folders`, which is what puts it at the head of recents
//! (#521).
//!
//! A GitHub target (a `github.com` URL, an `owner/name` shorthand, or a row
//! of the list) clones through `gh repo clone`, which carries gh's
//! credentials for private repositories and adds an `upstream` remote on a
//! fork; when gh is missing or signed out the clone falls back to `git
//! clone`, expanding a shorthand to `https://github.com/owner/name.git`.
//! Every other host clones with git. Both tools get the target after `--`
//! and `reject_unsafe_clone_url` refuses an option-shaped URL and the `ext::`
//! transport before anything is spawned.
//!
//! Reached from the palette's `Clone repository` row (`CloneRepository`, no
//! default chord) and the Workspaces rail's empty state.

use std::path::Path;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, AppContext, ClickEvent, Context, CursorStyle, Entity,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement,
    PathPromptOptions, SharedString, Styled, Window, deferred, div, ease_in_out, prelude::*, px,
    relative, svg,
};

use crate::PaneFlowApp;
use crate::settings::components::{menu_divider_color, menu_surface, select_option, with_alpha};
use crate::widgets::text_input::TextInput;

pub(crate) const CLONE_MODAL_WIDTH: f32 = 544.0;
pub(crate) const CLONE_MAX_LIST_HEIGHT: f32 = 360.0;
/// Wall clock for one clone. A large monorepo on a slow link can take
/// minutes; ten is the point past which the user would rather see an error.
const CLONE_DEADLINE: Duration = Duration::from_secs(600);
/// stdout cap for the clone itself; git and gh write next to nothing there.
const CLONE_OUTPUT_CAP: u64 = 256 * 1024;
const GITHUB_LIST_DEADLINE: Duration = Duration::from_secs(20);
const GITHUB_LIST_OUTPUT_CAP: u64 = 1024 * 1024;
const GITHUB_LIST_LIMIT: &str = "200";
const URL_PLACEHOLDER: &str = "Provide repository URL or pick a repository source.";
const GITHUB_PLACEHOLDER: &str = "Repository name (type to search)";
const GITHUB_SOURCE_LABEL: &str = "Clone from GitHub";
const GITHUB_SOURCE_GROUP: &str = "remote sources";
const GITHUB_CLI_HINT: &str =
    "Install the GitHub CLI and run gh auth login to list your repositories";
const CLONE_SWEEP_MS: u64 = 1400;
const CLONE_SWEEP_WIDTH: f32 = 0.3;
const CLONE_PROGRESS_FILL: u32 = 0x3fa266;
/// git's clone phases and the slice of the bar each one owns. Receiving
/// objects is the long one on every real clone, so it gets 70 points.
const CLONE_PHASES: [(&str, f32, f32); 6] = [
    ("Enumerating objects", 0.0, 0.02),
    ("Counting objects", 0.02, 0.05),
    ("Compressing objects", 0.05, 0.10),
    ("Receiving objects", 0.10, 0.80),
    ("Resolving deltas", 0.80, 0.95),
    ("Updating files", 0.95, 1.0),
];

/// What `gh` prints on stderr when no account is signed in.
const GH_SIGNED_OUT_MARKER: &str = "gh auth login";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloneTool {
    GitHubCli,
    Git,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CloneProgress {
    pub(crate) phase: &'static str,
    pub(crate) fraction: f32,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitHubRepo {
    pub(crate) name_with_owner: String,
    pub(crate) url: String,
}

pub(crate) enum CloneSource {
    Url,
    GitHub {
        loading: bool,
        repos: Vec<GitHubRepo>,
    },
}

pub(crate) struct CloneRepoState {
    pub(crate) url_input: Entity<TextInput>,
    pub(crate) source: CloneSource,
    pub(crate) selected: usize,
    pub(crate) running: bool,
    pub(crate) target: String,
    pub(crate) progress: Option<CloneProgress>,
    pub(crate) error: Option<String>,
    /// Whatever held focus when the modal opened, handed back on close (the
    /// command palette's `command_palette_return_focus` precedent, issue
    /// #108: an overlay that closes with nothing focused leaves every global
    /// chord without a handler).
    pub(crate) return_focus: Option<FocusHandle>,
}

enum CloneRow {
    GitHubSource,
    Url(String),
    Repo(GitHubRepo),
}

/// The guard every target passes before a command is built. Both tools get
/// the target after `--`, so a dash prefix cannot become an option, but git
/// still honours `ext::` as a transport that runs an arbitrary command.
pub(crate) fn reject_unsafe_clone_url(url: &str) -> Option<&'static str> {
    let trimmed = url.trim();
    if trimmed.starts_with('-') {
        return Some("A repository URL cannot start with a dash");
    }
    if trimmed
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ext:"))
    {
        return Some("The ext transport runs arbitrary commands and is refused");
    }
    None
}

/// The folder name the clone lands in: the last non-empty path segment with
/// a `.git` suffix stripped. `.`, `..`, and a dash prefix are refused so the
/// destination can never escape the picked parent or read as an option.
pub(crate) fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let tail = trimmed
        .rsplit(['/', ':', '\\'])
        .find(|segment| !segment.is_empty())?;
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    if name.is_empty() || name == "." || name == ".." || name.starts_with('-') {
        return None;
    }
    Some(name.to_string())
}

pub(crate) fn parse_github_repos(stdout: &[u8]) -> Vec<GitHubRepo> {
    let parsed: serde_json::Value = match serde_json::from_slice(stdout) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    parsed
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|row| {
            let name_with_owner = row.get("nameWithOwner")?.as_str()?.to_string();
            let url = row.get("url")?.as_str()?.to_string();
            if name_with_owner.is_empty() || reject_unsafe_clone_url(&url).is_some() {
                return None;
            }
            Some(GitHubRepo {
                name_with_owner,
                url,
            })
        })
        .collect()
}

pub(crate) fn filter_github_repos(repos: &[GitHubRepo], query: &str) -> Vec<GitHubRepo> {
    let query = query.trim().to_lowercase();
    repos
        .iter()
        .filter(|repo| {
            query.is_empty()
                || query
                    .split_whitespace()
                    .all(|word| repo.name_with_owner.to_lowercase().contains(word))
        })
        .cloned()
        .collect()
}

fn source_row_matches(query: &str) -> bool {
    let haystack = GITHUB_SOURCE_LABEL.to_lowercase();
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

/// One `git clone --progress` stderr line to a weighted fraction. Lines git
/// relays from the server carry a `remote:` prefix; `Checking out files` is
/// the older name of `Updating files`. Anything that is not a phase (the
/// `Cloning into` banner, warnings) is `None`.
pub(crate) fn parse_clone_progress(line: &str) -> Option<CloneProgress> {
    let line = line.trim();
    let line = line.strip_prefix("remote:").map_or(line, str::trim);
    let (label, rest) = line.split_once(':')?;
    let label = match label.trim() {
        "Checking out files" => "Updating files",
        other => other,
    };
    let (phase, low, high) = CLONE_PHASES
        .iter()
        .copied()
        .find(|(name, _, _)| *name == label)?;
    let rest = rest.trim();
    let percent = rest
        .split('%')
        .next()
        .and_then(|head| head.trim().parse::<f32>().ok())
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let detail = rest
        .split_once(')')
        .map_or("", |(_, tail)| tail)
        .trim()
        .trim_start_matches(',')
        .trim()
        .trim_end_matches("done.")
        .trim()
        .trim_end_matches(',')
        .trim()
        .replace(" | ", " · ");
    Some(CloneProgress {
        phase,
        fraction: low + (high - low) * percent / 100.0,
        detail: (!detail.is_empty()).then_some(detail),
    })
}

/// git redraws a progress line with `\r`, so both `\r` and `\n` end a line;
/// `carry` holds a line split across two stderr chunks.
fn split_progress_lines(carry: &mut Vec<u8>, chunk: &[u8], mut emit: impl FnMut(&str)) {
    for byte in chunk {
        if *byte == b'\r' || *byte == b'\n' {
            if !carry.is_empty() {
                emit(&String::from_utf8_lossy(carry));
                carry.clear();
            }
        } else {
            carry.push(*byte);
        }
    }
}

fn is_github_shorthand(target: &str) -> bool {
    let Some((owner, name)) = target.split_once('/') else {
        return false;
    };
    let segment_ok = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    };
    segment_ok(owner) && segment_ok(name)
}

fn github_host_of(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    let rest = lower
        .split_once("://")
        .map_or(lower.as_str(), |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    rest.strip_prefix("github.com")
        .is_some_and(|tail| tail.starts_with('/') || tail.starts_with(':'))
}

pub(crate) fn clone_tool_for(target: &str) -> CloneTool {
    let target = target.trim();
    if is_github_shorthand(target) || github_host_of(target) {
        CloneTool::GitHubCli
    } else {
        CloneTool::Git
    }
}

/// What `git clone` is given: a GitHub shorthand expands to its HTTPS remote
/// (the git fallback for a signed-out gh); everything else passes through.
pub(crate) fn git_target_for(target: &str) -> String {
    let target = target.trim();
    if is_github_shorthand(target) {
        format!("https://github.com/{target}.git")
    } else {
        target.to_string()
    }
}

fn clone_command(tool: CloneTool, target: &str, destination: &Path) -> std::process::Command {
    let mut command = match tool {
        CloneTool::GitHubCli => {
            let mut command = std::process::Command::new("gh");
            command
                .arg("repo")
                .arg("clone")
                .arg(target)
                .arg(destination)
                .arg("--")
                .arg("--progress")
                .env("GH_PAGER", "")
                .env("GH_PROMPT_DISABLED", "1")
                .env("NO_COLOR", "1");
            command
        }
        CloneTool::Git => {
            let mut command = std::process::Command::new("git");
            command
                .arg("clone")
                .arg("--progress")
                .arg("--")
                .arg(git_target_for(target))
                .arg(destination);
            command
        }
    };
    // A credential prompt on a pipe would hang until the deadline.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run_clone_command(
    command: std::process::Command,
    mut on_progress: impl FnMut(CloneProgress) + Send + 'static,
) -> Result<paneflow_process::BoundedOutput, paneflow_process::ProcError> {
    let mut carry = Vec::new();
    paneflow_process::run_with_timeout_tapping_stderr(
        command,
        CLONE_DEADLINE,
        CLONE_OUTPUT_CAP,
        move |chunk| {
            split_progress_lines(&mut carry, chunk, |line| {
                if let Some(progress) = parse_clone_progress(line) {
                    on_progress(progress);
                }
            });
        },
    )
}

/// gh is not on PATH, or it is and no account is signed in: the two cases
/// git can take over. Any other gh failure (a bad remote, a network error)
/// is reported as is; git would only fail the same way.
fn gh_is_unusable(
    result: &Result<paneflow_process::BoundedOutput, paneflow_process::ProcError>,
) -> bool {
    match result {
        Err(paneflow_process::ProcError::Spawn(_)) => true,
        Ok(output) => {
            !output.status.success()
                && String::from_utf8_lossy(&output.stderr).contains(GH_SIGNED_OUT_MARKER)
        }
        Err(_) => false,
    }
}

fn run_clone(
    target: &str,
    destination: &Path,
    on_progress: impl FnMut(CloneProgress) + Send + Clone + 'static,
) -> Result<(), String> {
    let tool = clone_tool_for(target);
    let mut result = run_clone_command(
        clone_command(tool, target, destination),
        on_progress.clone(),
    );
    if tool == CloneTool::GitHubCli && gh_is_unusable(&result) {
        log::info!("gh is missing or signed out; cloning {target} with git");
        result = run_clone_command(
            clone_command(CloneTool::Git, target, destination),
            on_progress,
        );
    }
    let output = result.map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    Err(clone_failure_message(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

/// The one line of a failed clone worth showing: git's `error:` line first
/// (a checkout failure), then `fatal:`, then the last line that is neither
/// the banner nor a progress report.
pub(crate) fn clone_failure_message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let prefixed = |prefix: &str| {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::trim)
    };
    prefixed("error:")
        .or_else(|| prefixed("fatal:"))
        .or_else(|| {
            lines.iter().copied().rfind(|line| {
                !line.starts_with("Cloning into") && parse_clone_progress(line).is_none()
            })
        })
        .unwrap_or("git clone failed")
        .to_string()
}

fn list_github_repos() -> Result<Vec<GitHubRepo>, String> {
    let mut command = std::process::Command::new("gh");
    command
        .args([
            "repo",
            "list",
            "--json",
            "nameWithOwner,url",
            "--limit",
            GITHUB_LIST_LIMIT,
        ])
        .env("GH_PAGER", "")
        .env("NO_COLOR", "1");
    let output =
        paneflow_process::run_with_timeout(command, GITHUB_LIST_DEADLINE, GITHUB_LIST_OUTPUT_CAP)
            .map_err(|_| GITHUB_CLI_HINT.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or(GITHUB_CLI_HINT)
            .to_string();
        return Err(message);
    }
    Ok(parse_github_repos(&output.stdout))
}

fn clone_progress_fill() -> gpui::Hsla {
    gpui::rgb(CLONE_PROGRESS_FILL).into()
}

/// The running block: `Cloning <name>` with the percentage on the trailing
/// edge, a 4 px track filled from the left, and git's phase and throughput
/// beneath. Before git reports anything a 30% segment sweeps the track on a
/// 1.4 s ease-in-out loop, so a slow remote still reads as alive.
fn render_clone_progress(
    target: &str,
    progress: Option<&CloneProgress>,
    ui: crate::theme::UiColors,
) -> AnyElement {
    let percent = progress.map(|progress| (progress.fraction * 100.0).round() as u32);
    let status = match progress {
        Some(CloneProgress {
            phase,
            detail: Some(detail),
            ..
        }) => format!("{phase} · {detail}"),
        Some(CloneProgress { phase, .. }) => (*phase).to_string(),
        None => "Connecting to the remote…".to_string(),
    };
    let fill = match progress {
        Some(progress) => div()
            .h_full()
            .w(relative(progress.fraction.clamp(0.02, 1.0)))
            .rounded_full()
            .bg(clone_progress_fill())
            .into_any_element(),
        None if crate::ui_primitives::reduce_motion() => div()
            .h_full()
            .w(relative(CLONE_SWEEP_WIDTH))
            .rounded_full()
            .bg(clone_progress_fill())
            .into_any_element(),
        None => div()
            .absolute()
            .top_0()
            .bottom_0()
            .w(relative(CLONE_SWEEP_WIDTH))
            .rounded_full()
            .bg(clone_progress_fill())
            .with_animation(
                "clone-progress-sweep",
                Animation::new(Duration::from_millis(CLONE_SWEEP_MS))
                    .repeat()
                    .with_easing(ease_in_out),
                |bar, delta| {
                    bar.left(relative(
                        -CLONE_SWEEP_WIDTH + (1.0 + CLONE_SWEEP_WIDTH) * delta,
                    ))
                },
            )
            .into_any_element(),
    };

    div()
        .px(px(8.))
        .pt(px(8.))
        .pb(px(10.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .min_w_0()
                        .overflow_x_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .text_color(ui.text)
                        .child(format!("Cloning {target}")),
                )
                .when_some(percent, |row, percent| {
                    row.child(
                        div()
                            .flex_none()
                            .pl(px(8.))
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(format!("{percent}%")),
                    )
                }),
        )
        .child(
            div()
                .relative()
                .w_full()
                .h(px(4.))
                .rounded_full()
                .bg(with_alpha(ui.text, 0.10))
                .overflow_hidden()
                .child(fill),
        )
        .child(div().text_size(px(11.)).text_color(ui.muted).child(status))
        .into_any_element()
}

impl PaneFlowApp {
    /// Open the modal on the URL source with the field focused. A second
    /// open while it is up (the palette row, the rail row) is a no-op rather
    /// than a reset, so a clone in flight keeps its progress block.
    pub(crate) fn open_clone_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.clone_repo.is_some() {
            return;
        }
        if self.session_restore.is_some() {
            self.show_toast("Restoring session", cx);
            return;
        }
        self.dismiss_transient_surfaces();
        if self.command_palette_open {
            self.close_command_palette(cx);
        }
        let return_focus = window.focused(cx);
        let url_input = cx.new(|cx| TextInput::new("", URL_PLACEHOLDER, cx));
        let focus = url_input.read(cx).focus_handle.clone();
        self.clone_repo = Some(CloneRepoState {
            url_input,
            source: CloneSource::Url,
            selected: 0,
            running: false,
            target: String::new(),
            progress: None,
            error: None,
            return_focus,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn handle_clone_repository(
        &mut self,
        _: &crate::CloneRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_clone_repo(window, cx);
    }

    /// Whether a clone is running right now. The modal refuses to close
    /// while it is, and the command palette refuses to open over it.
    pub(crate) fn clone_repo_running(&self) -> bool {
        self.clone_repo.as_ref().is_some_and(|clone| clone.running)
    }

    /// Escape, an outside click: fold the modal and hand focus back to
    /// whatever held it before, then the active workspace's first pane, then
    /// the empty-workspace placeholder. Ignored while a clone runs: the
    /// progress block is the only place the outcome is reported.
    pub(crate) fn close_clone_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clone) = self.clone_repo.take() else {
            return;
        };
        if clone.running {
            self.clone_repo = Some(clone);
            return;
        }
        cx.notify();
        if let Some(handle) = clone.return_focus {
            window.focus(&handle, cx);
            return;
        }
        let focused = match self.workspaces.get(self.active_idx) {
            Some(ws) => ws.focus_first(window, cx),
            None => false,
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }

    fn clone_repo_set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(clone) = self.clone_repo.as_mut() {
            clone.running = false;
            if let CloneSource::GitHub { loading, .. } = &mut clone.source {
                *loading = false;
            }
            clone.error = Some(message.into());
            cx.notify();
        }
    }

    fn clone_repo_rows(&self, cx: &Context<Self>) -> Vec<CloneRow> {
        let Some(clone) = self.clone_repo.as_ref() else {
            return Vec::new();
        };
        let query = clone.url_input.read(cx).value();
        let query = query.trim();
        match &clone.source {
            CloneSource::Url => {
                if query.is_empty() {
                    vec![CloneRow::GitHubSource]
                } else if source_row_matches(query) {
                    vec![CloneRow::GitHubSource, CloneRow::Url(query.to_string())]
                } else {
                    vec![CloneRow::Url(query.to_string())]
                }
            }
            CloneSource::GitHub { repos, .. } => filter_github_repos(repos, query)
                .into_iter()
                .map(CloneRow::Repo)
                .collect(),
        }
    }

    fn clone_repo_move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.clone_repo_rows(cx).len();
        let Some(clone) = self.clone_repo.as_mut() else {
            return;
        };
        if len == 0 {
            clone.selected = 0;
        } else {
            let current = clone.selected.min(len - 1) as isize;
            clone.selected = (current + delta).rem_euclid(len as isize) as usize;
        }
        cx.notify();
    }

    fn clone_repo_pick_github(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clone) = self.clone_repo.as_mut() else {
            return;
        };
        let url_input = cx.new(|cx| TextInput::new("", GITHUB_PLACEHOLDER, cx));
        let focus = url_input.read(cx).focus_handle.clone();
        clone.url_input = url_input;
        clone.source = CloneSource::GitHub {
            loading: true,
            repos: Vec::new(),
        };
        clone.selected = 0;
        clone.error = None;
        window.focus(&focus, cx);
        cx.notify();

        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let result = smol::unblock(list_github_repos).await;
            let _ = this.update(cx, |app, cx| {
                let Some(clone) = app.clone_repo.as_mut() else {
                    return;
                };
                let CloneSource::GitHub { loading, repos } = &mut clone.source else {
                    return;
                };
                *loading = false;
                match result {
                    Ok(listed) => *repos = listed,
                    Err(message) => clone.error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn clone_repo_back_to_sources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clone) = self.clone_repo.as_mut() else {
            return;
        };
        let url_input = cx.new(|cx| TextInput::new("", URL_PLACEHOLDER, cx));
        let focus = url_input.read(cx).focus_handle.clone();
        clone.url_input = url_input;
        clone.source = CloneSource::Url;
        clone.selected = 0;
        clone.error = None;
        window.focus(&focus, cx);
        cx.notify();
    }

    fn clone_repo_activate(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.clone_repo.as_ref().is_none_or(|clone| clone.running) {
            return;
        }
        let rows = self.clone_repo_rows(cx);
        let Some(row) = rows.into_iter().nth(idx) else {
            return;
        };
        match row {
            CloneRow::GitHubSource => self.clone_repo_pick_github(window, cx),
            CloneRow::Url(url) => self.clone_repo_start(url, window, cx),
            CloneRow::Repo(repo) => self.clone_repo_start(repo.url, window, cx),
        }
    }

    /// Enter: a typed URL clones as typed, otherwise the selected row runs.
    pub(crate) fn clone_repo_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clone) = self.clone_repo.as_ref() else {
            return;
        };
        if clone.running {
            return;
        }
        let typed = clone.url_input.read(cx).value().trim().to_string();
        let selected = clone.selected;
        if matches!(clone.source, CloneSource::Url) && !typed.is_empty() {
            self.clone_repo_start(typed, window, cx);
            return;
        }
        self.clone_repo_activate(selected, window, cx);
    }

    /// Guard the URL, ask for the parent folder, then clone off the render
    /// thread with progress relayed through a channel. Success files the
    /// destination as a workspace through `open_workspace_folders` (which
    /// records it at the head of recents and refuses at the workspace cap)
    /// and selects it; failure replaces the rows with the error line.
    fn clone_repo_start(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = reject_unsafe_clone_url(&url) {
            self.clone_repo_set_error(reason, cx);
            return;
        }
        let Some(name) = repo_name_from_url(&url) else {
            self.clone_repo_set_error("Enter a repository URL first", cx);
            return;
        };

        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Clone into".into()),
        });
        if let Some(clone) = self.clone_repo.as_mut() {
            clone.error = None;
        }
        cx.notify();

        cx.spawn_in(window, async move |this: gpui::WeakEntity<Self>, cx| {
            let picked = match receiver.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.pop(),
                _ => None,
            };
            let Some(parent) = picked else {
                return;
            };
            let destination = parent.join(&name);
            if destination.exists() {
                let _ = this.update(cx, |app, cx| {
                    app.clone_repo_set_error(
                        format!("{} already exists", destination.display()),
                        cx,
                    );
                });
                return;
            }
            let target = name.clone();
            let _ = this.update(cx, |app, cx| {
                if let Some(clone) = app.clone_repo.as_mut() {
                    clone.running = true;
                    clone.target = target;
                    clone.progress = None;
                    clone.error = None;
                }
                cx.notify();
            });

            let clone_url = url.clone();
            let clone_destination = destination.clone();
            let (progress_tx, progress_rx) = smol::channel::unbounded::<CloneProgress>();
            let clone_task = smol::unblock(move || {
                run_clone(&clone_url, &clone_destination, move |progress| {
                    let _ = progress_tx.try_send(progress);
                })
            });
            // The sender lives inside the clone closure, so this loop ends
            // when the clone does.
            while let Ok(progress) = progress_rx.recv().await {
                let _ = this.update(cx, |app, cx| {
                    if let Some(clone) = app.clone_repo.as_mut() {
                        clone.progress = Some(progress);
                        cx.notify();
                    }
                });
            }
            let result = clone_task.await;

            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok(()) => {
                    app.clone_repo = None;
                    app.open_workspace_folders(std::slice::from_ref(&destination), cx);
                    let cwd = destination.display().to_string();
                    if let Some(idx) = app.workspaces.iter().position(|ws| ws.cwd == cwd) {
                        app.select_workspace(idx, window, cx);
                    }
                    cx.notify();
                }
                Err(message) => app.clone_repo_set_error(message, cx),
            });
        })
        .detach();
    }

    pub(crate) fn handle_clone_repo_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => {
                let in_github = self
                    .clone_repo
                    .as_ref()
                    .is_some_and(|clone| matches!(clone.source, CloneSource::GitHub { .. }));
                if in_github {
                    self.clone_repo_back_to_sources(window, cx);
                } else {
                    self.close_clone_repo(window, cx);
                }
            }
            "enter" => self.clone_repo_confirm(window, cx),
            "up" => self.clone_repo_move_selection(-1, cx),
            "down" => self.clone_repo_move_selection(1, cx),
            _ => {}
        }
    }

    fn render_clone_row(
        &self,
        idx: usize,
        row: &CloneRow,
        selected: bool,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon, label, trailing): (&'static str, String, Option<&'static str>) = match row {
            CloneRow::GitHubSource => (
                "icons/brand-github.svg",
                GITHUB_SOURCE_LABEL.to_string(),
                Some(GITHUB_SOURCE_GROUP),
            ),
            CloneRow::Url(url) => (
                "icons/brand-git.svg",
                format!("Clone {url}"),
                Some("repository URL"),
            ),
            CloneRow::Repo(repo) => ("icons/brand-github.svg", repo.name_with_owner.clone(), None),
        };
        let aria = match trailing {
            Some(group) => format!("{label}, {group}"),
            None => label.clone(),
        };
        // DESIGN.md 7.2: the rows are listbox options carrying
        // `aria_selected`, the command palette's contract.
        select_option(
            SharedString::from(format!("clone-repo-row-{idx}")),
            selected,
            ui,
        )
        .aria_label(aria)
        .w_full()
        .justify_between()
        .cursor(CursorStyle::PointingHand)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.clone_repo_activate(idx, window, cx);
            cx.stop_propagation();
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .min_w_0()
                .child(
                    svg()
                        .size(px(14.))
                        .flex_none()
                        .path(icon)
                        .text_color(ui.text),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_x_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(ui.text)
                        .child(label),
                ),
        )
        .when_some(trailing, |row, group| {
            row.child(
                div()
                    .flex_none()
                    .pl(px(8.))
                    .text_size(px(11.))
                    .text_color(ui.muted)
                    .child(group),
            )
        })
        .into_any_element()
    }

    pub(crate) fn render_clone_repo(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(clone) = self.clone_repo.as_ref() else {
            return div().into_any_element();
        };
        let ui = crate::theme::ui_colors();
        let rows = self.clone_repo_rows(cx);
        let selected = clone.selected.min(rows.len().saturating_sub(1));
        let loading = matches!(clone.source, CloneSource::GitHub { loading: true, .. });
        let url_input = clone.url_input.clone();
        let running = clone.running;
        let target = clone.target.clone();
        let progress = clone.progress.clone();
        let error = clone.error.clone();

        let mut list = div()
            .id("clone-repo-list")
            .role(gpui::Role::ListBox)
            .aria_label("Repository sources")
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(px(4.))
            .max_h(px(CLONE_MAX_LIST_HEIGHT))
            .overflow_y_scroll();

        let status_row = |text: String, color: gpui::Hsla| {
            div()
                .px(px(8.))
                .py(px(12.))
                .text_size(px(12.))
                .text_color(color)
                .child(text)
        };

        if running {
            list = list.child(render_clone_progress(&target, progress.as_ref(), ui));
        } else if let Some(error) = error {
            list = list.child(status_row(error, ui.vc_deleted));
        } else if loading {
            list = list.child(status_row(
                "Loading your GitHub repositories…".to_string(),
                ui.muted,
            ));
        } else if rows.is_empty() {
            list = list.child(status_row("No matching repository".to_string(), ui.muted));
        } else {
            for (idx, row) in rows.iter().enumerate() {
                list = list.child(self.render_clone_row(idx, row, idx == selected, ui, cx));
            }
        }

        let card = menu_surface(div().id("clone-repo"), ui)
            .occlude()
            .track_focus(&self.clone_repo_focus)
            .on_key_down(cx.listener(Self::handle_clone_repo_key_down))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_clone_repo(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .w(px(CLONE_MODAL_WIDTH))
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .px(px(14.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(menu_divider_color(ui))
                    .text_size(px(13.))
                    .text_color(ui.text)
                    .child(url_input),
            )
            .child(list);

        deferred(
            div()
                .id("clone-repo-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(96.))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(crate::ui_primitives::menu_reveal("clone-repo-reveal", card)),
        )
        .with_priority(7)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_reads_an_ssh_remote() {
        assert_eq!(
            repo_name_from_url("git@github.com:theaamgroup/paneflow.git"),
            Some("paneflow".to_string())
        );
    }

    #[test]
    fn repo_name_reads_an_https_remote_with_a_trailing_slash() {
        assert_eq!(
            repo_name_from_url("https://github.com/theaamgroup/paneflow/"),
            Some("paneflow".to_string())
        );
    }

    #[test]
    fn repo_name_rejects_an_empty_url() {
        assert_eq!(repo_name_from_url("   "), None);
    }

    #[test]
    fn repo_name_rejects_a_traversal_tail() {
        assert_eq!(repo_name_from_url("https://example.com/repo/.."), None);
        assert_eq!(repo_name_from_url("https://example.com/repo/."), None);
        assert_eq!(repo_name_from_url("https://example.com/-flag"), None);
    }

    #[test]
    fn an_option_shaped_url_is_refused() {
        assert!(reject_unsafe_clone_url("--upload-pack=touch /tmp/pwned").is_some());
        assert!(reject_unsafe_clone_url("  -c core.pager=sh").is_some());
    }

    #[test]
    fn the_ext_transport_is_refused_in_any_case() {
        assert!(reject_unsafe_clone_url("ext::sh -c whoami").is_some());
        assert!(reject_unsafe_clone_url("EXT::sh -c whoami").is_some());
    }

    #[test]
    fn an_ordinary_remote_is_accepted() {
        assert!(reject_unsafe_clone_url("git@github.com:theaamgroup/paneflow.git").is_none());
        assert!(reject_unsafe_clone_url("https://github.com/theaamgroup/paneflow").is_none());
    }

    #[test]
    fn github_targets_clone_through_gh_and_the_rest_through_git() {
        assert_eq!(clone_tool_for("theaamgroup/paneflow"), CloneTool::GitHubCli);
        assert_eq!(
            clone_tool_for("https://github.com/theaamgroup/paneflow"),
            CloneTool::GitHubCli
        );
        assert_eq!(
            clone_tool_for("git@github.com:theaamgroup/paneflow.git"),
            CloneTool::GitHubCli
        );
        assert_eq!(
            clone_tool_for("ssh://git@github.com/theaamgroup/paneflow.git"),
            CloneTool::GitHubCli
        );
        assert_eq!(
            clone_tool_for("https://gitlab.com/theaamgroup/paneflow"),
            CloneTool::Git
        );
        assert_eq!(
            clone_tool_for("https://github.com.evil.example/x/y"),
            CloneTool::Git
        );
        assert_eq!(clone_tool_for("../paneflow"), CloneTool::Git);
        assert_eq!(clone_tool_for("paneflow"), CloneTool::Git);
        assert_eq!(clone_tool_for("/Users/me/repos/paneflow"), CloneTool::Git);
    }

    #[test]
    fn the_git_fallback_expands_a_shorthand_to_an_https_remote() {
        assert_eq!(
            git_target_for("theaamgroup/paneflow"),
            "https://github.com/theaamgroup/paneflow.git"
        );
        assert_eq!(
            git_target_for("git@github.com:theaamgroup/paneflow.git"),
            "git@github.com:theaamgroup/paneflow.git"
        );
    }

    /// Both tools get the target after `--` with progress on, git never
    /// prompts on the pipe, and gh runs without its pager or prompts.
    #[test]
    fn clone_commands_pass_the_target_after_a_double_dash() {
        let dest = Path::new("/tmp/paneflow");
        let git = clone_command(CloneTool::Git, "theaamgroup/paneflow", dest);
        let args: Vec<String> = git
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "clone",
                "--progress",
                "--",
                "https://github.com/theaamgroup/paneflow.git",
                "/tmp/paneflow",
            ]
        );
        assert!(
            git.get_envs()
                .any(|(k, v)| { k == "GIT_TERMINAL_PROMPT" && v.is_some_and(|v| v == "0") })
        );

        let gh = clone_command(CloneTool::GitHubCli, "theaamgroup/paneflow", dest);
        let args: Vec<String> = gh
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "repo",
                "clone",
                "theaamgroup/paneflow",
                "/tmp/paneflow",
                "--",
                "--progress",
            ]
        );
        assert!(
            gh.get_envs()
                .any(|(k, v)| k == "GH_PROMPT_DISABLED" && v.is_some_and(|v| v == "1"))
        );
    }

    /// The git fallback fires for a missing gh and a signed-out gh only;
    /// any other gh failure is reported as is.
    #[test]
    fn gh_is_unusable_only_when_missing_or_signed_out() {
        let spawn_failed: Result<paneflow_process::BoundedOutput, _> = Err(
            paneflow_process::ProcError::Spawn(std::io::Error::from(std::io::ErrorKind::NotFound)),
        );
        assert!(gh_is_unusable(&spawn_failed));
        assert!(!gh_is_unusable(&Err(paneflow_process::ProcError::Timeout)));

        let signed_out = std::process::Command::new("sh")
            .args([
                "-c",
                "echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4",
            ])
            .output()
            .unwrap();
        let signed_out = Ok(paneflow_process::BoundedOutput {
            status: signed_out.status,
            stdout: signed_out.stdout,
            stderr: signed_out.stderr,
        });
        assert!(gh_is_unusable(&signed_out));

        let not_found = std::process::Command::new("sh")
            .args([
                "-c",
                "echo 'GraphQL: Could not resolve to a Repository' >&2; exit 1",
            ])
            .output()
            .unwrap();
        let not_found = Ok(paneflow_process::BoundedOutput {
            status: not_found.status,
            stdout: not_found.stdout,
            stderr: not_found.stderr,
        });
        assert!(!gh_is_unusable(&not_found));
    }

    #[test]
    fn a_failed_checkout_reports_the_first_error_line() {
        let stderr = "Cloning into 'ghostty'...\rReceiving objects: 100% (10/10), done.\nerror: invalid path 'src/a:b.txt'\nfatal: unable to checkout working tree\nwarning: Clone succeeded, but checkout failed.\nand retry with 'git restore --source=HEAD :/'\n";
        assert_eq!(clone_failure_message(stderr), "invalid path 'src/a:b.txt'");
    }

    #[test]
    fn a_refused_remote_reports_the_fatal_line() {
        assert_eq!(
            clone_failure_message(
                "Cloning into 'x'...\nfatal: repository 'https://example.com/x/' not found\n"
            ),
            "repository 'https://example.com/x/' not found"
        );
        assert_eq!(clone_failure_message(""), "git clone failed");
    }

    #[test]
    fn progress_weights_received_objects_into_the_wide_band() {
        let progress =
            parse_clone_progress("Receiving objects:  50% (555/1110), 2.3 MiB | 1.2 MiB/s")
                .unwrap();
        assert_eq!(progress.phase, "Receiving objects");
        assert!((progress.fraction - 0.45).abs() < 0.001);
        assert_eq!(progress.detail.as_deref(), Some("2.3 MiB · 1.2 MiB/s"));
    }

    #[test]
    fn progress_reads_remote_phases_and_drops_the_done_marker() {
        let progress =
            parse_clone_progress("remote: Compressing objects: 100% (500/500), done.").unwrap();
        assert_eq!(progress.phase, "Compressing objects");
        assert!((progress.fraction - 0.10).abs() < 0.001);
        assert_eq!(progress.detail, None);
        assert_eq!(
            parse_clone_progress("remote: Enumerating objects: 1234, done.")
                .unwrap()
                .fraction,
            0.0
        );
        assert!(parse_clone_progress("Cloning into 'paneflow'...").is_none());
        assert_eq!(
            parse_clone_progress("Checking out files: 100% (3/3), done.")
                .unwrap()
                .phase,
            "Updating files"
        );
    }

    #[test]
    fn progress_lines_split_on_carriage_returns_across_chunks() {
        let mut carry = Vec::new();
        let mut seen = Vec::new();
        split_progress_lines(
            &mut carry,
            b"Resolving deltas:  10% (1/10)\rResolving del",
            |line| seen.push(line.to_string()),
        );
        split_progress_lines(&mut carry, b"tas: 100% (10/10), done.\n", |line| {
            seen.push(line.to_string())
        });
        assert_eq!(
            seen,
            vec![
                "Resolving deltas:  10% (1/10)".to_string(),
                "Resolving deltas: 100% (10/10), done.".to_string(),
            ]
        );
    }

    #[test]
    fn github_rows_parse_name_and_url() {
        let rows = parse_github_repos(
            br#"[{"nameWithOwner":"theaamgroup/paneflow","url":"https://github.com/theaamgroup/paneflow"},{"nameWithOwner":"x/y","url":"--upload-pack=sh"}]"#,
        );
        assert_eq!(
            rows,
            vec![GitHubRepo {
                name_with_owner: "theaamgroup/paneflow".to_string(),
                url: "https://github.com/theaamgroup/paneflow".to_string(),
            }]
        );
        assert!(parse_github_repos(b"not json").is_empty());
    }

    #[test]
    fn github_rows_filter_on_every_query_word() {
        let repos = parse_github_repos(
            br#"[{"nameWithOwner":"theaamgroup/paneflow","url":"a"},{"nameWithOwner":"theaamgroup/paneflow-web","url":"b"}]"#,
        );
        let hits = filter_github_repos(&repos, "pane web");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name_with_owner, "theaamgroup/paneflow-web");
        assert_eq!(filter_github_repos(&repos, "").len(), 2);
    }

    /// The whole pipeline against a real repository: `run_clone` over the
    /// `file://` transport (which, unlike a bare path, goes through the pack
    /// protocol and so emits `--progress` lines), the stderr tap, the line
    /// splitter, the phase parser, and the destination check.
    #[test]
    fn run_clone_clones_a_local_repository_and_reports_progress() {
        let root = std::env::temp_dir().join(format!(
            "paneflow-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&source)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(source.join("README.md"), "clone me\n").unwrap();
        git(&["add", "README.md"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"]);

        let target = format!("file://{}", source.display());
        assert_eq!(clone_tool_for(&target), CloneTool::Git);
        let destination = root.join(repo_name_from_url(&target).unwrap());
        assert_eq!(
            destination,
            root.join("source"),
            "the name is the URL's tail"
        );
        let destination = root.join("clone");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let result = run_clone(&target, &destination, move |progress| {
            sink.lock().unwrap().push(progress.phase);
        });
        assert_eq!(result, Ok(()));
        assert!(destination.join(".git").is_dir());
        assert!(destination.join("README.md").is_file());
        let seen = seen.lock().unwrap();
        assert!(
            seen.contains(&"Receiving objects"),
            "the tap must relay git's progress phases: {seen:?}"
        );

        // A second clone into the same folder is refused by git, and the
        // failure message is git's own line, not the progress stream.
        let again = run_clone(&target, &destination, |_| {});
        let message = again.unwrap_err();
        assert!(
            message.contains("already exists"),
            "git's refusal must surface: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The modal is wired into both entry points the issue names and the
    /// render root, and it lands the clone through `open_workspace_folders`
    /// (the one path that records recents, #521). Source-text assertions,
    /// like the #105 / #521 / #523 guards: `PaneFlowApp` cannot be built in
    /// a unit test.
    #[test]
    fn the_clone_modal_is_wired_into_the_palette_the_sidebar_and_the_render_root() {
        let production = include_str!("clone_repo.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of this module");
        for needle in [
            // The URL guard runs before the destination prompt, so nothing is
            // spawned for a refused target.
            "if let Some(reason) = reject_unsafe_clone_url(&url) {",
            "let Some(name) = repo_name_from_url(&url) else {",
            // Success lands through the recents-recording path and selects
            // the new workspace.
            "app.open_workspace_folders(std::slice::from_ref(&destination), cx);",
            "app.select_workspace(idx, window, cx);",
            // The clone runs off the render thread.
            "smol::unblock(move || {\n                run_clone(",
            // Close restores focus (issue #108).
            "if let Some(handle) = clone.return_focus {",
            "window.focus(&self.empty_workspace_focus, cx);",
            // DESIGN.md 7.2: listbox rows.
            ".role(gpui::Role::ListBox)",
            "select_option(",
        ] {
            assert!(production.contains(needle), "missing `{needle}`");
        }

        let sidebar = include_str!("sidebar/mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the sidebar module");
        assert!(
            sidebar.contains(".id(\"empty-clone-repo\")"),
            "the sidebar empty state must carry the Clone repository row"
        );
        assert!(
            sidebar.contains("this.open_clone_repo(w, cx);"),
            "the empty-state row must open the modal"
        );

        let main = include_str!("../main.rs");
        assert!(
            main.contains(".on_action(cx.listener(Self::handle_clone_repository))"),
            "the render root must handle CloneRepository"
        );
        assert!(
            main.contains("self.render_clone_repo(cx)"),
            "the render root must mount the modal"
        );

        // The palette reaches the modal as a global registry action.
        assert!(
            crate::keybindings::action_is_global("clone_repository"),
            "clone_repository must be a context-free registry action so the palette lists it"
        );

        // The palette refuses to open over a running clone and folds an idle
        // one, so the two overlays never stack at the same priority.
        let palette = include_str!("command_palette.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the palette module");
        assert!(
            palette.contains("if self.clone_repo_running() {"),
            "the palette must stay closed while a clone runs"
        );
        assert!(
            palette.contains("self.close_clone_repo(window, cx);"),
            "the palette must fold an idle clone modal"
        );
    }
}
