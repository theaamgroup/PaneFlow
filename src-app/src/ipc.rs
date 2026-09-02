//! JSON-RPC socket server for AI agent control.
//!
//! Listens on `<runtime_dir>/paneflow/paneflow.sock` (Unix domain socket).
//! Each connection reads newline-delimited JSON-RPC requests and writes
//! responses.
//!
//! The `interprocess` crate's `local_socket` module provides the Unix
//! domain socket. The wire protocol is newline-delimited JSON-RPC 2.0.
//!
//! ## Trust model - local-only, owner-UID enforcement (US-010)
//!
//! The IPC server is **strictly local**: it has no network surface,
//! no port binding, no remote identity. Trust derives entirely from
//! filesystem and kernel-credential boundaries:
//!
//! - **Socket file mode 0600**: set immediately after bind in
//!   `bind_socket`. Non-owner processes on the same machine cannot
//!   `connect()` past the kernel filesystem check.
//! - **Peer-UID enforcement**: every accepted connection runs
//!   `LOCAL_PEERCRED` and compares the peer's UID to the server's.
//!   A mismatch returns a JSON-RPC `-32001 permission denied` error
//!   envelope and closes the stream BEFORE any method dispatches.
//!   Defence-in-depth - if a privileged third party bypasses the
//!   file-mode check (e.g. mode-fixing automation), the kernel
//!   credential check still rejects them.
//!
//! No HMAC tokens, no TLS - both would add complexity without
//! meaningful gain on a local-only socket. If the IPC ever grows a
//! network surface, that decision must be revisited.
//!
//! ## Per-method blast radius (US-012 cli-hardening-followup-2026-Q3)
//!
//! The trust model above gates *who* can connect (same-UID only). It
//! does NOT gate *what* an authorised client can do. The methods
//! below carry different blast radii once connected:
//!
//! - `system.*`: read-only health checks. Safe.
//! - `workspace.list` / `workspace.current` / `workspace.select`: navigation
//!   with visible UI side effects and no file/system mutation.
//! - `workspace.close`: process/workspace lifecycle and possible managed-
//!   worktree retirement. Gated behind the orchestration opt-in and still
//!   routed through the visible close confirmation.
//! - `workspace.create`: spawns a PTY at `cwd`. `cwd` is
//!   canonicalised (US-014) and rejected if not a directory.
//! - `surface.split`: layout mutation, bounded by `MAX_PANES` on the tab
//!   owning the targeted surface (US-003, `prd-cli-tab-hierarchy`). Bare layout
//!   splits are navigation-level; spawn fields are gated like `workspace.up`.
//! - `workspace.up`: multi-pane creation. Navigation-only pane specs are
//!   allowed for same-UID clients, but `command`, `prompt`, `context`, and
//!   non-empty `env` and `managed_worktree` ownership are orchestration primitives gated behind
//!   `PANEFLOW_IPC_ORCHESTRATION=1`. `PANEFLOW_IPC_SCRIPTING=1` also enables
//!   them as a broader legacy opt-in.
//! - **`surface.send_text` / `surface.send_keystroke`: same-UID RCE
//!   primitive when enabled.** A connected client can inject
//!   arbitrary bytes (including `\n`) into any visible PTY,
//!   effectively running any shell command in the user's
//!   privileges. These are gated behind the
//!   `PANEFLOW_IPC_SCRIPTING=1` opt-in env var; when unset (the
//!   default), the handlers return JSON-RPC error
//!   `-32601 Method not enabled`. The intended consumer is the
//!   trusted same-UID `paneflow-ai-hook` binary; the wrapper
//!   installer can set the env var on the user's behalf with a
//!   visible prompt. `surface.send_keystroke` additionally
//!   rejects CRLF bytes regardless of the opt-in (CRLF injection
//!   bypass guard).
//! - `ai.*`: lifecycle telemetry from the AI hook. Read-only on
//!   the host UI side; safe.
//!
//! ## Methods
//!
//! - `system.ping` / `system.capabilities` / `system.identify` - stateless
//!   health checks handled directly on the socket thread.
//! - `workspace.list` / `workspace.current` / `workspace.select` - workspace
//!   navigation; `workspace.close` is an orchestration-gated lifecycle action.
//! - `workspace.create` - accepts `name` (string, default "Terminal"),
//!   `cwd` (string path, optional) and `layout` (optional `LayoutNode`
//!   JSON, US-001). When `layout` is present, the new workspace's pane
//!   tree is built from the layout in a single round-trip; when absent,
//!   behavior is unchanged (a single default pane). A malformed `layout`
//!   payload returns the JSON-RPC `-32602 Invalid params` error envelope
//!   and leaves no orphan workspace behind.
//! - `workspace.restore_layout` - apply a `LayoutNode` to the active
//!   workspace (used by session restore).
//! - `surface.list` / `surface.send_text` / `surface.send_keystroke` /
//!   `surface.split` - pane operations.
//! - `ai.session_start` / `ai.prompt_submit` / `ai.tool_use` /
//!   `ai.notification` / `ai.stop` / `ai.exit` / `ai.session_end` - AI
//!   hook lifecycle (`ai.exit` carries the wrapped agent binary's real
//!   exit status, EP-004 US-010).
//!
//! Handlers may return a structured JSON-RPC error by emitting the
//! `_jsonrpc_error` sentinel (see `app::ipc_handler::JsonRpcError`); the
//! dispatcher promotes it to a proper `error` envelope. Legacy
//! application errors returned as `{"error": "string"}` are also promoted
//! so clients never treat failures as successful `result` payloads.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use interprocess::ConnectWaitMode;
use interprocess::TryClone;
use interprocess::local_socket::{
    ConnectOptions, GenericFilePath, Listener, ListenerOptions, Stream, prelude::*,
};
// `ListenerNonblockingMode` is only referenced by the clobber-detection
// accept loop.
#[cfg(unix)]
use interprocess::local_socket::ListenerNonblockingMode;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// IPC request type - sent from socket thread to GPUI thread
// ---------------------------------------------------------------------------

pub struct IpcRequest {
    pub method: String,
    pub params: Value,
    pub _id: Value,
    pub response_tx: mpsc::Sender<Value>,
    /// Single CAS lifecycle (issue #38): `IPC_DISPATCH_QUEUED` → `STARTED`
    /// (GPUI, just before `handle_ipc`) or `CANCELLED` (socket 5 s timeout).
    /// Exactly one transition wins, so a timed-out `workspace.create` /
    /// `workspace.up` / `surface.split` cannot still run after `-32002`.
    pub dispatch: Arc<AtomicU8>,
    /// EP-003 US-010 (agent-control-plane): the socket peer's PID, captured
    /// from `LOCAL_PEERCRED` once per connection (None when the kernel does
    /// not expose a peer PID). Used only to trace writes
    /// granted by AI free-access mode; never an authorization input.
    pub caller_pid: Option<i64>,
}

/// Dispatch lifecycle for a GPUI-bound IPC request (issue #38).
///
/// One atomic replaces the previous `cancelled` + `started` pair. Both the
/// socket timeout path and the GPUI consumer CAS out of `QUEUED`.
pub(crate) const IPC_DISPATCH_QUEUED: u8 = 0;
pub(crate) const IPC_DISPATCH_STARTED: u8 = 1;
pub(crate) const IPC_DISPATCH_CANCELLED: u8 = 2;

