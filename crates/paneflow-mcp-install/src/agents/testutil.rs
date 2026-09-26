//! Shared test double for the orchestration tests in `cleanup`.
//!
//! `Mock` implements [`AgentConfigWriter`] with injected outcomes and counts
//! every call, so the cleanup can be exercised across every branch without
//! touching the filesystem, and a test can prove a pass read or edited no
//! config at all.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agents::{AgentConfigWriter, StatusOutcome, UninstallOutcome};

pub(crate) struct Mock {
    id: &'static str,
    status: Cell<Option<Result<StatusOutcome>>>,
    uninstall: Cell<Option<Result<UninstallOutcome>>>,
    pub(crate) status_calls: Cell<usize>,
    pub(crate) uninstall_calls: Cell<usize>,
}

impl Mock {
    /// An agent whose `paneflow` entry runs `command`, and whose uninstall
    /// removes it.
    pub(crate) fn with_entry(id: &'static str, command: &str) -> Self {
        Self::new(
            id,
            Ok(StatusOutcome::Installed {
                path: command.to_string(),
            }),
        )
    }

    /// An agent with no `paneflow` entry.
    pub(crate) fn without_entry(id: &'static str) -> Self {
        Self::new(id, Ok(StatusOutcome::NotInstalled))
    }

    /// An agent whose probe returns `status`.
    pub(crate) fn new(id: &'static str, status: Result<StatusOutcome>) -> Self {
        Self {
            id,
            status: Cell::new(Some(status)),
            uninstall: Cell::new(Some(Ok(UninstallOutcome::Removed {
                file: PathBuf::from(format!("/cfg/{id}.json")),
                backup: PathBuf::from(format!("/cfg/{id}.json.bak")),
            }))),
            status_calls: Cell::new(0),
            uninstall_calls: Cell::new(0),
        }
    }

    pub(crate) fn with_uninstall(self, r: Result<UninstallOutcome>) -> Self {
        self.uninstall.set(Some(r));
        self
    }
}

impl AgentConfigWriter for Mock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        self.id
    }
    fn uninstall(&self) -> Result<UninstallOutcome> {
        self.uninstall_calls.set(self.uninstall_calls.get() + 1);
        self.uninstall
            .take()
            .unwrap_or(Ok(UninstallOutcome::NothingToRemove))
    }
    fn status(&self, _bridge: Option<&Path>) -> Result<StatusOutcome> {
        self.status_calls.set(self.status_calls.get() + 1);
        self.status
            .take()
            .unwrap_or(Ok(StatusOutcome::NotInstalled))
    }
}
