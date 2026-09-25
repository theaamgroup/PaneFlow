//! `paneflow <verb>` scriptable CLI (EP-001, prd-cli-agent-orchestration).
//!
//! Talks to a RUNNING Paneflow instance over the existing IPC JSON-RPC socket
//! (`paneflow-ipc-client`) and exits before any GPUI init. `main.rs` dispatches
//! here only when `argv[1]` names a known verb ([`is_cli_verb`]) - mirroring the
//! `paneflow mcp …` intercept - so every other invocation (no args, unknown
//! args, `--help`/`--version`) is left untouched and the GUI
//! launch path is preserved. clap therefore never has to own the "no subcommand
//! => launch the GUI" default, and never eats the manually-parsed top-level
//! flags handled above it.

use clap::{Parser, Subcommand};
use paneflow_ipc_client::IpcClient;
use serde_json::Value;

mod selector;
mod send_cmd;

/// Process exit codes. Kept distinct so scripts can branch on the failure
/// kind. clap owns `2` for its own usage/parse errors (and `0` for
/// `--help`/`--version`), so the runtime codes start at `1` and avoid `2`.
pub const EXIT_OK: i32 = 0;
pub const EXIT_RUNTIME: i32 = 1;
pub const EXIT_TARGET: i32 = 3;
/// The verbs this CLI owns. `main.rs` gates the whole CLI dispatch (and the
/// manual `--help`/`--version` scans) on membership here so the GUI launch
/// path stays byte-for-byte unchanged for any other `argv[1]`.
///
/// Pane reads are not CLI verbs (issue #811): agents read panes through the
/// MCP bridge, and scripts call the `surface.*` / `fleet.list` / `agent.whoami`
/// JSON-RPC methods on the socket directly.
pub(crate) const VERBS: &[&str] = &["send", "key"];

/// Verbs shown in `paneflow --help`, one row per [`VERBS`] entry.
pub(crate) const HELP_VERBS: &[(&str, &str)] = &[
    ("send", "Inject text into a pane"),
    ("key", "Send a named keystroke to a pane"),
];

/// Offline intercepts handled in `main.rs` before clap (`mcp`, `hooks`).
/// Not in [`VERBS`]; still listed next to the verbs so unknown-verb errors
/// that point at `paneflow --help` actually show a complete command index.
pub(crate) const HELP_OFFLINE_COMMANDS: &[(&str, &str)] = &[
    ("mcp", "Install, status, or uninstall the MCP bridge"),
    ("hooks", "Install persistent agent-notification hooks"),
];

/// One padded row per [`HELP_VERBS`] entry, then [`HELP_OFFLINE_COMMANDS`].
pub(crate) fn format_help_commands() -> String {
    let width = HELP_VERBS
        .iter()
        .chain(HELP_OFFLINE_COMMANDS)
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (name, desc) in HELP_VERBS.iter().chain(HELP_OFFLINE_COMMANDS) {
        out.push_str(&format!("  {name:<width$}  {desc}\n"));
    }
    out
}

/// True when `argv[1]` names one of our subcommands.
pub fn is_cli_verb(arg: Option<&str>) -> bool {
    matches!(arg, Some(v) if VERBS.contains(&v))
}

/// True when `argv[1]` is shaped like a subcommand (present, non-empty, and not
/// a `-`/`--` flag) but is NOT one this CLI owns. `main.rs` calls this only
/// AFTER the `mcp`/`hooks`/known-verb intercepts have each had their chance and
/// exited, so a `true` here is an unmistakable typo (`paneflow blah`, a
/// mistyped `paneflow searh`): it prints an actionable "unknown verb" error and
/// exits non-zero instead of falling through to the GUI launch, which would
/// otherwise trip the single-instance guard with no message (EP-005 US-011).
/// A bare `paneflow` (argv[1] is `None`) returns `false` so the GUI still
/// launches, and a leading-`-` token is a flag, not a verb, so it stays on the
/// GUI/global-flag path.
pub fn looks_like_unknown_verb(arg: Option<&str>) -> bool {
    matches!(arg, Some(v) if !v.is_empty() && !v.starts_with('-') && !VERBS.contains(&v))
}

