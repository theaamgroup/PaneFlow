#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic
    )
)]
//! PaneFlow AI-binary shim.
//!
//! Copied (by US-008 extraction) under every `TerminalAgent` binary name
//! (`claude`, `codex`, `gemini`, `cursor-agent`, …) into the PaneFlow bin
//! cache dir, which US-009 prepends to the PTY's `$PATH`. When the user
//! runs one of those tools, this shim:
//!
//! 1. Reads its own filename via `current_exe()` to decide which tool to
//!    front for (`detect_tool`).
//! 2. PATH-walks `$PATH`, **excluding its own directory**, to locate the
//!    real AI binary (`find_real_binary`). Self-exclusion prevents an
//!    infinite exec-loop when the shim dir is first on `$PATH`.
//! 3. Runs the real binary with argv and env passed through. Both
//!    platforms spawn + wait (`Command::status()`): US-005's drop-cleanup
//!    guards and EP-004's `ai.exit` exit-status report are incompatible
//!    with `exec()`-style process replacement. The exit code is
//!    propagated verbatim (shell `128+signum` convention for signals).
//!
//! US-004 scope: detect / find / exec only. Hook config injection
//! (`.claude/settings.local.json` via US-005; `.codex/hooks.json` via
//! US-006) and env-var injection (`$PANEFLOW_AI_TOOL` / `$PANEFLOW_AI_PID`
//! for US-003 consumption) are added in later stories by wrapping around
//! this skeleton.

use std::env;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const PANEFLOW_AI_EVENT_SOURCE_ENV: &str = "PANEFLOW_AI_EVENT_SOURCE";
const PANEFLOW_AI_EVENT_SOURCE_INTERRUPT: &str = "interrupt";

mod detect;
mod exec;
mod hooks;

