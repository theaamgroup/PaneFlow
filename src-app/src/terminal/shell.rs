//! Shell resolution + automatic OSC 7 injection.
//!
//! `resolve_default_shell` picks the shell binary to launch in every PTY,
//! following a platform-specific fallback chain. `setup_shell_integration`
//! writes small per-shell rc scripts into Paneflow's local data dir
//! (`runtime_paths::shell_integration_dir`) and returns the extra CLI
//! args/env needed to wire them in.
//!
//! Keep this module shell-specific: no terminal state, no GPUI.

use std::collections::HashMap;

use paneflow_config::schema::TerminalSurfaceProfile;

/// zsh: ZDOTDIR-based injection. Our `.zshenv` restores the original ZDOTDIR
/// so all other dotfiles (`.zshrc`, `.zprofile`) load from `$HOME` as usual.
///
/// AI-hook PATH-prepend (re-applied via `precmd`): the PTY-level
/// `$PATH` prepend in `pty_session::inject_ai_hook_env` is invariably
/// undone by user `.zshrc`/`.bashrc` lines like
/// `export PATH="$HOME/.local/bin:$PATH"`, which demote PaneFlow's bin
/// dir behind the user's `~/.local/bin/claude` and bypass the shim
/// entirely. We re-prepend before every prompt - first invocation runs
/// after `.zshrc` finishes, so the first `claude` typed at the prompt
/// resolves to the shim. Idempotent + O(1) string work, invisible cost.
const ZSH_OSC7: &str = r#"# PaneFlow shell integration - OSC 7 CWD reporting
if [[ -n "${PANEFLOW_ORIG_ZDOTDIR+x}" ]]; then
    ZDOTDIR="${PANEFLOW_ORIG_ZDOTDIR}"
    unset PANEFLOW_ORIG_ZDOTDIR
else
    unset ZDOTDIR
