//! Cross-platform AI-hook wiring.
//!
//! US-008 - binary extraction & cache-dir layout.
//!   `extract::ensure_binaries_extracted` materializes the embedded
//!   `paneflow-shim` and `paneflow-ai-hook` binaries into
//!   `<cache_dir>/paneflow/bin/<version>/` with atomic rename + `chmod
//!   0o755` on Unix. The shim is written twice (as `claude` and `codex`)
//!   so the PTY's `$PATH`-prepend in US-009 resolves both tool names to
//!   the same underlying shim.
//!
//! `claude_hooks` - `paneflow hooks setup|status|uninstall`.
//!   Writes the persistent user-scope Claude hooks into
//!   `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`),
//!   pointing at the stable ai-hook copy `extract::ensure_ai_hook_extracted`
//!   materializes under `data_dir()`.
//!
//! Future stories (not in scope for US-008):
//! - US-009 - PATH-prepend in `pty_session` using this extraction path.
//! - US-011 - end-to-end integration tests over the whole pipeline.

pub mod claude_hooks;
pub mod extract;

use std::io::Write;
use std::path::Path;

/// Opt-in cross-process diagnostic for the sidebar-status hook chain.
///
/// Appends one line to the file named by `$PANEFLOW_HOOK_LOG` when that env
/// var names an absolute regular file; a silent no-op otherwise. This is the
/// SAME env var honoured by the `paneflow-shim` and `paneflow-ai-hook`
/// binaries, so the whole pipeline - app (PTY env + IPC server) → shell →
/// shim → agent → ai-hook - appends to one file. That lets a user trace
/// exactly where the chain breaks (e.g. shim never runs vs. hooks never
/// install vs. frame never reaches the server) from a single reproduction.
///
/// To capture: set an absolute `PANEFLOW_HOOK_LOG` (e.g.
/// `export PANEFLOW_HOOK_LOG="$HOME/paneflow-hooks.log"`), launch PaneFlow
/// from that same shell so it inherits the var, run an agent, then share the
/// file. Never panics - diagnostics must never break a PTY spawn.
pub(crate) fn hook_diag(msg: &str) {
    hook_diag_to(
        msg,
        std::env::var_os("PANEFLOW_HOOK_LOG")
            .as_deref()
            .map(Path::new),
    );
}

/// Append one diagnostic line when `log_path` is an absolute regular file.
///
/// Same writer contract as the shim and ai-hook helpers (#1027, #1028): a
/// relative path is never opened, since it would resolve against the app's
/// cwd. The final component is opened itself: `O_NOFOLLOW` fails the open
/// with ELOOP on a symlink, and `O_NONBLOCK` keeps a FIFO from stalling the
/// IPC connection thread before the hook frame is dispatched (#1058). The
/// line is written only when the opened descriptor is a regular file.
fn hook_diag_to(msg: &str, log_path: Option<&Path>) {
    use std::os::unix::fs::OpenOptionsExt;

    let Some(log_path) = log_path else {
        return;
    };
    if log_path.as_os_str().is_empty() || !log_path.is_absolute() {
        return;
    }
    let mut file = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(log_path)
    {
        Ok(file) => file,
        Err(_) => return,
    };
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return;
    }
    // Format the whole line first and emit it in ONE `write_all`: multiple
    // processes (app, shim, ai-hook ×N) append to this file concurrently, and
    // a single atomic append keeps lines from interleaving/dropping (a
    // per-argument `writeln!` issues several syscalls and tears under
    // concurrency).
    let line = format!("paneflow-app[{}]: {msg}\n", std::process::id());
    let _ = file.write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::hook_diag_to;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn hook_diag_appends_a_line_to_an_absolute_regular_log() {
        let directory = tempfile::TempDir::new().expect("temp directory");
        let path = directory.path().join("hook.log");
        hook_diag_to("first", Some(&path));
        hook_diag_to("second", Some(&path));
        let pid = std::process::id();
        assert_eq!(
            std::fs::read_to_string(&path).expect("read hook log"),
            format!("paneflow-app[{pid}]: first\npaneflow-app[{pid}]: second\n")
        );
    }

    #[test]
    fn hook_diag_ignores_a_relative_log_path() {
        let relative = PathBuf::from(format!(
            "paneflow-app-hook-diag-relative-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&relative);
        hook_diag_to("ignored", Some(&relative));
        let created = relative.exists();
        let _ = std::fs::remove_file(&relative);
        assert!(
            !created,
            "relative PANEFLOW_HOOK_LOG must not be opened against the cwd"
        );
    }

    #[test]
    fn hook_diag_does_not_follow_a_symlink() {
        let directory = tempfile::TempDir::new().expect("temp directory");
        let target = directory.path().join("target.log");
        std::fs::write(&target, "untouched").expect("create target");
        let link = directory.path().join("hook.log");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        hook_diag_to("ignored", Some(&link));
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "untouched",
            "diagnostics must not follow a symlinked PANEFLOW_HOOK_LOG"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("symlink metadata")
                .file_type()
                .is_symlink(),
            "diagnostics must not replace a symlinked PANEFLOW_HOOK_LOG"
        );
    }

    fn make_fifo(path: &Path) {
        let c_path = std::ffi::CString::new(path.to_str().expect("utf-8 path")).expect("cstring");
        // SAFETY: `c_path` is a valid NUL-terminated path that lives for the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    }

    /// Open the FIFO's read end without blocking, which both gives a writer a
    /// peer and releases a writer already blocked in `open(2)`.
    fn open_fifo_reader(path: &Path) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .expect("open FIFO read end")
    }

    /// Run `hook_diag_to` on a worker so a blocking open fails the test on a
    /// deadline instead of hanging the suite.
    fn hook_diag_returns_promptly(path: &Path) -> bool {
        let (done_tx, done_rx) = mpsc::channel();
        let worker_path = path.to_path_buf();
        let worker = std::thread::spawn(move || {
            hook_diag_to("ignored", Some(&worker_path));
            let _ = done_tx.send(());
        });
        let prompt = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        if !prompt {
            // Unblock the stuck writer so the worker can be joined.
            let _reader = open_fifo_reader(path);
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
        }
        let _ = worker.join();
        prompt
    }

    #[test]
    fn hook_diag_returns_promptly_on_a_fifo_without_a_reader() {
        let directory = tempfile::TempDir::new().expect("temp directory");
        let fifo = directory.path().join("hook.log");
        make_fifo(&fifo);
        assert!(
            hook_diag_returns_promptly(&fifo),
            "a reader-less FIFO at PANEFLOW_HOOK_LOG must not block the writer"
        );
    }

    #[test]
    fn hook_diag_writes_nothing_to_a_fifo_with_a_reader() {
        use std::io::Read;

        let directory = tempfile::TempDir::new().expect("temp directory");
        let fifo = directory.path().join("hook.log");
        make_fifo(&fifo);
        let mut reader = open_fifo_reader(&fifo);
        assert!(
            hook_diag_returns_promptly(&fifo),
            "a FIFO at PANEFLOW_HOOK_LOG must not block the writer"
        );
        let mut received = Vec::new();
        // Non-blocking read end: nothing written reads as EOF or EAGAIN.
        let _ = reader.read_to_end(&mut received);
        assert!(
            received.is_empty(),
            "only a regular file may receive diagnostics, got {:?}",
            String::from_utf8_lossy(&received)
        );
    }
}
