//! Inherited agent-session env scrub helpers (7 marker names).
//!
//! The ACP agent-spawn machinery (`spawn_acp_agent`, wire tracing,
//! secret redaction) was removed with the in-app chat; only this env
//! scrub survives, called once from `main()` before any thread spawns.
//! It strips `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`,
//! `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_ENTRYPOINT`,
//! `CLAUDE_CODE_EXECPATH`, `CLAUDE_CODE_MESSAGING_SOCKET`, and
//! `CLAUDE_CODE_MESSAGING_TOKEN`.

/// Identity and credential markers Claude Code exports into the processes it
/// spawns. Single source for the process-env ACP scrub and the PTY overlay
/// strip in `pty_session`.
///
/// `CLAUDECODE` is the original refusal ("cannot launch inside another
/// Claude Code session"). The rest are session identity / IPC credentials
/// a pane must never inherit; `assemble_pty_env` only overlays, so this
/// process-env scrub is the half that actually unsets them.
pub const INHERITED_AGENT_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// Remove inherited agent-session markers from the current process
/// environment so future subprocesses (which inherit it by default) do not
/// see them.
///
/// US-011 (cli-hardening-followup-2026-Q3): this helper MUST be
/// called from the very first lines of `main()`, before any
/// `std::thread::spawn`, `tokio::runtime::Builder::build`, or smol
/// executor initialization. Rust 1.85 made `std::env::remove_var`
/// `unsafe` because it races with concurrent `getenv` from any
/// other thread; the runtime sub-systems above all read env on
/// startup, so calling this before any thread exists is genuinely safe
/// by construction.
/// # Safety
///
/// Must be called before any other thread, async runtime, or foreign library
/// can concurrently read environment variables. Prefer
/// [`scrub_claudecode_from_command`] for per-child scrubbing after startup.
pub unsafe fn scrub_claudecode_env() {
    // SAFETY: called from main() before any thread::spawn or async
    // runtime init (US-011) -- no concurrent getenv possible.
    unsafe {
        for key in INHERITED_AGENT_SESSION_ENV {
            std::env::remove_var(key);
        }
    }
}

/// Remove inherited agent-session markers from one child command without
/// mutating global process environment.
pub fn scrub_claudecode_from_command(command: &mut std::process::Command) {
    for key in INHERITED_AGENT_SESSION_ENV {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hardcoded independently of `INHERITED_AGENT_SESSION_ENV` so shrinking
    // the production slice fails these tests instead of vacuously passing.
    const MARKERS: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
    ];

    #[test]
    fn scrub_claudecode_is_idempotent() {
        // SAFETY: test-only -- single-threaded test runner step. Sets, scrubs,
        // and re-scrubs to confirm the second call does not panic.
        unsafe {
            for key in MARKERS {
                std::env::set_var(key, "1");
            }
        }
        // SAFETY: this test holds no extra threads and only exercises these env vars.
        unsafe { scrub_claudecode_env() };
        for key in MARKERS {
            assert!(
                std::env::var(key).is_err(),
                "{key} must be scrubbed from the process env"
            );
        }
        // SAFETY: same as above.
        unsafe { scrub_claudecode_env() };
        for key in MARKERS {
            assert!(
                std::env::var(key).is_err(),
                "{key} must stay absent after a second scrub"
            );
        }
    }

    #[test]
    fn scrub_claudecode_from_command_is_local_to_child() {
        let mut command = std::process::Command::new("noop");
        for key in MARKERS {
            command.env(*key, "1");
        }
        scrub_claudecode_from_command(&mut command);
        for key in MARKERS {
            assert!(
                command
                    .get_envs()
                    .any(|(k, value)| k == *key && value.is_none()),
                "child command should explicitly remove {key}"
            );
        }
    }
}