/// GPUI: Queued → Started. False means the socket thread already cancelled
/// and returned `-32002`; skip `handle_ipc`.
///
/// Strong CAS only: a spurious `compare_exchange_weak` failure would skip a
/// live request while the socket thread waits forever for a handler that
/// never starts.
#[must_use]
pub(crate) fn try_start_dispatch(state: &AtomicU8) -> bool {
    state
        .compare_exchange(
            IPC_DISPATCH_QUEUED,
            IPC_DISPATCH_STARTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

/// Socket timeout: Queued → Cancelled. False means GPUI already started;
/// wait for the real response instead of returning `-32002`.
#[must_use]
pub(crate) fn try_cancel_dispatch(state: &AtomicU8) -> bool {
    state
        .compare_exchange(
            IPC_DISPATCH_QUEUED,
            IPC_DISPATCH_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

#[must_use]
pub(crate) fn dispatch_is_started(state: &AtomicU8) -> bool {
    state.load(Ordering::Acquire) == IPC_DISPATCH_STARTED
}

#[must_use]
pub(crate) fn dispatch_is_cancelled(state: &AtomicU8) -> bool {
    state.load(Ordering::Acquire) == IPC_DISPATCH_CANCELLED
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IpcState {
    Online,
    Disabled,
}

const IPC_STATE_ONLINE: u8 = 0;
const IPC_STATE_DISABLED: u8 = 1;

/// US-022: hard cap on the bytes a single newline-delimited request may
// US-013: JSON-RPC framing ceiling, centralized (see `crate::limits`). Still
// accessible as `super::MAX_REQUEST_LEN` from the tests submodule via this use.
use crate::limits::MAX_REQUEST_LEN;

/// US-022: ceiling on concurrently-served request IPC connections. The accept
/// loop spawns one blocking thread per connection; without a cap a same-UID
/// peer opening connections in a loop fans out unbounded OS threads.
const MAX_REQUEST_CONNECTIONS: usize = 16;

/// Persistent `events.subscribe` streams keep a socket open by design. They
/// get their own cap so watches cannot consume every short-request slot.
const MAX_SUBSCRIPTION_CONNECTIONS: usize = 16;

const MAX_CONCURRENT_CONNECTIONS: usize = MAX_REQUEST_CONNECTIONS + MAX_SUBSCRIPTION_CONNECTIONS;

/// EP-004 US-010: bounded queue from the socket handler threads to the GPUI
/// thread. Once 256 requests are pending, new GPUI-bound requests fail fast
/// with an overload error instead of growing memory without a cap.
pub(crate) const IPC_REQUEST_QUEUE_CAPACITY: usize = 256;

/// Issue #283: process-wide mirror of the `ai_unrestricted` config switch.
/// `system.capabilities` is answered on the socket thread, which has no
/// `cached_config`, while the live `surface.send_text` gate on the GPUI tick
/// is `env OR ai_unrestricted`. The GPUI thread writes this flag at startup,
/// on every config reload, and from the Settings toggle so the advertised
/// `scripting` capability agrees with the gate that actually accepts writes.
static AI_UNRESTRICTED: AtomicBool = AtomicBool::new(false);

/// Publish the current `ai_unrestricted` value to the socket thread.
pub(crate) fn set_ai_unrestricted(enabled: bool) {
    AI_UNRESTRICTED.store(enabled, Ordering::Relaxed);
}

/// Read the mirrored `ai_unrestricted` value (socket thread).
fn ai_unrestricted() -> bool {
    AI_UNRESTRICTED.load(Ordering::Relaxed)
}

/// The `scripting` capability `system.capabilities` advertises. Must equal
/// the effective `surface.send_text` / `surface.send_keystroke` write gate
/// (`ipc_handler::send_text_gate_open`): open when `PANEFLOW_IPC_SCRIPTING=1`
/// OR `ai_unrestricted` is on. Pure truth table, so it is unit-tested without
/// mutating the process environment.
pub(crate) fn scripting_capability_from(
    scripting_env: Option<&str>,
    ai_unrestricted: bool,
) -> bool {
    matches!(scripting_env, Some("1")) || ai_unrestricted
}

/// EP-004 US-011: maximum live IPC handlers the GPUI thread runs in one tick.
/// Remaining queued requests stay pending for the next scheduled tick.
pub(crate) const IPC_DRAIN_MAX_PER_TICK: usize = 64;

/// Cancelled requests do not spend live handler budget, but draining them is
/// still bounded so a backlog of timed-out requests cannot monopolize a tick.
pub(crate) const IPC_DRAIN_MAX_DEQUEUES_PER_TICK: usize = IPC_DRAIN_MAX_PER_TICK * 2;

/// US-022: idle read deadline per connection. A peer that opens a connection
/// and then sends nothing (or stops mid-stream) otherwise pins its handler
/// thread forever. Enforced at the OS level via `set_recv_timeout`. Generous
/// enough never to cut a real request (clients send immediately on connect
/// and use one connection per request).
const IPC_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for server-side writes. A peer that connects and stops draining
/// must not pin a handler thread while Paneflow tries to write a reply,
/// overload rejection, heartbeat, or event frame.
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct IpcStatus {
    state: Arc<AtomicU8>,
}

impl IpcStatus {
    fn online() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(IPC_STATE_ONLINE)),
        }
    }

    pub(crate) fn state(&self) -> IpcState {
        match self.state.load(Ordering::Acquire) {
            IPC_STATE_DISABLED => IpcState::Disabled,
            _ => IpcState::Online,
        }
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.state() == IpcState::Disabled
    }

    fn disable(&self) {
        self.state.store(IPC_STATE_DISABLED, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Socket server
// ---------------------------------------------------------------------------

/// Pure truth table for `PANEFLOW_ALLOW_MULTIPLE`. Only the documented
/// opt-in `=1` skips the singleton guard. Unset, empty, `0`, `false`,
/// and other truthy strings keep it. Extracted so the rule can be
/// unit-tested without mutating the process environment (unsafe on
/// recent Rust, and races with other threads under `cargo test`).
fn allow_multiple_from(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

/// Start the IPC server on a dedicated OS thread.
/// Returns the receiver for IPC requests to be polled by the GPUI thread.
///
/// The server monitors the socket file on disk and automatically
/// re-binds when another instance (e.g. `cargo run`) clobbers it. Without
/// this, the listener becomes orphaned (wrong inode) and all new connections
/// get `ECONNREFUSED`, silently disabling AI hook integration.
pub fn start_server() -> (
    mpsc::Receiver<IpcRequest>,
    IpcStatus,
    Arc<crate::ipc_events::EventBus>,
) {
    // US-012 (cli-hardening-followup-2026-Q3): one-time boot-time
    // warn-log when scripting is enabled. The per-call gate in
    // `surface.send_text` / `surface.send_keystroke` stays the
    // enforcement boundary; this log surfaces the active-RCE-primitive
    // posture in `paneflow-debug.log` so the operator notices when
    // PANEFLOW_IPC_SCRIPTING was inherited from a launcher script or
    // sourced .env file without their realising.
    let scripting_enabled = std::env::var("PANEFLOW_IPC_SCRIPTING").as_deref() == Ok("1");
    let orchestration_enabled =
        scripting_enabled || std::env::var("PANEFLOW_IPC_ORCHESTRATION").as_deref() == Ok("1");
    if scripting_enabled {
        tracing::warn!(
            "ipc.scripting_enabled is ON; any same-UID process can inject keystrokes into agent panes"
        );
    }
    if orchestration_enabled {
        tracing::warn!(
            "ipc.orchestration_enabled is ON; any same-UID process can create panes with commands, prompts, context, or env"
        );
    }

    let (tx, rx) = mpsc::sync_channel(IPC_REQUEST_QUEUE_CAPACITY);
    let status = IpcStatus::online();
    let thread_status = status.clone();

    // EP-002 (agent-control-plane): the outbound event bus. One handle stays in
    // start_server to be returned to the GPUI app (it broadcasts); a clone moves
    // into the IPC thread so each accepted connection can register a subscriber.
    let event_bus = crate::ipc_events::EventBus::new();
    let thread_event_bus = Arc::clone(&event_bus);

    // Singleton guard: probe the socket BEFORE the IPC thread spawns and
    // before `bind_socket` reclaims any stale socket. If
    // another live Paneflow instance is already listening, two parallel
    // processes will otherwise enter an endless mutual clobber loop -
    // each detects the other's rebind at the next 5 s health check, drops
    // its listener, and re-creates the file, perpetuating the cycle.
    // During every micro-window between drop and re-create, the AI shim's
    // `connect()` fails, an IPC message is silently lost, and a session's
    // `Thinking` / `Done` / `session_start` status stays stale forever.
    //
    // Escape hatch: `PANEFLOW_ALLOW_MULTIPLE=1` skips the guard for the
    // rare case of intentional side-by-side debug instances. Any other
    // value (unset, empty, `0`, `false`, `true`) keeps the singleton.
    // Tests do not call `start_server`, so they are unaffected.
    if !allow_multiple_from(std::env::var("PANEFLOW_ALLOW_MULTIPLE").ok().as_deref())
        && let Some(socket_spec) = socket_path_spec()
        && let Some(info) = detect_existing_instance(socket_spec.path())
    {
        eprintln!(
            "paneflow: another PaneFlow instance is already running on {}.\n\
             Existing instance: {}\n\
             Close the open window first, or set PANEFLOW_ALLOW_MULTIPLE=1 to override.",
            socket_spec.path().display(),
            info
        );
        log::error!(
            "singleton guard: refusing to start; existing instance on {} ({})",
            socket_spec.path().display(),
            info
        );
        std::process::exit(1);
    }

    // US-005 (cli-hardening-followup-2026-Q3): the IPC thread spawn
    // is fallible (RLIMIT_NPROC exhaustion on a low-ulimit container,
    // EAGAIN on a fork-bombed host). The previous `.expect()` panicked
    // the GPUI main thread on that error, killing every active agent.
    // Mirror the runtime spawn pattern at `runtime.rs:1022-1034`:
    // log + return the `rx` early with no live producer; the consumer
    // is now responsible for tolerating a never-firing channel
    // (it does -- the GPUI poll path checks `try_recv` non-blocking).
    let spawn_result = std::thread::Builder::new()
        .name("paneflow-ipc".into())
        .spawn(move || {
            let Some(socket_spec) = socket_path_spec() else {
                thread_status.disable();
                log::warn!(
                    "paneflow: could not resolve a usable IPC socket path - IPC server disabled. \
                     See earlier runtime_paths warnings for the specific cause."
                );
                return;
            };
            let socket_path = socket_spec.path().to_path_buf();

            // The socket lives on the filesystem, so the parent dir must exist.
            #[cfg(unix)]
            if !prepare_socket_parent(&socket_spec) {
                thread_status.disable();
                return;
            }

            let listener = match bind_socket(&socket_path) {
                Some(l) => l,
                None => {
                    thread_status.disable();
                    return;
                }
            };

            #[cfg(unix)]
            let mut our_ino = socket_inode(&socket_path).unwrap_or(0);
            #[cfg(unix)]
            let mut last_health_check = std::time::Instant::now();
            #[cfg(unix)]
            let mut listener = listener;

            // Non-blocking accept lets the loop periodically re-verify the
            // socket inode (clobber detection) without starving connections.
            #[cfg(unix)]
            listener
                .set_nonblocking(ListenerNonblockingMode::Accept)
                .ok();

            // US-022: bound the number of concurrently-served connections so a
            // peer opening sockets in a loop can't fan out unbounded threads.
            // Only this (single) accept thread increments; handler threads
            // decrement via the RAII guard below, so the load is exact.
            let active_connections = Arc::new(AtomicUsize::new(0));
            let active_subscriptions = Arc::new(AtomicUsize::new(0));

            // Decrement the live-connection count on any handler exit path
            // (return, EOF, panic-unwind). Hoisted out of the spawn closure so
            // it can be constructed BEFORE the spawn and moved in: if the spawn
            // itself fails, the closure (and this guard) is dropped, running the
            // decrement and restoring the slot the `fetch_add` below claimed.
            struct ActiveGuard(Arc<AtomicUsize>);
            impl Drop for ActiveGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }

            loop {
                match listener.accept() {
                    Ok(stream) => {
                        if active_connections.load(Ordering::Acquire) >= MAX_CONCURRENT_CONNECTIONS
                        {
                            reject_overloaded(stream);
                            continue;
                        }
                        active_connections.fetch_add(1, Ordering::AcqRel);
                        let guard = ActiveGuard(Arc::clone(&active_connections));
                        let tx = tx.clone();
                        let bus = Arc::clone(&thread_event_bus);
                        let subscriptions = Arc::clone(&active_subscriptions);
                        // EP-001 US-005 parity: use the fallible `Builder::spawn`,
                        // never the panicking `thread::spawn`. Under
                        // RLIMIT_NPROC / EAGAIN the latter panics and unwinds
                        // this accept thread, silently killing the IPC server
                        // (AI-hook status + MCP bridge go dark while the status
                        // flag still reads Online). On the `Err` path the moved
                        // `guard` and `stream` are dropped here -- the count is
                        // restored and the connection closed -- and the loop
                        // keeps accepting.
                        if let Err(e) = std::thread::Builder::new()
                            .name("paneflow-ipc-conn".into())
                            .spawn(move || {
                                let _guard = guard;
                                handle_connection(stream, tx, bus, subscriptions);
                            })
                        {
                            log::warn!(
                                "IPC: handler thread spawn failed ({e}); dropping this \
                                 connection. Check `ulimit -u` / container thread limits."
                            );
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No pending connection - brief sleep to avoid busy-spin
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        thread_status.disable();
                        log::error!("IPC accept error: {e}");
                        break;
                    }
                }

                // Every 5 seconds, verify our socket file hasn't been
                // clobbered (inode check).
                #[cfg(unix)]
                if last_health_check.elapsed() >= Duration::from_secs(5) {
                    last_health_check = std::time::Instant::now();
                    let current_ino = socket_inode(&socket_path).unwrap_or(0);
                    if current_ino != our_ino {
                        log::warn!(
                            "IPC socket clobbered (inode {} → {}), re-binding",
                            our_ino,
                            current_ino
                        );
                        drop(listener);
                        match bind_socket(&socket_path) {
                            Some(l) => {
                                l.set_nonblocking(ListenerNonblockingMode::Accept).ok();
                                listener = l;
                                our_ino = socket_inode(&socket_path).unwrap_or(0);
                            }
                            None => {
                                thread_status.disable();
                                return;
                            }
                        }
                    }
                }
            }

            // interprocess' auto name reclamation unlinks the socket file
            // on `Listener::drop`; this explicit remove is a belt-and-braces
            // no-op if the listener already unlinked it.
            #[cfg(unix)]
            let _ = remove_socket_file_if_socket(&socket_path, "shutdown cleanup");
        });
    if let Err(e) = spawn_result {
        status.disable();
        tracing::error!(
            "IPC disabled: paneflow-ipc thread spawn failed: {e}. \
             Check `ulimit -u` / container thread limits. \
             External clients (paneflow-ai-hook) will not connect."
        );
        // `tx` was moved into the closure regardless of spawn outcome,
        // so on error the closure (and its captured `tx`) is dropped
        // here. The receiver `rx` then sees `Err(Disconnected)` on
        // every subsequent `try_recv`. The consumer at
        // `app/ipc_handler.rs` uses a non-blocking bounded drain, so both
        // `Empty` and `Disconnected` resolve to "no IPC work this tick" --
        // the app runs normally, only external IPC clients can't reach it.
    }

    (rx, status, event_bus)
}

/// Bind a new listener at the given Unix socket path.
fn bind_socket(socket_path: &std::path::Path) -> Option<Listener> {
    // Remove any stale socket file from a crashed prior run. The
    // interprocess crate's name reclamation handles graceful shutdown;
    // this pre-clean covers `kill -9` / SIGKILL / crash paths.
    #[cfg(unix)]
    if !remove_socket_file_if_socket(socket_path, "stale IPC socket cleanup") {
        return None;
    }

    let name = match socket_path.to_fs_name::<GenericFilePath>() {
        Ok(n) => n,
        Err(e) => {
            log::error!(
                "Failed to build IPC socket name for {}: {e}",
                socket_path.display()
            );
            return None;
        }
    };

    let listener_result = ListenerOptions::new().name(name).create_sync();

    let listener = match listener_result {
        Ok(l) => l,
        Err(e) => {
            log::error!(
                "Failed to bind IPC socket at {}: {e}",
                socket_path.display()
            );
            return None;
        }
    };

    // chmod 0o600 - owner-only connect is the primary trust boundary.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // U-031: the 0600 mode is the PRIMARY trust boundary (peer-UID is
        // defence-in-depth). If chmod fails, the socket keeps its umask-derived
        // creation mode - possibly group/world-connectable - so fail closed:
        // remove the socket and refuse to serve rather than expose it.
        if let Err(e) =
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        {
            log::error!(
                "IPC server: failed to chmod socket {} to 0600 ({e}); refusing to serve",
                socket_path.display()
            );
            let _ = std::fs::remove_file(socket_path);
            return None;
        }
    }
    log::info!("IPC server listening on {}", socket_path.display());
    Some(listener)
}

#[cfg(unix)]
fn prepare_socket_parent(socket_spec: &crate::runtime_paths::IpcSocketPath) -> bool {
    let Some(parent) = socket_spec.path().parent() else {
        return true;
    };

    if socket_spec.owned_parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::error!(
                "IPC server: failed to create socket parent {} ({e}); refusing to serve",
                parent.display()
            );
            return false;
        }
        // Lock the socket's containing dir to the owner. Under
        // $XDG_RUNTIME_DIR this already holds, but the fallback chain
        // ($TMPDIR / ~/.cache/run) can land in a world-traversable
        // /tmp - 0700 stops other local users from reaching the socket
        // at all (defense-in-depth atop the socket's own 0600 +
        // SO_PEERCRED).
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)) {
            log::error!(
                "IPC server: failed to chmod owned socket parent {} to 0700 ({e}); refusing to serve",
                parent.display()
            );
            return false;
        }
        return true;
    }

    if parent.is_dir() && unowned_socket_parent_is_safe(parent) {
        true
    } else {
        log::error!(
            "IPC server: PANEFLOW_SOCKET_PATH parent {} is missing, not a directory, or group/world writable without sticky bit; refusing to serve",
            parent.display()
        );
        false
    }
}

#[cfg(unix)]
fn unowned_socket_parent_is_safe(parent: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(metadata) = std::fs::metadata(parent) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    let mode = metadata.permissions().mode();
    let writable_by_group_or_other = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    !writable_by_group_or_other || sticky
}

#[cfg(unix)]
fn remove_socket_file_if_socket(path: &std::path::Path, context: &str) -> bool {
    use std::os::unix::fs::FileTypeExt as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(e) => {
            log::error!(
                "IPC server: failed to inspect {} before {context} ({e}); refusing to serve",
                path.display()
            );
            return false;
        }
    };

    if !metadata.file_type().is_socket() {
        log::error!(
            "IPC server: refusing to remove non-socket path {} during {context}",
            path.display()
        );
        return false;
    }

    if let Err(e) = std::fs::remove_file(path) {
        log::error!(
            "IPC server: failed to remove stale socket {} during {context} ({e}); refusing to serve",
            path.display()
        );
        return false;
    }
    true
}

