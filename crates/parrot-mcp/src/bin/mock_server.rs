//! Mock MCP stdio server（开发/测试用）。
//! 工具：echo(message) 回显；fail() 返回工具级错误；exit() 使本进程退出
//! （用于验证 client 侧崩溃检测/热下线）。

use rmcp::schemars::JsonSchema;
use rmcp::serde::Deserialize;
use rmcp::{
    handler::server::wrapper::Parameters, model::*, tool, tool_handler, tool_router,
    ErrorData as McpError, ServerHandler, ServiceExt,
};

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rmcp::serde")]
#[schemars(crate = "rmcp::schemars")]
struct EchoArgs {
    message: String,
}

#[derive(Clone)]
struct MockServer;

#[tool_router]
impl MockServer {
    #[tool(description = "Echo the message argument back with an echo: prefix")]
    fn echo(
        &self,
        Parameters(EchoArgs { message }): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "echo: {message}"
        ))]))
    }

    #[tool(description = "Always returns a tool-level error result")]
    fn fail(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "mock failure detail",
        )]))
    }

    #[tool(description = "Terminate this mock server process (crash simulation)")]
    fn exit(&self) -> Result<CallToolResult, McpError> {
        // 延迟退出：先让 "bye" 响应送达 client，再终止进程模拟崩溃。
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        });
        Ok(CallToolResult::success(vec![ContentBlock::text("bye")]))
    }
}

#[tool_handler]
impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("parrot-mock-mcp", "0.1.0"))
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let service = MockServer
            .serve(rmcp::transport::stdio())
            .await
            .expect("serve stdio");
        let _ = service.waiting().await;
    });
}