fi
[[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]] && source "${ZDOTDIR:-$HOME}/.zshenv"
__paneflow_osc7() {
    # Index UTF-8 bytes. MULTIBYTE subscripting stays on if `LC_ALL=C` does not retune it,
    # and a low-byte mask then turns U+4E2D into '-'.
    emulate -L zsh
    setopt localoptions nomultibyte
    local LC_ALL=C
    local encoded='' byte hex code i
    for ((i=0; i<${#PWD}; i++)); do
        byte="${PWD:$i:1}"
        case "$byte" in
            [a-zA-Z0-9/._~-]) encoded+="$byte" ;;
            *) printf -v code '%d' "'$byte"; printf -v hex '%%%02X' "$code"; encoded+="$hex" ;;
        esac
    done
    printf '\e]7;file://%s%s\a' "${HOST}" "$encoded"
}
__paneflow_path_prepend() {
    [[ -z "${PANEFLOW_BIN_DIR-}" ]] && return
    # Strip every existing occurrence then prepend, keeping our dir first
    # regardless of what `.zshrc`/`.zprofile` did. Uses zsh's `path` tied
    # array so the change propagates to `$PATH` automatically.
    path=("${PANEFLOW_BIN_DIR}" "${(@)path:#${PANEFLOW_BIN_DIR}}")
}
autoload -Uz add-zsh-hook
if [[ -o interactive ]]; then
    __paneflow_osc133_precmd() {
        local ret=$?
        if [[ -n "${__paneflow_cmd_ran-}" ]]; then
            printf '\e]133;D;%s\a' "${ret}"
            unset __paneflow_cmd_ran
        fi
        printf '\e]133;A\a'
    }
    __paneflow_osc133_preexec() {
        __paneflow_cmd_ran=1
        printf '\e]133;C\a'
    }
    add-zsh-hook precmd __paneflow_osc133_precmd
    add-zsh-hook preexec __paneflow_osc133_preexec
fi
add-zsh-hook chpwd __paneflow_osc7
add-zsh-hook precmd __paneflow_path_prepend
__paneflow_osc7
__paneflow_path_prepend
"#;

/// bash: `--rcfile` replacement. Sources the real `.bashrc`, then appends
/// our OSC 7 function to PROMPT_COMMAND (preserving starship/oh-my-bash/etc.).
/// Same AI-hook PATH-prepend rationale as ZSH_OSC7 - PROMPT_COMMAND fires
/// before each prompt, after `.bashrc` has run.
const BASH_OSC7: &str = r#"# PaneFlow shell integration - OSC 7 CWD reporting
[[ -f ~/.bashrc ]] && source ~/.bashrc
__paneflow_osc7() {
    # `LC_ALL=C` makes `${PWD:$i:1}` one byte. bash 3.2 still reports 128..=255 as a
    # negative code; `%02X` would widen that. A low-byte mask turns U+4E2D into '-'.
    local LC_ALL=C
    local encoded='' byte hex code i
    for ((i=0; i<${#PWD}; i++)); do
        byte="${PWD:$i:1}"
        case "$byte" in
            [a-zA-Z0-9/._~-]) encoded+="$byte" ;;
            *)
                printf -v code '%d' "'$byte"
                code=$((code < 0 ? code + 256 : code))
                printf -v hex '%%%02X' "$code"
                encoded+="$hex"
                ;;
        esac
    done
    printf '\e]7;file://%s%s\a' "${HOSTNAME}" "$encoded"
}
__paneflow_path_prepend() {
    [[ -z "${PANEFLOW_BIN_DIR-}" ]] && return
    local p=":${PATH}:"
    p="${p//:${PANEFLOW_BIN_DIR}:/:}"
    p="${p#:}"; p="${p%:}"
    PATH="${PANEFLOW_BIN_DIR}:${p}"
    export PATH
}
__paneflow_osc133_precmd() {
    local ret=$?
    if [[ "${HISTCMD-0}" != "${__paneflow_histcmd-}" ]]; then
        [[ -n "${__paneflow_histcmd-}" ]] && printf '\e]133;D;%s\a' "${ret}"
        __paneflow_histcmd="${HISTCMD-0}"
    fi
    printf '\e]133;A\a'
}
PS0=$'\e]133;C\a'"${PS0-}"
PROMPT_COMMAND="__paneflow_osc133_precmd;__paneflow_osc7;__paneflow_path_prepend${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
__paneflow_path_prepend
"#;

/// fish: `--init-command` sourced script. Uses `--on-variable PWD` so it
/// fires on every directory change independently of the prompt function.
/// fish `--init-command` runs AFTER `config.fish`, so a one-shot prepend
/// is sufficient - but `fish_add_path -gp` is idempotent so a re-source
/// of this file is also safe.
const FISH_OSC7: &str = r#"# PaneFlow shell integration - OSC 7 CWD reporting
function __paneflow_osc7 --on-variable PWD
    set -l encoded (string escape --style=url -- "$PWD" | string replace -ai '%2F' '/')
    printf '\e]7;file://%s%s\a' (hostname) "$encoded"
end
__paneflow_osc7
if set -q PANEFLOW_BIN_DIR; and test -n "$PANEFLOW_BIN_DIR"
    fish_add_path -gp $PANEFLOW_BIN_DIR
end
if status is-interactive
    function __paneflow_osc133_prompt --on-event fish_prompt
        printf '\e]133;A\a'
    end
    function __paneflow_osc133_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end
    function __paneflow_osc133_postexec --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
    end
end
"#;

/// PowerShell 5.1 / 7 (pwsh): dot-sourced via `-NoExit -Command ". <path>"`,
/// which runs AFTER the user's `$PROFILE`, so any `prompt` function they
/// defined is already in place. We capture it as a ScriptBlock and wrap it
/// non-destructively so their prompt still renders while we emit OSC 7.
///
/// BEL terminator (``a``) matches the zsh/bash/fish emitters so PaneFlow's
/// shared OSC 7 parser handles Windows and Unix identically.
///
/// US-012 - prd-windows-port.md.
const PWSH_OSC7: &str = r#"# PaneFlow shell integration - OSC 7 CWD reporting (US-012)
# Non-destructive: wraps the existing `prompt` function so the user's
# prompt still renders. Loaded via `pwsh -NoExit -Command ". <this>"`.
# Dot-sourcing happens AFTER $PROFILE, so any user PATH mutations there
# have already run -- a one-shot prepend is sufficient. The `prompt`
# wrapper additionally re-asserts the prepend on every prompt for users
# who modify $env:PATH at runtime.

function global:__paneflow_path_prepend {
    if ([string]::IsNullOrEmpty($env:PANEFLOW_BIN_DIR)) { return }
    $sep = [System.IO.Path]::PathSeparator
    $entries = $env:PATH -split [regex]::Escape($sep) | Where-Object { $_ -ne $env:PANEFLOW_BIN_DIR }
    $env:PATH = (@($env:PANEFLOW_BIN_DIR) + $entries) -join $sep
}

function global:__paneflow_cwd_uri {
    $providerPath = (Get-Location).ProviderPath
    if ([string]::IsNullOrEmpty($providerPath)) { return $null }
    try {
        return ([System.Uri]$providerPath).AbsoluteUri
    } catch {
        return $null
    }
}

# PSReadLine owns the pre-exec boundary on PowerShell. Wrap its existing
# entry point after the user's profile has loaded so custom key handlers and
# prompt frameworks stay intact. The accepted line is returned unchanged.
if (-not $global:__paneflow_readline_wrapped -and (Test-Path function:PSConsoleHostReadLine)) {
    $global:__paneflow_prev_readline = $function:PSConsoleHostReadLine
    function global:PSConsoleHostReadLine {
        $__paneflow_line = & $global:__paneflow_prev_readline
        if (-not [string]::IsNullOrWhiteSpace([string]$__paneflow_line)) {
            [Console]::Write("$([char]27)]133;C$([char]7)")
        }
        $__paneflow_line
    }
    $global:__paneflow_readline_wrapped = $true
}

# Capture the CURRENT prompt as a ScriptBlock VALUE (snapshot) via
# `$function:prompt`, NOT `Get-Item function:prompt`. A FunctionInfo from
# Get-Item is a LIVE handle: its `.ScriptBlock` re-resolves to whatever
# `prompt` is at call time, which after we redefine `prompt` below is OUR
# wrapper -- so `& $prev.ScriptBlock` calls the wrapper again, recursing
# forever ("call depth overflow") and the prompt never renders. This bites
# hardest with Starship / oh-my-posh, which also redefine `prompt`. The
# $global:__paneflow_prompt_wrapped guard keeps a re-source from capturing
# our own wrapper as the "previous" prompt.
if (-not $global:__paneflow_prompt_wrapped) {
    $global:__paneflow_prev_prompt = $function:prompt
    function global:prompt {
        $__paneflow_ok = $?
        $__paneflow_last_exit = $global:LASTEXITCODE
        $__paneflow_history = (Get-History -Count 1).Id
        # Call the wrapped prompt FIRST, while $?/$LASTEXITCODE still reflect
        # the user's last command -- Starship / oh-my-posh read them to render
        # the exit-status segment. Our OSC 7 + PATH bookkeeping runs after.
        $global:LASTEXITCODE = $__paneflow_last_exit
        $__paneflow_out = if ($global:__paneflow_prev_prompt) { & $global:__paneflow_prev_prompt } else { "PS $($executionContext.SessionState.Path.CurrentLocation)> " }
        if ($null -ne $global:__paneflow_previous_history -and $__paneflow_history -ne $global:__paneflow_previous_history) {
            $__paneflow_code = if ($__paneflow_ok) { 0 } elseif ($null -ne $__paneflow_last_exit) { $__paneflow_last_exit } else { 1 }
            [Console]::Write("$([char]27)]133;D;$__paneflow_code$([char]7)")
        }
        $global:__paneflow_previous_history = $__paneflow_history
        [Console]::Write("$([char]27)]133;A$([char]7)")
        # OSC 7 with BEL terminator (matches zsh/bash/fish emitters). Use
        # [char]27 instead of `e: Windows PowerShell 5.1 treats `e as a
        # literal "e", which leaks "e]7;..." into the terminal.
        $__paneflow_cwd_uri = __paneflow_cwd_uri
        if ($__paneflow_cwd_uri) {
            [Console]::Write("$([char]27)]7;$__paneflow_cwd_uri$([char]7)")
        }
        __paneflow_path_prepend
        $__paneflow_out
    }
    $global:__paneflow_prompt_wrapped = $true
}
__paneflow_path_prepend
"#;

/// Resolve the default shell path following a platform-specific fallback chain
/// (US-006 - prd-windows-port.md). Returns the path that should be passed to
/// `portable-pty`'s `CommandBuilder::new`.
///
/// Unix chain: configured (if executable) → `$SHELL` → `/bin/sh`.
pub(super) fn resolve_default_shell(configured: Option<&str>) -> String {
    if let Some(path) = configured {
        if let Some(resolved) = configured_shell_if_usable(path) {
            return resolved;
        }
        log::warn!(
            "Configured default_shell {:?} not found or not executable, \
             falling back to platform defaults",
            path
        );
    }
    resolve_default_shell_fallback()
}

/// Validate that a user-configured shell entry resolves to an executable file.
/// Bare names (no path separators) are searched on PATH via `which` - this is
/// what lets `"default_shell": "pwsh"` work without the user having to
/// hard-code `/opt/homebrew/bin/pwsh`.
fn configured_shell_if_usable(path: &str) -> Option<String> {
    let has_separator = path.contains('/');
    let candidate: std::path::PathBuf = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else if has_separator {
        std::path::PathBuf::from(path)
    } else {
        // PATH search first; fall back to well-known install dirs so a
        // bare `"pwsh"` configured shell still resolves under a GUI launch whose
        // inherited PATH omits `/opt/homebrew/bin`. Without this, the entry was
        // silently rejected and the shell fell back to `/bin/sh`.
        which::which(path)
            .ok()
            .or_else(|| well_known_shell_dir_lookup(path))?
    };
    let is_executable = candidate.is_file() && {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&candidate)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
    };
    if is_executable {
        Some(candidate.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Probe a small set of well-known Unix install directories for a bare shell
/// name that the PATH search (`which`) missed. Covers the Homebrew prefixes
/// (`/opt/homebrew/bin` on Apple Silicon, `/usr/local/bin` on Intel) plus the
/// system dirs, so a configured `"pwsh"` / `"fish"` / etc. resolves even when a
/// GUI-launched process inherited a minimal PATH. The executable-bit check
/// is left to the caller.
fn well_known_shell_dir_lookup(name: &str) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        const DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
        DIRS.iter()
            .map(|dir| std::path::Path::new(dir).join(name))
            .find(|candidate| candidate.is_file())
    }
}

#[cfg(unix)]
fn resolve_default_shell_fallback() -> String {
    resolve_unix_default_shell_fallback(std::env::var("SHELL").ok().as_deref())
}

#[cfg(unix)]
fn resolve_unix_default_shell_fallback(shell_env: Option<&str>) -> String {
    if let Some(shell) = shell_env
        && let Some(resolved) = configured_shell_if_usable(shell)
    {
        return resolved;
    }
    if let Some(shell) = shell_env
        && !shell.trim().is_empty()
    {
        log::warn!(
            "SHELL {:?} not found or not executable, falling back to /bin/sh",
            shell
        );
    }
    configured_shell_if_usable("/bin/sh").unwrap_or_else(|| "/bin/sh".to_string())
}

/// Build a command that clears the terminal before launching an interactive
/// program, using syntax supported by the shell that will own the PTY.
///
/// In particular, `pwsh` does not support `&&` and uses `Clear-Host;`. When no
/// shell is configured, the platform fallback is resolved before selecting
/// syntax.
pub(crate) fn clear_then(command: &str, configured_shell: Option<&str>) -> String {
    clear_then_for_shell(command, &resolve_default_shell(configured_shell))
}

fn clear_then_for_shell(command: &str, shell: &str) -> String {
    let basename = shell
        .rsplit('/')
        .next()
        .unwrap_or(shell)
        .to_ascii_lowercase();
    match basename.as_str() {
        "pwsh" => format!("Clear-Host; {command}"),
        // Known POSIX shells: `clear` + `&&` sequencing is universally
        // supported (fish ≥3.0 included).
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "ash" | "mksh" => {
            format!("clear && {command}")
        }
        // US-042: unknown shell (nushell, elvish, xonsh, …) - don't assume
        // `&&`/`clear` exist. Launch the command bare so an exotic shell
        // doesn't eat a syntax error on the very first line.
        _ => command.to_string(),
    }
}

/// Render a filesystem path for a POSIX shell's rcfile/init argument.
fn to_shell_path(p: &std::path::Path) -> String {
    p.display().to_string()
}

/// Write OSC 7 shell integration scripts and return the extra shell args
/// and env vars needed to activate them. Scripts are written to
/// `runtime_paths::shell_integration_dir()/{zsh,bash,fish,pwsh}/`.
///
/// Unchanged bytes are left in place. A change is a same-directory rename so a
/// shell spawned by another pane never sources a truncated file.
///
/// Supported shells:
/// - **zsh, bash, fish** - BEL-terminated OSC 7 via per-prompt hooks.
/// - **pwsh** (PowerShell 7) (US-012) - `prompt` function wrapper,
///   dot-sourced so the user's `$PROFILE`-defined prompt still renders.
/// - **Shells without injection** (nushell, elvish, xonsh): rely on
///   `cwd_now()` fallback. On macOS this requires `proc_pidinfo()`.
pub(super) fn setup_shell_integration(
    shell: &str,
    env: &mut HashMap<String, String>,
    profile: TerminalSurfaceProfile,
) -> Vec<String> {
    let Some(base) = crate::runtime_paths::shell_integration_dir() else {
        return vec![];
    };

    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(shell);
    let key = basename.to_ascii_lowercase();
    match key.as_str() {
        "zsh" => {
            let dir = base.join("zsh");
            if std::fs::create_dir_all(&dir).is_err() {
                return vec![];
            }
            // U-022: if the rc write fails, abort activation rather than
            // hijacking ZDOTDIR to point at a dir with no `.zshenv` - that
            // would suppress the user's real zsh startup AND give no
            // integration. Bail before touching `env`.
            if publish_shell_integration_script(&dir.join(".zshenv"), ZSH_OSC7).is_err() {
                return vec![];
            }
            if let Ok(orig) = std::env::var("ZDOTDIR") {
                env.insert("PANEFLOW_ORIG_ZDOTDIR".into(), orig);
            }
            env.insert("ZDOTDIR".into(), dir.display().to_string());
            vec![]
        }
        "bash" => {
            let dir = base.join("bash");
            if std::fs::create_dir_all(&dir).is_err() {
                return vec![];
            }
            let rcfile = dir.join("bashrc");
            // U-022: abort if the write fails - handing bash `--rcfile <path>`
            // for a file that doesn't exist breaks startup instead of
            // gracefully falling back to the user's normal `.bashrc`.
            if publish_shell_integration_script(&rcfile, BASH_OSC7).is_err() {
                return vec![];
            }
            vec!["--rcfile".into(), to_shell_path(&rcfile)]
        }
        "fish" => {
            let dir = base.join("fish");
            if std::fs::create_dir_all(&dir).is_err() {
                return vec![];
            }
            let initfile = dir.join("osc7.fish");
            // U-022: abort if the write fails - sourcing a missing init file
            // errors fish startup rather than degrading cleanly.
            if publish_shell_integration_script(&initfile, FISH_OSC7).is_err() {
                return vec![];
            }
            vec![
                "--init-command".into(),
                format!("source {}", quote_fish_arg(&to_shell_path(&initfile))),
            ]
        }
        // US-012 - PowerShell 7 (`pwsh`) uses `function prompt { ... }` as the
        // hook. `-NoExit` keeps the shell interactive after the init command;
        // `-Command ". 'path'"` dot-sources our script AFTER the user's
        // `$PROFILE` has loaded any `prompt` they defined (so we can wrap
        // rather than replace it).
        "pwsh" => {
            let dir = base.join("pwsh");
            if std::fs::create_dir_all(&dir).is_err() {
                return vec![];
            }
            let initfile = dir.join("osc7.ps1");
            // U-022: abort if the write fails - dot-sourcing a missing script
            // breaks the pwsh session rather than degrading cleanly.
            if publish_shell_integration_script(&initfile, PWSH_OSC7).is_err() {
                return vec![];
            }
            // Single-quote the path and escape any embedded single
            // quotes ('' is the literal single-quote inside a single-
            // quoted PowerShell string). Guards against pathological
            // usernames without breaking the common case.
            let escaped = initfile.display().to_string().replace('\'', "''");
            powershell_startup_args(profile, format!(". '{escaped}'"))
        }
        _ => vec![],
    }
}

/// Publish `contents` at `path` without truncating the live file.
///
/// Pane spawns share these rc scripts. `std::fs::write` empties the file
/// before the new bytes land, so a shell that sources it in that window
/// skips the user's startup files (#703). Matching bytes are left untouched.
/// A change is written to a unique temporary file in the same directory and
/// renamed over `path`, which is atomic on the same volume. Dropping the
/// temporary file removes it when the write or rename fails.
fn publish_shell_integration_script(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    match std::fs::read(path) {
        Ok(existing) if existing == contents.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell integration script path has no parent directory",
        ));
    };

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    // `PersistError` still owns the temporary file; dropping it deletes it.
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn powershell_startup_args(profile: TerminalSurfaceProfile, init_command: String) -> Vec<String> {
    let mut args = Vec::new();
    if matches!(profile, TerminalSurfaceProfile::Agent) {
        args.push("-NoProfile".into());
    }
    args.extend(["-NoExit".into(), "-Command".into(), init_command]);
    args
}

fn quote_fish_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for ch in arg.chars() {
        match ch {
            '\\' | '"' | '$' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::{clear_then_for_shell, powershell_startup_args};
    use paneflow_config::schema::TerminalSurfaceProfile;

    // (B) Unix well-known-dir shell lookup: a bare name not on PATH still
    // resolves from a standard install dir (the macOS pwsh-under-Homebrew gap),
    // while a bogus name yields None. `/bin/sh` exists on every Unix target.
    #[cfg(unix)]
    #[test]
    fn osc7_emitters_encode_path_bytes_without_extra_frames() {
        for (shell, source) in [
            ("/bin/bash", super::BASH_OSC7),
            ("/bin/zsh", super::ZSH_OSC7),
        ] {
            let start = source.find("__paneflow_osc7() {").unwrap();
            let end = start + source[start..].find("\n}").unwrap() + 2;
            let script = format!("{}\n__paneflow_osc7", &source[start..end]);
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("percent%20 space#?é\u{7}\u{1b}\n");
            std::fs::create_dir(&cwd).unwrap();
            let output = std::process::Command::new(shell)
                .args(["-f", "-c", &script])
                .current_dir(&cwd)
                .output()
                .unwrap();
            assert!(output.status.success(), "{shell}: {:?}", output.stderr);
            let frame = String::from_utf8(output.stdout).unwrap();
            assert_eq!(frame.matches('\u{1b}').count(), 1, "{shell}: {frame:?}");
            assert_eq!(frame.matches('\u{7}').count(), 1, "{shell}: {frame:?}");
            assert!(
                frame.ends_with("/percent%2520%20space%23%3F%C3%A9%07%1B%0A\u{7}"),
                "{shell}: {frame:?}"
            );
        }
    }

    /// U+4E2D is one character and three UTF-8 bytes. Character subscripting plus
    /// `code & 255` reports that directory as `-`, which the session then stores.
    #[cfg(unix)]
    #[test]
    fn osc7_emitters_encode_cjk_directory_as_utf8_bytes() {
        assert!(
            super::ZSH_OSC7.contains("emulate -L zsh"),
            "zsh must enter zsh emulation before subscripting"
        );
        assert!(
            super::ZSH_OSC7.contains("setopt localoptions nomultibyte"),
            "zsh must index PWD as bytes even when MULTIBYTE is on"
        );
        assert!(
            !super::ZSH_OSC7.contains("& 255")
                && !super::ZSH_OSC7.contains("&255")
                && !super::BASH_OSC7.contains("& 255")
                && !super::BASH_OSC7.contains("&255"),
            "masking a codepoint with 255 encodes U+4E2D as '-'"
        );

        let marker = "\u{4E2D}";
        for (shell, source) in [
            ("/bin/bash", super::BASH_OSC7),
            ("/bin/zsh", super::ZSH_OSC7),
        ] {
            let start = source.find("__paneflow_osc7() {").unwrap();
            let end = start + source[start..].find("\n}").unwrap() + 2;
            let mut script = String::new();
            if shell.ends_with("zsh") {
                script.push_str("setopt multibyte\n");
            }
            script.push_str(&source[start..end]);
            script.push_str("\n__paneflow_osc7");

            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join(marker);
            std::fs::create_dir(&cwd).unwrap();
            let output = std::process::Command::new(shell)
                .args(["-f", "-c", &script])
                .env("LC_ALL", "C.UTF-8")
                .current_dir(&cwd)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{shell}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let frame = String::from_utf8(output.stdout).unwrap();
            assert_eq!(frame.matches('\u{1b}').count(), 1, "{shell}: {frame:?}");
            assert_eq!(frame.matches('\u{7}').count(), 1, "{shell}: {frame:?}");
            assert!(
                frame.contains("%E4%B8%AD"),
                "{shell}: U+4E2D must be UTF-8 percent-encoding, got {frame:?}"
            );
            assert!(
                !frame.contains("%2D"),
                "{shell}: U+4E2D must not encode as '-', got {frame:?}"
            );
            let decoded = percent_decode_osc7_path(&frame);
            assert!(
                decoded.ends_with(marker) && !decoded.ends_with('-'),
                "{shell}: decoded {decoded:?} from {frame:?}"
            );
            assert_eq!(
                std::fs::canonicalize(&decoded).unwrap(),
                std::fs::canonicalize(&cwd).unwrap(),
                "{shell}: stdout path {decoded:?}"
            );

            let mut state = super::super::TerminalState::new_display_only(6, 80);
            state.write_output(frame.as_bytes());
            state.sync();
            let stored = state
                .current_cwd
                .clone()
                .expect("OSC 7 should set the session cwd");
            assert!(
                stored.ends_with(marker) && !stored.ends_with('-'),
                "{shell}: session cwd {stored:?} from {frame:?}"
            );
            assert_eq!(
                std::fs::canonicalize(&stored).unwrap(),
                std::fs::canonicalize(&cwd).unwrap(),
                "{shell}: session cwd {stored:?}"
            );
        }
    }

    /// Path from one BEL-terminated OSC 7 frame. Stdout is checked on its own so a
    /// `%2D` encoding fails even when the session drops the frame.
    #[cfg(unix)]
    fn percent_decode_osc7_path(frame: &str) -> String {
        let marker = "]7;file://";
        let start = frame.find(marker).expect("OSC 7 file URI");
        let rest = &frame[start + marker.len()..];
        let path_at = rest.find('/').expect("absolute OSC 7 path");
        let encoded = rest[path_at..]
            .split(['\u{7}', '\u{1b}'])
            .next()
            .expect("OSC 7 path");
        let bytes = encoded.as_bytes();
        let mut output = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                assert!(
                    index + 2 < bytes.len(),
                    "truncated percent escape in {encoded:?}"
                );
                let high = hex_digit(bytes[index + 1]).expect("percent high digit");
                let low = hex_digit(bytes[index + 2]).expect("percent low digit");
                output.push((high << 4) | low);
                index += 3;
            } else {
                output.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8(output).expect("OSC 7 path must be UTF-8 after percent-decoding")
    }

    #[cfg(unix)]
    fn hex_digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    #[test]
    fn tilde_default_shell_expands_to_home() {
        assert!(
            super::configured_shell_if_usable("~/definitely-not-a-paneflow-shell").is_none(),
            "a missing ~/path must not be treated as a relative directory named ~"
        );
        let tilde = super::configured_shell_if_usable("~/../../bin/sh")
            .expect("~/../../bin/sh must expand under $HOME");
        let abs = super::configured_shell_if_usable("/bin/sh").expect("/bin/sh");
        assert_eq!(
            std::fs::canonicalize(&tilde).expect("tilde path"),
            std::fs::canonicalize(&abs).expect("abs path"),
            "~/../../bin/sh must be the same file as /bin/sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn well_known_shell_lookup_finds_sh_and_rejects_bogus() {
        // Resolves to a real `sh` file from some standard dir - the exact dir
        // varies (`/bin/sh` on macOS, `/usr/bin/sh` on many Linux distros), so
        // assert the basename, not the full path.
        let found = super::well_known_shell_dir_lookup("sh");
        assert!(
            found
                .as_deref()
                .is_some_and(|p| p.is_file() && p.file_name() == Some(std::ffi::OsStr::new("sh"))),
            "a bare `sh` must resolve from the well-known Unix dirs, got {found:?}"
        );
        assert!(
            super::well_known_shell_dir_lookup("definitely-not-a-real-shell-xyz").is_none(),
            "a non-existent bare name must not resolve"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_fallback_rejects_stale_shell_env() {
        let shell = super::resolve_unix_default_shell_fallback(Some(
            "/definitely/not/a/real/paneflow-shell",
        ));
        assert!(
            std::path::Path::new(&shell)
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new("sh")),
            "stale SHELL must fall back to sh, got {shell:?}"
        );
    }

    #[test]
    fn fish_init_command_quotes_spaces_and_metacharacters() {
        assert_eq!(
            super::quote_fish_arg("/Users/a/Application Support/paneflow/osc7.fish"),
            "\"/Users/a/Application Support/paneflow/osc7.fish\""
        );
        assert_eq!(
            super::quote_fish_arg("/tmp/$USER/osc7\"hook\".fish"),
            "\"/tmp/\\$USER/osc7\\\"hook\\\".fish\""
        );
    }

    #[test]
    fn clear_then_uses_powershell_51_compatible_syntax() {
        assert_eq!(clear_then_for_shell("claude", "pwsh"), "Clear-Host; claude");
        assert_eq!(
            clear_then_for_shell("kiro-cli chat", "pwsh"),
            "Clear-Host; kiro-cli chat"
        );
    }

    #[test]
    fn clear_then_uses_posix_syntax_for_unix_shells() {
        assert_eq!(
            clear_then_for_shell("opencode", "/bin/zsh"),
            "clear && opencode"
        );
        assert_eq!(
            clear_then_for_shell("kiro-cli chat", "/bin/zsh"),
            "clear && kiro-cli chat"
        );
    }

    #[test]
    fn clear_then_known_posix_shells_keep_clear() {
        for sh in ["/bin/bash", "/usr/bin/fish", "dash", "ksh", "/bin/sh"] {
            assert_eq!(clear_then_for_shell("x", sh), "clear && x", "shell {sh}");
        }
    }

    #[test]
    fn clear_then_unknown_shell_launches_bare() {
        // US-042: an unknown shell gets no clear prefix - we can't assume `&&`
        // or `clear` exist (nushell, elvish, xonsh, …).
        assert_eq!(clear_then_for_shell("opencode", "/usr/bin/nu"), "opencode");
        assert_eq!(clear_then_for_shell("claude", "elvish"), "claude");
    }

    #[test]
    fn pwsh_osc7_snapshots_prompt_and_avoids_recursion() {
        // Regression guard for the infinite-recursion bug that left the prompt
        // blank under Starship / oh-my-posh: capturing the previous prompt via a
        // live `Get-Item function:prompt` handle made `.ScriptBlock` re-resolve
        // to our own wrapper after redefinition -> "call depth overflow". The
        // fix snapshots the scriptblock by value (`$function:prompt`), invokes
        // it directly, and guards against re-wrapping.
        //
        // Asserted POSITIVELY (presence of the fixed code lines) rather than by
        // substring-absence: the anti-pattern strings (`Get-Item`,
        // `.ScriptBlock`) legitimately appear in this constant's own
        // explanatory comment, so an absence check would false-positive.
        let s = super::PWSH_OSC7;
        assert!(
            s.contains("$global:__paneflow_prev_prompt = $function:prompt"),
            "must snapshot the prompt by value via $function:prompt"
        );
        assert!(
            s.contains("& $global:__paneflow_prev_prompt"),
            "must invoke the captured scriptblock directly (not .ScriptBlock of a live handle)"
        );
        assert!(
            s.contains("__paneflow_prompt_wrapped"),
            "must guard against double-wrapping on re-source"
        );
    }

    #[test]
    fn pwsh_osc7_uses_powershell_51_safe_escape_and_file_uri() {
        let s = super::PWSH_OSC7;
        assert!(
            s.contains("$([char]27)]7;"),
            "OSC 7 must emit ESC via [char]27 for Windows PowerShell 5.1"
        );
        assert!(
            s.contains("$([char]7)"),
            "OSC 7 must emit BEL via [char]7 for Windows PowerShell 5.1"
        );
        assert!(
            s.contains("([System.Uri]$providerPath).AbsoluteUri"),
            "PowerShell CWD reporting must produce a real file:// URI"
        );
        assert!(
            !s.contains("`e]7;"),
            "`e is PowerShell 7-only for ESC and must not be used in shared 5.1/7 script"
        );
    }

    #[test]
    fn shell_integrations_emit_osc133_without_replacing_prompt_hooks() {
        assert!(super::ZSH_OSC7.contains("add-zsh-hook precmd __paneflow_osc133_precmd"));
        assert!(super::ZSH_OSC7.contains("add-zsh-hook preexec __paneflow_osc133_preexec"));
        assert!(super::BASH_OSC7.contains("PROMPT_COMMAND=\"__paneflow_osc133_precmd;"));
        assert!(super::BASH_OSC7.contains("PS0=$'\\e]133;C\\a'"));
        assert!(super::FISH_OSC7.contains("--on-event fish_postexec"));
        assert!(super::PWSH_OSC7.contains("function global:PSConsoleHostReadLine"));
        assert!(super::PWSH_OSC7.contains(")]133;C"));
        assert!(super::PWSH_OSC7.contains(")]133;D;"));
        assert!(super::PWSH_OSC7.contains(")]133;A"));
    }

    /// Readers of a shared rc file must observe only complete previous or new
    /// bytes. `std::fs::write` truncates first, so a tight reader loop would
    /// see an empty file while the replacement is in flight.
    #[test]
    fn shell_integration_rc_is_replaced_atomically() {
        use std::os::unix::fs::MetadataExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".zshenv");
        let first: Arc<str> = Arc::from("a".repeat(256 * 1024));
        let second: Arc<str> = Arc::from("b".repeat(256 * 1024));
        super::publish_shell_integration_script(&path, &first).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let saw_empty = Arc::new(AtomicBool::new(false));
        let saw_other = Arc::new(AtomicBool::new(false));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let path = path.clone();
            let first = Arc::clone(&first);
            let second = Arc::clone(&second);
            let stop = Arc::clone(&stop);
            let saw_empty = Arc::clone(&saw_empty);
            let saw_other = Arc::clone(&saw_other);
            readers.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match std::fs::read(&path) {
                        Ok(bytes)
                            if bytes.as_slice() == first.as_bytes()
                                || bytes.as_slice() == second.as_bytes() => {}
                        Ok(bytes) if bytes.is_empty() => {
                            saw_empty.store(true, Ordering::Relaxed);
                        }
                        _ => saw_other.store(true, Ordering::Relaxed),
                    }
                }
            }));
        }

        for _ in 0..24 {
            super::publish_shell_integration_script(&path, &second).unwrap();
            super::publish_shell_integration_script(&path, &second).unwrap();
            super::publish_shell_integration_script(&path, &first).unwrap();
            super::publish_shell_integration_script(&path, &first).unwrap();
        }

        let inode = std::fs::metadata(&path).unwrap().ino();
        super::publish_shell_integration_script(&path, &first).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "matching bytes must not replace the rc file"
        );

        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }

        assert!(
            !saw_empty.load(Ordering::Relaxed),
            "concurrent reader observed an empty shell rc"
        );
        assert!(
            !saw_other.load(Ordering::Relaxed),
            "concurrent reader observed a partial or missing shell rc"
        );
        assert_eq!(std::fs::read(&path).unwrap(), first.as_bytes());

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn powershell_agent_profile_skips_user_profile_noise() {
        assert_eq!(
            powershell_startup_args(TerminalSurfaceProfile::Agent, "init".into()),
            vec!["-NoProfile", "-NoExit", "-Command", "init"]
        );
        assert_eq!(
            powershell_startup_args(TerminalSurfaceProfile::Normal, "init".into()),
            vec!["-NoExit", "-Command", "init"]
        );
    }
}
