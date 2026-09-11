use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext};
use parrot_protocol::types::ToolOutput;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{CallToolRequestParams, ContentBlock, JsonObject, Tool as McpToolDef};
use rmcp::service::{NotificationContext, RoleClient, RunningService, ServiceError};
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};

/// 工具注册名：`mcp__<server_id>__<tool_name>`（spec §2，Claude Code 同款约定）。
pub fn qualified_tool_name(server_id: &str, tool_name: &str) -> String {
    format!("mcp__{server_id}__{tool_name}")
}

/// 把 MCP content 数组拍平为纯文本（spec §3.4 结果映射）。
pub fn flatten_content(content: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(i) => {
                parts.push(format!("[image: {}, {} bytes]", i.mime_type, i.data.len()));
            }
            ContentBlock::Audio(a) => {
                parts.push(format!("[audio: {}, {} bytes]", a.mime_type, a.data.len()));
            }
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[resource link: {}]", link.uri));
            }
            ContentBlock::Resource(r) => {
                let uri = match &r.resource {
                    rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri.clone(),
                    rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri.clone(),
                    _ => "(unknown resource)".to_string(),
                };
                parts.push(format!("[embedded resource: {uri}]"));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// rmcp `ServiceError` → 保真错误文案（spec §3.4 错误保真原则）。
pub fn map_service_error(server_id: &str, e: &ServiceError) -> String {
    match e {
        ServiceError::McpError(err) => match &err.data {
            Some(data) => format!(
                "MCP 协议错误 code={}: {}\ndata: {data}",
                err.code.0, err.message
            ),
            None => format!("MCP 协议错误 code={}: {}", err.code.0, err.message),
        },
        ServiceError::TransportClosed => {
            format!("MCP server {server_id} 连接已断开(进程可能已退出)")
        }
        ServiceError::Timeout { timeout } => format!("MCP 内部超时({timeout:?})"),
        ServiceError::Cancelled { reason } => match reason {
            Some(r) => format!("MCP 调用被取消: {r}"),
            None => "MCP 调用被取消".to_string(),
        },
        other => format!("MCP 调用失败: {other}"),
    }
}

/// server `tools/list_changed` 通知 → 唤醒 manager 重枚举。
#[derive(Clone)]
pub struct ListChangeNotify {
    pub tx: mpsc::Sender<()>,
}

impl ClientHandler for ListChangeNotify {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.tx.send(()).await;
    }
}

/// manager 与各 `McpTool` 共享的 rmcp client 服务句柄。
/// `call_tool` 只需 `&self`，读锁即可；`close_with_timeout` 需要 `&mut`，manager 用写锁。
pub type McpService = Arc<RwLock<RunningService<RoleClient, ListChangeNotify>>>;

/// 把单个 MCP 工具映射为 `parrot_core::Tool`（spec §3.4）。
pub struct McpTool {
    server_id: String,
    qualified_name: String,
    raw_name: String,
    description: String,
    input_schema: Value,
    service: McpService,
    call_timeout_secs: u64,
}

impl McpTool {
    pub fn new(
        server_id: &str,
        def: &McpToolDef,
        service: McpService,
        call_timeout_secs: u64,
    ) -> Self {
        Self {
            server_id: server_id.to_string(),
            qualified_name: qualified_tool_name(server_id, &def.name),
            raw_name: def.name.to_string(),
            description: def
                .description
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_default(),
            input_schema: def.schema_as_json_value(),
            service,
            call_timeout_secs,
        }
    }

    /// 无 `ToolContext` 的调用入口（测试/管理用途，与 `Tool::call` 共用核心逻辑）。
    pub async fn call_for_test(&self, arguments: Value) -> ToolOutput {
        self.invoke(arguments).await
    }

    async fn invoke(&self, arguments: Value) -> ToolOutput {
        let args: JsonObject = arguments.as_object().cloned().unwrap_or_default();
        let params = CallToolRequestParams::new(self.raw_name.clone()).with_arguments(args);
        let service = Arc::clone(&self.service);
        let call = async move {
            let svc = service.read().await;
            svc.call_tool(params).await
        };
        match tokio::time::timeout(Duration::from_secs(self.call_timeout_secs), call).await {
            Err(_) => ToolOutput {
                content: format!(
                    "MCP 调用超时(超过 {}s, server 未响应)",
                    self.call_timeout_secs
                ),
                is_error: true,
            },
            Ok(Err(e)) => {
                let detail = map_service_error(&self.server_id, &e);
                tracing::warn!(
                    server = %self.server_id,
                    tool = %self.qualified_name,
                    "MCP tool call failed: {detail}"
                );
                ToolOutput {
                    content: detail,
                    is_error: true,
                }
            }
            Ok(Ok(result)) => {
                let is_error = result.is_error.unwrap_or(false);
                if is_error {
                    tracing::warn!(
                        server = %self.server_id,
                        tool = %self.qualified_name,
                        "MCP tool reported error"
                    );
                }
                ToolOutput {
                    content: flatten_content(&result.content),
                    is_error,
                }
            }
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn call(&self, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        Ok(self.invoke(arguments).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ErrorCode, ImageContent, TextContent};
    use rmcp::service::ServiceError;
    use rmcp::ErrorData as McpError;

    #[test]
    fn qualified_name_format() {
        assert_eq!(
            qualified_tool_name("playwright", "browser_navigate"),
            "mcp__playwright__browser_navigate"
        );
    }

    #[test]
    fn flatten_text_joins_with_newline() {
        let blocks = vec![ContentBlock::text("a"), ContentBlock::text("b")];
        assert_eq!(flatten_content(&blocks), "a\nb");
    }

    #[test]
    fn flatten_image_becomes_placeholder() {
        let img = ContentBlock::Image(ImageContent::new("ABCD", "image/png"));
        assert_eq!(flatten_content(&[img]), "[image: image/png, 4 bytes]");
    }

    #[test]
    fn flatten_empty_is_empty() {
        assert_eq!(flatten_content(&[]), "");
    }

    #[test]
    fn service_error_mcp_error_includes_code_and_data() {
        let err = ServiceError::McpError(McpError::new(
            ErrorCode::INVALID_PARAMS,
            "bad input",
            Some(serde_json::json!({"hint": "x"})),
        ));
        let s = map_service_error("mock", &err);
        assert!(s.contains("code=-32602"), "got: {s}");
        assert!(s.contains("bad input"));
        assert!(
            s.contains(r#""hint":"x""#) || s.contains("hint"),
            "data 原样: {s}"
        );
    }

    #[test]
    fn service_error_transport_closed_mentions_server() {
        let s = map_service_error("pw", &ServiceError::TransportClosed);
        assert!(s.contains("pw") && s.contains("连接已断开"), "got: {s}");
    }

    #[test]
    fn text_content_new_helper_matches_construct() {
        let t = TextContent::new("hello");
        assert_eq!(t.text, "hello");
        let e = McpError::internal_error("boom", None);
        assert_eq!(e.code.0, -32603);
    }
}
