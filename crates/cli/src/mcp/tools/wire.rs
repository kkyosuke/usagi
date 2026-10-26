//! Shared JSON and usecase error boundaries for store-backed MCP tools.

use serde::{Serialize, de::DeserializeOwned};

use crate::mcp::tool::ToolError;

pub(super) fn parse<T: DeserializeOwned>(params: &str) -> Result<T, ToolError> {
    serde_json::from_str(params).map_err(|error| ToolError::InvalidParams(error.to_string()))
}

pub(super) fn output(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("MCP wire views must serialize")
}

pub(super) fn execution<T>(result: anyhow::Result<T>) -> Result<T, ToolError> {
    result.map_err(|error| ToolError::Execution(error.to_string()))
}
