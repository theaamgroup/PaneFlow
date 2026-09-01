use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use super::checksum::validate_sha256;
use super::{BuildResult, build_error};

const SUPPORTED_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    schema_version: u32,
    source_sha: String,
    ghostty_app_version: String,
    zig_version: String,
    header_path: PathBuf,
    header_sha256: String,
    bindings_path: PathBuf,
    bindings_sha256: String,
    notice_path: PathBuf,
    notice_sha256: String,
    sbom_path: PathBuf,
    sbom_sha256: String,
    api_version: String,
    build_mode: String,
    targets: BTreeMap<String, TargetConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "platform", rename_all = "lowercase")]
enum TargetConfig {
    Macos {
        archive_path: PathBuf,
        archive_sha256: String,
        archive_normalization: String,
        zig_target: String,
        link_name: String,
        system_libraries: Vec<String>,
        build_info_symbol: String,
    },
}

#[derive(Debug)]
pub(crate) enum PlatformContract {
    Macos { build_info_symbol: String },
}

#[derive(Debug)]
pub(crate) struct TargetContract {
    target: String,
    pub(crate) archive_path: PathBuf,
    pub(crate) archive_sha256: String,
    archive_normalization: String,
    zig_target: String,
    link_name: String,
    system_libraries: Vec<String>,
    source_sha: String,
    zig_version: String,
    header_path: PathBuf,
    pub(crate) header_sha256: String,
    pub(crate) bindings_sha256: String,
    build_mode: String,
    platform_contract: PlatformContract,
}

impl Manifest {
    pub(crate) fn parse(source: &str) -> BuildResult<Self> {
        let manifest: Self = toml::from_str(source)
            .map_err(|error| build_error(format!("invalid libghostty manifest: {error}")))?;
        if manifest.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(build_error(format!(
                "unsupported libghostty manifest schema {}; expected {SUPPORTED_SCHEMA_VERSION}",
                manifest.schema_version
            )));
        }
        validate_safe_relative_path(&manifest.header_path, "header_path")?;
        validate_safe_relative_path(&manifest.bindings_path, "bindings_path")?;
        validate_manifest_digest(&manifest.header_sha256, "header_sha256")?;
        validate_manifest_digest(&manifest.bindings_sha256, "bindings_sha256")?;
        validate_safe_relative_path(&manifest.notice_path, "notice_path")?;
        validate_safe_relative_path(&manifest.sbom_path, "sbom_path")?;
        validate_manifest_digest(&manifest.notice_sha256, "notice_sha256")?;
        validate_manifest_digest(&manifest.sbom_sha256, "sbom_sha256")?;
        if manifest.targets.is_empty() {
            return Err(build_error("libghostty manifest declares no targets"));
        }
        for (target, config) in &manifest.targets {
            validate_target_config(target, config)?;
        }
        Ok(manifest)
    }

    pub(crate) fn api_version(&self) -> &str {
        &self.api_version
    }

    pub(crate) fn ghostty_app_version(&self) -> &str {
        &self.ghostty_app_version
    }

    pub(crate) fn bindings_path(&self) -> &Path {
        &self.bindings_path
    }

    pub(crate) fn bindings_sha256(&self) -> &str {
        &self.bindings_sha256
    }

    /// The third-party notice is the app's only license inventory for a
    /// static archive `cargo deny` cannot see, and the SBOM is what a reviewer
    /// reads; both are pinned so a stale copy fails the build like a stale
    /// header would.
    pub(crate) fn notice_path(&self) -> &Path {
        &self.notice_path
    }

    pub(crate) fn notice_sha256(&self) -> &str {
        &self.notice_sha256
    }

    pub(crate) fn sbom_path(&self) -> &Path {
        &self.sbom_path
    }

    pub(crate) fn sbom_sha256(&self) -> &str {
        &self.sbom_sha256
    }

    pub(crate) fn target_contract(&self, target: &str) -> BuildResult<TargetContract> {
        let config = self.targets.get(target).ok_or_else(|| {
            build_error(format!(
                "libghostty linking is unsupported for target {target}; reviewed targets: {}",
                self.targets.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })?;

        let TargetConfig::Macos {
            archive_path,
            archive_sha256,
            archive_normalization,
            zig_target,
            link_name,
            system_libraries,
            build_info_symbol,
        } = config;

        Ok(TargetContract {
            target: target.to_owned(),
            archive_path: archive_path.clone(),
            archive_sha256: archive_sha256.clone(),
            archive_normalization: archive_normalization.clone(),
            zig_target: zig_target.clone(),
            link_name: link_name.clone(),
            system_libraries: system_libraries.clone(),
            source_sha: self.source_sha.clone(),
            zig_version: self.zig_version.clone(),
            header_path: self.header_path.clone(),
            header_sha256: self.header_sha256.clone(),
            bindings_sha256: self.bindings_sha256.clone(),
            build_mode: self.build_mode.clone(),
            platform_contract: PlatformContract::Macos {
                build_info_symbol: build_info_symbol.clone(),
            },
        })
    }
}

impl TargetContract {
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn header_path(&self) -> &Path {
        &self.header_path
    }

    pub(crate) fn link_name(&self) -> &str {
        &self.link_name
    }

    pub(crate) fn system_libraries(&self) -> &[String] {
        &self.system_libraries
    }

    pub(crate) fn corrective_action(&self) -> String {
        format!(
            "restore native/libghostty/prebuilt/{} from the vendored commit, or set PANEFLOW_LIBGHOSTTY_DIR to a prepared directory laid out the same way (see native/libghostty/README.md); Cargo performs no downloads",
            self.target
        )
    }

    pub(crate) fn expected_build_info(&self) -> Vec<(&'static str, String)> {
        let PlatformContract::Macos { build_info_symbol } = &self.platform_contract;
        vec![
            ("source_sha", self.source_sha.clone()),
            ("zig_version", self.zig_version.clone()),
            ("header_sha256", self.header_sha256.clone()),
            ("bindings_sha256", self.bindings_sha256.clone()),
            ("rust_target", self.target.clone()),
            ("zig_target", self.zig_target.clone()),
            ("optimize", self.build_mode.clone()),
            ("archive_normalization", self.archive_normalization.clone()),
            ("build_info_symbol", build_info_symbol.clone()),
        ]
    }
}

fn validate_target_config(target: &str, config: &TargetConfig) -> BuildResult<()> {
    if target.is_empty() {
        return Err(build_error(
            "libghostty manifest contains an empty target triple",
        ));
    }
    let TargetConfig::Macos {
        archive_path,
        archive_sha256,
        zig_target,
        link_name,
        system_libraries,
        build_info_symbol,
        ..
    } = config;
    if !system_libraries.is_empty() {
        return Err(build_error(format!(
            "macOS libghostty target `{target}` must not declare system libraries: libghostty-vt requires none on macOS"
        )));
    }
    validate_token(build_info_symbol, "build_info_symbol")?;
    validate_safe_relative_path(archive_path, "archive_path")?;
    validate_manifest_digest(archive_sha256, "archive_sha256")?;
    validate_token(zig_target, "zig_target")?;
    validate_token(link_name, "link_name")?;
    for library in system_libraries {
        validate_token(library, "system_libraries")?;
    }
    Ok(())
}

fn validate_token(value: &str, key: &str) -> BuildResult<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(build_error(format!(
            "unsafe libghostty manifest `{key}` value `{value}`"
        )));
    }
    Ok(())
}