/// Get the inode number of a filesystem path (0 if the file doesn't exist).
/// Unix-only: used by the clobber-detection health check.
#[cfg(unix)]
fn socket_inode(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}

/// Same budget as the probe's recv timeout. Applied to `connect` as well:
/// `Stream::connect` waits unbounded when the listen queue is full, and the
/// recv timeout never starts until connect returns.
const SINGLETON_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Connect with a wall-clock deadline. `Stream::connect` is unbounded.
fn connect_stream_with_timeout(
    socket_path: &std::path::Path,
    timeout: Duration,
) -> std::io::Result<Stream> {
    let name = socket_path.to_fs_name::<GenericFilePath>()?;
    ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(timeout))
        .connect_sync()
}

/// Probe `socket_path` to determine whether another live Paneflow instance
/// is already serving on it.
///
/// Returns `Some(identity_string)` if a `system.identify` round-trip
/// succeeds and the response advertises `"PaneFlow"` - the caller must
/// refuse to start. Returns `None` for any other outcome (missing file,
/// stale socket from a SIGKILL'd prior run, non-Paneflow listener, parse
/// failure, timeout) - the caller can safely proceed to `bind_socket`'s
/// existing remove-then-rebind path.
///
/// Resilient to the rebind race window: the legacy `bind_socket` recreates
/// the socket on every 5 s clobber-detection tick, and during the few-ms
/// window between `drop(listener)` and `create_sync()` a `connect()` would
/// spuriously return `ECONNREFUSED`. We retry up to 3 times with a short
/// inter-attempt sleep to cross that window deterministically.
///
/// Once this guard is universally deployed, the rebind loop never starts
/// (the second instance exits before bind), so the multi-attempt is
/// belt-and-braces for the transition period and for SIGKILL recovery
/// races where the OS hasn't yet released the file.
fn detect_existing_instance(socket_path: &std::path::Path) -> Option<String> {
    // Fast bail-out: no socket file at all = definitely no instance.
    // Avoids the connect overhead in the common cold-start case.
    #[cfg(unix)]
    if !socket_path.exists() {
        return None;
    }

    for attempt in 0..3 {
        if attempt > 0 {
            // Cross the legacy rebind window. The bind_socket recreate
            // path is bounded by `remove_file` + `create_sync` + chmod -
            // typically well under 10 ms; 70 ms is a comfortable margin.
            std::thread::sleep(Duration::from_millis(70));
        }

        let Ok(mut stream) = connect_stream_with_timeout(socket_path, SINGLETON_PROBE_TIMEOUT)
        else {
            continue;
        };

        // US-022: bound the probe at the OS level (`set_recv_timeout`, same
        // mechanism as the bridge client) instead of a scratch thread that
        // leaked on every timeout. 300 ms is generous for a stateless
        // socket-thread handler; a live but unresponsive process within that
        // budget is functionally indistinguishable from "no peer" and we
        // proceed to bind. A hostile squatter on the path can neither stall us
        // (the deadline) nor feed us an unbounded line (the `take` cap).
        if stream
            .set_recv_timeout(Some(SINGLETON_PROBE_TIMEOUT))
            .is_err()
        {
            continue;
        }

        // Stateless ping handled directly on the peer's socket thread
        // (see `handle_connection`), so a live instance responds in
        // microseconds without any GPUI round-trip.
        if stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"system.identify\"}\n")
            .is_err()
        {
            continue;
        }
        let _ = stream.flush();

        let mut line = String::new();
        if BufReader::new(stream)
            .take(MAX_REQUEST_LEN)
            .read_line(&mut line)
            .is_err()
        {
            continue;
        }

        // The `system.identify` result includes `"name":"PaneFlow"` (see
        // `handle_connection`). Match on the literal so a non-Paneflow
        // listener squatting on the same path doesn't pin us to exit -
        // we'd rather clobber an unknown squatter than refuse to start.
        if line.contains("\"PaneFlow\"") {
            return Some(line.trim().to_string());
        }
    }

    None
}

