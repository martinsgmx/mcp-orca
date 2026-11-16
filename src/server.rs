//! MCP server handler.

use std::sync::Arc;

use async_trait::async_trait;
use rust_mcp_sdk::{
    mcp_server::ServerHandler,
    schema::{
        schema_utils::CallToolError, CallToolRequestParams, CallToolResult, GetPromptRequestParams,
        GetPromptResult, Implementation, InitializeResult, ListPromptsResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams,
        ReadResourceResult, RpcError, ServerCapabilities, ServerCapabilitiesPrompts,
        ServerCapabilitiesResources, ServerCapabilitiesTools,
    },
    McpServer,
};
use serde_json::Map;

use crate::tools::{tool_error, ToolContext, ToolRegistry};

/// Build the MCP `InitializeResult` advertised to clients.
#[must_use]
pub fn server_details() -> InitializeResult {
    InitializeResult {
        server_info: Implementation {
            name: "mcp-orca".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("mcp-orca".into()),
            description: Some(
                "Structured OrcaSlicer G-code generation: mesh preparation, profile discovery/resolution, background slice jobs, and artifact management."
                    .into(),
            ),
            icons: Vec::new(),
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools {
                list_changed: Some(false),
            }),
            resources: Some(ServerCapabilitiesResources {
                list_changed: Some(false),
                subscribe: Some(false),
            }),
            prompts: Some(ServerCapabilitiesPrompts {
                list_changed: Some(false),
            }),
            completions: Some(Map::new()),
            ..Default::default()
        },
        meta: None,
        instructions: Some(instructions()),
        protocol_version: ProtocolVersion::V2025_11_25.into(),
    }
}

fn instructions() -> String {
    [
        "You are connected to mcp-orca, an OrcaSlicer G-code generation workspace.",
        "",
        "Key boundaries: no printer communication, no G-code execution.",
        "Slicing requires resolved essential presets: printer + filament + process (no invented defaults).",
        "Profile discovery is bounded to explicit/configured and common OS-detected locations (no whole-system scan).",
        "Custom/derived profiles are workspace-managed; any in-place edit must be backed up first.",
        "Gated actions (export/delete) create approval requests; confirm with orca_approval_decide and retry the identical call.",
        "Slicing uses the configured engine argv template (engine.slice_command); unconfigured engines fail clearly.",
    ]
    .join("\n")
}

/// MCP server handler.
pub struct OrcaServerHandler {
    registry: Arc<ToolRegistry>,
    context: Arc<ToolContext>,
}

impl OrcaServerHandler {
    #[must_use]
    pub fn new(registry: Arc<ToolRegistry>, context: Arc<ToolContext>) -> Self {
        Self { registry, context }
    }
}

#[async_trait]
impl ServerHandler for OrcaServerHandler {
    async fn handle_list_tools_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: self.registry.list_tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        let arguments = params.arguments.map(serde_json::Value::Object);
        match self
            .registry
            .call(&self.context, &params.name, arguments)
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => Ok(tool_error(&err)),
        }
    }

    async fn handle_list_resources_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListResourcesResult, RpcError> {
        Ok(ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources: Vec::new(),
        })
    }

    async fn handle_read_resource_request(
        &self,
        params: ReadResourceRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ReadResourceResult, RpcError> {
        Err(RpcError::invalid_params().with_message(format!("unknown resource '{}'", params.uri)))
    }

    async fn handle_list_prompts_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListPromptsResult, RpcError> {
        Ok(ListPromptsResult {
            meta: None,
            next_cursor: None,
            prompts: Vec::new(),
        })
    }

    async fn handle_get_prompt_request(
        &self,
        params: GetPromptRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<GetPromptResult, RpcError> {
        Err(RpcError::invalid_params().with_message(format!("unknown prompt '{}'", params.name)))
    }
}
