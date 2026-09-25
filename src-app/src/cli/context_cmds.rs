use paneflow_ipc_client::{IpcTransport, agent_context::identity_from_env};
use serde_json::Value;

use super::{CliError, EXIT_OK};

pub(super) fn whoami(client: &impl IpcTransport) -> Result<i32, CliError> {
    call(
        client,
        "agent.whoami",
        identity_from_env().map_err(CliError::target)?,
    )
}

fn call(client: &impl IpcTransport, method: &str, params: Value) -> Result<i32, CliError> {
    let result = client.call(method, params).map_err(CliError::runtime)?;
    super::print_json(&result)?;
    Ok(EXIT_OK)
}