/// Outcome of one capped request read (US-022).
#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    /// Clean end of stream.
    Eof,
    /// The line reached `MAX_REQUEST_LEN` without a newline - oversized.
    TooLong,
    /// A complete (or trailing) line was read into the buffer.
    Got,
}

/// Read one newline-delimited request into `line`, capped at
/// [`MAX_REQUEST_LEN`]. `Take` is rebuilt per call so the limit is per-line;
/// a line that hits the cap without a terminating newline is reported as
/// [`LineRead::TooLong`] rather than allocated unboundedly (the DoS the cap
/// exists to stop). Pure framing logic, unit-tested below.
fn read_capped_line(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<LineRead> {
    line.clear();
    // `by_ref()` reborrows so `Take` owns a `&mut reader`, not `reader` itself
    // (the cap is per-call, and the caller keeps the reader for the next line).
    let n = reader.by_ref().take(MAX_REQUEST_LEN).read_line(line)?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if n as u64 >= MAX_REQUEST_LEN && !line.ends_with('\n') {
        return Ok(LineRead::TooLong);
    }
    Ok(LineRead::Got)
}

fn read_request_line(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<LineRead> {
    read_capped_line(reader, line)
}

struct ActiveCountGuard {
    counter: Arc<AtomicUsize>,
}

impl ActiveCountGuard {
    fn try_acquire(counter: Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        loop {
            let current = counter.load(Ordering::Acquire);
            if current >= limit {
                return None;
            }
            if counter
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self { counter });
            }
        }
    }
}

impl Drop for ActiveCountGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn write_overloaded_error(writer: &mut Stream, message: &str) {
    let envelope = json!({
        "jsonrpc": "2.0",
        "error": {"code": -32000, "message": message},
        "id": Value::Null,
    });
    let _ = write_envelope(writer, &envelope);
}

/// US-022 backpressure: refuse a connection once the concurrency cap is hit.
/// Writes one JSON-RPC error envelope and drops the stream (closing it) so the
/// peer gets a structured rejection rather than a silent hang.
fn reject_overloaded(mut stream: Stream) {
    // Abort-safe write (CP-4): one structured rejection then drop the stream
    // so a busy server does not hang the peer. `write_envelope` keeps a
    // closed-socket write a returned error. `stream` is dropped right after
    // either way.
    write_overloaded_error(&mut stream, "server busy: too many concurrent connections");
}

/// EP-003 US-010 (agent-control-plane): the connected peer's PID, for tracing
/// writes granted by AI free-access mode. On macOS `LOCAL_PEERCRED` carries
/// no pid, so this returns `None` here. Best-effort and advisory only -
/// never an authorization input (the peer-UID check in `auth::check_peer`
/// is the security boundary).
#[cfg(unix)]
fn peer_pid(stream: &Stream) -> Option<i64> {
    stream
        .peer_creds()
        .ok()
        .and_then(|c| c.pid())
        .map(|p| p as i64)
}

