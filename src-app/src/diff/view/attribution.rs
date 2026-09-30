use super::*;

impl DiffView {
    /// Tooltip lines for the Review pane header: a count line, then one
    /// `Agent · <when>` row per attributed session. Empty when nothing is
    /// attributed.
    pub fn attribution_lines(&self) -> Vec<SharedString> {
        attribution_lines_for(&self.column.attribution)
    }

    /// Start session attribution for the rows that just landed (issue #1094).
    /// Attribution runs vendor session-list CLIs with a 30 s budget, so it
    /// follows the rows on its own task instead of holding them back.
    pub(super) fn request_attribution(&mut self, cx: &mut Context<Self>) {
        if let Some(generation) = self.column.begin_attribution() {
            self.spawn_attribution(generation, cx);
        }
    }

    fn spawn_attribution(&mut self, generation: u64, cx: &mut Context<Self>) {
        let load = attribution_load(
            self.column.path.to_string_lossy().into_owned(),
            self.column.branch.clone(),
        );
        cx.spawn(async move |this, cx| {
            let sessions = load.await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx| {
                    let finish = view.column.finish_attribution(generation, sessions);
                    if finish.applied {
                        cx.notify();
                    }
                    if let Some(next) = finish.rerun {
                        view.spawn_attribution(next, cx);
                    }
                });
            });
        })
        .detach();
    }
}

/// What a finished attribution run did.
#[derive(Debug, PartialEq, Eq)]
struct AttributionFinish {
    /// Nothing newer was shown, so the run's sessions were kept.
    applied: bool,
    /// Generation to attribute next, when newer rows landed meanwhile.
    rerun: Option<u64>,
}

impl Column {
    /// Claim the column's one attribution run for the current generation, or
    /// `None` when a run is already in flight; that run's finish reruns.
    fn begin_attribution(&mut self) -> Option<u64> {
        if self.attribution_in_flight {
            self.attribution_queued = true;
            return None;
        }
        self.attribution_in_flight = true;
        Some(self.generation)
    }

    /// Apply a finished run unless an equal or newer generation's result is
    /// already shown. A run superseded by a reload still applies: it depends
    /// only on this column's cwd and branch, which never change, and it is
    /// newer than what is shown. Discarding it would starve the header while
    /// reloads outpace a slow session-list CLI. So the header may briefly show
    /// the previous generation's attribution, for the same column and branch,
    /// until the rerun for the newest landed rows finishes.
    fn finish_attribution(
        &mut self,
        generation: u64,
        sessions: Vec<SessionMeta>,
    ) -> AttributionFinish {
        let applied = self
            .attribution_generation
            .is_none_or(|shown| generation >= shown);
        if applied {
            self.attribution = sessions;
            self.attribution_generation = Some(generation);
        }
        let rerun = std::mem::take(&mut self.attribution_queued) && generation != self.generation;
        self.attribution_in_flight = rerun;
        AttributionFinish {
            applied,
            rerun: rerun.then_some(self.generation),
        }
    }
}

fn attribution_load(
    cwd: String,
    branch: String,
) -> futures::future::BoxFuture<'static, Vec<SessionMeta>> {
    held_attribution_load().unwrap_or_else(|| {
        Box::pin(smol::unblock(move || {
            crate::agent_sessions::attribution_for_column(&cwd, &branch)
        }))
    })
}

#[cfg(not(test))]
fn held_attribution_load() -> Option<futures::future::BoxFuture<'static, Vec<SessionMeta>>> {
    None
}

// Test seam: while held, each attribution run waits on a sender the test
// takes, instead of running vendor CLIs. Runs are spawned from the
// foreground thread, so a thread-local is per test.
#[cfg(test)]
type HeldAttribution = futures::channel::oneshot::Sender<Vec<SessionMeta>>;

#[cfg(test)]
thread_local! {
    static HELD_ATTRIBUTION: RefCell<Option<Vec<HeldAttribution>>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn held_attribution_load() -> Option<futures::future::BoxFuture<'static, Vec<SessionMeta>>> {
    HELD_ATTRIBUTION.with(|held| {
        let mut held = held.borrow_mut();
        let senders = held.as_mut()?;
        let (tx, rx) = futures::channel::oneshot::channel();
        senders.push(tx);
        Some(Box::pin(async move { rx.await.unwrap_or_default() }) as _)
    })
}

/// Make this thread's attribution runs wait for the test to answer them.
#[cfg(test)]
pub(super) fn hold_attribution() {
    HELD_ATTRIBUTION.with(|held| *held.borrow_mut() = Some(Vec::new()));
}

