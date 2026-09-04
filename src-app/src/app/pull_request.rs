//! Whether a branch shown in the rail already has a pull request (issue #350,
//! upstream `9e924e5e` + `f6775d82`).
//!
//! The rail's other optional lines read files: `HEAD` for the branch, a
//! `git diff` for the counts. This one is a network answer, so it is cached
//! per `(repository, branch)`, refreshed on the same 30 s tick that refreshes
//! the git state, and only ever fetched while the `sidebar_show.pr` switch is
//! on. With the switch off, [`PaneFlowApp::refresh_pull_requests`] returns
//! before it can reach a process spawn: [`should_look_up`] is the whole
//! decision, and it is pinned by a unit test.
//!
//! `gh` rather than `api.github.com` directly: it already holds the user's
//! credentials, follows GitHub Enterprise hosts, and resolves which repository
//! a directory belongs to. The cost is that this is GitHub-only - a GitLab or
//! Gitea checkout simply never gets an icon, which is also what happens when
//! `gh` is missing or logged out.
//!
//! `gh` is found through `which`, which sees the login shell's `PATH` that
//! `login_shell_env` adopted at launch (a GUI app otherwise inherits launchd's
//! minimal one, and Homebrew's `gh` would be invisible). A miss is resolved
//! once per session, logged once at debug, and disables the feature: "not
//! installed" is "the switch does nothing", never an error surface.
//!
//! The subprocess environment is deliberately *not* scrubbed the way
//! `workspace::worktree::git_command` isolates git: `gh` reads `GH_TOKEN` and
//! its own config, and stripping them would fail auth silently and put
//! the repository into the failure backoff.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{Context, Hsla, Rgba};

use crate::PaneFlowApp;

/// How long a lookup stays good. Long enough that opening and closing tabs
/// does not re-query, short enough that a PR opened from the terminal shows up
/// while you are still in the same sitting.
const TTL: Duration = Duration::from_secs(300);
/// Wall-clock bound for one `gh` call, well under the 30 s tick.
const DEADLINE: Duration = Duration::from_secs(15);
const STDOUT_CAP: u64 = 256 * 1024;
/// How long a repository whose lookup failed is left alone before it is
/// asked again. `gh` exits 1 "for any reason" - a timeout, a rate limit, a
/// missing GitHub remote - so a failure cannot be told apart from a permanent
/// one, and blacklisting for the whole session turned one flaky call into
/// "no marker until restart" (PR #354 review). A backoff keeps the sidebar
/// refresh from hammering a dead remote while still recovering on its own.
const FAILURE_BACKOFF: Duration = Duration::from_secs(600);

/// A branch's pull request, in GitHub's own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrState {
    Draft,
    Open,
    Merged,
    Closed,
}

impl PrState {
    /// GitHub's status colors, in Primer's light and dark values.
    ///
    /// Borrowed rather than mapped onto PaneFlow's own palette: these four
    /// colors are what a pull request means to anyone who has used GitHub, and
    /// a green "merged" or a purple "open" would read as a different state
    /// rather than as a house style.
    pub(crate) fn color(self, ui: crate::theme::UiColors) -> Hsla {
        let light = ui.surface.l > 0.5;
        let hex = self.hex(light);
        Hsla::from(Rgba {
            r: ((hex >> 16) & 0xFF) as f32 / 255.0,
            g: ((hex >> 8) & 0xFF) as f32 / 255.0,
            b: (hex & 0xFF) as f32 / 255.0,
            a: 1.0,
        })
    }

    /// The Primer hex for this state on a light or a dark surface.
    fn hex(self, light: bool) -> u32 {
        match (self, light) {
            (PrState::Open, true) => 0x1a7f37,
            (PrState::Open, false) => 0x3fb950,
            (PrState::Draft, true) => 0x59636e,
            (PrState::Draft, false) => 0x9198a1,
            (PrState::Merged, true) => 0x8250df,
            (PrState::Merged, false) => 0xab7df8,
            (PrState::Closed, true) => 0xcf222e,
            (PrState::Closed, false) => 0xf85149,
        }
    }
}

/// The pull request a branch is carried by, as the rail needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PullRequest {
    pub number: u64,
    pub state: PrState,
}

struct Cached {
    /// `None` means "asked, and this branch has no pull request" - a real
    /// answer worth caching, not a missing one.
    value: Option<PullRequest>,
    at: Instant,
}

