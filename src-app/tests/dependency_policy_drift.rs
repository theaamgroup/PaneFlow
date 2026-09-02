#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! Guards against the dependency policy drifting away from the graph it
//! governs (issue #213).
//!
//! `cargo deny check` passes with warnings when `deny.toml` still names an
//! advisory, license, exception, or git source that no crate in `Cargo.lock`
//! matches, and when the lockfile holds a yanked release that has a
//! compatible patched successor. The gate stays green, so nothing forces the
//! cleanup. This test reads `deny.toml` and `Cargo.lock` off disk, the way
//! `dependency_source_policy.rs` does, and fails the suite if either drifts
//! back to the state the audit found.

use std::path::Path;

fn read_repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let contents = std::fs::read_to_string(&path);
    assert!(
        contents.is_ok(),
        "failed to read {}: {:?}",
        path.display(),
        contents.as_ref().err()
    );
    contents.unwrap_or_default()
}

/// `(name, version)` pairs for every `[[package]]` block in `Cargo.lock`.
fn locked_packages(lock: &str) -> Vec<(&str, &str)> {
    let mut packages = Vec::new();
    let mut name = None;
    for line in lock.lines() {
        if line == "[[package]]" {
            name = None;
        } else if let Some(rest) = line.strip_prefix("name = \"") {
            name = rest.strip_suffix('"');
        } else if let Some(version) = line
            .strip_prefix("version = \"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            packages.extend(name.take().map(|name| (name, version)));
        }
    }
    packages
}

/// `deny.toml` with every `#` comment stripped, so a rationale that mentions
/// a removed identifier historically does not count as the policy still
/// carrying it.
fn deny_policy_without_comments(deny: &str) -> String {
    deny.lines()
        .map(|line| line.split_once('#').map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn yanked_spin_releases_are_not_locked() {
    let lock = read_repo_file("../Cargo.lock");
    let packages = locked_packages(&lock);
    assert!(
        packages.len() > 100,
        "Cargo.lock parse produced only {} packages; the probe is broken",
        packages.len()
    );
    assert!(
        packages.iter().any(|(name, _)| *name == "spin"),
        "spin left the graph entirely; retire this test instead of keeping a \
         probe for a crate the GPUI pin no longer pulls"
    );

    for yanked in [("spin", "0.9.8"), ("spin", "0.10.0")] {
        assert!(
            !packages.contains(&yanked),
            "`{} {}` is back in Cargo.lock. That release is yanked on crates.io \
             and has an in-place patched successor; move it with \
             `cargo update -p {}@{} --precise <patched>` (issue #213).",
            yanked.0,
            yanked.1,
            yanked.0,
            yanked.1
        );
    }
}

#[test]
fn deny_policy_carries_no_entry_the_graph_cannot_match() {
    let deny = read_repo_file("../deny.toml");
    let policy = deny_policy_without_comments(&deny);
    assert!(
        policy.contains("\"RUSTSEC-2026-0194\"")
            && policy.contains("https://github.com/zed-industries/zed")
            && policy.contains("\"GPL-3.0-or-later\""),
        "deny.toml parse missed known live entries; the probe is broken"
    );

    // Each of these was reported by `cargo deny check advisories licenses
    // sources` as matching nothing in the graph (issue #213). A returning
    // entry means either the policy was pasted back from history or the
    // graph really grew the crate again; in the second case, update this
    // list alongside the rationale in deny.toml.
    let dead_policy_entries = [
        ("\"RUSTSEC-2023-0071\"", "`rsa` is not in Cargo.lock"),
        ("xxhash-rust@0.8", "`xxhash-rust` is not in Cargo.lock"),
        (
            "https://github.com/zed-industries/scap",
            "no locked crate comes from that repository",
        ),
        (
            "\"CDLA-Permissive-2.0\"",
            "no locked crate carries that license",
        ),
        ("\"NCSA\"", "no locked crate carries that license"),
        (
            "\"GPL-3.0\"",
            "every GPL crate in the graph is `GPL-3.0-or-later`",
        ),
    ];
    for (entry, why) in dead_policy_entries {
        assert!(
            !policy.contains(entry),
            "deny.toml names `{entry}` again, but {why}. cargo-deny reports it \
             as unmatched (issue #213); drop it or update this list with the \
             crate that now needs it."
        );
    }

    // The quick-xml rationale used to describe a graph that no longer
    // exists (arthjean/zed mermaid_render, quick-xml 0.38.4). Comments are
    // checked here on purpose: the ignore is only defensible while its
    // stated reason is true.
    for stale in [
        "arthjean",
        "mermaid_render",
        "quick-xml 0.38.4",
        "syntect/plist",
    ] {
        assert!(
            !deny.contains(stale),
            "deny.toml still explains an ignore with `{stale}`, which left the \
             graph. Re-derive the rationale from `cargo tree -i quick-xml@<ver> \
             --target all` (issue #213)."
        );
    }
}
