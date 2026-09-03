#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic
    )
)]
//! paneflow-ipc-client - blocking JSON-RPC client for Paneflow's local IPC socket.
//!
//! Mirrors the server wire protocol at `src-app/src/ipc.rs`: newline-delimited
//! JSON-RPC 2.0 over a Unix domain socket. Unlike `paneflow-ai-hook`
//! (fire-and-forget), this client is request/response - it reads back the
//! one-line response the server writes on the same connection.
//!
//! One connection per request: simple and robust (a stale connection can't
//! wedge the caller). The server's peer-UID check passes because the client
//! runs as the same user that launched Paneflow.
//!
//! Shared crate (no GPUI / `src-app` dependency): consumed both by the MCP
//! bridge (`paneflow-mcp`) and the `paneflow` CLI subcommands.

pub mod ai_hook;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, ConnectOptions, GenericFilePath, Stream};
use interprocess::ConnectWaitMode;
use serde_json::{json, Value};

/// Maximum size of one newline-delimited JSON-RPC frame on the local socket.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Wire timeout for a single request/response round-trip. The server always
/// writes a response (it can synthesize a `-32002` dispatch timeout
/// envelope), so a stall this long means the process is wedged.
const IPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Keep retries bounded even when the server remains overloaded. A timeout
/// consumes most of this budget, so it gets at most one short follow-up; fast
/// busy replies can use the full exponential-backoff sequence.
const IPC_CALL_BUDGET: Duration = Duration::from_secs(12);
const IPC_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];

/// U-029: per-reply read cap on the untrusted IPC socket. Mirrors the server's
/// `MAX_REQUEST_LEN` (`src-app/src/ipc.rs`). The recv timeout bounds wall-clock
/// time but not memory - a same-UID peer can deliver many GB before the
/// deadline - so the read is also byte-bounded and a reply that hits the cap
/// without a terminating newline is a framing error, not a partial parse.
const MAX_RESPONSE_LEN: u64 = MAX_FRAME_BYTES as u64;

/// Abstraction over "send a JSON-RPC request to Paneflow, get the `result`".
/// Lets callers (MCP layer, CLI) be unit-tested against a fake transport with
/// no live socket.
pub trait IpcTransport {
    /// Call a Paneflow IPC method. Returns the `result` value on success, or
    /// `Err(message)` on transport failure or a JSON-RPC `error` envelope.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// Live client bound to a resolved socket path.
pub struct IpcClient {
    socket: PathBuf,
    next_id: AtomicU64,
}

impl IpcClient {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            next_id: AtomicU64::new(1),
        }
    }
}

impl IpcTransport for IpcClient {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = build_request(id, method, params);
        call_with_sender(&self.socket, method, &request, send_and_receive)
    }
}

fn call_with_sender<F>(
    socket: &Path,
    method: &str,
    request: &Value,
    mut send: F,
) -> Result<Value, String>
where
    F: FnMut(&Path, &Value, Duration) -> io::Result<String>,
{
    let started = Instant::now();
    let mut retry_index = 0;
    loop {
        let remaining = IPC_CALL_BUDGET.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(format!(
                "paneflow IPC timed out at {} after {}s",
                socket.display(),
                IPC_CALL_BUDGET.as_secs()
            ));
        }

        let outcome = send(socket, request, remaining.min(IPC_TIMEOUT));
        let should_retry = match &outcome {
            Ok(line) => is_busy_retry_response(line),
            Err(error) => is_timeout(error) && method_is_safe_to_retry_after_timeout(method),
        };

        if should_retry {
            if let Some(delay) = IPC_RETRY_DELAYS.get(retry_index).copied() {
                if delay < IPC_CALL_BUDGET.saturating_sub(started.elapsed()) {
                    std::thread::sleep(delay);
                    retry_index += 1;
                    continue;
                }
            }
        }

        return match outcome {
            Ok(line) => parse_response(&line),
            Err(error) if is_timeout(&error) => Err(format!(
                "paneflow IPC timed out at {} ({error})",
                socket.display()
            )),
            Err(error) => Err(format!(
                "paneflow IPC unreachable at {} ({error}); is PaneFlow running?",
                socket.display()
            )),
        };
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn is_busy_retry_response(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return false;
    };
    let Some(error) = value.get("error") else {
        return false;
    };
    error.get("code").and_then(Value::as_i64) == Some(-32000)
        && error.get("message").and_then(Value::as_str) == Some("PaneFlow is busy; retry shortly")
}

fn method_is_safe_to_retry_after_timeout(method: &str) -> bool {
    matches!(
        method,
        "system.ping"
            | "system.capabilities"
            | "system.identify"
            | "workspace.list"
            | "workspace.current"
            | "surface.list"
            | "surface.read"
            | "surface.search"
            | "surface.status"
            | "fleet.list"
    )
}

/// Build a JSON-RPC 2.0 request frame.
pub(crate) fn build_request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Extract the `result` from a JSON-RPC response line, or translate an
/// `error` envelope / malformed line into `Err(message)`.
pub(crate) fn parse_response(line: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid JSON-RPC response from paneflow: {e}"))?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(
            "invalid JSON-RPC response from paneflow: missing `\"jsonrpc\": \"2.0\"`".to_string(),
        );
    }
    if let Some(message) = jsonrpc_error_message_from_value(&value) {
        return Err(message);
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| "paneflow response missing both `result` and `error`".to_string())
}

