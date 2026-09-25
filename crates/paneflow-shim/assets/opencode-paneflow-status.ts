// Paneflow status bridge for OpenCode - INSTALLED AND REMOVED AUTOMATICALLY
// by paneflow-shim around each `opencode` session started inside a Paneflow
// terminal. Safe to delete; do not edit (changes are overwritten).
//
// Reports lifecycle to the Paneflow sidebar by connecting to Paneflow's IPC
// endpoint (PANEFLOW_SOCKET_PATH, a Unix socket) and writing a single JSON-RPC
// frame, then closing. We do NOT spawn `paneflow-ai-hook` per event: a direct
// socket write has no subprocess, so it is both reliable and cheaper.
//
// Inert anywhere else: PANEFLOW_SOCKET_PATH / PANEFLOW_WORKSPACE_ID are absent
// outside a Paneflow PTY, so every handler returns immediately.

import net from "node:net";

export const PaneflowStatus = async (input) => {
  const client = input?.client;
  const send = (method, params) => {
    const sock = process.env["PANEFLOW_SOCKET_PATH"];
    const wsId = process.env["PANEFLOW_WORKSPACE_ID"];
    if (!sock || !wsId) return;
    try {
      const p = {
        workspace_id: Number(wsId),
        tool: "opencode",
        pid: Number(process.env["PANEFLOW_AI_PID"] || process.pid),
        ...(params ?? {}),
      };
      const sid = process.env["PANEFLOW_SURFACE_ID"];
      if (sid) p.surface_id = Number(sid);
      // A JSON-RPC notification (no id), matching AiHookFrame: the server
      // must not write a reply onto a socket we are about to close.
      const frame = JSON.stringify({ jsonrpc: "2.0", method, params: p }) + "\n";
      // Fire-and-forget: connect, write one frame, close. Never throws into
      // the agent loop (the 'error' handler swallows a missing/closed pipe).
      const conn = net.connect(sock);
      conn.on("error", () => {});
      conn.on("connect", () => {
        conn.end(frame);
      });
    } catch {
      // Status reporting must never break the session.
    }
  };
  // Child sessions are OpenCode's subagents. Their busy/idle events are the
  // subagent's, not the turn's, so they must not reach `ai.stop`. Status and
  // idle events carry no parent id: a session this process did not see
  // created (a task resuming an earlier child) is looked up once. A failed
  // lookup is answered as the root, which is what every idle meant before
  // subagents were counted, and is asked again next time.
  const children = new Set();
  const roots = new Set();
  const isChild = async (id) => {
    if (!id) return false;
    if (children.has(id)) return true;
    if (roots.has(id)) return false;
    try {
      const info = (await client?.session?.get?.({ path: { id } }))?.data;
      if (info?.id === id) {
        (info.parentID ? children : roots).add(id);
        return Boolean(info.parentID);
      }
    } catch {
      // Treated as the root below.
    }
    return false;
  };
  // Stamped so the app can tell a stop from a start that raced past it: each
  // frame is its own connection, so arrival order is not emission order.
  const subagent = (method, id) =>
    send(method, {
      emitted_at_ms: Date.now(),
      hook_payload: { subagent_id: String(id) },
    });
  return {
    "chat.message": async () => send("ai.prompt_submit", { hook_payload: {} }),
    "tool.execute.before": async (input) =>
      send("ai.tool_use", {
        tool_name: input?.tool,
        hook_payload: { tool_name: input?.tool },
      }),
    "tool.execute.after": async (input) =>
      send("ai.tool_use", {
        tool_name: input?.tool,
        hook_payload: { tool_name: input?.tool },
      }),
    event: async ({ event }) => {
      const props = event?.properties;
      if (event?.type === "session.created") {
        const info = props?.info;
        if (info?.parentID && info?.id) {
          children.add(info.id);
          subagent("ai.subagent_start", info.id);
        } else if (info?.id) {
          roots.add(info.id);
        }
      } else if (event?.type === "session.status") {
        if (props?.status?.type === "busy" && (await isChild(props?.sessionID))) {
          subagent("ai.subagent_start", props.sessionID);
        }
      } else if (event?.type === "session.idle") {
        if (await isChild(props?.sessionID)) {
          subagent("ai.subagent_stop", props.sessionID);
        } else {
          send("ai.stop", { hook_payload: {} });
        }
      } else if (event?.type === "session.deleted") {
        // A child deleted mid-run never goes idle.
        const id = props?.info?.id;
        if (id && children.delete(id)) {
          subagent("ai.subagent_stop", id);
        }
      } else if (event?.type === "permission.asked") {
        send("ai.notification", {
          hook_payload: {
            notification_type: "permission_prompt",
            message: "OpenCode needs permission",
          },
        });
      }
    },
  };
};