/// Pull requests by `(repository root, branch)`.
#[derive(Default)]
pub(crate) struct PrStates {
    entries: HashMap<(String, String), Cached>,
    /// Lookups in flight, so a 30 s tick landing on a slow `gh` does not stack
    /// a second call on the same branch.
    inflight: HashSet<(String, String)>,
    /// Repositories `gh` could not answer for: logged out, not a GitHub
    /// remote, or a transient timeout or rate limit - `gh` exits 1 for all of
    /// them alike. Left alone for [`FAILURE_BACKOFF`], then asked again, so
    /// a flaky call recovers without a restart while a dead remote is not
    /// probed every tick. Per repository, never global: one non-GitHub
    /// checkout must not silence the marker for the GitHub one beside it.
    /// Repository -> the instant before which it is not asked again.
    unavailable: HashMap<String, Instant>,
}

impl PrStates {
    fn key(repo_root: &str, branch: &str) -> (String, String) {
        (repo_root.to_string(), branch.to_string())
    }

    pub(crate) fn get(&self, repo_root: &str, branch: &str) -> Option<PullRequest> {
        self.entries
            .get(&Self::key(repo_root, branch))
            .and_then(|cached| cached.value)
    }

    /// Whether this branch is worth asking about right now: not already in
    /// flight, not answered recently, and in a repository `gh` can speak for.
    fn is_stale(&self, repo_root: &str, branch: &str) -> bool {
        self.is_stale_at(repo_root, branch, Instant::now())
    }

    /// [`Self::is_stale`] against an explicit clock, so the TTL is testable.
    fn is_stale_at(&self, repo_root: &str, branch: &str, now: Instant) -> bool {
        if self
            .unavailable
            .get(repo_root)
            .is_some_and(|until| now < *until)
        {
            return false;
        }
        let key = Self::key(repo_root, branch);
        if self.inflight.contains(&key) {
            return false;
        }
        self.entries
            .get(&key)
            .is_none_or(|cached| now.duration_since(cached.at) > TTL)
    }

    fn mark_inflight(&mut self, repo_root: &str, branch: &str) {
        self.inflight.insert(Self::key(repo_root, branch));
    }

    /// Record an answer. Returns `true` when it differs from what was cached,
    /// so the caller only repaints on a real delta.
    fn store(&mut self, repo_root: &str, branch: &str, value: Option<PullRequest>) -> bool {
        self.store_at(repo_root, branch, value, Instant::now())
    }

    fn store_at(
        &mut self,
        repo_root: &str,
        branch: &str,
        value: Option<PullRequest>,
        at: Instant,
    ) -> bool {
        let key = Self::key(repo_root, branch);
        self.inflight.remove(&key);
        let changed = self.entries.get(&key).map(|c| c.value) != Some(value);
        self.entries.insert(key, Cached { value, at });
        changed
    }

    fn mark_unavailable(&mut self, repo_root: &str, branch: &str) {
        self.mark_unavailable_at(repo_root, branch, Instant::now());
    }

    fn mark_unavailable_at(&mut self, repo_root: &str, branch: &str, now: Instant) {
        self.inflight.remove(&Self::key(repo_root, branch));
        self.unavailable
            .insert(repo_root.to_string(), now + FAILURE_BACKOFF);
    }
}

/// Whether a lookup may run at all: the switch first, then the tool.
///
/// The order is the contract of issue #350: with `sidebar_show.pr` off, no
/// `gh` process is spawned, and `gh_available` is not even called - which is
/// why it is a closure and not a bool. Pure so the decision is pinned without
/// a process runner in the loop.
pub(crate) fn should_look_up(pr_enabled: bool, gh_available: impl FnOnce() -> bool) -> bool {
    pr_enabled && gh_available()
}

/// Where `gh` lives, resolved once per session through the login shell's
/// `PATH`. `None` when it is not installed: logged once, then the feature
/// stays quiet for the session.
fn gh_binary() -> Option<&'static std::path::Path> {
    static GH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    GH.get_or_init(|| match which::which("gh") {
        Ok(path) => Some(path),
        Err(error) => {
            log::debug!(
                "sidebar_show.pr is on but `gh` is not on PATH ({error}); \
                 the pull-request marker stays off this session"
            );
            None
        }
    })
    .as_deref()
}