/// Take the attribution runs started since the last call, oldest first.
#[cfg(test)]
pub(super) fn take_held_attribution() -> Vec<HeldAttribution> {
    HELD_ATTRIBUTION.with(|held| {
        held.borrow_mut()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

fn attribution_lines_for(sessions: &[SessionMeta]) -> Vec<SharedString> {
    if sessions.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<SharedString> = Vec::with_capacity(sessions.len() + 1);
    lines.push(
        format!(
            "Attributed to {} session{}",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" }
        )
        .into(),
    );
    for s in sessions {
        let when = crate::agent_sessions::format_relative_time(&s.timestamp);
        lines.push(format!("{} · {when}", s.agent.label()).into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sessions::SessionAgent;

    fn session(agent: SessionAgent, ts: &str) -> SessionMeta {
        SessionMeta {
            agent,
            session_id: "s".into(),
            timestamp: ts.into(),
            cwd: "/repo".into(),
            git_branch: "main".into(),
            summary: None,
        }
    }

    #[test]
    fn attribution_lines_are_a_count_line_plus_agent_and_when_rows() {
        assert!(attribution_lines_for(&[]).is_empty());

        let sessions = [
            session(SessionAgent::Claude, "2026-06-01T10:00:00Z"),
            session(SessionAgent::Codex, "2026-05-01T10:00:00Z"),
        ];
        let lines: Vec<String> = attribution_lines_for(&sessions)
            .iter()
            .map(|l| l.to_string())
            .collect();
        let when = |ts: &str| crate::agent_sessions::format_relative_time(ts);
        assert_eq!(
            lines,
            vec![
                "Attributed to 2 sessions".to_string(),
                format!(
                    "{} · {}",
                    SessionAgent::Claude.label(),
                    when("2026-06-01T10:00:00Z")
                ),
                format!(
                    "{} · {}",
                    SessionAgent::Codex.label(),
                    when("2026-05-01T10:00:00Z")
                ),
            ]
        );
        for line in &lines {
            assert!(!line.contains('$'), "no cost figure: {line}");
            assert!(!line.contains("tokens:"), "no token total: {line}");
            assert!(!line.contains("prices v"), "no price-table footer: {line}");
        }

        let single = attribution_lines_for(&sessions[..1]);
        assert_eq!(single.len(), 2);
        assert_eq!(single[0].as_ref(), "Attributed to 1 session");
    }

    fn agents(col: &Column) -> Vec<SessionAgent> {
        col.attribution.iter().map(|s| s.agent).collect()
    }

    #[test]
    fn attribution_runs_one_at_a_time_and_applies_superseded_results() {
        let mut col = Column::new_loading("main".into(), PathBuf::from("."), None);
        col.generation = 1;
        assert_eq!(col.begin_attribution(), Some(1));

        // Rows for generation 2 land while the first run is in flight: no
        // second run starts beside it.
        col.generation = 2;
        assert_eq!(col.begin_attribution(), None);

        // The superseded run still applies (nothing newer is shown) and the
        // queued request reruns for the newest landed rows.
        let first = [session(SessionAgent::Codex, "2026-05-01T10:00:00Z")];
        assert_eq!(
            col.finish_attribution(1, first.to_vec()),
            AttributionFinish {
                applied: true,
                rerun: Some(2),
            }
        );
        assert_eq!(agents(&col), [SessionAgent::Codex]);
        assert!(col.attribution_in_flight);

        let current = [session(SessionAgent::Claude, "2026-06-01T10:00:00Z")];
        assert_eq!(
            col.finish_attribution(2, current.to_vec()),
            AttributionFinish {
                applied: true,
                rerun: None,
            }
        );
        assert_eq!(agents(&col), [SessionAgent::Claude]);
        assert!(!col.attribution_in_flight);
        assert_eq!(col.begin_attribution(), Some(2));
    }

    #[test]
    fn older_attribution_never_overwrites_a_newer_applied_result() {
        let mut col = Column::new_loading("main".into(), PathBuf::from("."), None);
        col.generation = 3;
        let newer = [session(SessionAgent::Claude, "2026-06-01T10:00:00Z")];
        assert!(col.finish_attribution(3, newer.to_vec()).applied);

        let older = [session(SessionAgent::Codex, "2026-05-01T10:00:00Z")];
        assert_eq!(
            col.finish_attribution(2, older.to_vec()),
            AttributionFinish {
                applied: false,
                rerun: None,
            }
        );
        assert_eq!(agents(&col), [SessionAgent::Claude]);
        assert_eq!(col.attribution_generation, Some(3));

        // The same generation again (a rerun that raced nothing) still applies.
        let again = [session(SessionAgent::Codex, "2026-06-02T10:00:00Z")];
        assert!(col.finish_attribution(3, again.to_vec()).applied);
        assert_eq!(agents(&col), [SessionAgent::Codex]);
    }
}
