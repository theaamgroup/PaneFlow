# Scripting reference

> CLI verbs, selectors, JSON-RPC methods, config keys, MCP tools, hooks, and exit codes for PaneFlow automation.

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

The CLI has no read verbs. Agents read panes through the
[MCP bridge](#mcp-bridge) tools; scripts and custom clients call the
`surface.list`, `surface.read`, `surface.search`, `surface.status`,
`fleet.list`, and `agent.whoami` [JSON-RPC methods](#json-rpc-methods)
on the socket directly.

## Selectors

The `<target>` of `send` and `key` is resolved client-side against
`surface.list`. The JSON-RPC methods take a numeric `surface_id`; the MCP
tools take a name or `surface_id`.

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
| `ai_injection_fence`    | `true`  | Wraps `surface.read` text in an untrusted terminal envelope       |
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
(-32602), the same rule the MCP tools apply.
`offset` starts from the end of the buffer. Passing an out-of-range
offset is an invalid-params error. If a requested window exceeds the IPC byte
cap, the response preserves its newest complete rows, reports their count in
`lines`, sets `eof: false`, and sets `truncated: true`.

The `fenced` JSON-RPC param defaults to `ai_injection_fence`; pass
`fenced: false` only from a trusted script.

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
| `surface.list`             | `workspace_id?`                                                                                 | `{surfaces:[{surface_id,name,title,cwd,cmd,workspace,workspace_id,scope,tab_id,tab_title}]}`; agents-pane surfaces have no `workspace_id` and are omitted when the filter is set |
| `surface.read`             | `surface_id`, `lines?`, `offset?`, `fenced?`, `workspace_id?`                                   | Scrollback, `output_generation`, `truncated`             |
| `surface.search`           | `surface_id`, `pattern`, `max_matches?`, `workspace_id?`                                        | Case-insensitive substring matches                       |
| `surface.status`           | `surface_id`                                                                                    | Agent state for one surface                              |
| `surface.send_text`        | `surface_id`, `text`, `submit?`, `paste?`                                                       | Gated PTY text write                                     |
| `surface.send_keystroke`   | `surface_id`, `keystroke`                                                                       | Env-gated non-submitting keystroke                       |
| `fleet.list`               | -                                                                                               | Read-only fleet snapshot                                 |
| `agent.whoami`             | `surface_id`, `workspace_id` (both required, from the caller's pane environment)                | Caller's pane identity; see the [MCP bridge](../../mcp-bridge.md#pane-identity-whoami) |
| `ai.session_start`         | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.prompt_submit`         | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.tool_use`              | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.notification`          | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.stop`                  | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.exit`                  | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.session_end`           | hook payload                                                                                    | Agent lifecycle event                                   |
| `ai.subagent_start`        | `pid`, hook payload with `subagent_id`                                                          | Subagent started; raises the sidebar running count      |
| `ai.subagent_stop`         | `pid`, hook payload with `subagent_id`                                                          | Subagent finished; never ends the parent's turn         |

Structured failures use JSON-RPC `error` envelopes: `-32602` invalid
params, `-32601` gated or unknown method, `-32001` permission,
`-32002` dispatch timeout, and `-32000` backpressure or shutdown.
Legacy handler errors are promoted into JSON-RPC `error` envelopes.

## MCP bridge

`paneflow-mcp` is a read-only stdio MCP server over the same PaneFlow
socket.

| Tool          | Params                              | Returns                                                             |
| ------------- | ----------------------------------- | ------------------------------------------------------------------- |
| `list_panes`  | -                                   | Panes with `surface_id`, `name`, `title`, `cwd`, `cmd`, `workspace`, `scope` |
| `read_pane`   | `target`, `lines?`, `offset?`       | Scrollback text                                                     |
| `search_pane` | `target`, `pattern`, `max_matches?` | Matching lines                                                      |
| `whoami`      | -                                   | The calling pane's own identity (`pane_id`, `surface_id`, workspace, tab, observed agents) |

It has no tool for typing, submitting, focusing, or splitting panes.
Returned terminal output is fenced as untrusted data.

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