/// Read the pull request of one branch. Blocking: the caller runs it through
/// `smol::unblock`.
///
/// `Err` means `gh` could not answer at all - the repository then sits out
/// [`FAILURE_BACKOFF`]. `Ok(None)` is a real answer: no pull request here.
fn lookup(
    gh: &std::path::Path,
    repo_root: &std::path::Path,
    branch: &str,
) -> Result<Option<PullRequest>, ()> {
    let mut cmd = std::process::Command::new(gh);
    // `gh` has no global directory flag - `-C` is rejected outright ("unknown
    // shorthand flag: 'C'"), which used to fail every lookup and blacklist the
    // repository for the session (upstream `f6775d82`). It resolves the
    // repository from the working directory instead.
    cmd.current_dir(repo_root)
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,state,isDraft",
            // A branch can carry a history of closed attempts under one open
            // pull request; take enough of them to pick the one that counts.
            "--limit",
            "10",
        ])
        // `gh` prints a survey/update notice on a tty and reads config from
        // the environment; neither matters here, but a pager would hang. The
        // rest of the environment stays: `GH_TOKEN` and `gh`'s config are how
        // it authenticates.
        .env("GH_PAGER", "")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    let out = paneflow_process::run_with_timeout(cmd, DEADLINE, STDOUT_CAP).map_err(|error| {
        log::debug!(
            "gh pr list could not run in {}: {error}",
            repo_root.display()
        );
    })?;
    if !out.status.success() {
        // The only trace of a repository entering the backoff: the
        // caller turns this `Err` into a silent blacklist entry, so without a
        // line here a wrong invocation looks exactly like "no GitHub remote".
        log::debug!(
            "gh pr list failed in {} ({}): {}",
            repo_root.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Err(());
    }
    parse_pr_list(&out.stdout).map_err(|error| {
        log::debug!(
            "gh pr list in {} returned something other than its JSON: {error}",
            repo_root.display()
        );
    })
}

/// `gh pr list --json number,state,isDraft` output to the branch's pull
/// request, or `Ok(None)` for an empty list.
fn parse_pr_list(stdout: &[u8]) -> Result<Option<PullRequest>, serde_json::Error> {
    let parsed: serde_json::Value = serde_json::from_slice(stdout)?;
    Ok(pick(parsed.as_array().map_or(&[], Vec::as_slice)))
}

/// The pull request that describes the branch, out of everything `gh` returned.
///
/// Open beats draft beats merged beats closed: a branch whose old attempt was
/// closed and whose current one is open is, to the person looking at the
/// rail, open. Ties go to the highest number, which is the most recent.
fn pick(rows: &[serde_json::Value]) -> Option<PullRequest> {
    rows.iter()
        .filter_map(|row| {
            let number = row.get("number")?.as_u64()?;
            let draft = row.get("isDraft").and_then(serde_json::Value::as_bool) == Some(true);
            let state = match row.get("state")?.as_str()? {
                "OPEN" if draft => PrState::Draft,
                "OPEN" => PrState::Open,
                "MERGED" => PrState::Merged,
                "CLOSED" => PrState::Closed,
                _ => return None,
            };
            Some(PullRequest { number, state })
        })
        .max_by_key(|pr| {
            let rank = match pr.state {
                PrState::Open => 3,
                PrState::Draft => 2,
                PrState::Merged => 1,
                PrState::Closed => 0,
            };
            (rank, pr.number)
        })
}

impl PaneFlowApp {
    /// The pull request of a branch, if one has been read. Answers only while
    /// the switch is on: a cached answer from before the switch was turned off
    /// must not keep painting the marker.
    pub(crate) fn pull_request_for(
        &self,
        repo_root: &std::path::Path,
        branch: &str,
    ) -> Option<PullRequest> {
        if !self.cached_config.sidebar_show.pr_enabled() || branch.is_empty() {
            return None;
        }
        self.pr_states.get(&repo_root.to_string_lossy(), branch)
    }

