use super::*;
use std::time::Duration;

#[test]
fn synthesized_hook_wait_returns_when_child_hangs() {
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("30");
    let start = std::time::Instant::now();
    run_synthesized_hook_with_deadline(cmd, Duration::from_millis(150));
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50),
        "must actually wait for the deadline, not fail to spawn; elapsed {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "hook wait must not block for the child lifetime; elapsed {elapsed:?}"
    );
}

#[test]
fn hook_stdout_cap_is_non_zero() {
    // A cap of 0 fails the bounded run on a single stray byte. Drive a
    // one-byte child through the same helper the synthesized hooks use so
    // this is a behavioral check, not a constant assert.
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("printf x; exit 0");
    let out = paneflow_process::run_with_timeout(cmd, HOOK_NOTIFY_TIMEOUT, HOOK_STDOUT_CAP)
        .expect("a zero stdout cap fails the bounded run on a single stray byte");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"x");
}

/// A hook child that writes to stdout must still be waited on successfully.
/// With `stdout_cap = 0` this returns `Err(OutputLimitExceeded)` and SIGKILLs
/// the child's process group before the IPC notify completes.
#[test]
fn synthesized_hook_tolerates_child_stdout() {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("printf 'hook chatter'; exit 0");
    let out = paneflow_process::run_with_timeout(cmd, HOOK_NOTIFY_TIMEOUT, HOOK_STDOUT_CAP)
        .expect("a hook that writes to stdout must not fail the bounded run");
    assert!(out.status.success());
}

/// The shim's interrupt set drives the `interrupted` flag on `ai.exit`;
/// `state_for_exit` on the app side classifies the same codes as a human
/// interruption rather than `Errored`.
#[test]
fn interrupt_exit_codes_are_hup_int_kill_and_term() {
    for code in [129, 130, 137, 143] {
        assert!(
            is_interrupt_exit_code(code),
            "128+signal code {code} is an interrupt"
        );
    }
    for code in [0, 1, 2, 127, 134, 139, -1] {
        assert!(
            !is_interrupt_exit_code(code),
            "code {code} is not an interrupt"
        );
    }
}

/// Extract the codes in the `matches!(exit_code, a | b | …)` arm of a
/// `fn <name>(exit_code: i32) -> bool` predicate, sorted.
fn interrupt_codes_in(source: &str, fn_name: &str) -> Vec<i32> {
    let start = source
        .find(&format!("fn {fn_name}(exit_code: i32) -> bool"))
        .expect("predicate signature must be present in the source");
    let arm = source[start..]
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("matches!(exit_code,")
                .and_then(|rest| rest.strip_suffix(')'))
        })
        .expect("predicate must be a single `matches!(exit_code, …)` arm");
    let mut codes: Vec<i32> = arm
        .split('|')
        .map(|code| {
            code.trim()
                .parse()
                .expect("every arm entry is an integer exit code")
        })
        .collect();
    codes.sort_unstable();
    codes
}

/// Drift pin against `src-app/src/ai_types.rs::is_human_interruption_exit`.
/// `src-app` is a bin-only crate, so the shim cannot import the app
/// predicate; the two sets are compared as source text instead. Changing
/// either copy without the other fails here.
#[test]
fn interrupt_exit_codes_match_app_human_interruption_set() {
    let shim = interrupt_codes_in(include_str!("../main.rs"), "is_interrupt_exit_code");
    let app = interrupt_codes_in(
        include_str!("../../../../src-app/src/ai_types.rs"),
        "is_human_interruption_exit",
    );
    assert!(!shim.is_empty(), "the shim arm must list at least one code");
    assert_eq!(
        shim, app,
        "paneflow-shim::is_interrupt_exit_code and paneflow-app::ai_types::is_human_interruption_exit must agree"
    );
    // Control: the parsed set is the one the shim actually executes, so a
    // parser that silently reads the wrong arm cannot turn this test into a
    // no-op.
    for code in -1..=255 {
        assert_eq!(
            is_interrupt_exit_code(code),
            shim.contains(&code),
            "parsed arm and runtime predicate disagree on {code}"
        );
    }
}
