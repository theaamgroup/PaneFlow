//! External URL opening helpers.

pub(crate) fn open_url(url: &str) -> std::io::Result<()> {
    open_url_impl(url)
}

/// Open an untrusted URL after requiring `http://` or `https://`.
///
/// Feed `html_url` and other attacker-controlled strings must go through
/// this, not [`open_url`]. `file://` / `javascript:` / unknown schemes are
/// refused so they never reach `open::that`.
pub(crate) fn open_http_url(url: &str) -> std::io::Result<()> {
    let validated = require_http_url(url)?;
    open_url_impl(&validated)
}

pub(crate) fn require_http_url(url: &str) -> std::io::Result<String> {
    crate::markdown::security::validate_link_url(url)
        .map(|v| v.as_str().to_string())
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("refusing to open non-http(s) URL ({err:?})"),
            )
        })
}

fn open_url_impl(url: &str) -> std::io::Result<()> {
    open::that(url)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