fn handle_connection(
    stream: Stream,
    request_tx: mpsc::SyncSender<IpcRequest>,
    event_bus: Arc<crate::ipc_events::EventBus>,
    active_subscriptions: Arc<AtomicUsize>,
) {
    // `Stream::try_clone` is provided by `interprocess::TryClone`. One
    // handle reads, the other writes, so request/response flow does not
    // fight over a single mutable cursor.
    let Ok(writer_stream) = stream.try_clone() else {
        return;
    };

    // US-010: peer-UID enforcement happens BEFORE we wrap `stream` in
    // a BufReader, because the cleanest way to query peer credentials
    // on `interprocess::local_socket::Stream` is the trait method
    // `Stream::peer_creds()` (brought in by `prelude::*`), and that
    // method needs the bare stream - once wrapped in BufReader, the
    // method is no longer reachable through `get_ref()` (BufReader
    // only re-exports `Read`-shaped methods). The check is
    // `#[cfg(unix)]`: compare the peer UID to the server UID and
    // reject mismatches.
    // On a peer-cred query failure we fall back to perms-0600 only
    // with a warn log (AC6) - the kernel filesystem check still
    // gates non-owner connects, so the residual exposure is bounded.
    let mut writer = writer_stream;

    #[cfg(unix)]
    {
        match auth::check_peer(&stream) {
            auth::AuthOutcome::Allow => {}
            auth::AuthOutcome::Deny {
                server_uid,
                peer_uid,
            } => {
                let envelope = json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": -32001,
                        "message": "permission denied: peer UID mismatch"
                    },
                    "id": Value::Null,
                });
                let _ = writeln!(&mut writer, "{}", envelope);
                let _ = writer.flush();
                log::warn!(
                    "IPC: rejecting connection (peer UID {}, server UID {})",
                    peer_uid,
                    server_uid
                );
                return;
            }
            auth::AuthOutcome::DegradedFallback => {
                // AC6: peer-cred query unavailable, perms-0600 stays
                // as the line of defence. Warn-log emitted inside
                // check_peer so the fallback isn't silent.
            }
        }
    }

    // EP-003 US-010: capture the peer PID once, while `stream` is still the
    // bare socket (peer_creds is unreachable through the BufReader wrapper
    // below). Threaded into each IpcRequest for the free-access write trace.
    let caller_pid = peer_pid(&stream);

    // US-022 / EP-004: drop a peer that opens a connection and then goes mute,
    // so it can't pin this handler thread forever. Unix sockets use the OS
    // receive timeout here. Issue #222: a refused deadline is a connection
    // setup failure (same policy as `push_bytes`), not something to proceed
    // past with no idle timeout - the read loop treats every error as a
    // disconnect, so an untimed `read_request_line` would block on a mute
    // peer until the process exits, holding one of the capped slots.
    if let Err(e) = stream.set_recv_timeout(Some(IPC_IDLE_TIMEOUT))
        && !socket_timeout_error_is_tolerable(&e)
    {
        log::warn!("ipc: could not set receive timeout on connection, closing: {e}");
        return;
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        match read_request_line(&mut reader, &mut line) {
            Ok(LineRead::Eof) => break,
            Ok(LineRead::TooLong) => {
                // US-022: oversized request → structured rejection + close,
                // never an unbounded allocation.
                let envelope = json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32600, "message": "request exceeds maximum length"},
                    "id": Value::Null,
                });
                // Abort-safe write (CP-4): see `write_envelope`.
                let _ = write_envelope(&mut writer, &envelope);
                break;
            }
            Ok(LineRead::Got) => {}
            // Idle timeout (WouldBlock) or any other read error → drop
            // the connection.
            Err(_) => break,
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut suppress_reply = false;
        let response = match serde_json::from_str::<Value>(line) {
            Ok(req) => {
                let id = req.get("id").cloned();
                let response_id = id.clone().unwrap_or(Value::Null);
                suppress_reply = id.is_none();
                match req.get("method").and_then(|m| m.as_str()) {
                    Some(method) => {
                        let method = method.to_string();
                        let params = req.get("params").cloned().unwrap_or(json!({}));

                        if method == "events.subscribe" {
                            let Some(_subscription_guard) = ActiveCountGuard::try_acquire(
                                Arc::clone(&active_subscriptions),
                                MAX_SUBSCRIPTION_CONNECTIONS,
                            ) else {
                                write_overloaded_error(
                                    &mut writer,
                                    "server busy: too many event subscriptions",
                                );
                                return;
                            };
                            serve_subscription(&mut writer, &params, &event_bus);
                            return;
                        }

                        if method.starts_with("ai.") {
                            crate::ai_hooks::hook_diag(&format!(
                                "ipc server received {method} (tool={:?} pid={:?} ws={:?})",
                                params.get("tool"),
                                params.get("pid"),
                                params.get("workspace_id"),
                            ));
                        }

                        match method.as_str() {
                            "system.ping" => {
                                json!({"jsonrpc": "2.0", "result": {"pong": true}, "id": response_id})
                            }
                            "system.capabilities" => {
                                let mut methods = vec![
                                    "system.ping",
                                    "system.capabilities",
                                    "system.identify",
                                    "workspace.list",
                                    "workspace.create",
                                    "workspace.select",
                                    "workspace.close",
                                    "workspace.current",
                                    "workspace.restore_layout",
                                    "workspace.up",
                                    "surface.list",
                                    "surface.read",
                                    "surface.search",
                                    "surface.rename",
                                    "surface.send_text",
                                    "surface.send_keystroke",
                                    "surface.split",
                                    "surface.focus",
                                    "surface.status",
                                    "fleet.list",
                                    "events.subscribe",
                                ];
                                methods.extend_from_slice(paneflow_ipc_client::ai_hook::METHODS);
                                json!({"jsonrpc": "2.0", "result": {
                                    "scripting": scripting_capability_from(
                                        std::env::var("PANEFLOW_IPC_SCRIPTING").ok().as_deref(),
                                        ai_unrestricted(),
                                    ),
                                    "orchestration": std::env::var("PANEFLOW_IPC_ORCHESTRATION")
                                        .is_ok_and(|v| v == "1")
                                        || std::env::var("PANEFLOW_IPC_SCRIPTING")
                                            .is_ok_and(|v| v == "1"),
                                    "methods": methods
                                }, "id": response_id})
                            }
                            "system.identify" => {
                                json!({"jsonrpc": "2.0", "result": {
                                    "name": "PaneFlow",
                                    "version": env!("CARGO_PKG_VERSION"),
                                    "protocol": "jsonrpc-2.0"
                                }, "id": response_id})
                            }
                            _ => dispatch_to_gpui(
                                &request_tx,
                                method,
                                params,
                                response_id,
                                caller_pid,
                            ),
                        }
                    }
                    None => {
                        suppress_reply = false;
                        json!({"jsonrpc": "2.0", "error": {"code": -32600, "message": "Invalid Request"}, "id": response_id})
                    }
                }
            }
            Err(e) => {
                json!({"jsonrpc": "2.0", "error": {"code": -32700, "message": format!("Parse error: {e}")}, "id": null})
            }
        };

        // JSON-RPC notifications do not receive replies. Requests with an `id`,
        // including `ai.*`, reply normally.
        if !suppress_reply && !write_envelope(&mut writer, &response) {
            break;
        }
    }
}

/// EP-002 / EP-006 (agent-control-plane): serve a persistent `events.subscribe`
/// stream. Registers a subscriber, writes a `subscribed` ack, then writes each
/// pushed event line until the client disconnects. A 30 s idle tick emits a
/// heartbeat (US-007) so a dead client is detected even when no events flow, and
/// any backlog shed under backpressure (US-004) is reported as a `dropped`
/// marker. Returns when a push fails (client gone) or the bus shuts down; the
/// `Subscription` drops here, unsubscribing (RAII).
///
/// Every push goes through [`push_frame`] / [`push_line`]. A write to a
/// closed Unix socket returns `BrokenPipe`, so the `watch` client's
/// disconnect is a clean RAII eviction of the `Subscription`.
fn serve_subscription(writer: &mut Stream, params: &Value, bus: &Arc<crate::ipc_events::EventBus>) {
    use std::sync::mpsc::RecvTimeoutError;

    const HEARTBEAT: Duration = Duration::from_secs(30);

    let filter = match crate::ipc_events::EventFilter::from_params(params) {
        Ok(f) => f,
        Err(msg) => {
            let err = json!({
                "jsonrpc": "2.0",
                "error": {"code": -32602, "message": msg},
                "id": Value::Null,
            });
            // Guarded like every other push: the subscribe request's
            // socket may already be closed by the time we reply.
            push_frame(writer, &err);
            return;
        }
    };
    let sub = bus.subscribe(filter);
    let ack = json!({"type": "subscribed", "id": sub.id});
    if !push_frame(writer, &ack) {
        return;
    }

    loop {
        // Report any events shed under backpressure since the last write.
        let dropped = sub.take_dropped();
        if dropped > 0 {
            let marker = json!({"type": "dropped", "count": dropped});
            if !push_frame(writer, &marker) {
                break;
            }
        }
        match sub.rx.recv_timeout(HEARTBEAT) {
            Ok(line) => {
                // `line` already carries its trailing newline.
                if !push_line(writer, line.as_bytes()) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let hb = json!({"type": "heartbeat"});
                if !push_frame(writer, &hb) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Write one JSON value as a newline-terminated frame to a subscription stream,
/// guarded by [`subscriber_connected`]. Returns `false` when the peer is gone or
/// the write fails - the caller breaks and the `Subscription` drops (RAII).
fn push_frame(writer: &mut Stream, value: &Value) -> bool {
    if !subscriber_connected(writer) {
        return false;
    }
    write_envelope(writer, value)
}

/// Like [`push_frame`] but for bytes that ALREADY carry their trailing newline
/// (a pushed event line, written verbatim). Same liveness guard.
fn push_line(writer: &mut Stream, line: &[u8]) -> bool {
    if !subscriber_connected(writer) {
        return false;
    }
    push_bytes(writer, line)
}

/// Serialize a JSON-RPC value as a newline-terminated frame and send it
/// abort-safely. `true` on success. Shared by the subscription push path and the
/// request/response + rejection writes, so every server-side write to the
/// socket goes through the same path.
fn write_envelope(writer: &mut Stream, value: &Value) -> bool {
    let mut frame = value.to_string();
    frame.push('\n');
    push_bytes(writer, frame.as_bytes())
}

/// Issue #222: the one `set_recv_timeout` / `set_send_timeout` failure a
/// connection survives. `Unsupported` means the transport has no timeout knob
/// at all, so proceeding without one is the only option; any other failure
/// means the OS refused a deadline this socket should honour, and carrying on
/// would let a mute or stalled peer pin the handler thread for good. Shared by
/// the receive path (`handle_connection`) and the send path (`push_bytes`) so
/// the two cannot drift.
fn socket_timeout_error_is_tolerable(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::Unsupported
}

/// Send raw bytes to the peer, `true` on success.
///
/// The `subscriber_connected` probe narrows but cannot close the disconnect
/// window: the peer can still vanish between the probe and this write, and the
/// rejection / reply writes have no probe at all. A write to a closed Unix
/// socket returns `BrokenPipe` cleanly.
fn push_bytes(writer: &mut Stream, buf: &[u8]) -> bool {
    if let Err(e) = writer.set_send_timeout(Some(IPC_WRITE_TIMEOUT))
        && !socket_timeout_error_is_tolerable(&e)
    {
        return false;
    }
    writer.write_all(buf).is_ok() && writer.flush().is_ok()
}

/// EP-006 US-013: Unix path - a no-op `true`. A write to a closed Unix socket
/// returns `Err(BrokenPipe)` cleanly (Rust ignores SIGPIPE), which the caller
/// already handles, so no pre-probe is needed.
fn subscriber_connected(_writer: &Stream) -> bool {
    true
}

fn dispatch_to_gpui(
    request_tx: &mpsc::SyncSender<IpcRequest>,
    method: String,
    params: Value,
    id: Value,
    caller_pid: Option<i64>,
) -> Value {
    let (resp_tx, resp_rx) = mpsc::channel();
    let dispatch = Arc::new(AtomicU8::new(IPC_DISPATCH_QUEUED));
    let ipc_req = IpcRequest {
        method: method.clone(),
        params,
        _id: id.clone(),
        response_tx: resp_tx,
        dispatch: Arc::clone(&dispatch),
        caller_pid,
    };

    match request_tx.try_send(ipc_req) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            return json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "PaneFlow is busy; retry shortly"}, "id": id});
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            return json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "App shutting down"}, "id": id});
        }
    }

    await_or_cancel(&resp_rx, &dispatch, Duration::from_secs(5), id)
}

