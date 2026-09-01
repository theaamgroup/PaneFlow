use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::checksum::{validate_sha256, verify_hash, verify_text_hash};
use super::manifest::TargetContract;
use super::{BuildResult, artifact_error, build_error};

pub(crate) struct ArtifactBundle {
    root: PathBuf,
    archive: PathBuf,
    header: PathBuf,
    bindings: PathBuf,
    build_info: PathBuf,
    uses_bundled_archive: bool,
}

#[derive(Debug)]
struct BuildInfo {
    values: HashMap<String, String>,
}

impl ArtifactBundle {
    pub(crate) fn resolve(
        workspace: &Path,
        contract: &TargetContract,
        prepared_override: Option<OsString>,
    ) -> Self {
        let bundled = workspace
            .join("native/libghostty/prebuilt")
            .join(contract.target());
        let (root, uses_bundled_archive) = match prepared_override {
            Some(path) => (PathBuf::from(path), false),
            None => (bundled, true),
        };
        Self {
            archive: root.join(&contract.archive_path),
            header: root.join(contract.header_path()),
            bindings: root.join("bindings.rs"),
            build_info: root.join("build-info.txt"),
            root,
            uses_bundled_archive,
        }
    }

    pub(crate) fn required_inputs(&self) -> Vec<&Path> {
        vec![
            self.archive.as_path(),
            self.header.as_path(),
            self.bindings.as_path(),
            self.build_info.as_path(),
        ]
    }

    pub(crate) fn link_directory(&self) -> BuildResult<&Path> {
        self.archive
            .parent()
            .ok_or_else(|| build_error("archive path must have a parent"))
    }

    pub(crate) fn validate(&self, contract: &TargetContract, action: &str) -> BuildResult<()> {
        validate_directory(&self.root)
            .map_err(|detail| artifact_error(contract.target(), &self.root, detail, action))?;
        for path in self.required_inputs() {
            require_file_beneath(&self.root, path, contract.target(), action)?;
        }
        verify_artifact_text(
            &self.header,
            &contract.header_sha256,
            contract.target(),
            action,
        )?;
        verify_artifact_text(
            &self.bindings,
            &contract.bindings_sha256,
            contract.target(),
            action,
        )?;

        let info_source = fs::read_to_string(&self.build_info).map_err(|error| {
            artifact_error(
                contract.target(),
                &self.build_info,
                format!("cannot read build metadata: {error}"),
                action,
            )
        })?;
        let info = BuildInfo::parse(&info_source).map_err(|detail| {
            artifact_error(contract.target(), &self.build_info, detail, action)
        })?;
        for (key, expected) in contract.expected_build_info() {
            let actual = info.required(key).map_err(|detail| {
                artifact_error(contract.target(), &self.build_info, detail, action)
            })?;
            if actual != expected {
                return Err(artifact_error(
                    contract.target(),
                    &self.build_info,
                    format!("build metadata `{key}` is `{actual}`, expected `{expected}`"),
                    action,
                ));
            }
        }

        let prepared_archive_hash = info.required("archive_sha256").map_err(|detail| {
            artifact_error(contract.target(), &self.build_info, detail, action)
        })?;
        validate_sha256(prepared_archive_hash).map_err(|detail| {
            artifact_error(contract.target(), &self.build_info, detail, action)
        })?;
        if self.uses_bundled_archive && prepared_archive_hash != contract.archive_sha256 {
            return Err(artifact_error(
                contract.target(),
                &self.build_info,
                format!(
                    "archive checksum metadata is `{prepared_archive_hash}`, expected `{}`",
                    contract.archive_sha256
                ),
                action,
            ));
        }
        let archive_hash = if self.uses_bundled_archive {
            contract.archive_sha256.as_str()
        } else {
            prepared_archive_hash
        };
        verify_hash(&self.archive, archive_hash)
            .map_err(|detail| artifact_error(contract.target(), &self.archive, detail, action))?;
        Ok(())
    }
}

