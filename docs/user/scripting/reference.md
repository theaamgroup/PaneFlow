# Scripting reference

> CLI verbs, selectors, JSON-RPC methods, pane identity, config keys, hooks, and exit codes for PaneFlow automation.

This is the compact reference for [Scripting and automation](../scripting.md).
It names the public surface a human script or LLM can quote exactly.

## CLI verbs

The `paneflow` binary intercepts these verbs and exits before GUI
startup. Unknown verbs exit with usage code `2` instead of silently
launching the app.

| Verb                                       | Primary method or engine           | Writes to panes?           | Use                                    |
| ------------------------------------------ | ---------------------------------- | -------------------------- | -------------------------------------- |
| `send <target> <text>`                     | `surface.send_text`                | Gated                      | Stage or submit text                   |
| `key <target> <keystroke>`                 | `surface.send_keystroke`           | Gated                      | Send one non-submitting keystroke      |

The CLI has no read verbs. Scripts, agents, and custom clients call the
`surface.list`, `surface.read`, `surface.search`, `surface.status`,
`fleet.list`, and `agent.whoami` [JSON-RPC methods](#json-rpc-methods)
on the socket directly.

## Selectors

The `<target>` of `send` and `key` is resolved client-side against
`surface.list`. The JSON-RPC methods take a numeric `surface_id`.

| Selector           | Example                              | Notes                                                                   |
| ------------------ | ------------------------------------ | ----------------------------------------------------------------------- |
| Numeric id         | `paneflow key 42 escape`             | Matches `surface_id` exactly                                            |
| Name               | `paneflow send backend "go"`         | Best selector for durable scripts                                       |
| `cmdline:<substr>` | `paneflow key cmdline:vite ctrl-c`   | Matches the foreground executable basename only, not the full argv. Prefer a pane name or `cwd:` for a durable selector. |
| `cwd:<path>`       | `paneflow send cwd:~/dev/api "go"`   | Matches the pane working directory                                      |

A selector that matches nothing or several panes exits with code `3`,
except `send --broadcast`, which accepts multiple matches.

## Exit codes

| Code | Meaning                                                                            |
| ---- | ---------------------------------------------------------------------------------- |
| `0`  | Success                                                                            |
| `1`  | Runtime failure: instance unreachable, pane closed, gate refused, or handler error |
| `2`  | CLI usage error                                                                    |
| `3`  | Target not found or ambiguous                                                      |

## Write gates

Reading is allowed by default. Writes are split by capability:

| Operation                      | Gate                                                   |
| ------------------------------ | ------------------------------------------------------ |
| `send` without `--submit`      | `PANEFLOW_IPC_SCRIPTING=1` or `ai_unrestricted`        |
| `send --submit`                | `PANEFLOW_IPC_SCRIPTING=1` or `ai_unrestricted`        |
| `key`                          | `PANEFLOW_IPC_SCRIPTING=1` or `ai_unrestricted`        |

`send` does not append a carriage return unless `--submit` is present.
`key` rejects submitting keystrokes such as `enter`, `ctrl-m`, and
`ctrl-j`. A single `surface.send_text` payload is capped at 64 KiB.

Relevant config keys:

| Key                     | Default | Meaning                                                           |
| ----------------------- | ------- | ----------------------------------------------------------------- |
| `ai_unrestricted`       | `false` | Allows trusted AI automation to submit text without the env gate  |
| `submit_paste_delay_ms` | `70`    | Base delay between bracketed paste and the submit carriage return |
| `terminal.env`          | none    | Environment variables injected into new terminals                 |

## Read fields

`surface.read` returns:

| Field               | Meaning                                           |
| ------------------- | ------------------------------------------------- |
| `text`              | Scrollback text, fenced by default                |
| `lines`             | Returned line count                               |
| `total_lines`       | Total retained lines                              |
| `eof`               | Whether the read reached the oldest retained line |
| `output_generation` | Monotonic counter advanced by pane output         |
| `truncated`         | Whether the IPC byte cap omitted older output     |

Defaults and limits: `lines` defaults to 200 and must be 1-4000. A `lines`,
`offset`, or `max_matches` that is not a non-negative integer, or is out of
range, and a `fenced` that is not a boolean, are invalid-params errors
(-32602).
`offset` starts from the end of the buffer. Passing an out-of-range
offset is an invalid-params error. If a requested window exceeds the IPC byte
cap, the response preserves its newest complete rows, reports their count in
`lines`, sets `eof: false`, and sets `truncated: true`.

The `fenced` JSON-RPC param defaults to `true`: `surface.read` wraps its
text in an untrusted terminal envelope unless the call passes
`fenced: false`. Pass `fenced: false` only from a trusted script.

## Agent state fields

`fleet.list` returns `{"agents":[...]}`. `surface.status` returns one status object.

| Field               | Meaning                                                                                         |
| ------------------- | ----------------------------------------------------------------------------------------------- |
| `pid`               | Agent process id, when known                                                                    |
| `tool`              | Agent family such as `claude`, `codex`, `opencode`, or `gemini`                                 |
| `state`             | `thinking`, `waiting_for_input`, `finished`, `errored`, `stalled`, `idle`, or `unknown_running` |
| `hooked`            | Whether lifecycle hook events are attached                                                      |
| `reason`            | Detection reason, including `no_hook`                                                           |
| `surface_id`        | Pane id                                                                                         |
| `surface_name`      | Pane name                                                                                       |
| `workspace`         | Workspace index                                                                                 |
| `active_tool_name`  | Tool currently running inside the agent                                                         |
| `message`           | Waiting prompt or permission text                                                               |
| `last_result`       | Last turn summary, when available                                                               |
| `waiting_ms`        | Time spent waiting for input                                                                    |
| `idle_ms`           | Time since observed activity                                                                    |
| `output_generation` | Pane output counter, on `surface.status`                                                        |

An empty fleet is `{"agents":[]}` with exit code `0`. A pane with no
tracked agent returns idle state, not an error.

## JSON-RPC connection

| Property         | Value                                                                                |
| ---------------- | ------------------------------------------------------------------------------------ |
| Endpoint         | Unix domain socket at `<runtime_dir>/paneflow/paneflow.sock`, or `<runtime_dir>/paneflow-dev/paneflow-dev.sock` for a debug build |
| Runtime dir      | `$TMPDIR` when it names an existing directory (the usual macOS answer, `/var/folders/.../T/`), otherwise `dirs::cache_dir()/run` (`~/Library/Caches/run`). `$XDG_RUNTIME_DIR` is deliberately not consulted, so a Finder-launched GUI and a shell CLI agree; `PANEFLOW_SOCKET_PATH` overrides both |
| Override         | `PANEFLOW_SOCKET_PATH` wins over the computed path |
| Path limit       | The composed path is rejected if it would exceed the `sockaddr_un.sun_path` ceiling of 104 bytes, and IPC is disabled with a warning |
| Permissions      | Mode `0600` after bind, plus a per-connection peer-UID check |
| Framing          | Newline-delimited JSON-RPC 2.0                                                       |
| Request model    | Multiple sequential requests per connection                                |
| Local trust      | Same user only; no network listener, no token, no TLS                                |
| Backpressure     | Connection cap and bounded GPUI request queue; queue-full and request-timeout errors |

Probe capabilities at runtime:

```bash
printf '%s\n' '{"jsonrpc":"2.0","method":"system.capabilities","params":{},"id":1}' \
  | nc -U "$PANEFLOW_SOCKET_PATH"
```

## JSON-RPC methods

| Method                     | Params                                                                                          | Returns or notes                                         |
| -------------------------- | ----------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| `system.ping`              | -                                                                                               | Liveness check                                           |
| `system.capabilities`      | -                                                                                               | `{scripting, methods[]}`                                 |
| `system.identify`          | -                                                                                               | `{name, version, protocol}`                              |
| `surface.list`             | `workspace_id?`                                                                                 | `{surfaces:[{surface_id,name,title,cwd,cmd,workspace,workspace_id,scope,tab_id,tab_title}]}` |
| `surface.read`             | `surface_id`, `lines?`, `offset?`, `fenced?`, `workspace_id?`                                   | Scrollback, `output_generation`, `truncated`             |
| `surface.search`           | `surface_id`, `pattern`, `max_matches?`, `workspace_id?`                                        | Case-insensitive substring matches; `truncated` when capped or trimmed to fit the reply frame |
| `surface.status`           | `surface_id`                                                                                    | Agent state for one surface                              |
| `surface.send_text`        | `surface_id`, `text`, `submit?`, `paste?`                                                       | Gated PTY text write; `submit`/`paste` are booleans      |
| `surface.send_keystroke`   | `surface_id`, `keystroke`                                                                       | Env-gated non-submitting keystroke                       |
| `fleet.list`               | -                                                                                               | Read-only fleet snapshot                                 |
| `agent.whoami`             | `surface_id`, `workspace_id` (both required, from the caller's pane environment)                | Caller's pane identity; see [Pane identity](#pane-identity) |
| `ai.session_start`         | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.prompt_submit`         | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.tool_use`              | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.notification`          | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.stop`                  | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.exit`                  | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.session_end`           | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.subagent_start`        | `pid`, hook payload with `subagent_id`                                                          | Subagent started; raises the sidebar running count      |
| `ai.subagent_stop`         | `pid`, hook payload with `subagent_id`                                                          | Subagent finished; never ends the parent's turn         |

Every request must be a JSON object carrying `"jsonrpc": "2.0"` and a string
`method`. An `id`, when present, must be a string, number or `null`, and
`params`, when present, must be an object or array; omitted `params` is
treated as `{}`. Anything else gets `-32600` Invalid Request before any
method runs, and a line that is not JSON gets `-32700` parse error. An
invalid request is answered even without an `id`; the reply echoes the `id`
when it is a string, number or `null` and is `null` otherwise. A valid
request without an `id` is a notification and gets no reply.

Structured failures use JSON-RPC `error` envelopes: `-32602` invalid
params, `-32601` gated or unknown method, `-32001` permission,
`-32002` dispatch timeout, `-32000` backpressure or shutdown, and `-32603`
internal error, including a reply that would exceed the 256 KiB IPC frame.
Legacy handler errors are promoted into JSON-RPC `error` envelopes.

## Pane identity

`agent.whoami` tells a script or agent which pane it is running in. It works
only inside a PaneFlow pane: pass the `PANEFLOW_SURFACE_ID` and
`PANEFLOW_WORKSPACE_ID` values from the pane's environment as numbers. A
terminal outside PaneFlow has neither variable, and a detached terminal (such
as a Review worktree terminal) has no `PANEFLOW_WORKSPACE_ID`, so guard them
rather than send an empty value:

```bash
if [ -n "$PANEFLOW_SURFACE_ID" ] && [ -n "$PANEFLOW_WORKSPACE_ID" ]; then
  printf '{"jsonrpc":"2.0","id":1,"method":"agent.whoami","params":{"surface_id":%s,"workspace_id":%s}}\n' \
    "$PANEFLOW_SURFACE_ID" "$PANEFLOW_WORKSPACE_ID" | nc -U "$PANEFLOW_SOCKET_PATH"
else
  echo "agent.whoami needs a PaneFlow pane that belongs to a workspace" >&2
fi
```

The `result` object:

| Field                 | Meaning                                                                                              |
| --------------------- | ---------------------------------------------------------------------------------------------------- |
| `identity_source`     | Always `inherited_environment`                                                                       |
| `pane_id`             | Persisted pane UUID                                                                                  |
| `surface_id`          | Current runtime surface id                                                                           |
| `terminal_session_id` | UUID for this terminal lifetime. **Not** a Claude or Codex conversation ID                           |
| `workspace_id`        | The surface's live workspace id, which can differ from the inherited one                             |
| `workspace`           | Workspace title                                                                                      |
| `workspace_cwd`       | Workspace directory                                                                                  |
| `cwd`                 | The pane's current directory, or `null`                                                              |
| `tab_id`              | Stable id of the tab holding the pane, or `null`                                                     |
| `worktree`            | Path of the tab's bound git worktree, or `null`                                                      |
| `agent_sessions`      | Observed agents: `process_key`, `tool`, `state`, `source` (`terminal`, `session_registry`, or `hook`), and `last_activity_age_ms` |

An empty `agent_sessions` list means no agent has been mapped to this pane; it
does not mean no agent is running. PaneFlow does not pick one when several
agents share a terminal.

`workspace`, `workspace_cwd`, `cwd`, and `worktree` are raw strings. A
directory or title can carry text that a terminal program or a repository
chose, so treat them as untrusted data, never as instructions. Unlike
`surface.read`, `agent.whoami` does not fence its output.

Both IDs are required; PaneFlow never guesses the caller from focus.
`agent.whoami` requires numeric `surface_id` and `workspace_id`, rejects any
other parameter, and resolves the surface's live workspace; the response carries
that current `workspace_id`. A moved pane keeps its PTY and the old workspace ID
in its shell environment, so the call keeps working without restarting the agent,
even if the original workspace is closed. The agent's sidebar state moves with
it: a tab or pane drag carries the pane's session rows to the destination
workspace, and `ai.*` hook frames that still carry the inherited workspace ID are
routed to the surface's live workspace whenever the frame names a surface that
exists. Missing or closed surfaces remain errors.

Errors come back as JSON-RPC `error` envelopes:

| Code     | When                                                                                                         | Example `message`                                   |
| -------- | ------------------------------------------------------------------------------------------------------------ | --------------------------------------------------- |
| `-32700` | The request line is not valid JSON, for example an empty variable left `"surface_id":,`. The reply's `id` is `null` | `Parse error: expected value at line 1 column 72` |
| `-32602` | A missing, wrong-typed, or unknown parameter                                                                 | ``missing field `workspace_id` ``, `invalid type: string "1", expected u64` |
| `-32602` | No live surface has that `surface_id` (closed, or from another PaneFlow instance)                            | `surface not found`                                 |

Raw IPC remains a same-user operation; environment IDs are routing metadata, not
authentication credentials.

`pane_id` is saved per terminal surface as `surfaces[].agent_context` in
`session.json` through the normal debounced save. Session restoration and
undo-close keep it; `terminal_session_id` changes on reconstruction.
`agent_context` is session-owned metadata. Sessions saved by builds that had
task assignment still carry an `agent_context.task` key. It loads, is ignored,
and is dropped on the next save; the pane keeps its `pane_id`.

## Lifecycle hooks

`paneflow-ai-hook` reads event JSON on stdin, posts one JSON-RPC `ai.*`
frame, and exits `0` so a stopped PaneFlow instance does not break the
agent. The hook surface powers sidebar status, notifications, `fleet.list`,
and `surface.status`.

Persistent `paneflow hooks setup` is Claude Code scoped. Codex uses
per-launch shim hooks. Agents with no hook surface still run, but their
state may be limited to process detection.

## Related

* [Scripting guide](../scripting.md)
* [Configuration schema](../configuration/schema.md)