/// Wait for the GPUI handler's response. If the request is still queued after
/// `timeout`, CAS Queued→Cancelled so the GPUI consumer skips it. Once the
/// handler has started, wait for the real response instead of telling the
/// client to retry a mutation that may still complete.
fn await_or_cancel(
    resp_rx: &mpsc::Receiver<Value>,
    dispatch: &AtomicU8,
    timeout: Duration,
    id: Value,
) -> Value {
    let queued_at = Instant::now();
    loop {
        let wait_for = if dispatch_is_started(dispatch) {
            Duration::from_millis(50)
        } else {
            match timeout.checked_sub(queued_at.elapsed()) {
                Some(remaining) => remaining.min(Duration::from_millis(50)),
                None => {
                    if try_cancel_dispatch(dispatch) {
                        return json!({"jsonrpc": "2.0", "error": {"code": -32002, "message": "Request dispatch timeout"}, "id": id});
                    }
                    // GPUI already CAS'd Queued→Started; wait for the result.
                    Duration::from_millis(50)
                }
            }
        };

        match resp_rx.recv_timeout(wait_for) {
            Ok(result) => return crate::app::ipc_handler::promote_response(result, id),
            Err(mpsc::RecvTimeoutError::Timeout) if dispatch_is_started(dispatch) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if queued_at.elapsed() >= timeout && try_cancel_dispatch(dispatch) {
                    return json!({"jsonrpc": "2.0", "error": {"code": -32002, "message": "Request dispatch timeout"}, "id": id});
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "App shutting down"}, "id": id});
            }
        }
    }
}

fn socket_path_spec() -> Option<crate::runtime_paths::IpcSocketPath> {
    crate::runtime_paths::socket_path_spec()
}

