use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use interprocess::local_socket::{prelude::*, ConnectOptions, GenericFilePath, Stream};
use interprocess::ConnectWaitMode;
use paneflow_ipc_client::ai_hook::{AiHookFrame, MAX_FRAME_BYTES};

/// Per-attempt wall-clock deadline, applied to BOTH the connect and the write.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// Backoffs between whole-delivery attempts (total attempts = 1 + len).
/// Retrying the connect ALONE loses the case this budget exists for: a write
/// that times out because the PaneFlow main thread was busy past the 500 ms
/// deadline at the moment the frame arrived. A dropped `ai.stop` leaves the
/// sidebar spinner on "thinking…" until the 300 s Stalled sweep, so the retry
/// must cover connect and write alike - the distinction is not observable
/// from this side of the socket and the budget is bounded either way.
const SEND_BACKOFF: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(300)];

pub(crate) fn send_frame(socket_path: &Path, frame: &AiHookFrame) -> io::Result<()> {
    let payload = serialize_frame(frame)?; // once: cap check is deterministic
    let mut result = send_once(socket_path, &payload);
    for backoff in SEND_BACKOFF {
        if result.is_ok() {
            return result;
        }
        std::thread::sleep(backoff);
        result = send_once(socket_path, &payload);
    }
    result
}

/// One bounded delivery attempt: bounded connect, then bounded write.
fn send_once(socket_path: &Path, payload: &[u8]) -> io::Result<()> {
    let mut stream = connect_once(socket_path)?;
    if let Err(error) = stream.set_send_timeout(Some(WRITE_TIMEOUT)) {
        if error.kind() != io::ErrorKind::Unsupported {
            return Err(error);
        }
    }
    stream.write_all(payload)?;
    stream.flush()
}

/// Connect with the same wall-clock deadline later applied to the write.
/// `Stream::connect` waits unbounded when the listen queue is full (#26).
fn connect_once(socket_path: &Path) -> io::Result<Stream> {
    let name = socket_path.to_fs_name::<GenericFilePath>()?;
    ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(WRITE_TIMEOUT))
        .connect_sync()
}

fn serialize_frame(frame: &AiHookFrame) -> io::Result<Vec<u8>> {
    let mut payload = serde_json::to_vec(&frame.to_value())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    payload.push(b'\n');
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "paneflow hook IPC frame exceeds the server size cap",
        ));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use paneflow_ipc_client::ai_hook::{AiHookMethod, AiHookParams, AiToolName};
    use serde_json::json;

    fn frame(payload: serde_json::Value) -> AiHookFrame {
        AiHookFrame::new(
            AiHookMethod::Stop,
            AiHookParams::new(
                1,
                AiToolName::parse("claude").expect("valid test tool"),
                payload,
            ),
        )
    }

    #[test]
    fn oversized_frame_is_rejected_before_connecting() {
        let error = send_frame(
            Path::new("this-path-is-never-opened"),
            &frame(json!({"message": "x".repeat(MAX_FRAME_BYTES)})),
        )
        .expect_err("oversized frame");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn frame_is_written_once_as_newline_delimited_json() {
        use std::io::{BufRead, BufReader};

        use interprocess::local_socket::{Listener, ListenerOptions};

        let directory = tempfile::TempDir::new().expect("temp directory");
        let path = directory.path().join("hook.sock");
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("socket name");
        let listener: Listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("listener");
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("read frame");
            line
        });

        send_frame(&path, &frame(json!({"session_id": "s1"}))).expect("send frame");
        let line = server.join().expect("server thread");
        assert!(line.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(value["method"], "ai.stop");
    }

    #[cfg(unix)]
    #[test]
    fn connection_is_retried_before_any_write() {
        use std::io::{BufRead, BufReader};

        use interprocess::local_socket::{Listener, ListenerOptions};

        let directory = tempfile::TempDir::new().expect("temp directory");
        let path = directory.path().join("late.sock");
        let server_path = path.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let name = server_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("socket name");
            let listener: Listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("listener");
            let stream = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("read frame");
            line
        });

        send_frame(&path, &frame(json!({}))).expect("connect retry succeeds");
        assert!(server.join().expect("server thread").ends_with('\n'));
    }

    /// Bounded-return smoke test: a listener that never `accept()`s will fill
    /// its backlog; connect must not wait unbounded. On Darwin this passes
    /// even with the timeout removed (backlog-full ⇒ immediate ECONNREFUSED).
    /// The `ConnectWaitMode::Timeout` wiring is proven by the next test.
    #[cfg(unix)]
    #[test]
    fn connect_stream_with_timeout_returns_before_deadline_when_listener_never_accepts() {
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Instant;

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

        let path_for_thread = path.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let start = Instant::now();
            let result = connect_once(&path_for_thread);
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

    /// proves wiring, not behaviour
    #[test]
    fn bounded_connect_is_wired_into_connect_once() {
        let src = include_str!("transport.rs");
        let start = src
            .find("fn connect_once")
            .expect("fn connect_once must exist");
        let body = &src[start..];
        let end = body
            .find("\n}")
            .expect("fn connect_once must have a closing brace");
        let body = &body[..=end];
        assert!(
            body.contains("ConnectWaitMode::Timeout(WRITE_TIMEOUT)"),
            "connect_once must apply a ConnectWaitMode timeout equal to WRITE_TIMEOUT"
        );
        assert!(
            !body.contains("Stream::connect("),
            "connect_once must not call unbounded Stream::connect"
        );
    }

    /// Server drops the first accepted stream before reading. A small frame
    /// can complete into the kernel send buffer and never surface EPIPE, so
    /// the blob is `MAX_FRAME_BYTES - 1024` (just under the 256 KiB frame cap)
    /// to exceed Darwin's default `net.local.stream.sendspace` (8 KiB) even if
    /// that sysctl is raised. The first write blocks / resets, and
    /// whole-delivery retry reconnects. A connect-only retry treats the first
    /// connect as success and never retries.
    #[cfg(unix)]
    #[test]
    fn write_error_is_retried_not_just_the_connect() {
        use std::io::{BufRead, BufReader};
        use std::sync::mpsc;

        use interprocess::local_socket::{Listener, ListenerOptions};

        let directory = tempfile::TempDir::new().expect("temp directory");
        let path = directory.path().join("drop-first.sock");
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("socket name");
        let listener: Listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("listener");
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let first = listener.accept().expect("accept first");
            drop(first);
            let stream = listener.accept().expect("accept second");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("read frame");
            let _ = tx.send(line);
        });

        let payload = json!({"session_id": "s1", "blob": "x".repeat(MAX_FRAME_BYTES - 1024)});
        send_frame(&path, &frame(payload)).expect("write retry succeeds");
        let line = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("second attempt delivers a frame");
        assert!(line.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(value["method"], "ai.stop");
        server.join().expect("server thread");
    }
}
