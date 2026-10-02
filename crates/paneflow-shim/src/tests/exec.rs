use crate::exec::spawn_parent_death_guard;
use paneflow_process::{Child, Command};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A long-lived stand-in for the agent the guard watches over.
///
/// Through `paneflow_process::Command`: a std spawn could copy the pipes
/// another test holds inheritable on purpose (#1127) and keep them for 30 s.
fn spawn_sleeping_child() -> Child {
    Command::new("sleep")
        .arg("30")
        .start()
        .expect("spawn `sleep 30` as the stand-in agent")
}

/// A parent PID that is guaranteed to differ from the real one, so the guard
/// sees a "reparent" on its first tick without anyone having to die.
fn bogus_parent_pid() -> u32 {
    // SAFETY: `getppid` is a trivial, argument-free syscall.
    let real = unsafe { libc::getppid() } as u32;
    real.wrapping_add(1)
}

/// Poll `try_wait` for up to `budget`; `None` means the child outlived it.
fn wait_for_exit(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < budget {
        if let Some(status) = child.try_wait().expect("try_wait on the stand-in child") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn parent_death_guard_sigkills_child_on_reparent() {
    let mut child = spawn_sleeping_child();
    let child_reaped = Arc::new(AtomicBool::new(false));

    spawn_parent_death_guard(child.id(), bogus_parent_pid(), Arc::clone(&child_reaped));

    // The guard polls every 500 ms; give it several ticks of slack.
    let status = wait_for_exit(&mut child, Duration::from_secs(5));
    child_reaped.store(true, Ordering::Release);
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("guard left the agent running after a detected reparent");
        }
    };
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "agent must die by SIGKILL on reparent, got {status:?}"
    );
}

#[test]
fn parent_death_guard_does_not_signal_once_child_is_reaped() {
    let mut child = spawn_sleeping_child();
    // `run_real` flips this the instant `child.wait()` returns; from then on
    // the child PID may belong to an unrelated process and must not be
    // probed or signalled, even though the parent check would fire.
    let child_reaped = Arc::new(AtomicBool::new(true));

    spawn_parent_death_guard(child.id(), bogus_parent_pid(), Arc::clone(&child_reaped));

    // Three guard ticks is enough for a faulty guard to have fired.
    let status = wait_for_exit(&mut child, Duration::from_millis(1_600));
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        status.is_none(),
        "guard must not touch an already-reaped child PID, but it exited with {status:?}"
    );
}

/// The agent keeps a descriptor its shell passed on, with or without another
/// listed descriptor that is gone by the spawn (#1126).
#[test]
fn agent_keeps_inherited_descriptors_and_starts_without_a_closed_one() {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    // Far above any descriptor this test process opens.
    const CLOSED: i32 = 4000;
    for listed_closed in [&[][..], &[CLOSED][..]] {
        let (mut reader, writer) = std::io::pipe().expect("pipe");
        let passed = writer.as_raw_fd();
        let mut command = paneflow_process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("printf passed >&{passed}"))
            .stdin(paneflow_process::Stdio::Null)
            .stdout(paneflow_process::Stdio::Null)
            .stderr(paneflow_process::Stdio::Null);
        let inherited: Vec<i32> = listed_closed.iter().copied().chain([passed]).collect();
        let status = crate::exec::start_agent(&mut command, &inherited)
            .and_then(|mut child| child.wait())
            .expect("a closed inherited descriptor must not stop the agent");
        drop(writer);
        let mut text = String::new();
        reader.read_to_string(&mut text).expect("read");
        assert!(status.success(), "{inherited:?}: {status:?}");
        assert_eq!(
            text, "passed",
            "the open descriptor must still reach the agent: {inherited:?}"
        );
    }
}

