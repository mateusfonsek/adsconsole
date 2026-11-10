//! rmcp `ServerHandler` over an `Engine`: one instance per platform.

use adsconsole_core::Platform;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;

use crate::engine::Engine;

const INSTRUCTIONS: &str = "AdsConsole advertising tools. Every account tool takes account_id; use list_accounts to see the accounts. Writes may wait for approval in AdsConsole (check the result with get_change_status) and applied changes can be reverted with undo_change.";

#[derive(Clone)]
pub struct McpService {
    pub engine: Engine,
}

fn server_name(p: Platform) -> &'static str {
    match p {
        Platform::GoogleAds => "adsconsole-google-ads",
        Platform::MetaAds => "adsconsole-meta-ads",
    }
}

impl ServerHandler for McpService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                server_name(self.engine.platform()),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    /// Reads the configuration on every call: toggles take effect without a restart.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let settings = self.engine.settings();
        let tools = self
            .engine
            .registry()
            .enabled(&settings)
            .into_iter()
            .map(|spec| spec.to_mcp_tool())
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let requested_by = ctx
            .client_info()
            .map(|i| format!("{} {}", i.name, i.version));
        let input = Value::Object(request.arguments.unwrap_or_default());
        match self
            .engine
            .call_tool(&request.name, input, requested_by)
            .await
        {
            Ok(v) => Ok(CallToolResult::structured(v).into()),
            Err(e) => Ok(e.to_call_result().into()),
        }
    }
}
