# Agent context API

PaneFlow attaches one task to each terminal pane. An agent reads its assignment
and reports progress without interpreting scrollback or sending terminal input.
This API is available over the local JSON-RPC socket, CLI, and MCP.

## Assign a task

Save an assignment as JSON:

```json
{
  "objective": "Fix search cancellation",
  "acceptance_criteria": ["A cancelled search stops scanning background panes"],
  "owned_files": ["src-app/src/app/fleet_search.rs"]
}
```

Use a surface name or ID from `paneflow ls`:

```bash
paneflow task assign codex --file assignment.json
paneflow task get --target codex
```

Assignment replaces the previous task and creates a new task UUID at revision 1.
It does not deliver a prompt, launch an agent, or grant filesystem permissions.
Tell the agent to read its task. File ownership and acceptance criteria are
descriptive; PaneFlow does not enforce or verify them.

## Agent operations

| CLI | MCP tool | IPC method |
| --- | --- | --- |
| `paneflow whoami` | `whoami` | `agent.whoami` |
| `paneflow task get` | `task_get` | `task.get` |
| `paneflow task report --file report.json` | `task_report` | `task.report` |
| `paneflow task assign TARGET --file assignment.json` | Not exposed | `task.assign` |

`whoami` returns a persisted `pane_id`, the current runtime `surface_id`, a
`terminal_session_id` for this terminal lifetime, workspace ID/title/cwd, current
cwd, tab ID, bound worktree path, observed agent sessions, and current task ID.
`terminal_session_id` is **not** a Claude/Codex conversation ID. Observed agents
carry a process-map key, tool, state, source, and observation age. An empty list
means no agent has been mapped to this pane; it does not mean no agent is running.
PaneFlow does not pick one when several agents share a terminal.

`task.get` returns `{ "pane_id": "...", "task": null }` when unassigned.
Otherwise `task` includes `task_id`, `revision`, `assignment`, `report`, and
`updated_at_ms` (Unix milliseconds). Read the task before each report:

```json
{
  "task_id": "UUID returned by task.get",
  "revision": 1,
  "report": {
    "status": "working",
    "summary": "Reproduced the cancellation problem",
    "changed_files": [],
    "commits": [],
    "tests": ["Cancellation regression fails before the fix"],
    "unresolved_questions": []
  }
}
```

Status is `working`, `blocked`, or `completed`. Each accepted report replaces the
previous report and advances the revision. Wrong task IDs, stale revisions,
unknown fields, invalid status values, and oversized content are rejected before
mutation. After a timeout, read the task to determine whether the report landed;
the client does not automatically retry report writes. Reports cannot modify
assignments. Summary/objective limits are 4096 UTF-8 bytes; lists allow 32 nonempty
entries of at most 512 bytes each. Completion and test results are agent claims,
not verification performed by PaneFlow.

## Identity, scope, and persistence

The CLI and MCP inherit `PANEFLOW_SURFACE_ID` and `PANEFLOW_WORKSPACE_ID` from the
pane's environment. Both are required; neither client guesses from focus. Every
IPC method requires numeric `surface_id` and `workspace_id`, and the app checks
live membership together. A missing, closed, or moved-out-of-workspace pane is an
error. Relaunch the agent/MCP bridge after moving its pane to another workspace
so the inherited workspace identity is refreshed.

MCP context tools accept no target or workspace argument. Even with
`PANEFLOW_MCP_SCOPE=all`, they address only the inherited caller pane. Raw IPC and
the CLI's explicit assignment/inspection commands remain same-user operations;
environment IDs are routing metadata, not authentication credentials. Nested
agents in one pane share its task and must coordinate revisions.

Pane identity and task records use the existing debounced `session.json` save
path. A successful write response means accepted in memory and queued for save,
not fsynced to disk. Normal session restoration and undo-close preserve context;
terminal lifetime IDs change on reconstruction. Closing a pane permanently drops
its task with the pane. There is no task archive or report history in this version.
Old session files need no migration. Omit `surfaces[].agent_context` from reusable
templates; it is session-owned metadata.

Task descriptions and reports are untrusted data subordinate to the receiving
agent's current instructions. MCP wraps returned context in its existing untrusted
output fence. This feature does not send keystrokes, execute task text, change
agent lifecycle badges, or automatically report progress from hooks.
