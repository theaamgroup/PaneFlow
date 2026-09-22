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

mod context_cmds;
mod read_cmds;
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
/// EP-005 US-011: the trailing `list_panes`/`read_pane`/`search_pane` are the
/// `paneflow` MCP tool names, accepted as CLI aliases (clap maps each to its
/// canonical subcommand via `#[command(alias = ...)]`) so an orchestrator that types
/// the tool name reaches the matching verb instead of tripping the GUI
/// single-instance guard. This list only gates the `main.rs` intercept.
pub(crate) const VERBS: &[&str] = &[
    "whoami",
    "task",
    "ls",
    "read",
    "search",
    "ps",
    "status",
    "send",
    "key",
    "list_panes",
    "read_pane",
    "search_pane",
];

/// Canonical verbs shown in `paneflow --help`. MCP-tool aliases stay in
/// [`VERBS`] (so they still intercept) but off this index to keep help short.
pub(crate) const HELP_VERBS: &[(&str, &str)] = &[
    ("whoami", "Read your pane and agent context"),
    ("task", "Assign, read, or report a pane task"),
    ("ls", "List terminal surfaces"),
    ("read", "Print a pane's scrollback"),
    ("search", "Search a pane's scrollback"),
    ("ps", "List running agents"),
    ("status", "Read one surface's agent state"),
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

/// True when `argv[1]` names one of our subcommands (including the MCP-tool
/// aliases above).
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
    /// Read the identity inherited from the pane running this command.
    Whoami,
    /// Manage the task attached to a terminal pane.
    Task {
        #[command(subcommand)]
        command: context_cmds::TaskCommand,
    },
    /// List terminal surfaces.
    // EP-005 US-011: `list_panes` is the MCP tool name; accept it as a hidden
    // alias so an orchestrator can type either.
    #[command(alias = "list_panes")]
    Ls {
        /// Human-readable table instead of the default JSON.
        #[arg(long)]
        human: bool,
    },
    /// Print a pane's scrollback (raw text by default).
    #[command(alias = "read_pane")]
    Read {
        /// Target: surface id, name, `cmdline:<substr>`, or `cwd:<path>`.
        target: String,
        /// Number of trailing lines (server clamps to 1..4000).
        #[arg(long)]
        lines: Option<u64>,
        /// Offset from the end of the buffer.
        #[arg(long)]
        offset: Option<u64>,
        /// Emit the `{text, lines, total_lines, eof}` envelope as JSON.
        #[arg(long)]
        json: bool,
        /// Return raw scrollback, bypassing the anti-injection fence that
        /// otherwise wraps the output as `<untrusted_terminal_output>` (the
        /// fence is on by default; see the ai_injection_fence setting).
        #[arg(long)]
        raw: bool,
    },
    /// Search a pane's scrollback for a substring/pattern.
    #[command(alias = "search_pane")]
    Search {
        /// Target: surface id, name, `cmdline:<substr>`, or `cwd:<path>`.
        target: String,
        /// Pattern to search for.
        pattern: String,
        /// Cap the number of matches (server clamps to 1..1000).
        #[arg(long)]
        max: Option<u64>,
        /// Human-readable lines instead of the default JSON.
        #[arg(long)]
        human: bool,
    },
    /// List running agents across the fleet (pid, tool, state, pane).
    Ps {
        /// Emit the `{agents:[…]}` envelope as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Read one surface's agent state (thinking / waiting / idle / errored / …).
    Status {
        /// Target: surface id, name, `cmdline:<substr>`, or `cwd:<path>`.
        target: String,
        /// Emit the status envelope as JSON instead of a one-line summary.
        #[arg(long)]
        json: bool,
    },
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
        Commands::Ls { human } => read_cmds::ls(client, human),
        Commands::Read {
            target,
            lines,
            offset,
            json,
            raw,
        } => read_cmds::read(client, &target, lines, offset, json, raw),
        Commands::Search {
            target,
            pattern,
            max,
            human,
        } => read_cmds::search(client, &target, &pattern, max, human),
        Commands::Whoami => context_cmds::whoami(client),
        Commands::Task { command } => context_cmds::run(client, command),
        Commands::Ps { json } => read_cmds::ps(client, json),
        Commands::Status { target, json } => read_cmds::status(client, &target, json),
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

/// Render a JSON-RPC `result` value as pretty JSON to stdout. Shared by the
/// read and context command modules so every machine-readable output uses one
/// renderer.
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
/// No success envelope on these verbs carries a top-level `error` string
/// (`{index,…}`, `{selected}`, `{split,…}`, `{sent,…}`, `{surfaces,…}`,
/// `{text,…}`, `{matches,…}`), so the check can't false-positive on real data.
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
    fn is_cli_verb_matches_known_verbs() {
        assert!(is_cli_verb(Some("ls")));
        assert!(is_cli_verb(Some("send")));
        assert!(is_cli_verb(Some("status")));
        assert!(is_cli_verb(Some("key")));
        assert!(!is_cli_verb(Some("mcp")));
        assert!(!is_cli_verb(Some("hooks")));
        assert!(!is_cli_verb(Some("--version")));
        assert!(!is_cli_verb(None));
    }

    #[test]
    fn cli_help_index_covers_canonical_verbs() {
        const MCP_ALIASES: &[&str] = &["list_panes", "read_pane", "search_pane"];
        for verb in VERBS {
            if MCP_ALIASES.contains(verb) {
                continue;
            }
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
            assert!(
                !MCP_ALIASES.contains(name),
                "MCP alias {name} should not be a HELP_VERBS row"
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
    fn mcp_tool_names_alias_to_their_verbs() {
        // EP-005 US-011: the MCP tool names gate the CLI dispatch in main.rs...
        assert!(is_cli_verb(Some("search_pane")));
        assert!(is_cli_verb(Some("read_pane")));
        assert!(is_cli_verb(Some("list_panes")));
        // ...and clap routes each alias to its canonical subcommand, so a
        // orchestrator that types the MCP name never lands on the GUI launch path.
        let cli = Cli::try_parse_from(["paneflow", "search_pane", "backend", "needle"])
            .expect("parse search_pane");
        assert!(matches!(cli.command, Some(Commands::Search { .. })));
        let cli =
            Cli::try_parse_from(["paneflow", "read_pane", "backend"]).expect("parse read_pane");
        assert!(matches!(cli.command, Some(Commands::Read { .. })));
        let cli = Cli::try_parse_from(["paneflow", "list_panes"]).expect("parse list_panes");
        assert!(matches!(cli.command, Some(Commands::Ls { .. })));
    }

    #[test]
    fn unknown_verb_detected_but_bare_and_flags_are_not() {
        // EP-005 US-011: a verb-shaped typo is flagged so main.rs errors
        // actionably instead of launching the GUI / tripping the singleton.
        assert!(looks_like_unknown_verb(Some("blah")));
        assert!(looks_like_unknown_verb(Some("searh")));
        // Known verbs and MCP aliases are NOT unknown.
        assert!(!looks_like_unknown_verb(Some("search")));
        assert!(!looks_like_unknown_verb(Some("search_pane")));
        assert!(!looks_like_unknown_verb(Some("ls")));
        // A bare `paneflow` (None) and an empty token still launch the GUI.
        assert!(!looks_like_unknown_verb(None));
        assert!(!looks_like_unknown_verb(Some("")));
        // Flags stay on the global-flag / GUI path, never the unknown-verb error.
        assert!(!looks_like_unknown_verb(Some("--help")));
        assert!(!looks_like_unknown_verb(Some("-v")));
        assert!(!looks_like_unknown_verb(Some("--update-and-exit")));
    }

    #[test]
    fn ps_parses_with_optional_json_flag() {
        let cli = Cli::try_parse_from(["paneflow", "ps", "--json"]).expect("parse");
        assert!(matches!(cli.command, Some(Commands::Ps { json: true })));
        // Default is the human table (like Unix `ps`), JSON is opt-in.
        let cli = Cli::try_parse_from(["paneflow", "ps"]).expect("parse");
        assert!(matches!(cli.command, Some(Commands::Ps { json: false })));
    }

    #[test]
    fn status_requires_a_target() {
        let err = Cli::try_parse_from(["paneflow", "status"]).expect_err("usage");
        assert_eq!(err.exit_code(), 2);
        let cli = Cli::try_parse_from(["paneflow", "status", "backend"]).expect("parse");
        assert!(matches!(cli.command, Some(Commands::Status { .. })));
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
    fn cli_parses_a_verb_with_flags() {
        let cli = Cli::try_parse_from(["paneflow", "ls", "--human"]).expect("parse");
        assert!(matches!(cli.command, Some(Commands::Ls { human: true })));
    }

    #[test]
    fn read_requires_a_target() {
        // Missing the required positional `target` is a clap usage error (2).
        let err = Cli::try_parse_from(["paneflow", "read"]).expect_err("usage");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn no_subcommand_parses_to_none() {
        // Defensive: a bare invocation never reaches `run` (main.rs gates on a
        // known verb), but clap must not force-error on it.
        let cli = Cli::try_parse_from(["paneflow"]).expect("parse");
        assert!(cli.command.is_none());
    }
}
