pub(crate) fn working_directory_from_ghostty(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (_, path) = rest.split_once('/')?;
        format!("/{path}")
    };
    percent_decode_uri_path(&path)
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
    fn osc7_preserves_drive_like_posix_path() {
        assert_eq!(
            working_directory_from_ghostty("file:///C:/dev/path%20with%20space/%C3%A9"),
            Some("/C:/dev/path with space/é".to_owned())
        );
    }
}