// ---------------------------------------------------------------------------
// US-010: peer-UID enforcement
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod auth {
    //! Peer-UID enforcement on the IPC server.
    //!
    //! Splits cleanly so each layer is testable in isolation:
    //!
    //! - [`authorize`]: pure policy decision - given a server UID and a
    //!   peer UID, allow or deny. No I/O, exhaustively unit-tested
    //!   (matching pair → allow, mismatched pair → deny).
    //! - [`server_uid`]: thin wrapper over `getuid(2)`.
    //! - [`check_peer`]: glue that runs `Stream::peer_creds()` (provided
    //!   by interprocess 2.4 - `LOCAL_PEERCRED` on macOS) and feeds
    //!   the result into `authorize`.
    //!
    //! [`check_peer`] returns an [`AuthOutcome`] the caller turns into
    //! the JSON-RPC envelope (or just keeps serving on
    //! `DegradedFallback`). The split keeps the policy fully covered
    //! by deterministic tests; the live-syscall integration is
    //! exercised by paneflow itself on every connection.

    use super::Stream;
    use interprocess::local_socket::prelude::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum AuthOutcome {
        /// Peer UID matches server UID - proceed to dispatch.
        Allow,
        /// Peer UID query succeeded and the value did NOT match the
        /// server's UID. Caller emits the JSON-RPC EPERM envelope.
        Deny { server_uid: u32, peer_uid: u32 },
        /// Peer UID could not be queried (very old kernel / exotic
        /// Unix without an `euid` field in `peer_creds()`). AC6:
        /// fall back to the perms-0600 file-mode line of defence and
        /// continue serving. The warn log fires inside [`check_peer`]
        /// so the fallback isn't silent.
        DegradedFallback,
    }

    /// Pure-function policy. Equality of effective UIDs is the
    /// allowlist.
    pub(super) fn authorize(server_uid: u32, peer_uid: u32) -> AuthOutcome {
        if server_uid == peer_uid {
            AuthOutcome::Allow
        } else {
            AuthOutcome::Deny {
                server_uid,
                peer_uid,
            }
        }
    }

    /// Resolve the running process's effective UID via `geteuid(2)`.
    ///
    /// `peer_creds().euid()` returns the peer's *effective* UID; we
    /// must compare against ours symmetrically. Calling `getuid()`
    /// (real UID) here would diverge from `geteuid()` under any
    /// privilege-separation wrapper (`sudo`, setuid, polkit-helped
    /// child) and either falsely accept or falsely reject a peer that
    /// shares one but not the other.
    pub(super) fn server_uid() -> u32 {
        // libc::uid_t is u32 on every supported target; the cast is a
        // no-op there but stays explicit for cross-target clarity.
        unsafe { libc::geteuid() as u32 }
    }

    /// Run the peer-credential query against the connected stream and
    /// translate the outcome. Defers the kernel-call mechanics to
    /// `interprocess::local_socket::Stream::peer_creds()` (`LOCAL_PEERCRED`
    /// on macOS); upstream owns the kernel call so paneflow doesn't
    /// duplicate `getsockopt` boilerplate per target.
    pub(super) fn check_peer(stream: &Stream) -> AuthOutcome {
        let server = server_uid();
        match stream.peer_creds() {
            Ok(creds) => match creds.euid() {
                Some(peer) => authorize(server, peer),
                None => {
                    // `peer_creds()` succeeded but the platform doesn't
                    // expose an effective UID (NetBSD ucred lacks
                    // euid, for example). Same fallback as the Err
                    // branch - perms-0600 stays as the line of
                    // defence.
                    log::warn!(
                        "IPC: peer-cred query returned no euid on this OS; \
                         falling back to perms-0600 only"
                    );
                    AuthOutcome::DegradedFallback
                }
            },
            Err(e) => {
                log::warn!("IPC: peer-cred query failed ({e}); falling back to perms-0600 only");
                AuthOutcome::DegradedFallback
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn authorize_accepts_matching_uid() {
            assert_eq!(authorize(1000, 1000), AuthOutcome::Allow);
            assert_eq!(authorize(0, 0), AuthOutcome::Allow);
        }

        #[test]
        fn authorize_rejects_mismatched_uid() {
            assert_eq!(
                authorize(1000, 1001),
                AuthOutcome::Deny {
                    server_uid: 1000,
                    peer_uid: 1001,
                }
            );
            assert_eq!(
                authorize(1000, 0),
                AuthOutcome::Deny {
                    server_uid: 1000,
                    peer_uid: 0,
                }
            );
        }

        /// `geteuid(2)` must return the same value on two successive
        /// calls - the kernel doesn't change a process's effective UID
        /// without an explicit `setuid(2)` / `seteuid(2)` call. Stable
        /// across calls is the property the auth path actually relies
        /// on (we capture the server euid once and compare every
        /// incoming peer euid against it).
        #[test]
        fn server_uid_is_stable() {
            let a = server_uid();
            let b = server_uid();
            assert_eq!(a, b, "geteuid must be stable across calls");
        }

        /// Symmetric to `authorize_accepts_matching_uid` - root running
        /// the server is an explicit policy choice, not an accidental
        /// bypass: any non-root peer is denied even when the server is
        /// uid 0. The matching-UID accept at `(0, 0)` is the only
        /// root-to-root path; that case is intentional (a privileged
        /// IPC client speaking to a privileged paneflow run by the
        /// same operator).
        #[test]
        fn authorize_root_server_rejects_non_root_peer() {
            assert!(matches!(
                authorize(0, 1000),
                AuthOutcome::Deny {
                    server_uid: 0,
                    peer_uid: 1000
                }
            ));
        }
    }
}

#[cfg(test)]
mod timeout_policy_tests {
    use super::socket_timeout_error_is_tolerable;
    use std::io::{Error, ErrorKind};

    /// Issue #222: the send path (`push_bytes`) and the receive path
    /// (`handle_connection`) must agree on which `set_*_timeout` failure is
    /// survivable. Only `Unsupported` (the transport has no timeout knob) is;
    /// every other kind means the peer would otherwise run unbounded.
    #[test]
    fn socket_timeout_setup_tolerates_only_unsupported() {
        assert!(socket_timeout_error_is_tolerable(&Error::from(
            ErrorKind::Unsupported
        )));
        for kind in [
            ErrorKind::InvalidInput,
            ErrorKind::PermissionDenied,
            ErrorKind::NotConnected,
            ErrorKind::BrokenPipe,
            ErrorKind::Other,
        ] {
            assert!(
                !socket_timeout_error_is_tolerable(&Error::from(kind)),
                "{kind:?} must close the connection"
            );
        }
    }

    /// The receive path must consult the same policy as the send path:
    /// discarding the `set_recv_timeout` result proceeds with no idle
    /// timeout, and the read loop then blocks forever on a mute peer.
    #[test]
    fn receive_path_does_not_discard_the_recv_timeout_result() {
        let src = include_str!("ipc.rs");
        let discarded = ["let _ = ", "stream.set_recv_timeout("].concat();
        assert!(
            !src.contains(&discarded),
            "set_recv_timeout result is discarded in handle_connection"
        );
    }
}

#[cfg(test)]
mod connection_limit_tests {
    use super::{ActiveCountGuard, MAX_SUBSCRIPTION_CONNECTIONS};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn subscription_slots_are_capped_and_released() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut guards = Vec::new();
        for _ in 0..MAX_SUBSCRIPTION_CONNECTIONS {
            guards.push(
                ActiveCountGuard::try_acquire(Arc::clone(&counter), MAX_SUBSCRIPTION_CONNECTIONS)
                    .expect("slot"),
            );
        }
        assert!(
            ActiveCountGuard::try_acquire(Arc::clone(&counter), MAX_SUBSCRIPTION_CONNECTIONS)
                .is_none()
        );
        drop(guards.pop());
        assert_eq!(
            counter.load(Ordering::Acquire),
            MAX_SUBSCRIPTION_CONNECTIONS - 1
        );
        assert!(
            ActiveCountGuard::try_acquire(Arc::clone(&counter), MAX_SUBSCRIPTION_CONNECTIONS)
                .is_some()
        );
    }
}

#[cfg(test)]
mod framing_tests {
    use super::{LineRead, MAX_REQUEST_LEN, read_capped_line};
    use std::io::Cursor;
    use std::time::Duration;

    #[test]
    fn capped_line_rejects_oversized_unterminated() {
        // US-022 negative test: a line that reaches the cap without a newline
        // is reported TooLong, never accumulated past the bound.
        let huge = vec![b'x'; MAX_REQUEST_LEN as usize + 64];
        let mut cur = Cursor::new(huge);
        let mut line = String::new();
        assert_eq!(
            read_capped_line(&mut cur, &mut line).unwrap(),
            LineRead::TooLong
        );
        assert!(line.len() as u64 <= MAX_REQUEST_LEN, "buffer stays bounded");
    }

    #[test]
    fn capped_line_accepts_normal_then_eof() {
        let mut cur = Cursor::new(b"{\"jsonrpc\":\"2.0\"}\n".to_vec());
        let mut line = String::new();
        assert_eq!(
            read_capped_line(&mut cur, &mut line).unwrap(),
            LineRead::Got
        );
        assert_eq!(line, "{\"jsonrpc\":\"2.0\"}\n");
        assert_eq!(
            read_capped_line(&mut cur, &mut line).unwrap(),
            LineRead::Eof
        );
    }

    #[test]
    fn capped_line_accepts_exactly_at_cap_with_newline() {
        // Boundary: a line of exactly MAX_REQUEST_LEN bytes whose final byte
        // is the newline is accepted (not a truncation).
        let mut body = vec![b'a'; MAX_REQUEST_LEN as usize - 1];
        body.push(b'\n');
        let mut cur = Cursor::new(body);
        let mut line = String::new();
        assert_eq!(
            read_capped_line(&mut cur, &mut line).unwrap(),
            LineRead::Got
        );
    }

    #[cfg(unix)]
    #[test]
    fn bind_socket_refuses_to_remove_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("paneflow.sock");
        std::fs::write(&path, b"do not delete").expect("write guard file");

        assert!(
            super::bind_socket(&path).is_none(),
            "regular files at the socket path must not be reclaimed"
        );
        assert_eq!(
            std::fs::read(&path).expect("regular file still exists"),
            b"do not delete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unowned_socket_parent_rejects_world_writable_without_sticky_bit() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod tempdir");
        assert!(!super::unowned_socket_parent_is_safe(dir.path()));

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o1777))
            .expect("chmod sticky tempdir");
        assert!(super::unowned_socket_parent_is_safe(dir.path()));
    }

    /// A listener that never `accept()`s will fill its backlog; the singleton
    /// probe's connect must not wait unbounded. Darwin AF_UNIX typically
    /// refuses once the queue is full; other kernels may surface `TimedOut`.
    #[cfg(unix)]
    #[test]
    fn connect_with_timeout_returns_before_deadline_when_listener_never_accepts() {
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-accept.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let mut held = Vec::new();
        let mut filled = false;
        loop {
            match UnixStream::connect(&path) {
                Ok(stream) => held.push(stream),
                Err(_) => {
                    filled = true;
                    break;
                }
            }
            if held.len() >= 1024 {
                break;
            }
        }
        assert!(
            filled && !held.is_empty(),
            "listen queue never filled (held {})",
            held.len()
        );

        let timeout = Duration::from_millis(150);
        let path_for_thread = path.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let start = Instant::now();
            let result = super::connect_stream_with_timeout(&path_for_thread, timeout);
            let _ = tx.send((result.map(|_| ()).map_err(|e| e.kind()), start.elapsed()));
        });
        let (result, elapsed) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("connect must return within 2s even if the listener never accept()s");
        assert!(
            result.is_err(),
            "full listen queue must not produce a live stream; got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "connect must not wait unbounded; elapsed {elapsed:?}"
        );
        drop(listener);
        drop(held);
    }
}

