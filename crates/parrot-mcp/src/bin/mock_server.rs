//! Mock MCP stdio server（开发/测试用）。
//! 工具：echo(message) 回显；fail() 返回工具级错误；exit() 使本进程退出
//! （用于验证 client 侧崩溃检测/热下线）；extend() 把自身替换为 extra 工具
//! 并广播 tools/list_changed（用于验证 client 侧 list_changed 重枚举）。

use std::sync::Arc;

use rmcp::model::*;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};

struct MockServer {
    /// 当前对外工具名列表（extend 调用后动态变化）。
    tools: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl MockServer {
    fn new() -> Self {
        Self {
            tools: Arc::new(tokio::sync::Mutex::new(vec![
                "echo".to_string(),
                "fail".to_string(),
                "exit".to_string(),
                "extend".to_string(),
            ])),
        }
    }

    fn tool_def(name: &str) -> Tool {
        let description = match name {
            "echo" => "Echo the message argument back with an echo: prefix",
            "fail" => "Always returns a tool-level error result",
            "exit" => "Terminate this mock server process (crash simulation)",
            "extend" => "Emit notifications/tools/list_changed adding an extra tool",
            _ => "Extra tool added dynamically by extend",
        };
        let input_schema: JsonObject = match name {
            "echo" => serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"]
            }))
            .expect("echo schema"),
            _ => JsonObject::default(),
        };
        Tool::new(name.to_string(), description, input_schema)
    }
}

impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("parrot-mock-mcp", "0.1.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let names = self.tools.lock().await.clone();
        Ok(ListToolsResult::with_all_items(
            names.iter().map(|n| Self::tool_def(n)).collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match &*request.name {
            "echo" => {
                let message = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(
                    CallToolResult::success(vec![ContentBlock::text(format!("echo: {message}"))])
                        .into(),
                )
            }
            "fail" => {
                Ok(CallToolResult::error(vec![ContentBlock::text("mock failure detail")]).into())
            }
            "exit" => {
                // 延迟退出：先让 "bye" 响应送达 client，再终止进程模拟崩溃。
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    std::process::exit(0);
                });
                Ok(CallToolResult::success(vec![ContentBlock::text("bye")]).into())
            }
            "extend" => {
                let mut names = self.tools.lock().await;
                if let Some(pos) = names.iter().position(|t| t == "extend") {
                    names[pos] = "extra".to_string();
                } else {
                    names.push("extra".to_string());
                }
                drop(names);
                let _ = context.peer.notify_tool_list_changed().await;
                Ok(CallToolResult::success(vec![ContentBlock::text("extended")]).into())
            }
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let service = MockServer::new()
            .serve(rmcp::transport::stdio())
            .await
            .expect("serve stdio");
        let _ = service.waiting().await;
    });
}
