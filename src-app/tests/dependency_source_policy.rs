#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

use std::path::Path;

#[test]
fn cargo_lock_git_sources_are_immutable() {
    let lock_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path);
    assert!(
        lock.is_ok(),
        "failed to read {}: {:?}",
        lock_path.display(),
        lock.as_ref().err()
    );
    let lock = lock.unwrap_or_default();

    for source_line in lock
        .lines()
        .filter(|line| line.starts_with("source = \"git+"))
    {
        let source = source_line
            .strip_prefix("source = \"git+")
            .and_then(|source| source.strip_suffix('"'))
            .unwrap_or_default();
        let (spec, resolved) = source.rsplit_once('#').unwrap_or_default();
        let revision = spec
            .rsplit_once("?rev=")
            .map(|(_, revision)| revision)
            .unwrap_or_default();
        assert!(
            revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "git source must use a full immutable revision: {source_line}"
        );
        assert_eq!(
            revision, resolved,
            "git source revision and resolved commit differ: {source_line}"
        );
    }
}