pub fn jsonrpc_error_message(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    jsonrpc_error_message_from_value(&value)
}

fn jsonrpc_error_message_from_value(value: &Value) -> Option<String> {
    let err = value.get("error")?;
    // JSON-RPC 2.0 requires `error` to be an object with an integer `code`
    // and a string `message`; anything else is a malformed frame, not an
    // error with a made-up code.
    let (Some(code), Some(message)) = (
        err.get("code").and_then(Value::as_i64),
        err.get("message").and_then(Value::as_str),
    ) else {
        return Some(format!(
            "malformed JSON-RPC error from paneflow (expected `{{\"code\": integer, \"message\": string}}`): {err}"
        ));
    };
    let data = err
        .get("data")
        .map(|data| format!(" ({data})"))
        .unwrap_or_default();
    Some(format!("paneflow error {code}: {message}{data}"))
}

/// Open a connection, write the newline-terminated request, and read back one
/// newline-delimited response line.
///
/// US-023: the read deadline is enforced at the OS level via socket
/// send/recv timeouts. The previous scratch-thread + `recv_timeout`
/// pattern leaked one OS thread
/// and one socket FD on every timeout - the spawned reader owned `stream` and
/// stayed blocked in `read_line` forever (no deadline ever reached it), so an
/// agent retrying `read_pane` against a wedged Paneflow exhausted the
/// long-lived bridge's threads/FDs. With an OS deadline, `read_line` returns
/// the error itself, the owning `BufReader` drops, and the FD is released.
/// Collapse an `ErrorKind::Unsupported` result to `Ok(())` - used only for
/// optional Unix socket-deadline setters. Any other error is forwarded
/// unchanged.
fn tolerate_unsupported(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(()),
        other => other,
    }
}

fn send_and_receive(socket: &Path, request: &Value, timeout: Duration) -> io::Result<String> {
    let started = Instant::now();
    let mut stream = connect_stream_with_timeout(socket, timeout)?;

    let mut payload =
        serde_json::to_vec(request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    payload.push(b'\n');

    {
        // Recompute what remains before each blocking phase so connect, write,
        // and read share one deadline instead of each receiving a fresh one.
        let write_timeout = remaining_timeout(started, timeout)?;
        tolerate_unsupported(stream.set_send_timeout(Some(write_timeout)))?;
        stream.write_all(&payload)?;
        stream.flush()?;

        let read_timeout = remaining_timeout(started, timeout)?;
        tolerate_unsupported(stream.set_recv_timeout(Some(read_timeout)))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        // U-029: cap the reply read at MAX_RESPONSE_LEN (Take rebuilt per call, so
        // the limit is per-reply) and treat hitting the cap without a terminating
        // newline as a framing error rather than feeding a truncated line to the
        // parser.
        match reader.by_ref().take(MAX_RESPONSE_LEN).read_line(&mut line) {
            Ok(n) if n as u64 >= MAX_RESPONSE_LEN && !line.ends_with('\n') => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "paneflow response exceeded the size cap",
            )),
            Ok(_) => Ok(line),
            // SO_RCVTIMEO surfaces as EAGAIN/`WouldBlock` (or `TimedOut` on
            // some stacks) - normalize both to a friendly timeout message.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "paneflow did not respond before the IPC deadline",
                ))
            }
            Err(e) => Err(e),
        }
    }
}

fn remaining_timeout(started: Instant, timeout: Duration) -> io::Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "paneflow did not respond before the IPC deadline",
            )
        })
}

/// Connect with the same wall-clock deadline later applied to recv/send.
/// `Stream::connect` waits unbounded when the listen queue is full.
fn connect_stream_with_timeout(socket: &Path, timeout: Duration) -> io::Result<Stream> {
    let name = socket.to_fs_name::<GenericFilePath>()?;
    ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(timeout))
        .connect_sync()
}

/// EP-002 (agent-control-plane): open a persistent `events.subscribe` stream.
/// Writes the subscribe request, then invokes `on_line` for every newline-
/// delimited event the server pushes, until the connection closes (server side)
/// or `on_line` returns `false`. Unlike [`send_and_receive`], the read side is
/// NOT deadline-bounded: an idle stream is normal (the server heartbeats every
/// 30 s), so only a real disconnect (EOF / error) ends the loop.
pub fn subscribe_stream(
    socket: &Path,
    params: Value,
    mut on_line: impl FnMut(&str) -> bool,
) -> io::Result<()> {
    let mut stream = connect_stream_with_timeout(socket, IPC_TIMEOUT)?;
    let request = build_request(1, "events.subscribe", params);
    let mut payload =
        serde_json::to_vec(&request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    while let Some(line) = read_capped_event_line(&mut reader, &mut buf)? {
        if line.trim().is_empty() {
            continue;
        }
        if !on_line(&line) {
            break;
        }
    }
    Ok(())
}

fn read_capped_event_line<R>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<String>>
where
    R: BufRead,
{
    buf.clear();
    loop {
        let remaining = MAX_RESPONSE_LEN.saturating_sub(buf.len() as u64);
        if remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "paneflow event line exceeded the size cap",
            ));
        }

        let read = reader.by_ref().take(remaining).read_until(b'\n', buf)?;
        if read == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "paneflow event stream ended mid-line",
            ));
        }

        if buf.last() == Some(&b'\n') {
            if buf.ends_with(b"\r\n") {
                buf.truncate(buf.len().saturating_sub(2));
            } else {
                buf.truncate(buf.len().saturating_sub(1));
            }
            let line = String::from_utf8(buf.clone())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            return Ok(Some(line));
        }
    }
}

