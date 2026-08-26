//! Fail-closed redirect policy for every update-flow HTTP GET.
//!
//! ureq follows 3xx hops internally and only the URL we passed in was
//! checked against [`super::checker::is_allowed_update_url`]. A compromised
//! or confused-deputy feed host could 302 off the allow-list (or onto
//! `file:`) and we would download those bytes. These helpers disable ureq
//! auto-follow and re-validate each `Location` before the next GET.

use ureq::Body;
use ureq::http::Response;

use super::checker::is_allowed_update_url;

/// Matches ureq's default. Counted as followed hops, not total GETs.
pub(crate) const MAX_UPDATE_REDIRECTS: u32 = 10;

#[derive(Debug)]
pub(crate) enum UpdateHttpError {
    Transport(ureq::Error),
    Policy(String),
}

impl std::fmt::Display for UpdateHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Policy(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for UpdateHttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            Self::Policy(_) => None,
        }
    }
}

impl From<ureq::Error> for UpdateHttpError {
    fn from(e: ureq::Error) -> Self {
        Self::Transport(e)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RedirectHop {
    Done,
    Next(String),
}

/// RFC 9110 redirect statuses that carry a new request-URI. 304 is 3xx but
/// is not a hop.
fn is_http_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Resolve `Location` against the request URL. Conservative: reject
/// whitespace, controls, backslashes, and empty values rather than guess.
pub(crate) fn resolve_redirect_location(current: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if location
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    if location.contains("://") {
        return Some(location.to_string());
    }
    let (scheme, rest) = current.split_once("://")?;
    if let Some(rest) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let (authority, path_query) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if location.starts_with('/') {
        return Some(format!("{scheme}://{authority}{location}"));
    }
    let path = path_query.split(['?', '#']).next().unwrap_or(path_query);
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Some(format!("{scheme}://{authority}{dir}{location}"))
}

/// Next hop, or `None` if `Location` is missing, malformed, or off-list.
pub(crate) fn allowed_redirect_url(current_url: &str, location: &str) -> Option<String> {
    let next = resolve_redirect_location(current_url, location)?;
    is_allowed_update_url(&next).then_some(next)
}

pub(crate) fn redirect_hop(
    current_url: &str,
    status: u16,
    location: Option<&str>,
) -> Result<RedirectHop, UpdateHttpError> {
    if !is_http_redirect(status) {
        return Ok(RedirectHop::Done);
    }
    let Some(location) = location.filter(|s| !s.is_empty()) else {
        return Err(UpdateHttpError::Policy(format!(
            "update redirect from {current_url} had HTTP {status} with no Location"
        )));
    };
    match allowed_redirect_url(current_url, location) {
        Some(next) => Ok(RedirectHop::Next(next)),
        None => Err(UpdateHttpError::Policy(format!(
            "update redirect from {current_url} left the allow-list ({location})"
        ))),
    }
}

/// GET `start_url`, re-validating every redirect hop. `fetch` must be built
/// with `max_redirects(0)` so ureq cannot hop on its own.
pub(crate) fn follow_allowed_redirects(
    start_url: &str,
    mut fetch: impl FnMut(&str) -> Result<Response<Body>, ureq::Error>,
) -> Result<Response<Body>, UpdateHttpError> {
    if !is_allowed_update_url(start_url) {
        return Err(UpdateHttpError::Policy(format!(
            "update URL is not on the allow-list: {start_url}"
        )));
    }
    let mut url = start_url.to_string();
    let mut followed = 0u32;
    loop {
        let response = fetch(&url)?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match redirect_hop(&url, status, location.as_deref())? {
            RedirectHop::Done => return Ok(response),
            RedirectHop::Next(next) => {
                if followed >= MAX_UPDATE_REDIRECTS {
                    return Err(UpdateHttpError::Policy(format!(
                        "update redirect from {start_url} exceeded {MAX_UPDATE_REDIRECTS} hops"
                    )));
                }
                followed += 1;
                drop(response);
                url = next;
            }
        }
    }
}

/// Convenience wrapper used by callers that already speak `anyhow`.
pub(crate) fn follow_allowed_redirects_anyhow(
    start_url: &str,
    fetch: impl FnMut(&str) -> Result<Response<Body>, ureq::Error>,
    transport_context: &'static str,
) -> anyhow::Result<Response<Body>> {
    match follow_allowed_redirects(start_url, fetch) {
        Ok(response) => Ok(response),
        Err(UpdateHttpError::Transport(e)) => Err(anyhow::Error::new(e).context(transport_context)),
        Err(e @ UpdateHttpError::Policy(_)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(status: u16, location: Option<&str>) -> Response<Body> {
        let mut builder = Response::builder().status(status);
        if let Some(loc) = location {
            builder = builder.header("Location", loc);
        }
        builder
            .body(Body::builder().data(Vec::<u8>::new()))
            .expect("synthetic response")
    }

    #[test]
    fn github_asset_redirect_to_objects_is_allowed() {
        let next = allowed_redirect_url(
            "https://github.com/theaamgroup/paneflow/releases/download/v1/x.dmg",
            "https://objects.githubusercontent.com/github-production-release-asset/x",
        );
        assert_eq!(
            next.as_deref(),
            Some("https://objects.githubusercontent.com/github-production-release-asset/x")
        );
    }

    #[test]
    fn github_asset_redirect_to_release_assets_cdn_is_allowed() {
        let next = allowed_redirect_url(
            "https://github.com/theaamgroup/paneflow/releases/download/v1/x.dmg",
            "https://release-assets.githubusercontent.com/github-production-release-asset/x",
        );
        assert!(
            next.is_some(),
            "GitHub's 2025+ asset CDN must stay on the hop allow-list"
        );
    }

    #[test]
    fn redirect_off_list_https_is_rejected() {
        assert!(
            allowed_redirect_url(
                "https://github.com/theaamgroup/paneflow/releases/download/v1/x.dmg",
                "https://evil.example/x.dmg",
            )
            .is_none()
        );
    }

    #[test]
    fn redirect_to_file_scheme_is_rejected() {
        assert!(allowed_redirect_url("https://github.com/x", "file:///etc/passwd").is_none());
    }

    #[test]
    fn protocol_relative_off_list_is_rejected() {
        assert!(allowed_redirect_url("https://github.com/x", "//evil.example/x").is_none());
    }

    #[test]
    fn relative_location_stays_on_github() {
        let next = allowed_redirect_url(
            "https://github.com/theaamgroup/paneflow/releases/download/v1/x.dmg",
            "/theaamgroup/paneflow/releases/download/v1/y.dmg",
        )
        .unwrap();
        assert_eq!(
            next,
            "https://github.com/theaamgroup/paneflow/releases/download/v1/y.dmg"
        );
    }

    #[test]
    fn loopback_http_redirect_stays_loopback() {
        let next = allowed_redirect_url("http://127.0.0.1:8080/latest", "/asset.dmg").unwrap();
        assert_eq!(next, "http://127.0.0.1:8080/asset.dmg");
    }

    #[test]
    fn resolve_rejects_control_and_backslash() {
        assert!(
            resolve_redirect_location("https://github.com/x", "https://github.com/\nx").is_none()
        );
        assert!(resolve_redirect_location("https://github.com/x", "https:\\\\evil.com").is_none());
        assert!(resolve_redirect_location("https://github.com/x", "  ").is_none());
    }

    #[test]
    fn redirect_hop_treats_200_and_304_as_final() {
        assert_eq!(
            redirect_hop("https://github.com/x", 200, None).unwrap(),
            RedirectHop::Done
        );
        assert_eq!(
            redirect_hop("https://github.com/x", 304, Some("https://evil.example/x")).unwrap(),
            RedirectHop::Done
        );
    }

    #[test]
    fn redirect_hop_302_without_location_is_policy() {
        let err = redirect_hop("https://github.com/x", 302, None).unwrap_err();
        assert!(matches!(err, UpdateHttpError::Policy(_)));
    }

    #[test]
    fn follow_does_not_fetch_disallowed_start_url() {
        let fetched = std::cell::Cell::new(false);
        let err = follow_allowed_redirects("https://evil.example/x", |_url| {
            fetched.set(true);
            Ok(fake(200, None))
        })
        .unwrap_err();
        assert!(matches!(err, UpdateHttpError::Policy(_)));
        assert!(!fetched.get(), "must not fetch an off-list start URL");
    }

    #[test]
    fn follow_does_not_fetch_off_list_hop() {
        let fetches = std::cell::RefCell::new(Vec::new());
        let err = follow_allowed_redirects("https://github.com/x", |url| {
            fetches.borrow_mut().push(url.to_string());
            Ok(fake(302, Some("https://evil.example/x")))
        })
        .unwrap_err();
        assert!(matches!(err, UpdateHttpError::Policy(_)));
        assert_eq!(&*fetches.borrow(), &["https://github.com/x".to_string()]);
    }

    #[test]
    fn follow_walks_allow_listed_hops_then_stops() {
        let fetches = std::cell::RefCell::new(Vec::new());
        let response = follow_allowed_redirects(
            "https://github.com/theaamgroup/paneflow/releases/download/v1/x.dmg",
            |url| {
                fetches.borrow_mut().push(url.to_string());
                if url.contains("objects.githubusercontent.com") {
                    Ok(fake(200, None))
                } else {
                    Ok(fake(
                        302,
                        Some("https://objects.githubusercontent.com/github-production-release-asset/x"),
                    ))
                }
            },
        )
        .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            &*fetches.borrow(),
            &[
                "https://github.com/theaamgroup/paneflow/releases/download/v1/x.dmg".to_string(),
                "https://objects.githubusercontent.com/github-production-release-asset/x"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn follow_caps_redirect_loops() {
        let n = std::cell::Cell::new(0u32);
        let err = follow_allowed_redirects("https://github.com/a", |_| {
            n.set(n.get() + 1);
            Ok(fake(302, Some("/a")))
        })
        .unwrap_err();
        assert!(matches!(err, UpdateHttpError::Policy(msg) if msg.contains("exceeded")));
        assert_eq!(n.get(), MAX_UPDATE_REDIRECTS + 1);
    }
}