use detect::{detect_tool, find_real_binary};
use exec::run_real;
use hooks::{
    merge_codebuddy_hooks, merge_cursor_hooks, merge_gemini_hooks, merge_qoder_hooks,
    remove_cursor_hooks, remove_gemini_hooks, remove_paneflow_hooks, remove_qoder_hooks,
    CodexHookConfigGuard, GrokHookFileGuard, HermesHookConfigGuard, HookConfigGuard, HookInstall,
    HookInstallSkip, ManagedHookConfigGuard, ManagedHookSpec, OpenCodePluginGuard,
    PiExtensionGuard,
};

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Opt-in diagnostic logging for the sidebar-status hook chain. Mirrors
/// `paneflow-ai-hook`'s `diagnose()`: appends one line to `$PANEFLOW_HOOK_LOG`
/// when set and non-empty, a silent no-op otherwise. Deliberately NOT stderr -
/// the shim sits in front of the agent's TUI and stderr noise would corrupt
/// it (and Claude Code surfaces hook stderr in its UI). The app, shim, agent,
/// and ai-hook all honour the same env var, so one file captures the whole
/// pipeline and shows exactly where the chain stops.
///
/// EP-002 US-004 (agent-control-plane-hardening): `pub(crate)` so the hook
/// installer (`hooks.rs`) can pinpoint WHICH `None` branch it took - the
/// top-level `install_hook_guard = None` line alone cannot tell a persistent-
/// hook skip from a filesystem refusal.
pub(crate) fn diagnose(msg: &str) {
    let Some(path) = env::var_os("PANEFLOW_HOOK_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    // One atomic append (whole line incl. newline) so concurrent writers
    // (app, shim, ai-hook) don't interleave or drop lines.
    let line = format!("paneflow-shim[{}]: {msg}\n", std::process::id());
    let _ = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn main() -> ExitCode {
    let Some(tool) = detect_tool() else {
        // Direct invocation (`./paneflow-shim`) or unexpected rename. Exit 2
        // matches `getopts` convention for "usage error" - the one case
        // where stderr output is acceptable because the user's command
        // cannot proceed regardless of PaneFlow state.
        eprintln!(
            "paneflow-shim: invoked under an unexpected name; copy or \
             hardlink this binary under one of the Paneflow-wrapped agent \
             CLI names ('claude', 'codex', 'gemini', …) and put that \
             directory first on $PATH."
        );
        return ExitCode::from(2);
    };

    let Some(real) = find_real_binary(tool) else {
        // Same rationale: stderr output is the bash convention for
        // "command not found" (exit 127). The user's invocation cannot
        // succeed, so silent fail would be worse than a clear message.
        eprintln!("paneflow-shim: could not find real '{tool}' on PATH after self-exclusion");
        return ExitCode::from(127);
    };

    // Install hook config guards before spawning the child, remove on drop.
    // The binding is held to end of `main` so destructors fire after
    // `run_real` returns; `None` is the graceful-degradation path for a
    // read-only FS / missing permissions (PRD C4) - and for every wrapped
    // tool with no hook integration yet (the shim still provides the
    // universal `ai.exit`/`ai.session_end` lifecycle below).
    let _hook_guard = match install_hook_guard(tool) {
        Ok(HookInstall::Installed(guard)) => {
            diagnose(&format!("install_hook_guard({tool}) = installed"));
            Some(guard)
        }
        Ok(HookInstall::Skipped(reason)) => {
            diagnose(&format!(
                "install_hook_guard({tool}) = skipped ({reason:?})"
            ));
            None
        }
        Err(error) => {
            diagnose(&format!("install_hook_guard({tool}) = failed ({error})"));
            None
        }
    };

    let args: Vec<OsString> = env::args_os().skip(1).collect();

    let (code, agent_exit) = run_real(tool, &real, &args);

    // EP-004 US-010: report the agent binary's REAL exit status. The shell's
    // ChildExit only carries the shell's exit; this is the one place that
    // knows the agent's. Emitted BEFORE `notify_session_end` - both wait on
    // the hook subprocess under a 2s deadline, so the server is guaranteed to
    // see `ai.exit` (which may set `Errored`) before `ai.session_end` (which
    // spares an `Errored` session instead of removing it). `None` (spawn or
    // wait failure) emits nothing - the server keeps today's behavior.
    let interrupted_exit = agent_exit.is_some_and(is_interrupt_exit_code);
    if let Some(exit_code) = agent_exit {
        notify_exit(tool, exit_code, interrupted_exit);
    }

    // The real AI binary has exited. Neither claude nor codex fires a
    // session-end hook event of their own, so the sidebar loader would
    // stick indefinitely if the user quit during a `Thinking` turn (no
    // `Stop` hook fired ⇒ no 5s auto-reset armed). Best-effort poke at
    // `paneflow-ai-hook SessionEnd` to send a single `ai.session_end`
    // IPC frame; the server clears `ai_state` to `Inactive`. Any failure
    // here is silent - the worst case is a stale loader, not a broken
    // shell.
    notify_session_end(tool, interrupted_exit);

    code
}

/// Per-tool hook-config installation. One guard variant per config FORMAT:
/// Claude Code keeps its dedicated guard (persistent-hooks precedence logic);
/// Codex keeps its TOML+JSON pair;
/// everything else rides [`ManagedHookConfigGuard`] parameterized by
/// location + merge/remove pair. Tools without a hook integration return
/// `None` - they still get the shim's universal exit/session-end lifecycle.
// Fields are never READ - they exist solely so the wrapped guard's `Drop`
// (hook-config cleanup) fires when `main` returns.
#[allow(dead_code)]
enum ToolHookGuard {
    Claude(HookConfigGuard),
    Codex(CodexHookConfigGuard),
    Managed(ManagedHookConfigGuard),
    Pi(PiExtensionGuard),
    OpenCode(OpenCodePluginGuard),
    Hermes(HermesHookConfigGuard),
    Grok(GrokHookFileGuard),
}

fn install_hook_guard(tool: &str) -> std::io::Result<HookInstall<ToolHookGuard>> {
    match tool {
        "claude" => HookConfigGuard::install().map(|outcome| outcome.map(ToolHookGuard::Claude)),
        "codex" => CodexHookConfigGuard::install().map(|outcome| outcome.map(ToolHookGuard::Codex)),
        // Claude-Code-compatible clones: same settings.local.json format,
        // project-local dir, different event coverage.
        "codebuddy" => ManagedHookConfigGuard::install_in_cwd(ManagedHookSpec::new(
            ".codebuddy",
            "settings.local.json",
            "CodeBuddy",
            merge_codebuddy_hooks,
            remove_paneflow_hooks,
        ))
        .map(|outcome| outcome.map(ToolHookGuard::Managed)),
        "qodercli" => ManagedHookConfigGuard::install_in_cwd(ManagedHookSpec::new(
            ".qoder",
            "settings.local.json",
            "Qoder",
            merge_qoder_hooks,
            remove_qoder_hooks,
        ))
        .map(|outcome| outcome.map(ToolHookGuard::Managed)),
        // User-scope JSON agents (their project files are primary configs,
        // often git-tracked - mutating those would churn the user's diff for
        // the whole session). Gemini is matcher-grouped; Cursor is flat.
        "gemini" => ManagedHookConfigGuard::install_in_home(ManagedHookSpec::new(
            ".gemini",
            "settings.json",
            "Gemini CLI",
            merge_gemini_hooks,
            remove_gemini_hooks,
        ))
        .map(|outcome| outcome.map(ToolHookGuard::Managed)),
        "cursor-agent" => ManagedHookConfigGuard::install_in_home(ManagedHookSpec::new(
            ".cursor",
            "hooks.json",
            "Cursor",
            merge_cursor_hooks,
            remove_cursor_hooks,
        ))
        .map(|outcome| outcome.map(ToolHookGuard::Managed)),
        // TypeScript-plugin agents: an embedded bridge file is materialized
        // (and, for OpenCode, declared in opencode.json) for the session.
        "pi" => PiExtensionGuard::install().map(|outcome| outcome.map(ToolHookGuard::Pi)),
        "opencode" => {
            OpenCodePluginGuard::install().map(|outcome| outcome.map(ToolHookGuard::OpenCode))
        }
        // YAML config, string-level marked block (comment-preserving).
        "hermes" => {
            HermesHookConfigGuard::install().map(|outcome| outcome.map(ToolHookGuard::Hermes))
        }
        // Dedicated merged hook file - wholly Paneflow-owned, zero RMW.
        "grok" => GrokHookFileGuard::install().map(|outcome| outcome.map(ToolHookGuard::Grok)),
        // Deliberately ABSENT (documented, not forgotten):
        // - "copilot": no hook/JSON-stream surface exists at all.
        // - "kiro-cli": hooks live inside PER-AGENT definition files
        //   (`~/.kiro/agents/<name>.json`) - injecting would mean rewriting
        //   every agent the user defined, and the default agent has no
        //   file to extend. No per-session surface exists.
        // - "droid": hooks are dashboard-managed (closed-source).
        // - "agy" / "openclaw" / the rest: no stable public hook surface.
        // They all still get the universal `ai.exit`/`ai.session_end`
        // lifecycle plus the sidebar's process-scan "running" row.
        _ => Ok(HookInstall::Skipped(HookInstallSkip::UnsupportedTool)),
    }
}

/// EP-004 US-010: best-effort notify of `ai.exit { exit_code }` after the
/// real AI binary exits. Same contract as [`notify_session_end`] (sibling
/// hook binary, bounded wait, silent failure); the raw code rides in
/// `PANEFLOW_AI_EXIT_CODE` since the hook's stdin is null on
/// shim-synthesized events.
fn is_interrupt_exit_code(exit_code: i32) -> bool {
    matches!(exit_code, 129 | 130 | 137 | 143)
}

/// Outer wait+kill for a synthesized hook. The hook's own IPC connect+write
/// is already bounded at 500 ms; this stops a wedged child from pinning the
/// PTY after the wrapped agent has exited.
const HOOK_NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);

/// stdout capture cap for a synthesized hook child. `paneflow-ai-hook` writes
/// nothing to stdout, but `run_with_timeout` pipes it regardless (it overrides
/// the caller's `Stdio::null()`), and a capture limit now FAILS the run and
/// SIGKILLs the child's process group rather than truncating. A cap of `0`
/// therefore turns a single stray byte into a lost `ai.exit` /
/// `ai.session_end`. 64 KiB matches the crate's own `STDERR_CAP`, is orders of
/// magnitude beyond anything the hook could legitimately emit, and still bounds
/// a PATH-hijacked impostor.
const HOOK_STDOUT_CAP: u64 = 64 * 1024;
// A regression of this cap to 0 fails to compile.
const _: () = assert!(HOOK_STDOUT_CAP >= 64 * 1024);

fn run_synthesized_hook(cmd: std::process::Command) {
    run_synthesized_hook_with_deadline(cmd, HOOK_NOTIFY_TIMEOUT);
}

fn run_synthesized_hook_with_deadline(cmd: std::process::Command, deadline: Duration) {
    let _ = paneflow_process::run_with_timeout(cmd, deadline, HOOK_STDOUT_CAP);
}

fn notify_exit(tool: &str, exit_code: i32, interrupted: bool) {
    let Some(hook_path) = locate_sibling_hook_binary() else {
        return;
    };
    let mut cmd = std::process::Command::new(&hook_path);
    cmd.arg("Exit")
        .env("PANEFLOW_AI_TOOL", tool)
        .env("PANEFLOW_AI_PID", std::process::id().to_string())
        .env("PANEFLOW_AI_EXIT_CODE", exit_code.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if interrupted {
        cmd.env(
            PANEFLOW_AI_EVENT_SOURCE_ENV,
            PANEFLOW_AI_EVENT_SOURCE_INTERRUPT,
        );
    }
    run_synthesized_hook(cmd);
}

/// Best-effort notify of `ai.session_end` after the real AI binary exits.
///
/// Locates `paneflow-ai-hook` next to this shim binary (US-008 extracts
/// both into the same cache dir, and `current_exe()` handles symlink/
/// hardlink resolution per `find_real_binary` precedent), then spawns it
/// with `SessionEnd` and the `PANEFLOW_AI_TOOL` env so the hook tags the
/// frame with the right tool identity. Inherits `PANEFLOW_SOCKET_PATH`
/// and `PANEFLOW_WORKSPACE_ID` from the shim's own env (they were set
/// by `pty_session::inject_ai_hook_env`).
///
/// Bounded wait (`HOOK_NOTIFY_TIMEOUT`): the hook's only work is a
/// single Unix-socket write of a tiny JSON frame, typically <5 ms. The
/// PRD's 15 ms latency budget for shim overhead (US-004 AC) is preserved
/// on the success path. A wedged hook (unbounded connect, stuck child)
/// is killed at 2 s so the PTY is not pinned after the agent exits.
fn notify_session_end(tool: &str, interrupted: bool) {
    let Some(hook_path) = locate_sibling_hook_binary() else {
        return;
    };
    let mut cmd = std::process::Command::new(&hook_path);
    cmd.arg("SessionEnd")
        .env("PANEFLOW_AI_TOOL", tool)
        .env("PANEFLOW_AI_PID", std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if interrupted {
        cmd.env(
            PANEFLOW_AI_EVENT_SOURCE_ENV,
            PANEFLOW_AI_EVENT_SOURCE_INTERRUPT,
        );
    }
    run_synthesized_hook(cmd);
}

/// Resolve `paneflow-ai-hook` sitting in the same directory as this shim
/// binary. Returns `None` if `current_exe()` fails or the sibling isn't a
/// regular file - in either case, the caller silently skips notification.
pub(crate) fn locate_sibling_hook_binary() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = "paneflow-ai-hook";
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tests/agents.rs"]
mod agent_tests;
#[cfg(test)]
#[path = "tests/detect.rs"]
mod detect_tests;
#[cfg(test)]
#[path = "tests/hook_config.rs"]
mod hook_config_tests;
#[cfg(test)]
#[path = "tests/notify.rs"]
mod notify_tests;