/// What a single read slice of [`subscribe_stream_timed`] yielded.
pub enum StreamEvent<'a> {
    /// A complete, non-empty event line from the server (JSON).
    Line(&'a str),
    /// `slice` elapsed with no complete line: the caller's quiescence tick.
    /// This is the signal a bare [`subscribe_stream`] cannot deliver.
    Tick,
    /// EOF or a mid-stream socket error: the server vanished.
    Closed,
}

/// EP-003 US-007 (agent-control-plane-hardening): a [`subscribe_stream`] variant
/// whose read side IS deadline-bounded by `slice`. Where `subscribe_stream`
/// blocks forever between events, this wakes every `slice` with a
/// [`StreamEvent::Tick`] so the caller can detect the ABSENCE of events (output
/// quiescence) - the basis of `wait --idle`, with zero client-side polling of
/// pane content. A complete line yields [`StreamEvent::Line`]; EOF or a
/// mid-stream socket error yields [`StreamEvent::Closed`] then returns `Ok(())`
/// (the caller maps it to a clean "server gone" exit). Only a failed connect /
/// subscribe-write returns `Err` (no instance). `on_event` returns `false` to
/// stop.
///
/// Unlike [`send_and_receive`], the recv deadline here is REQUIRED, not
/// best-effort: the `Tick` contract is impossible without it, and a socket
/// that cannot set a recv timeout (`Unsupported`) would block forever in
/// `read_line` instead of ticking - a hang past the caller's overall
/// deadline. So an `Unsupported` recv timeout is surfaced as `Err`; callers
/// that still need quiescence can fall back to another deterministic clock.
pub fn subscribe_stream_timed(
    socket: &Path,
    params: Value,
    slice: Duration,
    mut on_event: impl FnMut(StreamEvent<'_>) -> bool,
) -> io::Result<()> {
    let mut stream = connect_stream_with_timeout(socket, IPC_TIMEOUT)?;
    // REQUIRED (see the doc note): without a recv deadline the read below would
    // block forever between events, so refuse rather than hang.
    stream.set_recv_timeout(Some(slice)).map_err(|e| {
        if e.kind() == io::ErrorKind::Unsupported {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "the event stream needs a recv-timeout-capable Unix socket",
            )
        } else {
            e
        }
    })?;
    let request = build_request(1, "events.subscribe", params);
    let mut payload =
        serde_json::to_vec(&request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    // BYTES, not a `String`: `read_line` validates UTF-8 on every read, so a
    // multibyte codepoint bisected by a recv-slice boundary would surface as
    // `InvalidData` and be mis-read as a disconnect. `read_until(b'\n')` defers
    // validation to the complete line. Reused across slices so a split line is
    // reassembled rather than fed in halves.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Bound each line at the same 256 KiB cap as a request/response reply,
        // so a same-UID server flooding one unterminated line can't grow `buf`
        // without bound (parity with `send_and_receive`). `remaining` shrinks as
        // the line accumulates across slices.
        let remaining = MAX_RESPONSE_LEN.saturating_sub(buf.len() as u64);
        if remaining == 0 {
            // One line exceeded the cap without terminating: framing abuse - the
            // server is not speaking our protocol, treat it as gone.
            on_event(StreamEvent::Closed);
            return Ok(());
        }
        match reader.by_ref().take(remaining).read_until(b'\n', &mut buf) {
            // Clean EOF: the server closed the stream.
            Ok(0) => {
                on_event(StreamEvent::Closed);
                return Ok(());
            }
            // A whole line landed (terminated by the newline).
            Ok(_) if buf.last() == Some(&b'\n') => {
                let keep = {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim();
                    line.is_empty() || on_event(StreamEvent::Line(line))
                };
                buf.clear();
                if !keep {
                    return Ok(());
                }
            }
            // `Ok(n>0)` with no trailing newline = EOF mid-line (or the cap was
            // hit, handled by `remaining == 0` next pass): server gone.
            Ok(_) => {
                on_event(StreamEvent::Closed);
                return Ok(());
            }
            // The recv slice elapsed with no (further) bytes: a quiescence tick.
            // Any partial bytes already read stay in `buf` for the next slice.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if !on_event(StreamEvent::Tick) {
                    return Ok(());
                }
            }
            // A mid-stream socket error means the peer vanished; surface it as
            // Closed (a clean caller exit), not Err (which means "no instance").
            Err(_) => {
                on_event(StreamEvent::Closed);
                return Ok(());
            }
        }
    }
}

