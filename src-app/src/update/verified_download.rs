//! Shared signed-asset download for self-update installers.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Download `asset_url` to `dest`, verify its detached minisign sibling, then
/// promote the verified bytes into place.
///
/// The caller owns installer-specific policy through `max_bytes`,
/// `connect_timeout`, `body_timeout`, and `label`. HTTP timeouts are split:
/// ureq 3.3 `timeout_global` is DNS-through-body, so a short global would
/// kill a 60-100 MB DMG on a mediocre link. Connect/DNS/headers stay short;
/// only the response-body phase uses `body_timeout`.
pub(crate) fn download_verified_asset(
    asset_url: &str,
    dest: &Path,
    max_bytes: u64,
    connect_timeout: Duration,
    body_timeout: Duration,
    label: &str,
) -> Result<()> {
    log::info!("self-update/{label}: downloading {asset_url}");

    let agent = large_asset_http_agent(connect_timeout, body_timeout);
    let partial = append_suffix(dest, ".partial")?;
    let mut response = super::redirect::follow_allowed_redirects_anyhow(
        asset_url,
        |url| {
            agent
                .get(url)
                .header(
                    "User-Agent",
                    &format!("paneflow/{}", env!("CARGO_PKG_VERSION")),
                )
                .call()
        },
        "Could not download update. Try again when online.",
    )?;
    if !response.status().is_success() {
        bail!(
            "Update download returned HTTP {}. Try again later.",
            response.status()
        );
    }

    let stream_result = {
        let reader = response.body_mut().as_reader();
        let mut reader = Read::take(reader, max_bytes + 1);
        let mut file = std::fs::File::create(&partial)
            .with_context(|| format!("create {}", partial.display()))?;
        std::io::copy(&mut reader, &mut file)
            .with_context(|| format!("stream {label} to disk"))
            .and_then(|written| {
                file.sync_all()
                    .with_context(|| format!("flush {label} to disk"))?;
                Ok(written)
            })
    };
    let written = match stream_result {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    if written > max_bytes {
        let _ = std::fs::remove_file(&partial);
        bail!(
            "Update download exceeded {} MiB - aborting.",
            max_bytes / 1024 / 1024
        );
    }

    if let Err(e) = super::signature::fetch_and_verify(&partial, asset_url) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

    std::fs::rename(&partial, dest)
        .with_context(|| format!("rename {} -> {}", partial.display(), dest.display()))?;
    Ok(())
}

/// ureq agent for a large signed asset. `timeout_global` stays unset: it
/// covers DNS through the last body byte, which is the wrong shape for a
/// 60-100 MB DMG. Phase timeouts: resolve/connect/send/recv-headers use
/// `connect_timeout`; `timeout_recv_body` uses `body_timeout`.
fn large_asset_http_agent(connect_timeout: Duration, body_timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_resolve(Some(connect_timeout))
        .timeout_connect(Some(connect_timeout))
        .timeout_send_request(Some(connect_timeout))
        .timeout_recv_response(Some(connect_timeout))
        .timeout_recv_body(Some(body_timeout))
        .max_redirects(0)
        .build()
        .into()
}

fn append_suffix(path: &Path, suffix: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("path has no file name: {}", path.display()))?;
    let mut name = name.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_suffix_preserves_full_name() {
        assert_eq!(
            append_suffix(Path::new("/tmp/foo.tar.gz"), ".partial").unwrap(),
            PathBuf::from("/tmp/foo.tar.gz.partial")
        );
    }

    #[test]
    fn append_suffix_rejects_pathless_input() {
        assert!(append_suffix(Path::new("/"), ".partial").is_err());
    }

    #[test]
    fn download_timeout_splits_connect_and_body() {
        // Dummy values so this test does not depend on the DMG constants
        // and does not open a socket.
        let connect = Duration::from_secs(7);
        let body = Duration::from_secs(99);
        let timeouts = large_asset_http_agent(connect, body).config().timeouts();
        assert_eq!(
            timeouts.global, None,
            "timeout_global is DNS-through-body; a short global would kill the DMG"
        );
        assert_eq!(timeouts.per_call, None);
        assert_eq!(timeouts.resolve, Some(connect));
        assert_eq!(timeouts.connect, Some(connect));
        assert_eq!(timeouts.send_request, Some(connect));
        assert_eq!(timeouts.recv_response, Some(connect));
        assert_eq!(timeouts.recv_body, Some(body));
        assert_eq!(
            large_asset_http_agent(connect, body)
                .config()
                .max_redirects(),
            0,
            "ureq must not follow redirects; hop hosts are re-validated in redirect.rs"
        );
    }
}
