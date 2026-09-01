# libghostty-vt native input

`manifest.toml` is the single source of truth for PaneFlow's native
libghostty-vt input. Cargo never fetches Ghostty, runs bindgen, invokes Zig,
or mutates an artifact: `crates/paneflow-libghostty-sys/build.rs` verifies the
checked-in files against the manifest's hashes and links the reviewed archive,
so a standard checkout only verifies and links repository content. **No Zig
toolchain is needed to build PaneFlow.**

This fork ships exactly one target, `aarch64-apple-darwin`, under
`prebuilt/aarch64-apple-darwin/`. The archive, header, bindings and
`build-info.txt` were vendored byte-for-byte from upstream
`arthjean/paneflow` tag `v0.10.0` (`b4da6ba`) on 2026-08-31 (issue #184);
the Linux and Windows archives upstream also ships were not copied.

The pinned source is Ghostty
`f2d5758f6305867dc36b36293c6165d8152b853e` built with Zig 0.16.0 in
`ReleaseFast` (`ghostty_app_version` `1.3.2-dev+f2d5758f6`, ABI
`api_version` `0.1.0`). `bindings.rs` is pregenerated from the pinned C
header. Its normalized UTF-8 checksum is verified both in the workspace and in
the prepared artifact. Regenerate bindings only from that exact header, then
update `bindings_sha256` and the reviewed target.

## macOS arm64 archive

Reviewed fingerprint (`archive_sha256`):
`d81dafad9975987fc582977f24af06c9255b901196c07b00be42b85ffe8dba03`
(2,616,664 bytes, Mach-O `arm64` ar archive).

Upstream produces it with its `scripts/build-libghostty-macos.sh
--verify-reproducible`: cross-built from a Linux host (Ghostty only takes the
Apple `libtool` combine path when the build host is Darwin), `--seed 0 -j1` to
pin Zig's build order, every path that reaches debug info fixed at
`macos_canonical_source_path` / `macos_canonical_zig_path`, `llvm-strip -S` on
each member, and `llvm-ar crsD --format=darwin` repacking in `LC_ALL=C`
basename order. Those normalization tools come from the `llvm-tools`
component of the toolchain recorded in `macos_llvm_version`, which is why that
value names a rustc release. The recipe is captured in
`prebuilt/aarch64-apple-darwin/build-info.txt`, which the build script checks
against the manifest.

That build script is **not** part of this fork. The daily `cargo build` never
rebuilds the archive; an independent rebuild-and-compare job is a follow-up to
#184, not a build prerequisite. Until it exists, the archive's provenance is
upstream's review plus the hash match above.

`PANEFLOW_LIBGHOSTTY_DIR` points the build at a prepared directory instead of
`prebuilt/aarch64-apple-darwin/`. The same checks apply: the archive, header,
bindings and `build-info.txt` must all match the manifest, and symlinked inputs
are rejected.

## ABI and licensing

`paneflow-libghostty-sys` is the only raw ABI surface. The safe wrapper
(`paneflow-terminal-ghostty`) owns every native handle, copies borrowed
callback data before returning, releases Ghostty allocations through their
matching Ghostty destructor, catches Rust panics inside callbacks, and
validates API version, discriminants, callback signatures, sizes, alignments,
and field offsets before constructing a terminal.

The manifest records the archive-member license inventory and pins
`THIRD_PARTY_NOTICES.md` (`notice_sha256`) and `sbom.cdx.json`
(`sbom_sha256`). `scripts/bundle-macos.sh` installs the notice into the app
bundle as `Contents/Resources/ThirdPartyLicenses/libghostty.txt`; `cargo deny`
cannot see the static archive, so that notice is the license inventory for it.