/// Resolve the Paneflow IPC socket path. `PANEFLOW_SOCKET_PATH` (inherited
/// from the Paneflow PTY through the agent that launched this process) is
/// authoritative - it carries the exact path the running instance bound.
/// Falls back to the current build profile's default (`paneflow-dev` in debug,
/// `paneflow` in release).
///
/// LOCKSTEP: this chain re-implements the server's resolution in
/// `src-app/src/runtime_paths.rs` (`socket_path_spec_from`) without its `dirs`
/// dependency, and the two must be kept in lockstep or the client dials an
/// endpoint the server never bound (#217). In particular: the override is read
/// as `OsString`, so a non-UTF-8 value is honoured like the server's; a
/// non-UTF-8 `$TMPDIR`, or one that is not an existing directory, is treated
/// as unset and falls through to the cache dir; and the composed path is
/// rejected against the same `sun_path` ceiling.
pub fn resolve_socket_path() -> Option<PathBuf> {
    resolve_socket_path_from(&|key| std::env::var_os(key))
}

/// [`resolve_socket_path`] over an explicit env lookup, mirroring the server's
/// `socket_path_spec_from` seam so tests never mutate process-global env.
#[cfg(unix)]
fn resolve_socket_path_from(env: &impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(path) = socket_path_from_env(env("PANEFLOW_SOCKET_PATH")) {
        // Same reject behaviour as the server: an absolute but over-long
        // override yields no endpoint at all (the server refuses to bind it);
        // it does NOT fall through to the default path.
        return check_sun_path_fits(&path).then_some(path);
    }
    let path = default_socket_path_from(env)?;
    check_sun_path_fits(&path).then_some(path)
}

/// Validate a `PANEFLOW_SOCKET_PATH` value: present and absolute. A relative
/// path means the env was clobbered or we're outside a Paneflow PTY. Takes the
/// raw `OsString` (never a UTF-8 `String`) so a non-UTF-8 override is honoured
/// exactly like the server's `runtime_paths::socket_path_from_env`.
pub(crate) fn socket_path_from_env(raw: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(raw?);
    path.is_absolute().then_some(path)
}

/// Best-effort default socket path.
///
/// LOCKSTEP with the server's `runtime_dir_from` + path composition in
/// `src-app/src/runtime_paths.rs`. Uses raw env (no `dirs` dep) to keep the
/// dependency tree minimal; on macOS `dirs::cache_dir()` is
/// `$HOME/Library/Caches`, so the last-resort branch is equivalent.
///
/// `$XDG_RUNTIME_DIR` is skipped so a Finder-launched GUI (no XDG) and a
/// terminal CLI (XDG often set from the login profile) compose the same path.
/// Chain: `$TMPDIR`, then `$HOME/Library/Caches/run`.
#[cfg(unix)]
fn default_socket_path_from(env: &impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let runtime = env("TMPDIR")
        // Same acceptance as the server (`runtime_paths::runtime_dir_from`):
        // a non-UTF-8 value is treated as unset and falls through.
        .and_then(|raw| raw.into_string().ok())
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        // Same as the server (#289): a $TMPDIR that is not an existing
        // directory (stale from a previous boot) reads as unset, because the
        // server bound under the cache dir in that case.
        .filter(|p| p.is_dir())
        // Last resort, mirroring the server's `dirs::cache_dir().join("run")`
        // (`runtime_paths::runtime_dir_from`). Without this, a client whose
        // $TMPDIR is stripped (launchd/cron) returned None - "IPC unreachable"
        // - even though the server had bound under the cache dir.
        .or_else(|| cache_run_dir_from(env))?;
    let subdir = if cfg!(debug_assertions) {
        "paneflow-dev"
    } else {
        "paneflow"
    };
    let socket_file = if cfg!(debug_assertions) {
        "paneflow-dev.sock"
    } else {
        "paneflow.sock"
    };
    Some(runtime.join(subdir).join(socket_file))
}

/// Compute `<cache_dir>/run` from raw env, mirroring the server's last-resort
/// fallback without taking a `dirs` dependency (the whole point of this crate's
/// minimal tree). macOS: `$HOME/Library/Caches`.
#[cfg(unix)]
fn cache_run_dir_from(env: &impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches").join("run"))
}

/// macOS `sockaddr_un.sun_path` is `[c_char; 104]`. LOCKSTEP with
/// `runtime_paths::MAX_SOCKET_PATH_BYTES` (`src-app/src/runtime_paths.rs`).
#[cfg(unix)]
const MAX_SOCKET_PATH_BYTES: usize = 104;