#[derive(Parser, Debug)]
#[command(
    name = "paneflow",
    version,
    about = "Drive a running PaneFlow instance from the shell",
    // The GUI launch (no subcommand) is handled in main.rs, never here, so a
    // bare `paneflow` never reaches clap. `Option<Commands>` keeps clap from
    // forcing `subcommand_required` / `arg_required_else_help` regardless.
    subcommand_required = false,
    arg_required_else_help = false
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Inject text into a pane WITHOUT submitting it (human-in-loop).
    ///
    /// Requires `PANEFLOW_IPC_SCRIPTING=1` on the running instance; the text is
    /// written verbatim with no trailing newline so the user/agent reviews and
    /// presses Enter themselves - unless `--submit` is passed explicitly.
    Send {
        /// Target: surface id, name, `cmdline:<substr>`, or `cwd:<path>`.
        target: String,
        /// Text to inject (no trailing carriage return is added by default).
        text: String,
        /// Send to EVERY pane matching the target (a multi-match selector is
        /// an error without this flag). Prints a `{sent, failed}` report.
        #[arg(long)]
        broadcast: bool,
        /// Submit the text (append a carriage return). Explicit opt-in: this
        /// is the ONLY way the CLI ever submits on the user's behalf, and it
        /// still requires the instance-side scripting gate.
        #[arg(long)]
        submit: bool,
        /// Force bracketed-paste delivery: the text is wrapped in
        /// `ESC[200~`/`ESC[201~` and, with `--submit`, the carriage return is
        /// sent separately after a calibrated delay so a TUI agent does not
        /// swallow it (EP-001). `--submit` toward an agent pane enables this
        /// automatically; pass `--paste` to force it (e.g. toward a shell) or
        /// `--paste` alone to wrap a non-submitted inject.
        #[arg(long)]
        paste: bool,
        /// Ask the agent to write its complete result to this file and print
        /// `REPORT_DONE <path>` after the file is fully written. The path is
        /// resolved relative to the caller's current directory.
        #[arg(long, value_name = "PATH")]
        report_file: Option<String>,
    },
    /// Send a named keystroke (e.g. `escape`, `ctrl-c`, `tab`) to a pane.
    ///
    /// Requires `PANEFLOW_IPC_SCRIPTING=1` on the running instance. Keystrokes
    /// that would submit a line (`enter`, `ctrl-m`, `ctrl-j`) are refused -
    /// submission is exclusive to `send --submit`.
    Key {
        /// Target: surface id, name, `cmdline:<substr>`, or `cwd:<path>`.
        target: String,
        /// Dash-separated keystroke description ("escape", "ctrl-c", "alt-f").
        keystroke: String,
    },
}

/// A CLI failure carrying the process exit code to surface for it.
#[derive(Debug)]
pub struct CliError {
    pub code: i32,
    pub message: String,
}

impl CliError {
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_RUNTIME,
            message: message.into(),
        }
    }

    pub fn target(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_TARGET,
            message: message.into(),
        }
    }
}

/// Entry point invoked by `main.rs` when `argv[1]` is a known verb. Parses the
/// args with clap, opens a client to the running instance, dispatches, and
/// returns the process exit code.
pub fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // clap prints `--help`/`--version` (exit 0) and usage errors (exit 2)
        // itself; we just relay its code rather than letting `parse()` abort
        // the process from inside a GUI binary.
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };

    // `command` is always `Some` here: main.rs only calls `run` when argv[1]
    // is a known verb. The `None` arm is unreachable in practice.
    let Some(command) = cli.command else {
        return EXIT_OK;
    };

    let client = match connect() {
        Ok(client) => client,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_RUNTIME;
        }
    };

    match dispatch(command, &client) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("paneflow: {}", e.message);
            e.code
        }
    }
}

/// Resolve the socket path and build a client. The path is resolved eagerly
/// (honoring `PANEFLOW_SOCKET_PATH`), but a missing instance only surfaces as
/// an "unreachable … is Paneflow running?" error on the first `call`, so a
/// resolvable-but-dead socket is not a `connect` failure.
fn connect() -> Result<IpcClient, String> {
    let socket = paneflow_ipc_client::resolve_socket_path().ok_or_else(|| {
        "paneflow: cannot locate the IPC socket; is PaneFlow running? \
         (set PANEFLOW_SOCKET_PATH if you launched the CLI outside a PaneFlow pane)"
            .to_string()
    })?;
    Ok(IpcClient::new(socket))
}

/// Route a parsed subcommand to its handler.
fn dispatch(command: Commands, client: &IpcClient) -> Result<i32, CliError> {
    match command {
        Commands::Send {
            target,
            text,
            broadcast,
            submit,
            paste,
            report_file,
        } => send_cmd::send(
            client,
            &target,
            &text,
            broadcast,
            submit,
            paste,
            report_file.as_deref(),
        ),
        Commands::Key { target, keystroke } => send_cmd::key(client, &target, &keystroke),
    }
}

/// Render a JSON-RPC `result` value as pretty JSON to stdout, so every
/// machine-readable `send` / `key` output uses one renderer.
pub(super) fn print_json(value: &Value) -> Result<(), CliError> {
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?;
    println!("{rendered}");
    Ok(())
}

