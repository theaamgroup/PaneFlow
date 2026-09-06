use std::io::Read;
use std::path::PathBuf;

use clap::Subcommand;
use paneflow_ipc_client::{IpcTransport, agent_context::identity_from_env};
use serde_json::{Value, json};

use super::{CliError, EXIT_OK};

#[derive(Debug, Subcommand)]
pub(super) enum TaskCommand {
    /// Read your task, or explicitly inspect another pane's assignment.
    Get {
        #[arg(long)]
        target: Option<String>,
    },
    /// Replace a pane's assignment with JSON containing objective, acceptance_criteria, and owned_files.
    Assign {
        target: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Report your progress with JSON containing task_id, revision, and report.
    Report {
        #[arg(long)]
        file: PathBuf,
    },
}

pub(super) fn whoami(client: &impl IpcTransport) -> Result<i32, CliError> {
    call(
        client,
        "agent.whoami",
        identity_from_env().map_err(CliError::target)?,
    )
}

pub(super) fn run(client: &impl IpcTransport, command: TaskCommand) -> Result<i32, CliError> {
    match command {
        TaskCommand::Get { target } => {
            let identity = match target {
                Some(target) => target_identity(client, &target)?,
                None => identity_from_env().map_err(CliError::target)?,
            };
            call(client, "task.get", identity)
        }
        TaskCommand::Assign { target, file } => {
            let mut params = target_identity(client, &target)?;
            params["assignment"] = read_json(&file)?;
            call(client, "task.assign", params)
        }
        TaskCommand::Report { file } => {
            let mut params = identity_from_env().map_err(CliError::target)?;
            let payload = read_json(&file)?;
            let fields = payload
                .as_object()
                .ok_or_else(|| CliError::target("report file must be a JSON object"))?;
            for (key, value) in fields {
                if !matches!(key.as_str(), "task_id" | "revision" | "report") {
                    return Err(CliError::target(format!("unknown report field: {key}")));
                }
                params[key] = value.clone();
            }
            call(client, "task.report", params)
        }
    }
}

fn read_json(path: &std::path::Path) -> Result<Value, CliError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| CliError::runtime(e.to_string()))?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| CliError::runtime(e.to_string()))?;
    if bytes.len() > 128 * 1024 {
        return Err(CliError::target("task JSON exceeds 128 KiB"));
    }
    serde_json::from_slice(&bytes).map_err(|e| CliError::target(e.to_string()))
}

fn target_identity(client: &impl IpcTransport, target: &str) -> Result<Value, CliError> {
    let surface_id = super::selector::resolve_target(client, target)?;
    let list = client
        .call("surface.list", json!({}))
        .map_err(CliError::runtime)?;
    let workspace_id = list["surfaces"]
        .as_array()
        .and_then(|surfaces| {
            surfaces
                .iter()
                .find(|surface| surface["surface_id"].as_u64() == Some(surface_id))
        })
        .and_then(|surface| surface["workspace_id"].as_u64())
        .ok_or_else(|| CliError::target("target pane has no live workspace"))?;
    Ok(json!({"surface_id": surface_id, "workspace_id": workspace_id}))
}

fn call(client: &impl IpcTransport, method: &str, params: Value) -> Result<i32, CliError> {
    let result = client.call(method, params).map_err(CliError::runtime)?;
    super::print_json(&result)?;
    Ok(EXIT_OK)
}
