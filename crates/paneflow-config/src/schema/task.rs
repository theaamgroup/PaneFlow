//! Persisted pane identity and its current agent task. Reports are agent claims,
//! not independently verified outcomes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentContext {
    pub pane_id: String,
    pub task: Option<AgentTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskAssignment {
    pub objective: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub owned_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Working,
    Blocked,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskReport {
    pub status: TaskStatus,
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub commits: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub unresolved_questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentTask {
    pub task_id: String,
    pub revision: u64,
    pub assignment: TaskAssignment,
    pub report: Option<TaskReport>,
    pub updated_at_ms: u64,
}

fn text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 4096
}

fn list(values: &[String]) -> bool {
    values.len() <= 32 && values.iter().all(|value| text(value) && value.len() <= 512)
}

impl TaskAssignment {
    pub fn validate(&self) -> Result<(), &'static str> {
        if text(&self.objective) && list(&self.acceptance_criteria) && list(&self.owned_files) {
            Ok(())
        } else {
            Err("objective must be 1..4096 bytes; lists allow 32 nonempty entries of at most 512 bytes")
        }
    }
}

impl TaskReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if text(&self.summary)
            && [
                &self.changed_files,
                &self.commits,
                &self.tests,
                &self.unresolved_questions,
            ]
            .into_iter()
            .all(|values| list(values))
        {
            Ok(())
        } else {
            Err("summary must be 1..4096 bytes; lists allow 32 nonempty entries of at most 512 bytes")
        }
    }
}

impl AgentTask {
    /// Compare before mutation: retries and reports for replaced assignments
    /// cannot overwrite a newer result. A successful report advances revision.
    pub fn apply_report(
        &mut self,
        task_id: &str,
        revision: u64,
        report: TaskReport,
        updated_at_ms: u64,
    ) -> Result<(), &'static str> {
        report.validate()?;
        if self.task_id != task_id || self.revision != revision {
            return Err("task changed; call task.get before reporting again");
        }
        let next = self
            .revision
            .checked_add(1)
            .ok_or("task revision exhausted")?;
        self.report = Some(report);
        self.revision = next;
        self.updated_at_ms = updated_at_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> AgentTask {
        AgentTask {
            task_id: "assignment-1".into(),
            revision: 1,
            assignment: TaskAssignment {
                objective: "Fix search".into(),
                acceptance_criteria: vec![],
                owned_files: vec![],
            },
            report: None,
            updated_at_ms: 1,
        }
    }

    fn report() -> TaskReport {
        TaskReport {
            status: TaskStatus::Working,
            summary: "Reproduced".into(),
            changed_files: vec![],
            commits: vec![],
            tests: vec![],
            unresolved_questions: vec![],
        }
    }

    #[test]
    fn stale_and_replaced_task_reports_leave_the_record_intact() {
        let mut task = task();
        task.apply_report("assignment-1", 1, report(), 2)
            .expect("first report");
        let saved = task.clone();
        assert!(task.apply_report("assignment-1", 1, report(), 3).is_err());
        assert!(task.apply_report("old-assignment", 2, report(), 3).is_err());
        assert_eq!(task, saved);
    }

    #[test]
    fn oversized_reports_do_not_advance_revision() {
        let mut task = task();
        let mut report = report();
        report.tests = vec!["test".into(); 33];
        assert!(task.apply_report("assignment-1", 1, report, 2).is_err());
        assert_eq!(task.revision, 1);
        assert!(task.report.is_none());
    }

    #[test]
    fn context_round_trip_preserves_identity_assignment_and_report() {
        let mut task = task();
        task.apply_report("assignment-1", 1, report(), 2)
            .expect("report");
        let context = AgentContext {
            pane_id: "pane-1".into(),
            task: Some(task),
        };
        let encoded = serde_json::to_string(&context).expect("serialize");
        assert_eq!(
            serde_json::from_str::<AgentContext>(&encoded).expect("restore"),
            context
        );
    }
}