/// LOCKSTEP: same predicate as the server's `runtime_paths::check_sun_path_fits`
/// (`src-app/src/runtime_paths.rs`). `bind()` needs room for the trailing NUL
/// inside `sun_path`, so a path of *exactly* the array size does not fit -
/// reject `>=`, not `>`. The server `log::warn!`s here; this crate has no `log`
/// dependency, so the client simply reports no endpoint ("IPC unreachable") for
/// a path the server refused to bind anyway.
#[cfg(unix)]
fn check_sun_path_fits(path: &Path) -> bool {
    path.as_os_str().len() < MAX_SOCKET_PATH_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerate_unsupported_swallows_only_unsupported() {
        // Regression: some local-socket stacks reject I/O deadlines with
        // ErrorKind::Unsupported. That must NOT fail the IPC call; any other
        // error must still propagate.
        assert!(tolerate_unsupported(Ok(())).is_ok());
        assert!(
            tolerate_unsupported(Err(io::Error::from(io::ErrorKind::Unsupported))).is_ok(),
            "Unsupported timeout must be tolerated"
        );
        let other = tolerate_unsupported(Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert_eq!(
            other.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "a real error must still propagate unchanged"
        );
    }

    #[test]
    fn build_request_has_jsonrpc_envelope() {
        let req = build_request(7, "surface.list", json!({}));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "surface.list");
        assert_eq!(req["params"], json!({}));
    }

    #[test]
    fn parse_response_extracts_result() {
        let line = r#"{"jsonrpc":"2.0","result":{"surfaces":[]},"id":1}"#;
        let result = parse_response(line).expect("ok");
        assert_eq!(result, json!({"surfaces": []}));
    }

    #[test]
    fn parse_response_translates_error_envelope() {
        let line = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"surface_id 9 not found"},"id":1}"#;
        let err = parse_response(line).expect_err("err");
        assert!(err.contains("-32602"), "got: {err}");
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn jsonrpc_error_message_detects_stream_error_line() {
        let line = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"bad filter"},"id":null}"#;
        let err = jsonrpc_error_message(line).expect("error");
        assert!(err.contains("-32602"), "got: {err}");
        assert!(err.contains("bad filter"), "got: {err}");
        assert!(jsonrpc_error_message(r#"{"type":"subscribed"}"#).is_none());
    }

    #[test]
    fn parse_response_rejects_missing_result_and_error() {
        let line = r#"{"jsonrpc":"2.0","id":1}"#;
        assert!(parse_response(line).is_err());
    }

    #[test]
    fn parse_response_rejects_malformed_json() {
        assert!(parse_response("not json").is_err());
    }

    #[test]
    fn parse_response_rejects_frame_without_jsonrpc_version() {
        // A bare `{"result": ...}` is not a JSON-RPC 2.0 response and must not
        // be accepted as a success.
        assert!(parse_response(r#"{"result":{},"id":1}"#).is_err());
        assert!(parse_response(r#"{"jsonrpc":"1.0","result":{},"id":1}"#).is_err());
    }

    #[test]
    fn parse_response_rejects_malformed_error_member() {
        // `error` must be `{code: i64, message: string}`; a missing code must
        // not be reported as "paneflow error 0".
        for line in [
            r#"{"jsonrpc":"2.0","error":{},"id":1}"#,
            r#"{"jsonrpc":"2.0","error":{"message":"no code"},"id":1}"#,
            r#"{"jsonrpc":"2.0","error":{"code":-32602},"id":1}"#,
            r#"{"jsonrpc":"2.0","error":{"code":"-32602","message":"x"},"id":1}"#,
            r#"{"jsonrpc":"2.0","error":"boom","id":1}"#,
        ] {
            let err = parse_response(line).expect_err("malformed error must fail parse");
            assert!(
                !err.contains("error 0:"),
                "synthesized code 0 for {line}: {err}"
            );
            assert!(
                !err.contains("unknown error"),
                "synthesized message for {line}: {err}"
            );
        }
        assert!(jsonrpc_error_message(r#"{"error":{},"id":null}"#)
            .is_some_and(|e| !e.contains("error 0:")));
    }

    #[test]
    fn parse_response_keeps_real_error_code_and_data() {
        let line = r#"{"jsonrpc":"2.0","error":{"code":-32001,"message":"nope","data":{"surface_id":9}},"id":1}"#;
        let err = parse_response(line).expect_err("err");
        assert!(err.contains("-32001"), "got: {err}");
        assert!(err.contains("nope"), "got: {err}");
        assert!(err.contains("surface_id"), "error.data dropped: {err}");
    }

    #[test]
    fn capped_event_line_rejects_oversized_unterminated_frame() {
        let data = vec![b'x'; MAX_RESPONSE_LEN as usize];
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut buf = Vec::new();
        let err = read_capped_event_line(&mut reader, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn capped_event_line_reads_one_frame() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"{\"type\":\"ai.stop\"}\nrest"));
        let mut buf = Vec::new();
        let line = read_capped_event_line(&mut reader, &mut buf)
            .expect("read")
            .expect("line");
        assert_eq!(line, "{\"type\":\"ai.stop\"}");
    }

    #[test]
    fn socket_path_from_env_requires_absolute() {
        // Absolute Unix domain-socket path. Relative paths are rejected.
        let absolute = "/run/user/1000/paneflow/paneflow.sock";
        assert_eq!(
            socket_path_from_env(Some(std::ffi::OsString::from(absolute))),
            Some(PathBuf::from(absolute))
        );
        assert_eq!(
            socket_path_from_env(Some(std::ffi::OsString::from("relative/path.sock"))),
            None
        );
        assert_eq!(
            socket_path_from_env(Some(std::ffi::OsString::from(""))),
            None
        );
        assert_eq!(socket_path_from_env(None), None);
    }

    /// US-005 AC: a full request/response round-trip over a real local socket
    /// (not just the pure helpers). Spins up an `interprocess` listener that
    /// speaks the Paneflow framing - read one newline-delimited request, echo
    /// its `id` back in a JSON-RPC `result` envelope. The test path is a
    /// filesystem Unix socket.
    #[cfg(unix)]
    #[test]
    fn ipc_client_round_trips_against_a_live_socket() {
        use interprocess::local_socket::{Listener, ListenerOptions};
        use interprocess::TryClone;

        // Serialised against the $TMPDIR-mutating tests below:
        // `TempDir::new()` reads $TMPDIR, and
        // `paneflow_socket_path_env_wins_when_absolute` briefly points it
        // at a path that does not exist. Reading it without the lock is a
        // race that fails this test with a spurious NotFound.
        let _env = SocketEnvGuard::take();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow-test.sock");
        let name = path.as_path().to_fs_name::<GenericFilePath>().unwrap();
        let listener: Listener = ListenerOptions::new().name(name).create_sync().unwrap();

        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("accept");
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: Value = serde_json::from_str(line.trim()).expect("parse request");
            // Echo the client's id back, mirroring the real server contract.
            let response = json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {"surfaces": [{"surface_id": 1u64, "name": "cargo-run"}]},
            });
            let mut serialized = serde_json::to_string(&response).unwrap();
            serialized.push('\n');
            writer
                .write_all(serialized.as_bytes())
                .expect("write response");
            writer.flush().expect("flush");
        });

        let client = IpcClient::new(path);
        let result = client.call("surface.list", json!({})).expect("call ok");
        assert_eq!(result["surfaces"][0]["name"], "cargo-run");

        server.join().expect("server thread");
    }

    #[test]
    fn call_retries_busy_and_timeout() {
        let request = build_request(1, "surface.read", json!({}));
        let mut attempts = 0;
        let result = call_with_sender(
            Path::new("/unused-test-socket"),
            "surface.read",
            &request,
            |_, _, _| {
                attempts += 1;
                match attempts {
                    1 => Ok(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": {"code": -32000, "message": "PaneFlow is busy; retry shortly"},
                    })
                    .to_string()),
                    2 => Err(io::Error::from(io::ErrorKind::TimedOut)),
                    _ => Ok(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"retried": true},
                    })
                    .to_string()),
                }
            },
        )
        .expect("busy response and timed-out read should both retry");
        assert_eq!(result, json!({"retried": true}));
        assert_eq!(attempts, 3);
    }

    #[test]
    fn call_does_not_retry_timeout_for_mutation_or_shutdown_response() {
        let mutation_request = build_request(1, "surface.send_text", json!({}));
        let mut timeout_attempts = 0;
        let timeout = call_with_sender(
            Path::new("/unused-test-socket"),
            "surface.send_text",
            &mutation_request,
            |_, _, _| {
                timeout_attempts += 1;
                Err(io::Error::from(io::ErrorKind::TimedOut))
            },
        )
        .expect_err("mutating timeout must not retry");
        assert_eq!(timeout_attempts, 1);
        assert!(timeout.contains("timed out"), "got: {timeout}");
        assert!(!timeout.contains("unreachable"), "got: {timeout}");

        let read_request = build_request(2, "surface.read", json!({}));
        let mut shutdown_attempts = 0;
        let shutdown = call_with_sender(
            Path::new("/unused-test-socket"),
            "surface.read",
            &read_request,
            |_, _, _| {
                shutdown_attempts += 1;
                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "error": {"code": -32000, "message": "App shutting down"},
                })
                .to_string())
            },
        )
        .expect_err("shutdown response must not retry");
        assert_eq!(shutdown_attempts, 1);
        assert!(shutdown.contains("App shutting down"), "got: {shutdown}");
    }

    #[cfg(unix)]
    #[test]
    fn ipc_client_call_errors_when_socket_missing() {
        // Serialised against the $TMPDIR-mutating tests below:
        // `TempDir::new()` reads $TMPDIR, and
        // `paneflow_socket_path_env_wins_when_absolute` briefly points it
        // at a path that does not exist. Reading it without the lock is a
        // race that fails this test with a spurious NotFound.
        let _env = SocketEnvGuard::take();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.sock");
        let client = IpcClient::new(path);
        let err = client
            .call("surface.list", json!({}))
            .expect_err("must fail with no listener");
        assert!(err.contains("unreachable"), "got: {err}");
    }

    /// A listener that never `accept()`s will fill its backlog; the next
    /// `connect` must not wait unbounded. Darwin AF_UNIX typically returns
    /// `ConnectionRefused` once the queue is full; other kernels may surface
    /// `TimedOut` via `ConnectWaitMode::Timeout`. Either is a bounded failure.
    #[cfg(unix)]
    #[test]
    fn connect_stream_with_timeout_returns_before_deadline_when_listener_never_accepts() {
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Instant;

        // Serialised against the $TMPDIR-mutating tests below:
        // `TempDir::new()` reads $TMPDIR, and
        // `paneflow_socket_path_env_wins_when_absolute` briefly points it
        // at a path that does not exist. Reading it without the lock is a
        // race that fails this test with a spurious NotFound.
        let _env = SocketEnvGuard::take();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("never-accept.sock");
        let listener = UnixListener::bind(&path).unwrap();
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
            let result = connect_stream_with_timeout(&path_for_thread, timeout);
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

    /// Env vars are process-global; tests that mutate them must be serialised.
    #[cfg(unix)]
    static SOCKET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    struct SocketEnvGuard {
        socket: Option<String>,
        xdg: Option<String>,
        tmp: Option<String>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl SocketEnvGuard {
        fn take() -> Self {
            let guard = SOCKET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            Self {
                socket: std::env::var("PANEFLOW_SOCKET_PATH").ok(),
                xdg: std::env::var("XDG_RUNTIME_DIR").ok(),
                tmp: std::env::var("TMPDIR").ok(),
                _guard: guard,
            }
        }

        fn clear(&self) {
            // SAFETY: serialised by SOCKET_ENV_LOCK. Every test that
            // reads or writes these vars must hold that lock - reads
            // count, because `tempfile::TempDir::new()` consults
            // $TMPDIR and would observe a half-applied window.
            unsafe {
                std::env::remove_var("PANEFLOW_SOCKET_PATH");
                std::env::remove_var("XDG_RUNTIME_DIR");
                std::env::remove_var("TMPDIR");
            }
        }
    }

    #[cfg(unix)]
    impl Drop for SocketEnvGuard {
        fn drop(&mut self) {
            // SAFETY: serialised by SOCKET_ENV_LOCK (still held via _guard).
            unsafe {
                match &self.socket {
                    Some(v) => std::env::set_var("PANEFLOW_SOCKET_PATH", v),
                    None => std::env::remove_var("PANEFLOW_SOCKET_PATH"),
                }
                match &self.xdg {
                    Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                    None => std::env::remove_var("XDG_RUNTIME_DIR"),
                }
                match &self.tmp {
                    Some(v) => std::env::set_var("TMPDIR", v),
                    None => std::env::remove_var("TMPDIR"),
                }
            }
        }
    }

    #[cfg(unix)]
    fn expected_socket_under(runtime: &Path) -> PathBuf {
        let subdir = if cfg!(debug_assertions) {
            "paneflow-dev"
        } else {
            "paneflow"
        };
        let socket_file = if cfg!(debug_assertions) {
            "paneflow-dev.sock"
        } else {
            "paneflow.sock"
        };
        runtime.join(subdir).join(socket_file)
    }

    #[cfg(unix)]
    #[test]
    fn paneflow_socket_path_env_wins_when_absolute() {
        let g = SocketEnvGuard::take();
        g.clear();
        // SAFETY: SOCKET_ENV_LOCK held.
        unsafe {
            std::env::set_var("PANEFLOW_SOCKET_PATH", "/tmp/paneflow-isolated.sock");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
            std::env::set_var("TMPDIR", "/tmp/macos-stub");
        }
        assert_eq!(
            resolve_socket_path(),
            Some(PathBuf::from("/tmp/paneflow-isolated.sock"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_runtime_dir_is_ignored_when_tmpdir_is_set() {
        let g = SocketEnvGuard::take();
        g.clear();
        let xdg = tempfile::TempDir::new().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: SOCKET_ENV_LOCK held.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", xdg.path());
            std::env::set_var("TMPDIR", tmp.path());
        }
        let p = resolve_socket_path().expect("runtime dir must resolve");
        assert_eq!(p, expected_socket_under(tmp.path()));
        assert!(
            !p.starts_with(xdg.path()),
            "must not compose the socket under $XDG_RUNTIME_DIR; got {}",
            p.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_runtime_dir_is_not_used_when_tmpdir_is_unset() {
        let g = SocketEnvGuard::take();
        g.clear();
        let xdg = tempfile::TempDir::new().unwrap();
        // SAFETY: SOCKET_ENV_LOCK held.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", xdg.path()) };
        let p = resolve_socket_path().expect("cache dir must resolve");
        assert!(
            !p.starts_with(xdg.path()),
            "must not fall back to $XDG_RUNTIME_DIR; got {}",
            p.display()
        );
        let cache_suffix = expected_socket_under(Path::new("Library/Caches/run"));
        assert!(
            p.ends_with(&cache_suffix),
            "last resort is ~/Library/Caches/run; got {}",
            p.display()
        );
    }

    /// Build a value that is valid on-disk bytes but not UTF-8, to pin the
    /// server-parity acceptance rules.
    #[cfg(unix)]
    fn non_utf8_os(bytes: &[u8]) -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        let value = std::ffi::OsString::from_vec(bytes.to_vec());
        assert!(value.to_str().is_none(), "fixture must be non-UTF-8");
        value
    }

    /// Env lookup over a fixed `OsString` table, mirroring the server's test
    /// seam in `src-app/src/runtime_paths.rs`; anything not listed reads as
    /// unset. Never touches process-global env, so it needs no lock.
    #[cfg(unix)]
    fn fake_env(
        vars: Vec<(&'static str, std::ffi::OsString)>,
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_socket_path_override_is_honoured_like_the_server() {
        let raw = non_utf8_os(b"/tmp/paneflow-\xff.sock");
        let env = fake_env(vec![
            ("PANEFLOW_SOCKET_PATH", raw.clone()),
            ("TMPDIR", std::ffi::OsString::from("/tmp/macos-stub")),
        ]);
        assert_eq!(
            resolve_socket_path_from(&env),
            Some(PathBuf::from(raw)),
            "the server reads the override as OsString (runtime_paths.rs:socket_path_from_env); \
             a non-UTF-8 value must be honoured, not silently ignored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overlong_default_path_returns_none_like_the_server() {
        // A TMPDIR of at least 120 bytes → joined path blows past 104. It
        // has to exist on disk, or the resolver reads it as unset and falls
        // through to the cache dir instead of hitting the ceiling.
        let _env = SocketEnvGuard::take();
        let tmp = tempfile::TempDir::new().unwrap();
        let long_dir = tmp.path().join("x".repeat(120));
        std::fs::create_dir(&long_dir).unwrap();
        assert!(long_dir.as_os_str().len() >= 120);
        let env = fake_env(vec![("TMPDIR", long_dir.into_os_string())]);
        assert!(
            resolve_socket_path_from(&env).is_none(),
            "an over-long sun_path must yield no endpoint, matching the server's refusal to bind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sun_path_ceiling_is_exclusive_like_the_server() {
        // `bind()` needs the trailing NUL inside `sun_path`, so a path of
        // exactly `MAX_SOCKET_PATH_BYTES` does not fit; one byte shorter does.
        let at_limit = "/".to_string() + &"x".repeat(MAX_SOCKET_PATH_BYTES - 1);
        assert_eq!(at_limit.len(), MAX_SOCKET_PATH_BYTES);
        let env = fake_env(vec![
            ("PANEFLOW_SOCKET_PATH", std::ffi::OsString::from(&at_limit)),
            ("TMPDIR", std::ffi::OsString::from("/tmp/macos-stub")),
        ]);
        assert!(
            resolve_socket_path_from(&env).is_none(),
            "an absolute but over-long override must yield None (not fall through to the \
             default): the server refuses to bind it"
        );

        let under_limit = "/".to_string() + &"x".repeat(MAX_SOCKET_PATH_BYTES - 2);
        assert_eq!(under_limit.len(), MAX_SOCKET_PATH_BYTES - 1);
        let env = fake_env(vec![(
            "PANEFLOW_SOCKET_PATH",
            std::ffi::OsString::from(&under_limit),
        )]);
        assert_eq!(
            resolve_socket_path_from(&env),
            Some(PathBuf::from(&under_limit))
        );
    }

    /// Production wiring: `resolve_socket_path()` itself must read the
    /// override through `var_os`, not UTF-8 `var` (#217 - the reported
    /// defect was at the `std::env::var` call site).
    #[cfg(unix)]
    #[test]
    fn resolve_socket_path_reads_the_override_as_os_string() {
        let g = SocketEnvGuard::take();
        g.clear();
        let raw = non_utf8_os(b"/tmp/paneflow-\xff-live.sock");
        // SAFETY: SOCKET_ENV_LOCK held.
        unsafe { std::env::set_var("PANEFLOW_SOCKET_PATH", &raw) };
        assert_eq!(resolve_socket_path(), Some(PathBuf::from(raw)));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_tmpdir_falls_back_to_cache_dir_like_the_server() {
        let g = SocketEnvGuard::take();
        g.clear();
        // SAFETY: SOCKET_ENV_LOCK held.
        unsafe { std::env::set_var("TMPDIR", non_utf8_os(b"/tmp/bad-\xff-dir")) };
        let p = resolve_socket_path().expect("cache fallback must resolve");
        let cache_suffix = expected_socket_under(Path::new("Library/Caches/run"));
        assert!(
            p.ends_with(&cache_suffix),
            "a non-UTF-8 $TMPDIR must read as unset, falling back to \
             ~/Library/Caches/run like the server; got {}",
            p.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn tmpdir_used_when_set() {
        let g = SocketEnvGuard::take();
        g.clear();
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: SOCKET_ENV_LOCK held.
        unsafe { std::env::set_var("TMPDIR", tmp.path()) };
        assert_eq!(
            resolve_socket_path(),
            Some(expected_socket_under(tmp.path()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_tmpdir_falls_back_to_cache_dir_like_the_server() {
        // A stale $TMPDIR naming a path that no longer exists reads as unset
        // on the server (`runtime_paths::runtime_dir_from`, #289), so the
        // client must fall through to the same cache-dir endpoint or it
        // dials a socket the server never bound.
        let _env = SocketEnvGuard::take();
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("gone");
        assert!(!missing.exists(), "fixture must not exist");
        let env = fake_env(vec![
            ("TMPDIR", missing.clone().into_os_string()),
            ("HOME", std::ffi::OsString::from("/Users/stub")),
        ]);
        let p = resolve_socket_path_from(&env).expect("cache fallback must resolve");
        assert_eq!(
            p,
            expected_socket_under(Path::new("/Users/stub/Library/Caches/run")),
            "a $TMPDIR that is not an existing directory is treated as unset"
        );
    }
}