#[cfg(test)]
mod dispatch_state_tests {
    use super::{
        IPC_DISPATCH_CANCELLED, IPC_DISPATCH_QUEUED, IPC_DISPATCH_STARTED, dispatch_is_cancelled,
        dispatch_is_started, try_cancel_dispatch, try_start_dispatch,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::thread;

    #[test]
    fn queued_to_started_succeeds_and_blocks_cancel() {
        let state = AtomicU8::new(IPC_DISPATCH_QUEUED);
        assert!(try_start_dispatch(&state));
        assert_eq!(state.load(Ordering::Acquire), IPC_DISPATCH_STARTED);
        assert!(dispatch_is_started(&state));
        assert!(!dispatch_is_cancelled(&state));
        assert!(
            !try_cancel_dispatch(&state),
            "started handlers must not be cancelled behind the client"
        );
        assert_eq!(state.load(Ordering::Acquire), IPC_DISPATCH_STARTED);
    }

    #[test]
    fn queued_to_cancelled_succeeds_and_blocks_start() {
        let state = AtomicU8::new(IPC_DISPATCH_QUEUED);
        assert!(try_cancel_dispatch(&state));
        assert_eq!(state.load(Ordering::Acquire), IPC_DISPATCH_CANCELLED);
        assert!(dispatch_is_cancelled(&state));
        assert!(!dispatch_is_started(&state));
        assert!(
            !try_start_dispatch(&state),
            "GPUI must not run handle_ipc after the client got -32002"
        );
        assert_eq!(state.load(Ordering::Acquire), IPC_DISPATCH_CANCELLED);
    }

    #[test]
    fn start_and_cancel_are_noops_from_their_own_terminal_states() {
        let started = AtomicU8::new(IPC_DISPATCH_STARTED);
        assert!(!try_start_dispatch(&started));
        assert_eq!(started.load(Ordering::Acquire), IPC_DISPATCH_STARTED);

        let cancelled = AtomicU8::new(IPC_DISPATCH_CANCELLED);
        assert!(!try_cancel_dispatch(&cancelled));
        assert_eq!(cancelled.load(Ordering::Acquire), IPC_DISPATCH_CANCELLED);
    }

    #[test]
    fn concurrent_start_and_cancel_exactly_one_wins() {
        for _ in 0..128 {
            let state = Arc::new(AtomicU8::new(IPC_DISPATCH_QUEUED));
            let for_start = Arc::clone(&state);
            let for_cancel = Arc::clone(&state);
            let start_thread = thread::spawn(move || try_start_dispatch(&for_start));
            let cancel_thread = thread::spawn(move || try_cancel_dispatch(&for_cancel));
            let started = start_thread.join().expect("start thread");
            let cancelled = cancel_thread.join().expect("cancel thread");
            assert_ne!(
                started, cancelled,
                "Queued→Started and Queued→Cancelled must be mutually exclusive"
            );
            assert_eq!(started, dispatch_is_started(&state));
            assert_eq!(cancelled, dispatch_is_cancelled(&state));
            let observed = state.load(Ordering::Acquire);
            assert!(
                observed == IPC_DISPATCH_STARTED || observed == IPC_DISPATCH_CANCELLED,
                "race must leave a terminal state, got {observed}"
            );
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::{
        IPC_DISPATCH_QUEUED, IPC_DISPATCH_STARTED, IpcRequest, await_or_cancel,
        dispatch_is_cancelled, dispatch_is_started, dispatch_to_gpui, try_start_dispatch,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicU8;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    fn test_ipc_request() -> IpcRequest {
        let (response_tx, _response_rx) = mpsc::channel();
        IpcRequest {
            method: "surface.read".to_string(),
            params: json!({}),
            _id: json!(1),
            response_tx,
            dispatch: Arc::new(AtomicU8::new(IPC_DISPATCH_QUEUED)),
            caller_pid: None,
        }
    }

    #[test]
    fn dispatch_to_gpui_returns_overload_when_request_queue_full() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(test_ipc_request()).unwrap();

        let resp = dispatch_to_gpui(
            &tx,
            "surface.read".to_string(),
            json!({ "surface_id": 1 }),
            json!("req-overload"),
            None,
        );

        assert_eq!(resp["error"]["code"], -32000);
        assert_eq!(resp["error"]["message"], "PaneFlow is busy; retry shortly");
        assert_eq!(resp["id"], "req-overload");
    }

    #[test]
    fn dispatch_to_gpui_returns_shutdown_when_receiver_dropped() {
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);

        let resp = dispatch_to_gpui(
            &tx,
            "surface.read".to_string(),
            json!({ "surface_id": 1 }),
            json!("req-closed"),
            None,
        );

        assert_eq!(resp["error"]["code"], -32000);
        assert_eq!(resp["error"]["message"], "App shutting down");
        assert_eq!(resp["id"], "req-closed");
    }

    #[test]
    fn await_or_cancel_sets_flag_and_errors_on_timeout() {
        // When the GPUI handler is not started within the deadline,
        // await_or_cancel must (a) return a -32002 timeout envelope to the
        // client AND (b) CAS Queued→Cancelled so the GPUI consumer skips the
        // not-yet-run handler - preventing a duplicate non-idempotent mutation
        // on the client's retry. _tx is kept alive so we exercise the Timeout
        // path (not Disconnected); a short deadline keeps the test fast.
        let (_tx, rx) = mpsc::channel::<serde_json::Value>();
        let dispatch = AtomicU8::new(IPC_DISPATCH_QUEUED);
        let resp = await_or_cancel(&rx, &dispatch, Duration::from_millis(20), json!(7));

        assert!(
            dispatch_is_cancelled(&dispatch),
            "timeout must CAS Queued→Cancelled so the GPUI side skips the request"
        );
        assert!(
            !try_start_dispatch(&dispatch),
            "GPUI must not start a request after the client got -32002"
        );
        assert_eq!(resp["error"]["code"], -32002);
        assert_eq!(resp["id"], 7);
    }

    #[test]
    fn await_or_cancel_waits_for_started_handler_instead_of_cancelling() {
        let (tx, rx) = mpsc::channel::<serde_json::Value>();
        let dispatch = Arc::new(AtomicU8::new(IPC_DISPATCH_STARTED));
        let send_dispatch = Arc::clone(&dispatch);
        std::thread::spawn(move || {
            assert!(dispatch_is_started(&send_dispatch));
            std::thread::sleep(Duration::from_millis(40));
            tx.send(json!({"status": "ok"})).unwrap();
        });

        let resp = await_or_cancel(&rx, &dispatch, Duration::from_millis(5), json!(9));

        assert!(
            !dispatch_is_cancelled(&dispatch),
            "started handlers must not be cancelled behind the client"
        );
        assert!(dispatch_is_started(&dispatch));
        assert_eq!(resp["result"]["status"], "ok");
        assert_eq!(resp["id"], 9);
    }

    #[test]
    fn await_or_cancel_passes_through_response_without_cancelling() {
        // The happy path: a response arrives before the deadline → no cancel,
        // result promoted under `result` (no `_jsonrpc_error` sentinel here).
        let (tx, rx) = mpsc::channel::<serde_json::Value>();
        tx.send(json!({"status": "ok"})).unwrap();
        let dispatch = AtomicU8::new(IPC_DISPATCH_QUEUED);
        let resp = await_or_cancel(&rx, &dispatch, Duration::from_secs(5), json!(3));

        assert!(
            !dispatch_is_cancelled(&dispatch),
            "a timely response must not set Cancelled"
        );
        assert_eq!(resp["result"]["status"], "ok");
        assert_eq!(resp["id"], 3);
    }
}

#[cfg(test)]
mod allow_multiple_tests {
    /// Issue #53: `PANEFLOW_ALLOW_MULTIPLE` is value-gated (`=1`), not
    /// presence-gated. Mirror the `PANEFLOW_IPC_SCRIPTING` truth table.
    #[test]
    fn allow_multiple_only_literal_one() {
        assert!(
            !super::allow_multiple_from(None),
            "unset env must keep the singleton"
        );
        assert!(
            !super::allow_multiple_from(Some("")),
            "empty string must keep the singleton"
        );
        assert!(
            !super::allow_multiple_from(Some("0")),
            "explicit 0 must keep the singleton"
        );
        assert!(
            !super::allow_multiple_from(Some("false")),
            "false must keep the singleton"
        );
        assert!(
            !super::allow_multiple_from(Some("true")),
            "truthy strings other than \"1\" must keep the singleton"
        );
        assert!(
            super::allow_multiple_from(Some("1")),
            "the documented opt-in value must skip the singleton"
        );
    }
}

#[cfg(test)]
mod capabilities_tests {
    /// Issue #283: `system.capabilities.scripting` must report the effective
    /// write gate, not just the env var. With `ai_unrestricted` on and the env
    /// unset the server accepts `surface.send_text`, so a client probing the
    /// capability (`paneflow flow run` with `submit = true`) must not be
    /// refused on `scripting: false`.
    #[test]
    fn scripting_capability_reports_the_effective_write_gate() {
        assert!(
            !super::scripting_capability_from(None, false),
            "both off must read as disabled (unchanged legacy behavior)"
        );
        assert!(
            !super::scripting_capability_from(Some("0"), false),
            "explicit 0 without free-access must read as disabled"
        );
        assert!(
            super::scripting_capability_from(Some("1"), false),
            "the env gate alone still advertises scripting"
        );
        assert!(
            super::scripting_capability_from(None, true),
            "ai_unrestricted must advertise scripting without the env gate"
        );
        assert!(super::scripting_capability_from(Some("1"), true));

        // The mirror the socket thread reads round-trips what the GPUI
        // thread publishes. Kept in this one test so no parallel test
        // observes a half-written global.
        super::set_ai_unrestricted(true);
        assert!(super::ai_unrestricted());
        super::set_ai_unrestricted(false);
        assert!(!super::ai_unrestricted());
    }
}
