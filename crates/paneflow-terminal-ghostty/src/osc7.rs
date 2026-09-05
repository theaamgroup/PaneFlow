pub(crate) fn working_directory_from_ghostty(raw: &str) -> Option<String> {
    static HOSTNAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hostname = HOSTNAME.get_or_init(|| {
        let mut bytes = [0u8; 256];
        // SAFETY: the buffer is writable for its entire supplied length.
        if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
            return String::new();
        }
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    });
    local_working_directory(raw, hostname)
}

fn local_working_directory(raw: &str, hostname: &str) -> Option<String> {
    let rest = raw.strip_prefix("file://")?;
    let (authority, suffix) = rest.split_once('/')?;
    let empty_authority = authority.is_empty();
    let authority = authority.trim_end_matches('.');
    let hostname = hostname.trim_end_matches('.');
    if !empty_authority
        && !authority.eq_ignore_ascii_case("localhost")
        && (hostname.is_empty() || !authority.eq_ignore_ascii_case(hostname))
    {
        return None;
    }
    let path = percent_decode_uri_path(&format!("/{suffix}"))?;
    if path.chars().any(char::is_control) {
        return None;
    }
    Some(path)
}

fn percent_decode_uri_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            match (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            ) {
                (Some(high), Some(low)) => {
                    output.push((high << 4) | low);
                    index += 3;
                }
                _ => {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_authorities_and_control_free_paths_are_accepted() {
        for uri in [
            "file:///tmp",
            "file://localhost/tmp",
            "file://MY-MAC.local./tmp",
        ] {
            assert_eq!(
                local_working_directory(uri, "my-mac.local"),
                Some("/tmp".into())
            );
        }
        for uri in [
            "file://remote/tmp",
            "file://localhost:22/tmp",
            "file://user@localhost/tmp",
            "file:///tmp/%00",
            "file:///tmp/%1B",
            "file:///tmp/\n",
        ] {
            assert_eq!(
                local_working_directory(uri, "my-mac.local"),
                None,
                "{uri:?}"
            );
        }
    }

    #[test]
    fn osc7_preserves_drive_like_posix_path() {
        assert_eq!(
            working_directory_from_ghostty("file:///C:/dev/path%20with%20space/%C3%A9"),
            Some("/C:/dev/path with space/é".to_owned())
        );
    }
}
