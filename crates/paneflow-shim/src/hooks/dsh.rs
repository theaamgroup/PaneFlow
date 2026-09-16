use super::owned_files::{
    cleanup_matching_owned_file, install_owned_file, sweep_matching_owned_file,
};
use super::{
    home_unavailable, merge_strict_matcher_hooks_for_events, paneflow_ipc_reachable,
    refuse_symlink, HookInstall, HookInstallResult, HookInstallSkip, HookLease,
};
use paneflow_agent_config::{home_dir, with_config_lock};
use std::env;
use std::path::{Path, PathBuf};

const DSH_HOOK_EVENTS: &[&str] = &["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"];
pub(crate) const DSH_HOOKS_BASENAME: &str = "hooks.json";
pub(crate) const DSH_OVERLAY_BASENAME: &str = "paneflow-overlay.yml";
const BRIDGE_PACKAGE: &str = "@deepseek-ai/dsh-hooks-claude-code";
const BRIDGE_ROW_ID: &str = "paneflow-hooks";

pub(crate) struct DshOverlayGuard {
    hooks_path: PathBuf,
    overlay_path: PathBuf,
    hooks_source: String,
    overlay_source: String,
    hooks_lease: HookLease,
    overlay_lease: HookLease,
}

impl DshOverlayGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let home = dsh_home().ok_or_else(home_unavailable)?;
        let directory = home.join("paneflow");
        if !paneflow_ipc_reachable() {
            sweep_overlay(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        if !bridge_is_resolvable(&home) {
            sweep_overlay(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::BridgeUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlink(directory, "DeepSeek Harness overlay")?;
        std::fs::create_dir_all(directory)?;
        let hooks_path = directory.join(DSH_HOOKS_BASENAME);
        let overlay_path = directory.join(DSH_OVERLAY_BASENAME);
        let mut hooks_lease = HookLease::acquire(&hooks_path)?;
        let mut overlay_lease = HookLease::acquire(&overlay_path)?;
        let hooks_source = hooks_source()?;
        let overlay_source = render_overlay(&hooks_path);
        with_config_lock(&hooks_path, || {
            install_owned_file(&hooks_path, &hooks_source, &mut hooks_lease)
        })?;
        if let Err(error) = with_config_lock(&overlay_path, || {
            install_owned_file(&overlay_path, &overlay_source, &mut overlay_lease)
        }) {
            cleanup_matching_owned_file(&hooks_path, &mut hooks_lease, &hooks_source);
            return Err(error);
        }
        Ok(Self {
            hooks_path,
            overlay_path,
            hooks_source,
            overlay_source,
            hooks_lease,
            overlay_lease,
        })
    }

    pub(crate) fn overlay_path(&self) -> &Path {
        &self.overlay_path
    }
}

impl Drop for DshOverlayGuard {
    fn drop(&mut self) {
        cleanup_matching_owned_file(
            &self.overlay_path,
            &mut self.overlay_lease,
            &self.overlay_source,
        );
        cleanup_matching_owned_file(&self.hooks_path, &mut self.hooks_lease, &self.hooks_source);
    }
}

pub(crate) fn dsh_home() -> Option<PathBuf> {
    match env::var_os("DSH_HOME") {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => home_dir().map(|home| home.join(".dsh")),
    }
}

fn bridge_is_resolvable(dsh_home: &Path) -> bool {
    dsh_home
        .join("profiles")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh-hooks-claude-code")
        .is_dir()
}

fn hooks_source() -> std::io::Result<String> {
    let mut root = serde_json::json!({});
    merge_strict_matcher_hooks_for_events(&mut root, DSH_HOOK_EVENTS)?;
    Ok(serde_json::to_string_pretty(&root).map_err(std::io::Error::other)? + "\n")
}

pub(crate) fn render_overlay(hooks_path: &Path) -> String {
    let config_path = yaml_single_quoted(&hooks_path.to_string_lossy());
    format!(
        "- insert:\n    - id: {BRIDGE_ROW_ID}\n      name: '{BRIDGE_PACKAGE}'\n      config:\n        configPath: '{config_path}'\n"
    )
}

fn yaml_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

fn sweep_overlay(directory: &Path) {
    let hooks_path = directory.join(DSH_HOOKS_BASENAME);
    let overlay_path = directory.join(DSH_OVERLAY_BASENAME);
    if let Ok(source) = hooks_source() {
        sweep_matching_owned_file(&hooks_path, &source);
    }
    sweep_matching_owned_file(&overlay_path, &render_overlay(&hooks_path));
}