    /// Every `(repository, branch)` the rail is currently showing: each
    /// workspace's own branch, plus the branch of every bound tab (#347).
    fn pr_probe_targets(&self) -> Vec<(std::path::PathBuf, String)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for ws in &self.workspaces {
            let Some(repo_root) = ws.repo_root.clone() else {
                continue;
            };
            let mut push = |branch: &str, out: &mut Vec<_>| {
                if branch.is_empty() {
                    return;
                }
                if seen.insert((repo_root.clone(), branch.to_string())) {
                    out.push((repo_root.clone(), branch.to_string()));
                }
            };
            if ws.is_git_repo {
                push(&ws.git_branch, &mut out);
            }
            for tab in ws.tabs() {
                if let Some(git) = self.tab_checkout_git(tab) {
                    push(&git.branch.clone(), &mut out);
                }
            }
        }
        out
    }

    /// Refresh the pull requests of every branch the rail shows, off the render
    /// thread. A no-op while the switch is off: nothing is drawn from it, so
    /// nothing justifies the calls - and the `gh` probe below is not reached.
    pub(crate) fn refresh_pull_requests(&mut self, cx: &mut Context<Self>) {
        if !should_look_up(self.cached_config.sidebar_show.pr_enabled(), || {
            gh_binary().is_some()
        }) {
            return;
        }
        // Resolved above and cached for the session, so this is a lookup in a
        // `OnceLock`, not a second PATH walk.
        let Some(gh) = gh_binary() else {
            return;
        };
        let stale: Vec<(std::path::PathBuf, String)> = self
            .pr_probe_targets()
            .into_iter()
            .filter(|(repo_root, branch)| {
                self.pr_states
                    .is_stale(&repo_root.to_string_lossy(), branch)
            })
            .collect();
        for (repo_root, branch) in stale {
            self.pr_states
                .mark_inflight(&repo_root.to_string_lossy(), &branch);
            let gh = gh.to_path_buf();
            cx.spawn(
                async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                    let read = smol::unblock({
                        let repo_root = repo_root.clone();
                        let branch = branch.clone();
                        move || lookup(&gh, &repo_root, &branch)
                    })
                    .await;
                    let _ = cx.update(|cx| {
                        this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                            let key = repo_root.to_string_lossy();
                            match read {
                                Ok(value) => {
                                    if app.pr_states.store(&key, &branch, value) {
                                        cx.notify();
                                    }
                                }
                                Err(()) => app.pr_states.mark_unavailable(&key, &branch),
                            }
                        })
                    });
                },
            )
            .detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FAILURE_BACKOFF, PrState, PrStates, PullRequest, TTL, parse_pr_list, pick, should_look_up,
    };
    use std::time::{Duration, Instant};

    fn row(number: u64, state: &str, draft: bool) -> serde_json::Value {
        serde_json::json!({ "number": number, "state": state, "isDraft": draft })
    }

    #[test]
    fn the_switch_gates_the_lookup_before_gh_is_consulted() {
        // Issue #350's first "done when": with the switch off, no `gh`
        // process is ever spawned - not even when `gh` is installed, and the
        // PATH is not even walked to find out.
        assert!(!should_look_up(false, || {
            panic!("gh presence consulted with the switch off")
        }));
        // And an absent `gh` keeps the feature quiet with the switch on.
        assert!(!should_look_up(true, || false));
        assert!(should_look_up(true, || true));
    }

    #[test]
    fn refresh_returns_before_the_gh_probe_when_the_switch_is_off() {
        // The order in `refresh_pull_requests` is the contract: the config
        // check must precede the `gh_binary()` resolution, or a user with the
        // switch off would still pay a PATH lookup and a debug line.
        let production = include_str!("pull_request.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        let body = production
            .split("fn refresh_pull_requests(")
            .nth(1)
            .expect("refresh_pull_requests exists");
        let switch = body
            .find("sidebar_show.pr_enabled()")
            .expect("refresh checks the switch");
        let probe = body.find("gh_binary()").expect("refresh resolves gh");
        let spawn = body.find("cx.spawn(").expect("refresh spawns the lookup");
        assert!(
            switch < probe && probe < spawn,
            "switch, then gh presence, then the spawn: {switch} < {probe} < {spawn}"
        );
        assert!(
            !production
                .split("fn lookup(")
                .next()
                .expect("everything before lookup")
                .contains("Command::new("),
            "the only process spawn lives in `lookup`, behind the gate"
        );
    }

    #[test]
    fn an_open_pull_request_outranks_a_closed_attempt() {
        // The rail answers "is this branch in review?", so a branch whose
        // first attempt was closed and whose second is open reads as open,
        // whatever order `gh` listed them in.
        let picked = pick(&[row(12, "OPEN", false), row(40, "CLOSED", false)]).unwrap();
        assert_eq!(picked.number, 12);
        assert_eq!(picked.state, PrState::Open);
    }

    #[test]
    fn a_draft_is_its_own_state_and_the_newest_wins_a_tie() {
        assert_eq!(pick(&[row(3, "OPEN", true)]).unwrap().state, PrState::Draft);
        let picked = pick(&[row(7, "MERGED", false), row(9, "MERGED", false)]).unwrap();
        assert_eq!(picked.number, 9, "the most recent of equals is the answer");
    }

    #[test]
    fn no_rows_is_an_answer_and_junk_is_not_a_crash() {
        assert!(pick(&[]).is_none());
        assert!(pick(&[serde_json::json!({ "number": 1 })]).is_none());
        assert!(pick(&[row(1, "SOMETHING_NEW", false)]).is_none());
    }

    #[test]
    fn gh_output_parses_to_a_state_and_a_color_per_state() {
        // The exact shape `gh pr list --json number,state,isDraft` prints.
        let stdout = br#"[{"isDraft":false,"number":42,"state":"MERGED"},{"isDraft":true,"number":51,"state":"OPEN"}]"#;
        let pr = parse_pr_list(stdout).unwrap().unwrap();
        assert_eq!(
            pr,
            PullRequest {
                number: 51,
                state: PrState::Draft
            }
        );
        assert_eq!(parse_pr_list(b"[]").unwrap(), None, "no PR is an answer");
        assert!(
            parse_pr_list(b"gh: To get started with GitHub CLI, please run: gh auth login")
                .is_err(),
            "non-JSON output is a failure, not an empty answer"
        );

        // Each state has its own Primer color on both surfaces, and no two
        // states share one: a merged and an open marker must never look alike.
        for light in [true, false] {
            let hexes = [
                PrState::Open.hex(light),
                PrState::Draft.hex(light),
                PrState::Merged.hex(light),
                PrState::Closed.hex(light),
            ];
            for (i, a) in hexes.iter().enumerate() {
                for b in &hexes[i + 1..] {
                    assert_ne!(a, b, "two states share a color on light={light}");
                }
            }
        }
        assert_eq!(
            PrState::Open.hex(false),
            0x3fb950,
            "GitHub's open green, dark"
        );
        assert_eq!(
            PrState::Open.hex(true),
            0x1a7f37,
            "GitHub's open green, light"
        );
    }

    #[test]
    fn a_cached_answer_ages_out_after_the_ttl_and_inflight_is_not_asked_twice() {
        let mut states = PrStates::default();
        let t0 = Instant::now();
        assert!(
            states.is_stale_at("/repo", "main", t0),
            "never asked: stale"
        );

        states.mark_inflight("/repo", "main");
        assert!(
            !states.is_stale_at("/repo", "main", t0),
            "a lookup in flight is not asked again"
        );

        let pr = Some(PullRequest {
            number: 7,
            state: PrState::Open,
        });
        assert!(
            states.store_at("/repo", "main", pr, t0),
            "first answer is a change"
        );
        assert_eq!(states.get("/repo", "main"), pr);
        assert!(
            !states.is_stale_at("/repo", "main", t0 + TTL - Duration::from_secs(1)),
            "fresh inside the TTL"
        );
        assert!(
            states.is_stale_at("/repo", "main", t0 + TTL + Duration::from_secs(1)),
            "stale once the TTL has passed"
        );

        // Merging the PR: the next answer replaces the old one and reports the
        // delta; the same answer again is not a repaint.
        let merged = Some(PullRequest {
            number: 7,
            state: PrState::Merged,
        });
        assert!(states.store_at("/repo", "main", merged, t0 + TTL * 2));
        assert!(!states.store_at("/repo", "main", merged, t0 + TTL * 3));
        // "No pull request" is a cached answer too, and a change from merged.
        assert!(states.store_at("/repo", "main", None, t0 + TTL * 4));
        assert_eq!(states.get("/repo", "main"), None);
        assert!(!states.is_stale_at("/repo", "main", t0 + TTL * 4));
    }

    #[test]
    fn a_failed_lookup_blacklists_only_that_repository() {
        let mut states = PrStates::default();
        states.mark_inflight("/gitlab", "feature");
        let now = Instant::now();
        states.mark_unavailable_at("/gitlab", "feature", now);
        assert!(
            !states.is_stale_at("/gitlab", "feature", now),
            "the failed repo is not asked again inside the backoff"
        );
        assert!(
            !states.is_stale_at(
                "/gitlab",
                "feature",
                now + FAILURE_BACKOFF - Duration::from_secs(1)
            ),
            "still inside the backoff"
        );
        assert!(
            states.is_stale_at("/gitlab", "feature", now + FAILURE_BACKOFF),
            "a transient failure is retried once the backoff has passed (PR #354 review)"
        );
        assert!(
            !states.is_stale_at("/gitlab", "another-branch", now),
            "the blacklist is per repository, not per branch"
        );
        assert!(
            states.is_stale_at("/github", "feature", now),
            "a sibling repository is unaffected"
        );
        assert_eq!(states.get("/gitlab", "feature"), None);
    }
}
