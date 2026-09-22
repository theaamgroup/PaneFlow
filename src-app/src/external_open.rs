//! External URL opening helpers.

use std::process::{Command, ExitStatus, Stdio};

/// How often a running workspace launcher is polled for its exit status.
const WORKSPACE_LAUNCH_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Launch an external workspace tool (an editor's CLI shim, `open`) and wait
/// for it to exit, entirely off the render thread (issue #530).
///
/// The spawn runs under `smol::unblock`, stdio is nulled so the child never
/// inherits the app's descriptors, and the binary, arguments, PID, and exit
/// status are logged at info level. `try_wait` is polled every
/// [`WORKSPACE_LAUNCH_POLL`] so a launcher that hangs keeps only this task
/// pending, never a frame. The status is returned to the caller, which is
/// what lets a broken `zed .` or a permission-denied `open` produce a toast
/// instead of vanishing silently.
pub(crate) async fn run_workspace_command(mut command: Command) -> std::io::Result<ExitStatus> {
    let description = format!(
        "binary={:?} cwd={:?} args={:?}",
        command.get_program(),
        command.get_current_dir(),
        command.get_args().collect::<Vec<_>>()
    );
    log::info!("workspace launch: {description}");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let result = smol::unblock(move || command.spawn()).await;
    let mut child = match result {
        Ok(child) => child,
        Err(error) => {
            log::warn!(
                "workspace launch failed: {description} error={error} os_error={:?}",
                error.raw_os_error()
            );
            return Err(error);
        }
    };
    log::info!("workspace spawned: {description} pid={}", child.id());
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                log::info!(
                    "workspace process exited: {description} pid={} status={status}",
                    child.id()
                );
                return Ok(status);
            }
            Ok(None) => {
                smol::Timer::after(WORKSPACE_LAUNCH_POLL).await;
            }
            Err(error) => {
                log::warn!("workspace process wait failed: {description} error={error}");
                return Err(error);
            }
        }
    }
}

pub(crate) fn open_url(url: &str) -> std::io::Result<()> {
    open_url_impl(url)
}

/// Open an untrusted URL after requiring `http://` or `https://`.
///
/// Untrusted or user-facing web links must go through this, not [`open_url`].
/// `file://` / `javascript:` / unknown schemes are refused so they never
/// reach `open::that`.
pub(crate) fn open_http_url(url: &str) -> std::io::Result<()> {
    let validated = require_http_url(url)?;
    open_url_impl(&validated)
}

pub(crate) fn require_http_url(url: &str) -> std::io::Result<String> {
    validate_link_url(url)
        .map(|v| v.as_str().to_string())
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("refusing to open non-http(s) URL ({err:?})"),
            )
        })
}

/// A URL that [`validate_link_url`] accepted. The wrapped string is the
/// caller's original text: no normalisation, so a UI can show it back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ValidatedUrl(String);