fn validate_manifest_digest(value: &str, key: &str) -> BuildResult<()> {
    validate_sha256(value)
        .map_err(|detail| build_error(format!("invalid libghostty manifest `{key}`: {detail}")))
}

fn validate_safe_relative_path(path: &Path, key: &str) -> BuildResult<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(build_error(format!(
            "libghostty manifest `{key}` must be a safe relative path"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const MANIFEST: &str = include_str!("../../../../native/libghostty/manifest.toml");
    pub(crate) const MACOS_TARGET: &str = "aarch64-apple-darwin";

    /// The reviewed manifest with the macOS target's archive digest and system
    /// library list overridden.
    ///
    /// `native/libghostty/manifest.toml` declares the target, so
    /// the fixture rewrites that block in place rather than appending a second
    /// one, which TOML would reject as a duplicate table. Every other field
    /// stays exactly as reviewed, and the contract is still exercised from the
    /// same `Manifest::parse` entry point the build script uses.
    pub(crate) fn macos_manifest(archive_sha256: &str, system_libraries: &str) -> String {
        let header = format!("[targets.\"{MACOS_TARGET}\"]");
        let start = MANIFEST
            .find(&header)
            .expect("the reviewed manifest declares the macOS target");
        let end = MANIFEST[start + header.len()..]
            .find("\n[")
            .map_or(MANIFEST.len(), |offset| start + header.len() + offset + 1);
        let patched: String = MANIFEST[start..end]
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with("archive_sha256 = ") {
                    format!("archive_sha256 = \"{archive_sha256}\"\n")
                } else if line.starts_with("system_libraries = ") {
                    format!("system_libraries = {system_libraries}\n")
                } else {
                    line.to_owned()
                }
            })
            .collect();
        format!("{}{patched}{}", &MANIFEST[..start], &MANIFEST[end..])
    }

    #[test]
    fn current_manifest_builds_typed_target_contracts() -> BuildResult<()> {
        let manifest = Manifest::parse(MANIFEST)?;
        let macos = manifest.target_contract("aarch64-apple-darwin")?;
        assert_eq!(macos.zig_target, "aarch64-macos");
        assert_eq!(macos.link_name(), "ghostty-vt");
        assert!(macos.system_libraries().is_empty());
        Ok(())
    }

    #[test]
    fn rejects_unknown_schema_before_selecting_a_target() {
        let source = MANIFEST.replacen("schema_version = 2", "schema_version = 3", 1);
        let error = Manifest::parse(&source).expect_err("schema 3 must be rejected");
        assert!(
            error
                .to_string()
                .contains("unsupported libghostty manifest schema")
        );
    }

    #[test]
    fn rejects_undeclared_link_target() -> BuildResult<()> {
        let manifest = Manifest::parse(MANIFEST)?;
        // `aarch64-apple-darwin` is declared, so the undeclared-target
        // assertion uses the Intel Mac triple, which stays closed until a
        // separate PRD opens that target.
        let error = manifest
            .target_contract("x86_64-apple-darwin")
            .expect_err("undeclared targets must not silently skip linking");
        assert!(error.to_string().contains("linking is unsupported"));
        Ok(())
    }

    #[test]
    fn builds_the_macos_target_contract() -> BuildResult<()> {
        let manifest = Manifest::parse(&macos_manifest(&"0".repeat(64), "[]"))?;
        let macos = manifest.target_contract(MACOS_TARGET)?;
        assert_eq!(macos.zig_target, "aarch64-macos");
        assert_eq!(macos.link_name(), "ghostty-vt");
        // `build_support::run` emits one `dylib` directive per system library,
        // so an empty list is what keeps the macOS link line to a single
        // `rustc-link-lib=static=ghostty-vt` with no `dylib` and no
        // `framework` entry.
        assert!(macos.system_libraries().is_empty());
        assert!(
            macos
                .corrective_action()
                .contains("PANEFLOW_LIBGHOSTTY_DIR")
        );
        assert!(
            macos
                .corrective_action()
                .contains("native/libghostty/README.md")
        );
        Ok(())
    }

    #[test]
    fn macos_contract_requires_the_ten_keys_the_build_script_writes() -> BuildResult<()> {
        let manifest = Manifest::parse(&macos_manifest(&"0".repeat(64), "[]"))?;
        let macos = manifest.target_contract(MACOS_TARGET)?;
        let mut required = macos
            .expected_build_info()
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        // `archive_sha256` is required by `ArtifactBundle::validate` rather
        // than compared against a manifest string, so it completes the set
        // that upstream's build script writes into `build-info.txt`.
        required.push("archive_sha256");
        required.sort_unstable();
        assert_eq!(
            required,
            [
                "archive_normalization",
                "archive_sha256",
                "bindings_sha256",
                "build_info_symbol",
                "header_sha256",
                "optimize",
                "rust_target",
                "source_sha",
                "zig_target",
                "zig_version",
            ]
        );
        assert!(
            macos
                .expected_build_info()
                .contains(&("rust_target", MACOS_TARGET.to_owned()))
        );
        assert!(
            macos
                .expected_build_info()
                .contains(&("build_info_symbol", "ghostty_build_info".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn rejects_macos_system_libraries() {
        let source = macos_manifest(&"0".repeat(64), "[\"System\"]");
        let error = Manifest::parse(&source)
            .expect_err("libghostty-vt requires no system libraries on macOS");
        assert!(
            error
                .to_string()
                .contains("must not declare system libraries"),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("requires none on macOS"));
    }

    #[test]
    fn rejects_an_unrecognized_platform_tag() {
        let source = macos_manifest(&"0".repeat(64), "[]")
            .replace("platform = \"macos\"", "platform = \"darwin\"");
        let error = Manifest::parse(&source)
            .expect_err("an unknown platform must be rejected")
            .to_string();
        assert!(
            error.contains("unknown variant `darwin`"),
            "the error must name the unrecognized platform: {error}"
        );
        assert!(
            error.contains("macos"),
            "the error must list the recognized platform: {error}"
        );
    }

    #[test]
    fn rejects_workspace_path_escape() {
        let source = MANIFEST.replacen(
            "bindings_path = \"native/libghostty/bindings.rs\"",
            "bindings_path = \"../bindings.rs\"",
            1,
        );
        let error = Manifest::parse(&source).expect_err("parent traversal must be rejected");
        assert!(error.to_string().contains("safe relative path"));
    }
}
