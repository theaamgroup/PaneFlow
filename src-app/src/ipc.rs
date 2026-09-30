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
//! - `ai.session_start` / `ai.prompt_submit` / `ai.tool_use` /
//!   `ai.notification` / `ai.stop` / `ai.exit` / `ai.session_end` /
//!   `ai.subagent_start` / `ai.subagent_stop` - AI hook lifecycle
//!   (`ai.exit` carries the wrapped agent binary's real exit status,
//!   EP-004 US-010).
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
    pub response_tx: mpsc::Sender<Value>,
    /// Single CAS lifecycle (issue #38): `IPC_DISPATCH_QUEUED` → `STARTED`
    /// (GPUI, just before `handle_ipc`) or `CANCELLED` (socket 5 s timeout).
    /// Exactly one transition wins, so a timed-out mutation cannot still run after `-32002`.
    pub dispatch: Arc<AtomicU8>,
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

    /// A status that never starts the socket thread. Persist tests need the
    /// field without binding the user's IPC socket.
    #[cfg(test)]
    pub(crate) fn disabled_for_test() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(IPC_STATE_DISABLED)),
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
pub fn start_server() -> (mpsc::Receiver<IpcRequest>, IpcStatus) {
    // US-012 (cli-hardening-followup-2026-Q3): one-time boot-time
    // warn-log when scripting is enabled. The per-call gate in
    // `surface.send_text` / `surface.send_keystroke` stays the
    // enforcement boundary; this log surfaces the active-RCE-primitive
    // posture in `paneflow-debug.log` so the operator notices when
    // PANEFLOW_IPC_SCRIPTING was inherited from a launcher script or
    // sourced .env file without their realising.
    let scripting_enabled = std::env::var("PANEFLOW_IPC_SCRIPTING").as_deref() == Ok("1");
    if scripting_enabled {
        tracing::warn!(
            "ipc.scripting_enabled is ON; any same-UID process can inject keystrokes into agent panes"
        );
    }
    let (tx, rx) = mpsc::sync_channel(IPC_REQUEST_QUEUE_CAPACITY);
    let status = IpcStatus::online();
    let thread_status = status.clone();

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
            // It is also required by `accept_uninheritable`: a blocking accept
            // under the spawn exclusion would stall every spawn in the app.
            #[cfg(unix)]
            if let Err(e) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
                thread_status.disable();
                log::error!("IPC: could not make the listener non-blocking ({e}); IPC disabled");
                return;
            }

            // US-022: bound the number of concurrently-served connections so a
            // peer opening sockets in a loop can't fan out unbounded threads.
            // Only this (single) accept thread increments; handler threads
            // decrement via the RAII guard below, so the load is exact.
            let active_connections = Arc::new(AtomicUsize::new(0));

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
                match accept_uninheritable(&listener) {
                    Ok(stream) => {
                        if active_connections.load(Ordering::Acquire) >= MAX_REQUEST_CONNECTIONS {
                            reject_overloaded(stream);
                            continue;
                        }
                        active_connections.fetch_add(1, Ordering::AcqRel);
                        let guard = ActiveGuard(Arc::clone(&active_connections));
                        let tx = tx.clone();
                        // EP-001 US-005 parity: use the fallible `Builder::spawn`,
                        // never the panicking `thread::spawn`. Under
                        // RLIMIT_NPROC / EAGAIN the latter panics and unwinds
                        // this accept thread, silently killing the IPC server
                        // (AI-hook status + CLI/scripts go dark while the status
                        // flag still reads Online). On the `Err` path the moved
                        // `guard` and `stream` are dropped here -- the count is
                        // restored and the connection closed -- and the loop
                        // keeps accepting.
                        if let Err(e) = std::thread::Builder::new()
                            .name("paneflow-ipc-conn".into())
                            .spawn(move || {
                                let _guard = guard;
                                handle_connection(stream, tx);
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
                                if let Err(e) = l.set_nonblocking(ListenerNonblockingMode::Accept) {
                                    thread_status.disable();
                                    log::error!(
                                        "IPC: could not make the re-bound listener non-blocking \
                                         ({e}); IPC disabled"
                                    );
                                    return;
                                }
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

    (rx, status)
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

    // Issue #1115: macOS has no `SOCK_CLOEXEC`, so `interprocess` marks the
    // new socket close-on-exec in a second call. Keep spawns out of that window.
    let listener_result =
        paneflow_process::with_spawns_excluded(|| ListenerOptions::new().name(name).create_sync());

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

/// Accept one pending connection, never while a spawn is in flight.
///
/// Issue #1115: macOS has no `accept4`, so the accepted descriptor is marked
/// close-on-exec by a second call. A child spawned between the two keeps the
/// connection open for its whole life, and the client sees no EOF when the
/// handler drops its end. An in-flight spawn reads as `WouldBlock`, which the
/// accept loop already retries on its next tick.
///
/// `listener` must be non-blocking (`ListenerNonblockingMode::Accept`): the
/// accept runs under the spawn exclusion, so a blocking one would stall every
/// spawn in the app until a client connects.
fn accept_uninheritable(listener: &Listener) -> std::io::Result<Stream> {
    paneflow_process::try_with_spawns_excluded(|| listener.accept())
        .unwrap_or_else(|| Err(std::io::ErrorKind::WouldBlock.into()))
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
        // mechanism as the IPC client crate) instead of a scratch thread that
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

fn handle_connection(stream: Stream, request_tx: mpsc::SyncSender<IpcRequest>) {
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

    // US-022 / EP-004: drop a peer that opens a connection and then goes mute,
    // so it can't pin this handler thread forever. Unix sockets use the OS
    // receive timeout here. Issue #222: a refused deadline is a connection
    // setup failure (same policy as `push_bytes`), not something to proceed
    // past with no idle timeout - the read loop treats every error as a
    // disconnect, so an untimed `read_request_line` would block on a mute
    // peer until the process exits, holding one of the capped slots.
    //
    // Issue #824: a fire-and-forget client (`paneflow-ai-hook`) can write its
    // one frame and close before this runs, and XNU refuses `SO_RCVTIMEO` with
    // `EINVAL` once the peer is fully disconnected. That frame is already in
    // the receive buffer, so drain it in non-blocking mode instead of closing:
    // reads return the buffered bytes, then EOF, and a `WouldBlock` read error
    // ends the loop, so no read can pin this thread even if `EINVAL` ever
    // arises for another reason.
    if let Err(e) = stream.set_recv_timeout(Some(IPC_IDLE_TIMEOUT))
        && !socket_timeout_error_is_tolerable(&e)
    {
        let drain = e.raw_os_error() == Some(libc::EINVAL) && stream.set_nonblocking(true).is_ok();
        if !drain {
            log::warn!("ipc: could not set receive timeout on connection, closing: {e}");
            return;
        }
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
                let id = req.get("id");
                // Echo only an id of a valid JSON-RPC type; anything else
                // cannot be determined, so the (error) reply carries `null`.
                let response_id = id
                    .filter(|id| is_valid_request_id(id))
                    .cloned()
                    .unwrap_or(Value::Null);
                suppress_reply = id.is_none();
                match request_method(&req) {
                    Some(method) => {
                        let method = method.to_string();
                        let params = req.get("params").cloned().unwrap_or(json!({}));

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
                                let methods = supported_methods();
                                json!({"jsonrpc": "2.0", "result": {
                                    "scripting": scripting_capability_from(
                                        std::env::var("PANEFLOW_IPC_SCRIPTING").ok().as_deref(),
                                        ai_unrestricted(),
                                    ),
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
                            _ => dispatch_to_gpui(&request_tx, method, params, response_id),
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

/// The method of a valid JSON-RPC 2.0 request envelope: an object carrying
/// `"jsonrpc": "2.0"`, a string `method`, an `id` (when present) that is a
/// string, number or null, and `params` (when present) that is an object or
/// array. `None` for anything else, which the caller answers with `-32600
/// Invalid Request` before any dispatch (issue #1098). An invalid envelope is
/// never a notification, so it is answered even without an `id`. The reply
/// echoes an `id` of a valid type and is `null` otherwise, the JSON-RPC 2.0
/// rule for an id that cannot be determined.
fn request_method(req: &Value) -> Option<&str> {
    if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return None;
    }
    if req.get("id").is_some_and(|id| !is_valid_request_id(id)) {
        return None;
    }
    if req
        .get("params")
        .is_some_and(|params| !(params.is_object() || params.is_array()))
    {
        return None;
    }
    req.get("method").and_then(Value::as_str)
}

/// JSON-RPC 2.0: a request id is a string, a number or null.
fn is_valid_request_id(id: &Value) -> bool {
    matches!(id, Value::String(_) | Value::Number(_) | Value::Null)
}

/// Serialize a JSON-RPC value as a newline-terminated frame and send it
/// abort-safely. `true` on success. Request/response and rejection writes
/// share this path.
fn write_envelope(writer: &mut Stream, value: &Value) -> bool {
    push_bytes(writer, encode_frame(value).as_bytes())
}

/// The exact bytes [`write_envelope`] puts on the wire: compact JSON plus the
/// terminating newline. Shared so frame-size tests measure the real encoding.
///
/// Issue #1071: the client reads at most [`MAX_REQUEST_LEN`] bytes per reply,
/// newline included, and rejects a longer frame without parsing it. A reply
/// that would not fit (an unbudgeted result, or a huge echoed request id) is
/// replaced by a `-32603` error the client can read.
pub(crate) fn encode_frame(value: &Value) -> String {
    let mut frame = value.to_string();
    frame.push('\n');
    if frame.len() > MAX_REQUEST_LEN as usize {
        log::warn!(
            "IPC reply of {} bytes exceeds the {MAX_REQUEST_LEN}-byte frame cap; sending -32603",
            frame.len()
        );
        return oversized_reply_frame(value.get("id"));
    }
    frame
}

/// The `-32603` frame [`encode_frame`] sends in place of a reply over the
/// frame cap. It echoes the request id when that still fits, else `null`;
/// with a `null` id the frame is a fixed, small string.
fn oversized_reply_frame(id: Option<&Value>) -> String {
    let error_frame = |id: Value| {
        let mut frame = json!({
            "jsonrpc": "2.0",
            "error": {"code": -32603, "message": "response exceeds IPC frame cap"},
            "id": id,
        })
        .to_string();
        frame.push('\n');
        frame
    };
    let frame = error_frame(id.cloned().unwrap_or(Value::Null));
    if frame.len() <= MAX_REQUEST_LEN as usize {
        frame
    } else {
        error_frame(Value::Null)
    }
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
/// A write to a closed Unix socket returns `BrokenPipe` cleanly.
fn push_bytes(writer: &mut Stream, buf: &[u8]) -> bool {
    if let Err(e) = writer.set_send_timeout(Some(IPC_WRITE_TIMEOUT))
        && !socket_timeout_error_is_tolerable(&e)
    {
        return false;
    }
    writer.write_all(buf).is_ok() && writer.flush().is_ok()
}

fn dispatch_to_gpui(
    request_tx: &mpsc::SyncSender<IpcRequest>,
    method: String,
    params: Value,
    id: Value,
) -> Value {
    if !supported_methods().contains(&method.as_str()) {
        return json!({"jsonrpc": "2.0", "error": {"code": -32601, "message": format!("Method not found: {method}")}, "id": id});
    }
    let (resp_tx, resp_rx) = mpsc::channel();
    let dispatch = Arc::new(AtomicU8::new(IPC_DISPATCH_QUEUED));
    let ipc_req = IpcRequest {
        method: method.clone(),
        params,
        response_tx: resp_tx,
        dispatch: Arc::clone(&dispatch),
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
mod peer_closed_tests {
    use super::{IpcRequest, Stream, handle_connection};
    use interprocess::local_socket::{GenericFilePath, Listener, ListenerOptions, prelude::*};
    use paneflow_ipc_client::ai_hook::METHOD_STOP;
    use serde_json::json;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// How long a stray copy of the client socket may keep the peer open. A
    /// concurrently forked child releases it at exec or exit.
    const PEER_CLOSE_DEADLINE: Duration = Duration::from_secs(10);

    fn bind_listener(path: &Path) -> Listener {
        let name = path.to_fs_name::<GenericFilePath>().expect("socket name");
        ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("bind listener")
    }

    /// Connect and write one fire-and-forget `ai.stop` frame. The caller
    /// closes the client.
    fn connect_and_write_stop_frame(path: &Path) -> UnixStream {
        let mut client = UnixStream::connect(path).expect("connect");
        let frame = json!({"jsonrpc": "2.0", "method": METHOD_STOP, "params": {}});
        client
            .write_all(format!("{frame}\n").as_bytes())
            .expect("write frame");
        client
    }

    /// Wait for XNU to treat the peer as closed, and return the error it then
    /// gives for `SO_RCVTIMEO`.
    ///
    /// Issue #1112: `drop(client)` closes only this thread's descriptor, and
    /// the peer is closed only once the last copy of the client socket goes.
    /// Other tests in this binary spawn children concurrently (portable-pty
    /// shells, re-execs, `/bin/sh` helpers). A child forked at the wrong
    /// moment holds a copy until it execs or exits; macOS has no atomic
    /// close-on-exec for `socket()`, so an unlucky fork keeps it for the
    /// child's whole life. While a copy is open `SO_RCVTIMEO` succeeds, so
    /// retry until it fails rather than asserting on the first attempt.
    fn wait_until_peer_closed(server: &Stream) -> std::io::Error {
        let deadline = Instant::now() + PEER_CLOSE_DEADLINE;
        loop {
            match server.set_recv_timeout(Some(Duration::from_secs(1))) {
                Err(err) => return err,
                Ok(()) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(()) => panic!(
                    "SO_RCVTIMEO on a peer-closed socket must fail on macOS, but it \
                     still succeeded after {PEER_CLOSE_DEADLINE:?}: another process \
                     is holding a copy of the client socket"
                ),
            }
        }
    }

    /// The buffered frame reaches the GPUI queue exactly once, then the
    /// handler sees EOF and returns.
    fn assert_single_frame_dispatched(server: Stream) {
        let (request_tx, request_rx) = mpsc::sync_channel::<IpcRequest>(4);
        let handler = std::thread::spawn(move || handle_connection(server, request_tx));

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the buffered frame must reach the GPUI queue");
        assert_eq!(request.method, METHOD_STOP);
        let _ = request.response_tx.send(json!({"ok": true}));

        handler.join().expect("handler thread");
        assert!(
            request_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "exactly one frame is dispatched, then the handler sees EOF"
        );
    }

    /// Issue #824: a fire-and-forget client writes one frame and closes before
    /// the handler sets its receive timeout. XNU then refuses `SO_RCVTIMEO`
    /// with `EINVAL`; the buffered frame must still be dispatched.
    #[test]
    fn frame_from_a_peer_that_already_closed_is_still_dispatched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("peer-closed.sock");
        let listener = bind_listener(&path);

        drop(connect_and_write_stop_frame(&path));

        let server = listener.accept().expect("accept");
        // Precondition: this is the bug path, not a socket that still accepts
        // a receive timeout.
        let err = wait_until_peer_closed(&server);
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "got {err:?}");

        assert_single_frame_dispatched(server);
    }

    /// Issue #1112: a deterministic stand-in for the concurrent fork that made
    /// the test above flaky. A child holds a copy of the client socket after
    /// `drop(client)`, so an immediate `SO_RCVTIMEO` still succeeds (the old
    /// hard precondition panicked here). The precondition converges once the
    /// child lets go, and the #824 path still dispatches the frame.
    #[test]
    fn peer_closed_precondition_waits_out_a_stray_holder_of_the_client_fd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stray-holder.sock");
        let listener = bind_listener(&path);

        let client = connect_and_write_stop_frame(&path);
        // `/bin/cat` holds the client socket as its stdout and blocks reading
        // its stdin until `release` closes, so the test decides when the last
        // copy goes.
        let (hold, release) = std::io::pipe().expect("pipe");
        let mut holder = Command::new("/bin/cat")
            .stdin(hold)
            .stdout(OwnedFd::from(client.try_clone().expect("clone client")))
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the fd holder");
        drop(client);

        let server = listener.accept().expect("accept");
        server
            .set_recv_timeout(Some(Duration::from_secs(1)))
            .expect("a stray copy of the client keeps the peer open");

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(release);
        });
        let err = wait_until_peer_closed(&server);
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "got {err:?}");
        releaser.join().expect("releaser thread");
        assert!(holder.wait().expect("reap the fd holder").success());

        assert_single_frame_dispatched(server);
    }

    /// Issue #1071: the server echoes any request id. A raw-socket request
    /// that fits the request cap but carries a huge id must still get a reply
    /// frame the capped client can read and parse.
    #[test]
    fn huge_request_id_gets_a_reply_that_fits_the_client_frame() {
        use std::io::{BufRead, BufReader, Read};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge-id.sock");
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("socket name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("bind listener");

        // Fill the request to exactly the server's read cap. The
        // `system.identify` result is longer than its method name, so the
        // echoed reply is over the cap.
        let shell = json!({"jsonrpc": "2.0", "method": "system.identify", "id": ""}).to_string();
        let pad = super::MAX_REQUEST_LEN as usize - shell.len() - 1;
        let request = json!({"jsonrpc": "2.0", "method": "system.identify", "id": "i".repeat(pad)});
        let mut frame = request.to_string();
        frame.push('\n');
        assert_eq!(frame.len(), super::MAX_REQUEST_LEN as usize);

        let mut client = UnixStream::connect(&path).expect("connect");
        let server = listener.accept().expect("accept");
        let (request_tx, _request_rx) = mpsc::sync_channel::<IpcRequest>(4);
        let handler = std::thread::spawn(move || handle_connection(server, request_tx));
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        client.write_all(frame.as_bytes()).expect("write request");

        // Read exactly as `paneflow-ipc-client` does.
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take(paneflow_ipc_client::MAX_FRAME_BYTES as u64)
            .read_line(&mut line)
            .expect("read reply");
        assert!(
            line.ends_with('\n'),
            "reply of {n}+ bytes overran the client frame cap"
        );
        let reply: serde_json::Value = serde_json::from_str(&line).expect("reply parses");
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["error"]["code"], -32603, "{reply}");
        assert_eq!(reply["id"], serde_json::Value::Null);

        drop(reader);
        drop(client);
        handler.join().expect("handler thread");
    }

    /// Issue #1098: a frame without `"jsonrpc":"2.0"`, with an `id` that is
    /// not a string/number/null, or with `params` that is not an object/array
    /// is not a JSON-RPC 2.0 request. It gets `-32600 Invalid Request` before
    /// any dispatch, even with no `id` (an invalid envelope is not a
    /// notification), and an invalid-type id is answered as `null`. Valid 2.0
    /// pings on the same connection still succeed.
    #[test]
    fn invalid_jsonrpc_version_is_rejected_before_dispatch() {
        use serde_json::Value;
        use std::io::{BufRead, BufReader};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad-version.sock");
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("socket name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("bind listener");

        let client = UnixStream::connect(&path).expect("connect");
        let server = listener.accept().expect("accept");
        let (request_tx, request_rx) = mpsc::sync_channel::<IpcRequest>(8);
        let handler = std::thread::spawn(move || handle_connection(server, request_tx));
        // Stand-in for the GPUI thread: record and answer anything dispatched.
        let gpui = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for request in request_rx {
                methods.push(request.method.clone());
                let _ = request.response_tx.send(json!({"ok": true}));
            }
            methods
        });
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut writer = client.try_clone().expect("clone");
        let mut reader = BufReader::new(client);
        let mut round_trip = |frame: &str| -> Value {
            writer
                .write_all(format!("{frame}\n").as_bytes())
                .expect("write request");
            let mut line = String::new();
            reader.read_line(&mut line).expect("read reply");
            serde_json::from_str(&line).expect("reply parses")
        };

        let invalid = [
            (r#"{"method":"surface.list","params":{},"id":1}"#, json!(1)),
            (
                r#"{"jsonrpc":2,"method":"surface.list","params":{},"id":2}"#,
                json!(2),
            ),
            (
                r#"{"jsonrpc":"1.0","method":"system.ping","id":3}"#,
                json!(3),
            ),
            (
                r#"{"jsonrpc":"1.0","method":"surface.list","params":{},"id":4}"#,
                json!(4),
            ),
            (
                r#"{"jsonrpc":"1.0","method":"ai.stop","params":{}}"#,
                Value::Null,
            ),
            ("5", Value::Null),
            // An id of an invalid type is not echoed back.
            (
                r#"{"jsonrpc":"1.0","method":"x","id":{"a":1}}"#,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","method":"surface.list","id":true}"#,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","method":"surface.list","params":{},"id":[7]}"#,
                Value::Null,
            ),
            // `params`, when present, must be an object or an array.
            (
                r#"{"jsonrpc":"2.0","method":"surface.list","params":5,"id":8}"#,
                json!(8),
            ),
            (
                r#"{"jsonrpc":"2.0","method":"surface.list","params":"x","id":9}"#,
                json!(9),
            ),
            (
                r#"{"jsonrpc":"2.0","method":"ai.stop","params":null}"#,
                Value::Null,
            ),
        ];
        for (frame, id) in invalid {
            let reply = round_trip(frame);
            assert_eq!(reply["jsonrpc"], "2.0", "{frame} -> {reply}");
            assert_eq!(reply["error"]["code"], -32600, "{frame} -> {reply}");
            assert_eq!(reply["id"], id, "{frame} -> {reply}");
            assert!(reply.get("result").is_none(), "{frame} -> {reply}");
        }

        let pong = round_trip(r#"{"jsonrpc":"2.0","method":"system.ping","id":6}"#);
        assert_eq!(pong["result"]["pong"], true, "{pong}");
        assert_eq!(pong["id"], 6);
        // A string or null id and array params are valid envelopes.
        let pong = round_trip(r#"{"jsonrpc":"2.0","method":"system.ping","params":[],"id":"s"}"#);
        assert_eq!(pong["result"]["pong"], true, "{pong}");
        assert_eq!(pong["id"], "s");
        let pong = round_trip(r#"{"jsonrpc":"2.0","method":"system.ping","id":null}"#);
        assert_eq!(pong["result"]["pong"], true, "{pong}");
        assert_eq!(pong["id"], Value::Null);

        drop(writer);
        drop(reader);
        handler.join().expect("handler thread");
        let dispatched = gpui.join().expect("gpui stand-in");
        assert!(
            dispatched.is_empty(),
            "invalid envelopes reached GPUI dispatch: {dispatched:?}"
        );
    }
}

/// Issue #1115: the IPC server creates its sockets only while no guarded
/// spawn is in flight, so a child spawned through `paneflow_process::spawn`
/// can never copy a socket that is not close-on-exec yet.
#[cfg(test)]
mod spawn_exclusion_tests {
    use super::{accept_uninheritable, bind_socket};
    use interprocess::local_socket::{ListenerNonblockingMode, prelude::*};
    use std::io::{PipeReader, PipeWriter, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// Lets the held child continue to exec. Also sent on drop, so a failed
    /// assertion never leaves the spawn thread blocked.
    struct Release(PipeWriter);

    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.write_all(b"r");
        }
    }

    /// Start a guarded spawn on another thread and return once it is in
    /// flight. The forked child blocks before exec until `Release` fires, so
    /// `paneflow_process::spawn` does not return until then.
    fn spawn_held_in_flight() -> (Release, JoinHandle<()>) {
        let (mut ready_rx, ready_tx): (PipeReader, PipeWriter) = std::io::pipe().expect("pipe");
        let (release_rx, release_tx) = std::io::pipe().expect("pipe");
        let (ready_fd, release_rx_fd, release_tx_fd) = (
            ready_tx.as_raw_fd(),
            release_rx.as_raw_fd(),
            release_tx.as_raw_fd(),
        );
        let spawner = std::thread::spawn(move || {
            let mut command = Command::new("/usr/bin/true");
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: `close`, `write`, and `read` are async-signal-safe,
            // and the closure only touches descriptors it was handed.
            // Closing the child's copy of the release writer lets the
            // test's drop of `Release` reach it as EOF as well.
            unsafe {
                command.pre_exec(move || {
                    libc::close(release_tx_fd);
                    libc::write(ready_fd, b"x".as_ptr().cast(), 1);
                    let mut byte = 0_u8;
                    libc::read(release_rx_fd, (&raw mut byte).cast(), 1);
                    Ok(())
                });
            }
            let mut child = paneflow_process::spawn(&mut command).expect("spawn held child");
            let _ = child.wait();
            drop((ready_tx, release_rx));
        });
        let mut byte = [0_u8; 1];
        ready_rx
            .read_exact(&mut byte)
            .expect("the held child reports that it has forked");
        (Release(release_tx), spawner)
    }

    #[test]
    fn ipc_accept_waits_for_an_in_flight_spawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("accept.sock");
        let listener = bind_socket(&path).expect("bind");
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .expect("nonblocking accept");
        let _client = UnixStream::connect(&path).expect("connect");

        let (release, spawner) = spawn_held_in_flight();
        let during = accept_uninheritable(&listener);
        assert!(
            matches!(&during, Err(err) if err.kind() == std::io::ErrorKind::WouldBlock),
            "a pending connection must not be accepted while a spawn is in flight: {during:?}"
        );

        drop(release);
        spawner.join().expect("spawner thread");
        // Other tests in this binary spawn too; retry past their brief holds.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match accept_uninheritable(&listener) {
                Ok(_stream) => break,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "accept never resumed");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        }
    }

    #[test]
    fn ipc_bind_waits_for_an_in_flight_spawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bind.sock");

        let (release, spawner) = spawn_held_in_flight();
        let bound = Arc::new(AtomicBool::new(false));
        let binder = {
            let bound = Arc::clone(&bound);
            std::thread::spawn(move || {
                let listener = bind_socket(&path);
                bound.store(true, Ordering::SeqCst);
                listener.is_some()
            })
        };
        // An unguarded bind finishes in well under a millisecond.
        std::thread::sleep(Duration::from_millis(300));
        let bound_during_spawn = bound.load(Ordering::SeqCst);
        drop(release);
        spawner.join().expect("spawner thread");
        let listened = binder.join().expect("binder thread");

        assert!(
            !bound_during_spawn,
            "the IPC socket must not be created while a spawn is in flight"
        );
        assert!(listened, "bind must succeed once the spawn returns");
    }

    /// A column-0 attribute that compiles the next item only under test:
    /// `#[cfg(test)]` or `#[cfg(all(test, ...))]`, never `#[cfg(not(test))]`.
    fn is_test_cfg(line: &str) -> bool {
        line == "#[cfg(test)]" || line.starts_with("#[cfg(all(test,")
    }

    /// The name in `mod NAME;` (any visibility), for an out-of-line module.
    fn out_of_line_module(line: &str) -> Option<&str> {
        let line = line
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("pub ");
        line.strip_prefix("mod ")?.strip_suffix(';')
    }

    /// The production lines of `src`, numbered from 1, with every top-level
    /// test-only item left out, however late it sits in the file: a block
    /// item runs to its column-0 `}`. Also returns the names of out-of-line
    /// test-only modules (`#[cfg(test)] mod name;`).
    fn production_lines(src: &str) -> (Vec<(usize, &str)>, Vec<String>) {
        let lines: Vec<&str> = src.lines().collect();
        let mut kept = Vec::new();
        let mut test_modules = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            if !is_test_cfg(lines[index]) {
                kept.push((index + 1, lines[index]));
                index += 1;
                continue;
            }
            // Further attributes and doc comments belong to the same item.
            let mut item = index + 1;
            while item < lines.len() && {
                let line = lines[item].trim_start();
                line.starts_with("#[") || line.starts_with("//")
            } {
                item += 1;
            }
            let Some(first) = lines.get(item) else {
                break;
            };
            if let Some(name) = out_of_line_module(first) {
                test_modules.push(name.to_string());
            }
            index = test_item_end(&lines, item) + 1;
        }
        (kept, test_modules)
    }

    /// The index of the last line of the top-level item that starts at
    /// `first`, as rustfmt lays it out. A block item (`fn`, `mod`, `impl`,
    /// with its head on one or several lines) opens with a column-0 line
    /// ending in `{` and closes at the next column-0 `}`. Any other item ends
    /// at its first line ending in `;`, or on its own line when that line
    /// opens and closes a block (`fn f() {}`). When unsure this ends early,
    /// so the rest is scanned as production: a guard that scans too much
    /// fails loudly, one that scans too little passes silently.
    fn test_item_end(lines: &[&str], first: usize) -> usize {
        for (at, line) in lines.iter().enumerate().skip(first) {
            let column_zero = !line.starts_with(char::is_whitespace);
            if line.ends_with(';') || (column_zero && line.ends_with('}')) {
                return at;
            }
            if column_zero && line.ends_with('{') {
                let close = lines[at + 1..]
                    .iter()
                    .position(|line| line.starts_with('}'))
                    .unwrap_or_else(|| panic!("test-only item at line {} never closes", at + 1));
                return at + 1 + close;
            }
        }
        first
    }

    /// Each test-only item shape rustfmt produces is skipped exactly, and
    /// the production line after it is still scanned.
    #[test]
    fn production_lines_skip_only_test_only_items() {
        let src = [
            "#[cfg(test)]",
            "static GATE: Mutex<Vec<PathBuf>> =",
            "    Mutex::new(Vec::new());",
            "fn after_static() {}",
            "#[cfg(test)]",
            "type Map =",
            "    HashMap<(A, String), Arc<B>>;",
            "fn after_type() {}",
            "#[cfg(test)]",
            "fn one_line() {}",
            "fn after_one_line() {}",
            "#[cfg(test)]",
            "/// doc",
            "#[allow(dead_code)]",
            "const X: u8 = 1;",
            "fn after_const() {}",
            "#[cfg(test)]",
            "fn multi_line_head(",
            "    a: u8,",
            ") -> u8 {",
            "    a",
            "}",
            "fn after_fn() {}",
            "#[cfg(not(test))]",
            "fn not_test() {}",
            "#[cfg(all(test, unix))]",
            "mod tests {",
            "    fn inner() {}",
            "}",
            "#[cfg(test)]",
            "pub(crate) mod helpers;",
            "fn after_mod() {}",
        ]
        .join("\n");
        let (kept, modules) = production_lines(&src);
        let kept: Vec<&str> = kept.into_iter().map(|(_, line)| line).collect();
        assert_eq!(
            kept,
            [
                "fn after_static() {}",
                "fn after_type() {}",
                "fn after_one_line() {}",
                "fn after_const() {}",
                "fn after_fn() {}",
                "#[cfg(not(test))]",
                "fn not_test() {}",
                "fn after_mod() {}",
            ]
        );
        assert_eq!(modules, ["helpers"]);
    }

    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).expect("read source dir");
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Where `mod name;` declared in `file` lives: `name.rs` or `name/`.
    fn module_path(file: &std::path::Path, name: &str) -> std::path::PathBuf {
        let parent = file.parent().expect("source file has a parent");
        let owns_dir = matches!(
            file.file_name().and_then(|n| n.to_str()),
            Some("main.rs" | "lib.rs" | "mod.rs")
        );
        let base = if owns_dir {
            parent.to_path_buf()
        } else {
            parent.join(file.file_stem().expect("source file stem"))
        };
        let single = base.join(format!("{name}.rs"));
        if single.is_file() {
            single
        } else {
            base.join(name)
        }
    }

    /// Every child PaneFlow spawns outside the PTY goes through
    /// `paneflow_process`, whose `spawn` holds the exclusion the IPC server
    /// creates its sockets under. A direct `Command::spawn`, `output`, or
    /// `status` could copy a socket that is not close-on-exec yet, and a
    /// `Stdio::piped()` pipe is inheritable for a moment while std creates it
    /// (issue #1124).
    #[test]
    fn production_child_spawns_go_through_paneflow_process() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(root.is_dir(), "{} is not a directory", root.display());
        let mut files = Vec::new();
        rust_sources(&root, &mut files);
        files.sort();
        assert!(
            files.len() > 100,
            "the walk found only {} files",
            files.len()
        );

        let mut production = Vec::new();
        let mut test_only = Vec::new();
        for path in files {
            let src = std::fs::read_to_string(&path).expect("read source");
            let (kept, test_modules) = production_lines(&src);
            test_only.extend(test_modules.iter().map(|name| module_path(&path, name)));
            let kept: Vec<(usize, String)> = kept
                .into_iter()
                .map(|(number, line)| (number, line.to_string()))
                .collect();
            production.push((path, kept));
        }

        // Self-checks: a guard that scans nothing passes every tree.
        assert!(
            test_only.contains(&root.join("startup_bench.rs")),
            "`#[cfg(test)] mod startup_bench;` in main.rs was not recognised"
        );
        let main = &production
            .iter()
            .find(|(path, _)| *path == root.join("main.rs"))
            .expect("main.rs was walked")
            .1;
        assert!(
            main.iter().any(|(_, line)| line == "impl PaneFlowApp {"),
            "main.rs production code after its first test module was not scanned"
        );
        let git = &production
            .iter()
            .find(|(path, _)| *path == root.join("workspace/git.rs"))
            .expect("workspace/git.rs was walked")
            .1;
        assert!(
            git.iter()
                .any(|(_, line)| line.starts_with("fn git_entry_exists(")),
            "workspace/git.rs production code after a multi-line test-only static was not scanned"
        );

        const PIPED: &str = "Stdio::piped()";
        let direct = [
            ".spawn()",
            ".output()",
            ".status()",
            "Command::spawn(",
            "Command::output(",
            "Command::status(",
            // The `open` crate runs `/usr/bin/open` through a plain `Command`.
            "open::that(",
            "open::that_detached(",
            "open::that_in_background(",
            "open::with(",
            "open::with_detached(",
            "open::with_in_background(",
            // Issue #1124: on macOS std's `Stdio::piped()` pipe ends stay
            // inheritable for a moment inside `Command::spawn`; ask
            // `paneflow_process::spawn_piped` for pipes instead.
            PIPED,
        ];
        // The PTY guard spawns on the render thread, where `spawn_piped`'s
        // wait for in-flight spawns could stall the UI; its control pipe
        // moves to `spawn_piped` once the spawn leaves that thread (#1129).
        // Drop this entry then.
        let piped_allowed = [root.join("agents/parent_guard.rs")];
        assert!(
            piped_allowed.iter().all(|path| path.is_file()),
            "a Stdio::piped() allowlist entry no longer exists: {piped_allowed:?}"
        );
        let mut offenders = Vec::new();
        for (path, lines) in &production {
            // Component-wise: covers a test-only `name.rs` and all of `name/`.
            if test_only.iter().any(|skip| path.starts_with(skip)) {
                continue;
            }
            let name = path.strip_prefix(&root).expect("under src").display();
            for (number, line) in lines {
                if !line.trim_start().starts_with("//")
                    && direct.iter().any(|call| {
                        line.contains(call) && !(*call == PIPED && piped_allowed.contains(path))
                    })
                {
                    offenders.push(format!("{name}:{number}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "spawn children through paneflow_process::spawn (or its spawn_piped / \
             run_with_timeout / spawn_detached helpers), not directly, and get pipes from \
             spawn_piped, not Stdio::piped() (issues #1115, #1124):\n{}",
            offenders.join("\n")
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
            response_tx,
            dispatch: Arc::new(AtomicU8::new(IPC_DISPATCH_QUEUED)),
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
    /// capability before submitting text must not be
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

    /// Issue #283: `set_ai_unrestricted` must run in `PaneFlowApp::new`
    /// before the socket listener binds. Session restore sits between those
    /// two calls and can take hundreds of ms; a client that treats socket
    /// appearance as readiness before submitting text
    /// would otherwise see `scripting: false` even when `ai_unrestricted` is
    /// on. The GPUI tick later opens the write gate - that restore-window
    /// hole is the bug this pin guards.
    #[test]
    fn ai_unrestricted_is_seeded_before_the_socket_binds() {
        let source = include_str!("app/bootstrap.rs");
        let seed_offset = source
            .find("crate::ipc::set_ai_unrestricted")
            .expect("bootstrap seeds AI_UNRESTRICTED");
        let bind_offset = source
            .find("ipc::start_server()")
            .expect("bootstrap binds the IPC socket");
        assert!(
            seed_offset < bind_offset,
            "set_ai_unrestricted must run before the socket binds; seeding after \
             restore leaves a window where system.capabilities reports scripting: false \
             while ai_unrestricted is on"
        );
    }
}

/// The socket advertises and dispatches the same method set.
fn supported_methods() -> Vec<&'static str> {
    let mut methods = vec![
        "system.ping",
        "system.capabilities",
        "system.identify",
        "surface.list",
        "surface.read",
        "surface.search",
        "surface.send_text",
        "surface.send_keystroke",
        "surface.status",
        "fleet.list",
        "agent.whoami",
    ];
    methods.extend_from_slice(paneflow_ipc_client::ai_hook::METHODS);
    methods
}

#[cfg(test)]
mod removed_method_tests {
    use super::*;

    #[test]
    fn removed_methods_are_not_advertised_or_dispatched() {
        let (tx, rx) = mpsc::sync_channel(1);
        for (namespace, verb) in [
            ("workspace", "create"),
            ("workspace", "select"),
            ("surface", "split"),
            ("surface", "focus"),
            ("events", "subscribe"),
            ("workspace", "up"),
            ("workspace", "list"),
            ("workspace", "current"),
            ("workspace", "close"),
            ("workspace", "restore_layout"),
            ("surface", "rename"),
            ("task", "get"),
            ("task", "assign"),
            ("task", "report"),
        ] {
            let method = format!("{namespace}.{verb}");
            assert!(!supported_methods().contains(&method.as_str()));
            let response = dispatch_to_gpui(&tx, method, json!({}), json!(42));
            assert_eq!(response["error"]["code"], -32601);
            assert_eq!(response["id"], 42);
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        }
        for retained in [
            "surface.list",
            "surface.read",
            "surface.search",
            "agent.whoami",
        ] {
            assert!(supported_methods().contains(&retained));
        }
        assert!(
            paneflow_ipc_client::ai_hook::METHODS
                .iter()
                .all(|method| supported_methods().contains(method))
        );
    }
}
