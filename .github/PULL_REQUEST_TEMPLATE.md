## Summary

<!-- What changed and why? -->

## User-visible behavior

<!-- Describe the behavior users will notice, or write "None". -->

## Validation

<!-- Check every command you ran. For UI changes, include manual verification. -->

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Manual UI check, if applicable

## macOS check

This fork targets macOS on Apple Silicon only.

- [ ] Built and run on Apple Silicon
- [ ] No new `#[cfg(target_os = "linux")]` or `#[cfg(windows)]` branches added

## Screenshots / recordings

<!-- Add screenshots, GIFs, or a short recording for UI changes. -->