/// Reject a server reply that carries a *legacy* application error.
///
/// A handful of server handlers signal cap/validation failures (split at
/// `send_text` over the 64 KiB limit) with
/// an ad-hoc `{"error": "<message>"}` payload that does NOT use the
/// `_jsonrpc_error` sentinel. The dispatcher therefore promotes them under
/// `result`, so the transport's `parse_response` returns `Ok` and the command
/// would otherwise print the error and exit 0 - breaking the scriptability
/// contract (US-005 AC4 "code non-zéro", US-006 AC3). Calling this on every
/// `result` before printing maps that legacy shape to a non-zero `CliError`.
///
/// No `send` / `key` success envelope carries a top-level `error` string
/// (`{sent,…}`), so the check can't false-positive on real data.
pub(super) fn reject_legacy_error(result: Value) -> Result<Value, CliError> {
    if let Some(message) = result.get("error").and_then(Value::as_str) {
        return Err(CliError::runtime(message.to_string()));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_control_verbs_are_unknown_and_absent_from_help() {
        for args in [
            vec!["paneflow", "new"],
            vec!["paneflow", "select", "0"],
            vec!["paneflow", "split", "v"],
            vec!["paneflow", "focus", "pane"],
        ] {
            let verb = args[1];
            assert!(!is_cli_verb(Some(verb)));
            assert!(looks_like_unknown_verb(Some(verb)));
            assert_eq!(
                Cli::try_parse_from(&args)
                    .expect_err("removed verb")
                    .exit_code(),
                2
            );
            assert!(!HELP_VERBS.iter().any(|(name, _)| *name == verb));
        }
    }

    #[test]
    fn removed_flow_is_unknown_and_absent_from_help() {
        assert!(!VERBS.contains(&"flow"));
        assert!(!HELP_VERBS.iter().any(|(name, _)| *name == "flow"));
        assert!(looks_like_unknown_verb(Some("flow")));
        assert!(Cli::try_parse_from(["paneflow", "flow", "run", "file.toml"]).is_err());
        assert!(
            !format_help_commands()
                .lines()
                .any(|line| line.trim_start().starts_with("flow "))
        );
    }

    #[test]
    fn removed_up_is_unknown_and_absent_from_help() {
        assert!(!VERBS.contains(&"up"));
        assert!(!HELP_VERBS.iter().any(|(name, _)| *name == "up"));
        assert!(looks_like_unknown_verb(Some("up")));
        assert!(Cli::try_parse_from(["paneflow", "up", "file.toml"]).is_err());
        assert!(
            !format_help_commands()
                .lines()
                .any(|line| line.trim_start().starts_with("up "))
        );
    }

    #[test]
    fn removed_watch_is_unknown_and_absent_from_help() {
        assert!(!VERBS.contains(&"watch"));
        assert!(!HELP_VERBS.iter().any(|(name, _)| *name == "watch"));
        assert!(looks_like_unknown_verb(Some("watch")));
        assert!(Cli::try_parse_from(["paneflow", "watch"]).is_err());
        assert!(
            !format_help_commands()
                .lines()
                .any(|line| line.trim_start().starts_with("watch "))
        );
    }

    #[test]
    fn removed_task_is_unknown_and_absent_from_help() {
        assert!(!VERBS.contains(&"task"));
        assert!(!HELP_VERBS.iter().any(|(name, _)| *name == "task"));
        assert!(looks_like_unknown_verb(Some("task")));
        for args in [
            vec!["paneflow", "task", "get"],
            vec!["paneflow", "task", "assign", "pane", "--file", "task.json"],
            vec!["paneflow", "task", "report", "--file", "report.json"],
        ] {
            assert_eq!(
                Cli::try_parse_from(&args)
                    .expect_err("removed verb")
                    .exit_code(),
                2
            );
        }
        assert!(
            !format_help_commands()
                .lines()
                .any(|line| line.trim_start().starts_with("task "))
        );
    }

    #[test]
    fn removed_wait_is_unknown_and_absent_from_help() {
        assert!(!VERBS.contains(&"wait"));
        assert!(!HELP_VERBS.iter().any(|(name, _)| *name == "wait"));
        assert!(looks_like_unknown_verb(Some("wait")));
        assert!(
            Cli::try_parse_from(["paneflow", "wait", "--match", "pane", "--pattern", "done"])
                .is_err()
        );
        assert!(
            !format_help_commands()
                .lines()
                .any(|line| line.trim_start().starts_with("wait "))
        );
    }

    #[test]
    fn removed_read_verbs_are_unknown_and_absent_from_help() {
        for args in [
            vec!["paneflow", "ls"],
            vec!["paneflow", "read", "backend"],
            vec!["paneflow", "search", "backend", "needle"],
            vec!["paneflow", "ps"],
            vec!["paneflow", "status", "backend"],
            vec!["paneflow", "whoami"],
            vec!["paneflow", "list_panes"],
            vec!["paneflow", "read_pane", "backend"],
            vec!["paneflow", "search_pane", "backend", "needle"],
        ] {
            let verb = args[1];
            assert!(!VERBS.contains(&verb));
            assert!(!is_cli_verb(Some(verb)));
            assert!(looks_like_unknown_verb(Some(verb)));
            assert_eq!(
                Cli::try_parse_from(&args)
                    .expect_err("removed verb")
                    .exit_code(),
                2
            );
            assert!(!HELP_VERBS.iter().any(|(name, _)| *name == verb));
            assert!(
                !format_help_commands()
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("{verb} ")))
            );
        }
    }

    #[test]
    fn is_cli_verb_matches_known_verbs() {
        assert!(is_cli_verb(Some("send")));
        assert!(is_cli_verb(Some("key")));
        assert!(!is_cli_verb(Some("mcp")));
        assert!(!is_cli_verb(Some("hooks")));
        assert!(!is_cli_verb(Some("--version")));
        assert!(!is_cli_verb(None));
    }

    #[test]
    fn cli_help_index_covers_canonical_verbs() {
        for verb in VERBS {
            assert!(
                HELP_VERBS.iter().any(|(name, _)| name == verb),
                "canonical verb {verb} missing from HELP_VERBS"
            );
        }
        for (name, _) in HELP_VERBS {
            assert!(
                VERBS.contains(name),
                "HELP_VERBS entry {name} is not in VERBS"
            );
        }
        for (name, _) in HELP_OFFLINE_COMMANDS {
            assert!(
                !VERBS.contains(name),
                "offline command {name} should stay out of VERBS (main.rs intercepts it)"
            );
        }
        let listing = format_help_commands();
        for (name, desc) in HELP_VERBS.iter().chain(HELP_OFFLINE_COMMANDS) {
            assert!(
                listing.lines().any(|line| {
                    let line = line.trim_start();
                    line.starts_with(name)
                        && line[name.len()..].starts_with(char::is_whitespace)
                        && line.contains(desc)
                }),
                "help listing missing {name}: {listing}"
            );
        }
    }

    #[test]
    fn unknown_verb_detected_but_bare_and_flags_are_not() {
        // EP-005 US-011: a verb-shaped typo is flagged so main.rs errors
        // actionably instead of launching the GUI / tripping the singleton.
        assert!(looks_like_unknown_verb(Some("blah")));
        assert!(looks_like_unknown_verb(Some("searh")));
        // Known verbs are NOT unknown.
        assert!(!looks_like_unknown_verb(Some("send")));
        assert!(!looks_like_unknown_verb(Some("key")));
        // A bare `paneflow` (None) and an empty token still launch the GUI.
        assert!(!looks_like_unknown_verb(None));
        assert!(!looks_like_unknown_verb(Some("")));
        // Flags stay on the global-flag / GUI path, never the unknown-verb error.
        assert!(!looks_like_unknown_verb(Some("--help")));
        assert!(!looks_like_unknown_verb(Some("-v")));
        assert!(!looks_like_unknown_verb(Some("--update-and-exit")));
    }

    #[test]
    fn send_flags_default_off() {
        // The human-in-loop default: no broadcast, no submit, no paste unless
        // explicit (EP-001 US-002: absent `--paste`, the server auto-decides).
        let cli = Cli::try_parse_from(["paneflow", "send", "backend", "hi"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Send {
                broadcast: false,
                submit: false,
                paste: false,
                ..
            })
        ));
        let cli = Cli::try_parse_from(["paneflow", "send", "--broadcast", "--submit", "sh", "go"])
            .expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Send {
                broadcast: true,
                submit: true,
                paste: false,
                ..
            })
        ));
        // EP-001 US-002 AC2: `--paste` is an explicit, parseable override.
        let cli =
            Cli::try_parse_from(["paneflow", "send", "--paste", "agent", "hi"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Send { paste: true, .. })
        ));
        let cli = Cli::try_parse_from([
            "paneflow",
            "send",
            "--report-file",
            "reports/out.md",
            "agent",
            "hi",
        ])
        .expect("parse");
        assert!(
            matches!(cli.command, Some(Commands::Send { report_file: Some(p), .. }) if p == "reports/out.md")
        );
    }

    #[test]
    fn key_requires_target_and_keystroke() {
        let err = Cli::try_parse_from(["paneflow", "key", "backend"]).expect_err("usage");
        assert_eq!(err.exit_code(), 2);
        let cli = Cli::try_parse_from(["paneflow", "key", "backend", "escape"]).expect("parse");
        assert!(matches!(cli.command, Some(Commands::Key { .. })));
    }

    #[test]
    fn no_subcommand_parses_to_none() {
        // Defensive: a bare invocation never reaches `run` (main.rs gates on a
        // known verb), but clap must not force-error on it.
        let cli = Cli::try_parse_from(["paneflow"]).expect("parse");
        assert!(cli.command.is_none());
    }
}