impl BuildInfo {
    fn parse(source: &str) -> Result<Self, String> {
        let mut values = HashMap::new();
        for (index, line) in source.lines().enumerate() {
            let line_number = index + 1;
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("invalid build metadata at line {line_number}"))?;
            if key.is_empty() || value.is_empty() {
                return Err(format!(
                    "empty build metadata key or value at line {line_number}"
                ));
            }
            if values.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err(format!("duplicate build metadata key `{key}`"));
            }
        }
        Ok(Self { values })
    }

    fn required(&self, key: &str) -> Result<&str, String> {
        self.values
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| format!("libghostty build info is missing `{key}`"))
    }
}

pub(crate) fn validate_regular_file_beneath(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| format!("required input escaped its root: {error}"))?;
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Err("required input must be below its root".to_owned());
    }

    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        if !matches!(component, Component::Normal(_)) {
            return Err("required input contains an unsafe path component".to_owned());
        }
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("cannot inspect required input: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "required input path contains symlink {}",
                current.display()
            ));
        }
        let is_last = index + 1 == components.len();
        if is_last && !metadata.is_file() {
            return Err("required input is not a regular file".to_owned());
        }
        if !is_last && !metadata.is_dir() {
            return Err(format!(
                "required input parent is not a directory: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

fn require_file_beneath(root: &Path, path: &Path, target: &str, action: &str) -> BuildResult<()> {
    validate_regular_file_beneath(root, path)
        .map_err(|detail| artifact_error(target, path, detail, action))
}

fn validate_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect prepared root: {error}"))?;
    if metadata.file_type().is_symlink() {
        Err("prepared root must not be a symlink".to_owned())
    } else if metadata.is_dir() {
        Ok(())
    } else {
        Err("prepared root is not a directory".to_owned())
    }
}

fn verify_artifact_text(
    path: &Path,
    expected: &str,
    target: &str,
    action: &str,
) -> BuildResult<()> {
    verify_text_hash(path, expected).map_err(|detail| artifact_error(target, path, detail, action))
}

#[cfg(test)]
mod tests {
    use super::super::manifest::Manifest;
    use super::super::manifest::tests::{MACOS_TARGET, macos_manifest};
    use super::*;
    use sha2::{Digest, Sha256};

    const MANIFEST: &str = include_str!("../../../../native/libghostty/manifest.toml");

    #[test]
    fn build_info_rejects_duplicate_keys() {
        let error = BuildInfo::parse("source_sha=one\nsource_sha=two\n")
            .expect_err("duplicate metadata must be rejected");
        assert!(error.contains("duplicate build metadata key"));
    }

    #[test]
    fn build_info_reports_missing_required_key() {
        let info = BuildInfo::parse("source_sha=one\n").expect("fixture must parse");
        let error = info
            .required("zig_version")
            .expect_err("missing metadata must be rejected");
        assert!(error.contains("missing `zig_version`"));
    }

    /// The committed macOS tree is validated as one unit.
    ///
    /// `ArtifactBundle::validate` is pure path and checksum work, so anyone
    /// editing `native/libghostty/prebuilt/aarch64-apple-darwin/` gets the same
    /// failure the app build would raise, from this crate's tests alone.
    #[test]
    fn validates_the_reviewed_macos_bundle_as_one_unit() -> BuildResult<()> {
        validates_the_reviewed_bundle(MACOS_TARGET)
    }

    fn validates_the_reviewed_bundle(target: &str) -> BuildResult<()> {
        let manifest = Manifest::parse(MANIFEST)?;
        let contract = manifest.target_contract(target)?;
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = crate_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| build_error("test crate must live under <workspace>/crates"))?;
        let bundle = ArtifactBundle::resolve(workspace, &contract, None);
        bundle.validate(&contract, "replace reviewed fixture")
    }

    /// A macOS bundle laid out exactly as upstream's build script writes it (the
    /// shape of the vendored `prebuilt/aarch64-apple-darwin/` tree), paired with
    /// the manifest that declares its target.
    ///
    /// Returns the manifest source and the ten `build-info.txt` entries.
    fn macos_fixture(root: &Path) -> BuildResult<(String, Vec<(String, String)>)> {
        const HEADER: &str = "#define GHOSTTY_VT 1\n";
        const BINDINGS: &str = "// generated bindings\n";
        const ARCHIVE: &[u8] = b"!<arch>\nfixture";
        const NORMALIZATION: &str =
            "fixed-zig-source-cache-prefix+zig-build-seed0-j1+llvm-strip-debug+llvm-ar-D-darwin";

        fs::create_dir_all(root.join("include/ghostty"))?;
        fs::create_dir_all(root.join("lib"))?;
        fs::write(root.join("include/ghostty/vt.h"), HEADER)?;
        fs::write(root.join("bindings.rs"), BINDINGS)?;
        fs::write(root.join("lib/libghostty-vt.a"), ARCHIVE)?;

        let archive_sha256 = format!("{:x}", Sha256::digest(ARCHIVE));
        let header_sha256 = format!("{:x}", Sha256::digest(HEADER.as_bytes()));
        let bindings_sha256 = format!("{:x}", Sha256::digest(BINDINGS.as_bytes()));
        let source = macos_manifest(&archive_sha256, "[]");
        let source = replace_manifest_value(&source, "header_sha256", &header_sha256);
        let source = replace_manifest_value(&source, "bindings_sha256", &bindings_sha256);

        let source_sha = manifest_value(MANIFEST, "source_sha");
        let zig_version = manifest_value(MANIFEST, "zig_version");
        let build_info = [
            ("source_sha", source_sha.as_str()),
            ("zig_version", zig_version.as_str()),
            ("header_sha256", header_sha256.as_str()),
            ("bindings_sha256", bindings_sha256.as_str()),
            ("rust_target", MACOS_TARGET),
            ("zig_target", "aarch64-macos"),
            ("optimize", "ReleaseFast"),
            ("archive_normalization", NORMALIZATION),
            ("archive_sha256", archive_sha256.as_str()),
            ("build_info_symbol", "ghostty_build_info"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<Vec<_>>();
        write_build_info(root, &build_info)?;
        Ok((source, build_info))
    }

    fn write_build_info(root: &Path, entries: &[(String, String)]) -> BuildResult<()> {
        let body = entries
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect::<String>();
        fs::write(root.join("build-info.txt"), body)?;
        Ok(())
    }

    /// Read a top-level string value out of the reviewed manifest.
    ///
    /// The build-info fixture has to agree with the pinned manifest, so reading
    /// the pin instead of restating it keeps the fixture correct across a pin
    /// bump.
    fn manifest_value(source: &str, key: &str) -> String {
        let prefix = format!("{key} = \"");
        source
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|value| value.strip_suffix('"'))
            .expect("the reviewed manifest declares the requested pin key")
            .to_owned()
    }

    fn replace_manifest_value(source: &str, key: &str, value: &str) -> String {
        source
            .lines()
            .map(|line| {
                if line.starts_with(&format!("{key} = \"")) {
                    format!("{key} = \"{value}\"")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn macos_bundle_requires_only_the_platform_neutral_inputs() -> BuildResult<()> {
        let root = tempfile::tempdir()?;
        let (source, _) = macos_fixture(root.path())?;
        let manifest = Manifest::parse(&source)?;
        let contract = manifest.target_contract(MACOS_TARGET)?;
        let bundle = ArtifactBundle::resolve(root.path(), &contract, Some(root.path().into()));
        let inputs = bundle
            .required_inputs()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 4, "unexpected macOS inventory: {inputs:?}");
        assert!(inputs.iter().any(|path| path.ends_with("libghostty-vt.a")));
        assert!(inputs.iter().any(|path| path.ends_with("vt.h")));
        assert!(inputs.iter().any(|path| path.ends_with("bindings.rs")));
        assert!(inputs.iter().any(|path| path.ends_with("build-info.txt")));
        bundle.validate(&contract, &contract.corrective_action())
    }

    #[test]
    fn macos_bundle_names_every_missing_build_info_key() -> BuildResult<()> {
        let root = tempfile::tempdir()?;
        let (source, build_info) = macos_fixture(root.path())?;
        let manifest = Manifest::parse(&source)?;
        let contract = manifest.target_contract(MACOS_TARGET)?;
        let action = contract.corrective_action();
        assert_eq!(build_info.len(), 10);
        for (key, _) in &build_info {
            let reduced = build_info
                .iter()
                .filter(|(candidate, _)| candidate != key)
                .cloned()
                .collect::<Vec<_>>();
            write_build_info(root.path(), &reduced)?;
            let bundle = ArtifactBundle::resolve(root.path(), &contract, Some(root.path().into()));
            let error = bundle
                .validate(&contract, &action)
                .expect_err("a truncated build-info.txt must be rejected");
            let error = error.to_string();
            assert!(
                error.contains(&format!("missing `{key}`")),
                "error must name the missing key `{key}`: {error}"
            );
            assert!(error.contains("PANEFLOW_LIBGHOSTTY_DIR"));
        }
        write_build_info(root.path(), &build_info)?;
        Ok(())
    }

    #[test]
    fn macos_bundled_tree_is_pinned_to_the_manifest_checksum() -> BuildResult<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace
            .path()
            .join("native/libghostty/prebuilt")
            .join(MACOS_TARGET);
        fs::create_dir_all(&root)?;
        let (source, _) = macos_fixture(&root)?;
        let manifest = Manifest::parse(&source)?;
        let contract = manifest.target_contract(MACOS_TARGET)?;
        let action = contract.corrective_action();
        ArtifactBundle::resolve(workspace.path(), &contract, None).validate(&contract, &action)?;

        fs::write(root.join("lib/libghostty-vt.a"), b"!<arch>\ntampered")?;
        let error = ArtifactBundle::resolve(workspace.path(), &contract, None)
            .validate(&contract, &action)
            .expect_err("a tampered bundled archive must be rejected")
            .to_string();
        assert!(error.contains("checksum"), "{error}");
        assert!(error.contains("PANEFLOW_LIBGHOSTTY_DIR"), "{error}");
        Ok(())
    }

    #[test]
    fn macos_bundle_names_a_missing_required_file() -> BuildResult<()> {
        let root = tempfile::tempdir()?;
        let (source, _) = macos_fixture(root.path())?;
        let manifest = Manifest::parse(&source)?;
        let contract = manifest.target_contract(MACOS_TARGET)?;
        fs::remove_file(root.path().join("lib/libghostty-vt.a"))?;
        let bundle = ArtifactBundle::resolve(root.path(), &contract, Some(root.path().into()));
        let error = bundle
            .validate(&contract, &contract.corrective_action())
            .expect_err("a truncated macOS bundle must be rejected");
        let error = error.to_string();
        assert!(error.contains("libghostty-vt.a"), "{error}");
        assert!(error.contains("PANEFLOW_LIBGHOSTTY_DIR"), "{error}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn macos_bundle_rejects_a_symlinked_required_input() -> BuildResult<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let (source, _) = macos_fixture(root.path())?;
        let manifest = Manifest::parse(&source)?;
        let contract = manifest.target_contract(MACOS_TARGET)?;
        let bindings = root.path().join("bindings.rs");
        let elsewhere = root.path().join("bindings.real.rs");
        fs::rename(&bindings, &elsewhere)?;
        symlink(&elsewhere, &bindings)?;
        let bundle = ArtifactBundle::resolve(root.path(), &contract, Some(root.path().into()));
        let error = bundle
            .validate(&contract, &contract.corrective_action())
            .expect_err("symlinked required inputs must be rejected");
        assert!(error.to_string().contains("contains symlink"), "{error}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_required_input() -> BuildResult<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let target = root.path().join("target");
        let link = root.path().join("link");
        fs::write(&target, b"content")?;
        symlink(&target, &link)?;
        let error = validate_regular_file_beneath(root.path(), &link)
            .expect_err("symlinks must be rejected");
        assert!(error.contains("contains symlink"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_parent_directory() -> BuildResult<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("archive.a"), b"content")?;
        let link = root.path().join("lib");
        symlink(outside.path(), &link)?;
        let archive = link.join("archive.a");
        let error = validate_regular_file_beneath(root.path(), &archive)
            .expect_err("symlinked parent directories must be rejected");
        assert!(error.contains("contains symlink"));
        Ok(())
    }
}