/// Issue #1136: `run_real` lists the descriptors the agent keeps before it
/// starts the SIGINT watcher, the one shim thread that creates pipes (the
/// interrupt Stop hook's `spawn_piped`). Listed after it, a hook pipe still
/// waiting for its close-on-exec `fcntl` could be handed to the agent.
#[test]
fn agent_descriptors_are_listed_before_any_pipe_creating_thread_starts() {
    let src = include_str!("../exec.rs");
    let body = src
        .split("pub(crate) fn run_real(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("run_real body");
    let position = |needle: &str| {
        let found: Vec<usize> = body.match_indices(needle).map(|(at, _)| at).collect();
        assert_eq!(found.len(), 1, "one `{needle}` in run_real: {found:?}");
        found[0]
    };
    let listed = position("paneflow_process::inheritable_descriptors()");
    let watcher = position("install_sigint_watcher(tool);");
    let spawned = position("start_agent(&mut cmd, &inherited)");
    assert!(
        listed < watcher,
        "list the agent's descriptors before the SIGINT watcher starts"
    );
    assert!(watcher < spawned, "the watcher starts before the agent");
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("read source dir");
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// A column-0 attribute that compiles the next item only under test.
fn is_test_cfg(line: &str) -> bool {
    line == "#[cfg(test)]" || line.starts_with("#[cfg(all(test,")
}

/// The production part of a shim source file: everything before its trailing
/// `#[cfg(test)] mod tests { ... }` block. Test-only items earlier in the file
/// stay in, so a spawn there fails the guard loudly rather than passing it.
fn production_part(path: &Path, src: &str) -> String {
    const TRAILING: &str = "\n#[cfg(test)]\nmod tests {\n";
    let Some(start) = src.rfind(TRAILING) else {
        return src.to_string();
    };
    let body = &src[start + TRAILING.len()..];
    let close = body
        .lines()
        .position(|line| line.starts_with('}'))
        .unwrap_or_else(|| panic!("{}: `mod tests` never closes", path.display()));
    assert!(
        body.lines()
            .skip(close + 1)
            .all(|line| line.trim().is_empty()),
        "{}: code after the trailing `mod tests` block would go unscanned",
        path.display()
    );
    src[..start].to_string()
}

/// Issue #1127: every child the shim starts goes through `paneflow_process`,
/// as `production_child_spawns_go_through_paneflow_process` in
/// `src-app/src/ipc.rs` requires of the app. A direct `Command::spawn`,
/// `output` or `status` starts a std child, which inherits every descriptor
/// not marked close-on-exec; `paneflow_process::Command` hands it only what
/// it is given (#1126). Pipes come from `spawn_piped`, the one pipe source,
/// because it closes the parent's copy of the child's ends (#1124). Test
/// code, under `src/tests/` or a trailing `mod tests`, may still spawn
/// directly.
#[test]
fn production_child_spawns_go_through_paneflow_process() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let test_dir = root.join("tests");
    assert!(root.is_dir(), "{} is not a directory", root.display());
    assert!(
        test_dir.is_dir(),
        "{} is not a directory",
        test_dir.display()
    );
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();
    let (test_files, production_files): (Vec<PathBuf>, Vec<PathBuf>) = files
        .into_iter()
        .partition(|path| path.starts_with(&test_dir));
    assert!(
        production_files.len() >= 5,
        "the walk found only {} production files",
        production_files.len()
    );

    let mut production = Vec::new();
    let mut declared_tests = Vec::new();
    for path in &production_files {
        let src = std::fs::read_to_string(path).expect("read source");
        // Skipping `src/tests/` is sound only while every file there is
        // reached through a test-only `#[path = "tests/..."]` module.
        let lines: Vec<&str> = src.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if let Some(file) = line
                .strip_prefix("#[path = \"tests/")
                .and_then(|rest| rest.strip_suffix("\"]"))
            {
                assert!(
                    index > 0 && is_test_cfg(lines[index - 1]),
                    "{}:{}: a `tests/` module must be test-only",
                    path.display(),
                    index + 1
                );
                declared_tests.push(test_dir.join(file));
            }
        }
        production.push((path.clone(), production_part(path, &src)));
    }
    declared_tests.sort();
    assert_eq!(
        declared_tests, test_files,
        "every file under src/tests/ must be a declared test-only module"
    );

    // Self-checks: a guard that scans nothing passes every tree.
    let scanned = |name: &str, needle: &str| {
        production
            .iter()
            .find(|(path, _)| *path == root.join(name))
            .unwrap_or_else(|| panic!("{name} was not walked"))
            .1
            .contains(needle)
    };
    assert!(scanned("exec.rs", "fn send_interrupt_stop("));
    assert!(scanned("exec.rs", "fn spawn_parent_death_guard("));
    assert!(scanned("main.rs", "fn locate_sibling_hook_binary("));
    assert!(
        !scanned(
            "hooks/claude.rs",
            "fn worktree_panes_target_the_main_checkout_dot_claude("
        ),
        "hooks/claude.rs's trailing test module was not split off"
    );

    const PIPED: &str = "Stdio::piped()";
    let direct = [
        ".spawn()",
        ".output()",
        ".status()",
        "Command::spawn(",
        "Command::output(",
        "Command::status(",
        // Function-path uses, such as `.map(Command::spawn)`.
        "::spawn)",
        "::output)",
        "::status)",
        PIPED,
        // A hand-made pipe: `spawn_piped` is the one pipe source, because
        // it closes the parent's copy of the child's ends.
        "io::pipe()",
        "libc::pipe(",
    ];
    let mut offenders = Vec::new();
    for (path, src) in &production {
        let name = path.strip_prefix(&root).expect("under src").display();
        for (number, line) in src.lines().enumerate() {
            if !line.trim_start().starts_with("//") && direct.iter().any(|c| line.contains(c)) {
                offenders.push(format!("{name}:{}: {}", number + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "spawn shim children through paneflow_process::Command (or its spawn_piped / \
         run_with_timeout helpers), not directly, and get pipes from spawn_piped, not \
         {PIPED} (issues #1124, #1126, #1127):\n{}",
        offenders.join("\n")
    );
}
