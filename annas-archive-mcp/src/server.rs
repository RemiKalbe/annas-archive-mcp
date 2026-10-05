use std::sync::Arc;

use annas_archive_api::{AnnasArchiveClient, Error, SearchOptions};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
};

use crate::tools::{DetailsParams, DownloadParams, SearchParams};

#[derive(Clone)]
pub struct AnnasArchiveServer {
    client: Arc<AnnasArchiveClient>,
    #[allow(dead_code)] // Used by #[tool_router] macro
    tool_router: ToolRouter<Self>,
}

impl AnnasArchiveServer {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            client: Arc::new(AnnasArchiveClient::new(api_key)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler]
impl ServerHandler for AnnasArchiveServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Access Anna's Archive to search for and get information about books, papers, magazines, comics, and other documents. get_details and get_download_url require the ANNAS_ARCHIVE_API_KEY environment variable. If search is blocked by the browser check, call pass_browser_check and search again.".to_string()
            ),
        }
    }
}

#[tool_router]
impl AnnasArchiveServer {
    #[tool(
        description = "Search Anna's Archive for books, papers, magazines, comics, and other documents"
    )]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let options = SearchOptions::new(&params.query);
        let options = if let Some(page) = params.page {
            options.with_page(page)
        } else {
            options
        };

        match self.client.search(options).await {
            Ok(response) => {
                let json = serde_json::to_string_pretty(&response).map_err(|e| {
                    rmcp::ErrorData::internal_error(format!("Serialize error: {e}"), None)
                })?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(Error::BrowserCheck) => Ok(CallToolResult::error(vec![Content::text(
                "Search failed: blocked by Anna's Archive's browser check. Call pass_browser_check so the user can pass it in a browser window, then search again.",
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Search failed: {e}"
            ))])),
        }
    }

    #[tool(
        description = "Open a browser window where the user passes Anna's Archive's browser check. Call this when search reports being blocked by the browser check, then search again. Waits up to 2 minutes for the user and needs them at the computer."
    )]
    async fn pass_browser_check(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.client.pass_browser_check().await {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(
                "Browser check passed. Searches will work for about 15 minutes.",
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Browser check failed: {e}"
            ))])),
        }
    }

    #[tool(description = "Get detailed metadata for an item by its MD5 hash")]
    async fn get_details(
        &self,
        Parameters(params): Parameters<DetailsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.client.get_details(&params.md5).await {
            Ok(details) => {
                let json = serde_json::to_string_pretty(&details).map_err(|e| {
                    rmcp::ErrorData::internal_error(format!("Serialize error: {e}"), None)
                })?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Failed to get details: {e}"
            ))])),
        }
    }

    #[tool(
        description = "Get a fast download URL for an item (requires ANNAS_ARCHIVE_API_KEY environment variable)"
    )]
    async fn get_download_url(
        &self,
        Parameters(params): Parameters<DownloadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self
            .client
            .get_download_url(&params.md5, params.path_index, params.domain_index)
            .await
        {
            Ok(info) => {
                let json = serde_json::to_string_pretty(&info).map_err(|e| {
                    rmcp::ErrorData::internal_error(format!("Serialize error: {e}"), None)
                })?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Failed to get download URL: {e}"
            ))])),
        }
    }
}