impl ValidatedUrl {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why [`validate_link_url`] refused a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
enum UrlError {
    DisallowedScheme(String),
    MissingScheme,
    TooLong,
    Malformed,
}

/// Hard cap on a link handed to the OS handler.
const MAX_LINK_URL_LEN: usize = 8 * 1024;

const ALLOWED_LINK_SCHEMES: &[&str] = &["http", "https"];

/// Accept only absolute `http://` and `https://` URLs.
///
/// `file://`, `javascript:`, `data:`, bare hosts, userinfo, whitespace, and
/// backslashes are refused. The scheme match is case-insensitive; the
/// returned string keeps the caller's original characters.
fn validate_link_url(url: &str) -> Result<ValidatedUrl, UrlError> {
    if url.len() > MAX_LINK_URL_LEN {
        return Err(UrlError::TooLong);
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(UrlError::Malformed);
    }
    let scheme = match extract_scheme(url) {
        Some(scheme) => scheme,
        None => return Err(UrlError::MissingScheme),
    };
    let scheme_lower = scheme.to_ascii_lowercase();
    if !ALLOWED_LINK_SCHEMES.contains(&scheme_lower.as_str()) {
        return Err(UrlError::DisallowedScheme(scheme_lower));
    }
    let rest = url
        .get(scheme.len()..)
        .filter(|rest| rest.starts_with("://"))
        .ok_or(UrlError::Malformed)?;
    let authority = &rest[3..];
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    if authority.is_empty() || authority.contains('@') {
        return Err(UrlError::Malformed);
    }
    Ok(ValidatedUrl(url.to_string()))
}

/// RFC 3986 scheme prefix. Minimum length two so a Windows drive letter
/// (`C:`) is not a scheme.
fn extract_scheme(input: &str) -> Option<&str> {
    let colon_idx = input.find(':')?;
    let prefix = &input[..colon_idx];
    if prefix.len() < 2 {
        return None;
    }
    let mut chars = prefix.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(prefix)
}

fn open_url_impl(url: &str) -> std::io::Result<()> {
    open::that(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/bin/sh` launcher stub: no Gatekeeper first-exec scan, so the
    /// bounded waits below never depend on it.
    fn sh_stub(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        path
    }

    #[test]
    fn workspace_command_reports_missing_executable() {
        let error = smol::block_on(run_workspace_command(Command::new(
            "paneflow-missing-workspace-editor-67",
        )))
        .expect_err("a missing launcher must surface as a spawn error");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn workspace_command_reports_a_failed_exit_status() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("workspace space & (\u{e9})");
        std::fs::create_dir(&cwd).expect("workspace dir");
        let launcher = sh_stub(
            temp.path(),
            "editor launcher.sh",
            "printf '%s' \"$1\" > arg.txt; exit 7",
        );
        let mut command = Command::new(launcher);
        command.current_dir(&cwd).arg(".");
        let status = smol::block_on(run_workspace_command(command)).expect("stub spawns");
        assert_eq!(
            status.code(),
            Some(7),
            "the launcher's exit must reach the caller"
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("arg.txt"))
                .expect("stub ran in the workspace")
                .trim(),
            ".",
            "the launcher must receive `.` inside the workspace cwd"
        );
    }

    #[test]
    fn workspace_command_nulls_the_launcher_stdio() {
        let temp = tempfile::tempdir().expect("tempdir");
        // `read` on a null stdin returns EOF at once; an inherited stdin
        // would block on the test harness's terminal or pipe instead.
        let launcher = sh_stub(
            temp.path(),
            "stdio probe.sh",
            "if read -r _line; then exit 3; fi; echo out; echo err >&2; exit 0",
        );
        let started = std::time::Instant::now();
        let status =
            smol::block_on(run_workspace_command(Command::new(launcher))).expect("stub spawns");
        assert!(status.success(), "stdin must be /dev/null: {status}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the launcher must not wait on inherited stdio"
        );
    }

    #[test]
    fn html_url_scheme_rejects_file_javascript_and_unknown() {
        for url in [
            "file:///bin/sh",
            "javascript:alert(1)",
            "data:text/html,<script>x</script>",
            "vbscript:msgbox",
            "smb://evil/share",
            "example.com",
            "",
        ] {
            let err = require_http_url(url).expect_err(url);
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{url}");
        }
    }

    #[test]
    fn html_url_scheme_accepts_http_and_https() {
        assert_eq!(
            require_http_url("https://github.com/theaamgroup/paneflow/releases/tag/v1").unwrap(),
            "https://github.com/theaamgroup/paneflow/releases/tag/v1"
        );
        assert_eq!(
            require_http_url("http://127.0.0.1:8080/").unwrap(),
            "http://127.0.0.1:8080/"
        );
        assert_eq!(
            require_http_url("HTTPS://github.com/x").unwrap(),
            "HTTPS://github.com/x"
        );
    }

    #[test]
    fn link_url_https_is_accepted() {
        let url = validate_link_url("https://example.com/path?q=1").expect("https accepted");
        assert_eq!(url.as_str(), "https://example.com/path?q=1");
    }

    #[test]
    fn link_url_http_is_accepted() {
        let url = validate_link_url("http://localhost:3000/").expect("http accepted");
        assert_eq!(url.as_str(), "http://localhost:3000/");
    }

    #[test]
    fn link_url_file_is_rejected() {
        let err = validate_link_url("file:///bin/sh").expect_err("file rejected");
        assert!(matches!(err, UrlError::DisallowedScheme(scheme) if scheme == "file"));
    }

    #[test]
    fn link_url_javascript_is_rejected() {
        let err = validate_link_url("javascript:alert(1)").expect_err("js rejected");
        assert!(matches!(err, UrlError::DisallowedScheme(scheme) if scheme == "javascript"));
    }

    #[test]
    fn link_url_data_is_rejected() {
        let err =
            validate_link_url("data:text/html,<script>x</script>").expect_err("data rejected");
        assert!(matches!(err, UrlError::DisallowedScheme(scheme) if scheme == "data"));
    }

    #[test]
    fn link_url_vbscript_is_rejected() {
        let err = validate_link_url("vbscript:msgbox").expect_err("vbscript rejected");
        assert!(matches!(err, UrlError::DisallowedScheme(scheme) if scheme == "vbscript"));
    }

    #[test]
    fn link_url_bare_string_is_rejected() {
        let err = validate_link_url("example.com").expect_err("bare host rejected");
        assert!(matches!(err, UrlError::MissingScheme));
    }

    #[test]
    fn link_url_scheme_match_is_case_insensitive() {
        let url = validate_link_url("HTTPS://example.com").expect("https accepted");
        assert_eq!(url.as_str(), "HTTPS://example.com");
    }

    #[test]
    fn link_url_allowed_scheme_must_be_absolute() {
        let err = validate_link_url("https:example.com/path").expect_err("relative rejected");
        assert!(matches!(err, UrlError::Malformed));
        let err = validate_link_url("https:///path").expect_err("empty authority rejected");
        assert!(matches!(err, UrlError::Malformed));
    }

    #[test]
    fn link_url_rejects_confusing_payloads() {
        let err = validate_link_url("https://example.com/\nfile:///bin/sh")
            .expect_err("newline rejected");
        assert!(matches!(err, UrlError::Malformed));
        let err = validate_link_url("https:\\\\example.com").expect_err("backslash rejected");
        assert!(matches!(err, UrlError::Malformed));
        let err =
            validate_link_url("https://user@example.com/path").expect_err("userinfo rejected");
        assert!(matches!(err, UrlError::Malformed));
    }

    #[test]
    fn link_url_too_long_is_rejected() {
        let huge = format!("https://x.com/{}", "a".repeat(MAX_LINK_URL_LEN));
        let err = validate_link_url(&huge).expect_err("oversized rejected");
        assert!(matches!(err, UrlError::TooLong));
    }

    #[test]
    fn allowlist_is_http_https_only() {
        assert_eq!(ALLOWED_LINK_SCHEMES, &["http", "https"]);
    }
}
