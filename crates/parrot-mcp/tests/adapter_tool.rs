//! McpTool 适配器行为测试：借助 mock server 验证 name/schema/调用映射。

use parrot_core::tool::Tool;
use parrot_mcp::{qualified_tool_name, McpService};
use rmcp::service::ServiceExt;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

async fn connect_mock() -> (McpService, Vec<rmcp::model::Tool>) {
    let cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot_mcp_mock_server"));
    let transport = rmcp::transport::TokioChildProcess::new(cmd).expect("spawn mock server");
    let (tx, _rx) = mpsc::channel::<()>(4);
    let service = parrot_mcp::ListChangeNotify { tx }
        .serve(transport)
        .await
        .expect("handshake");
    let tools = service.list_all_tools().await.expect("list tools");
    (Arc::new(RwLock::new(service)), tools)
}

#[tokio::test]
async fn mcp_tool_trait_semantics() {
    let (service, defs) = connect_mock().await;
    let echo = defs.iter().find(|t| t.name == "echo").expect("echo tool");
    let tool = parrot_mcp::McpTool::new("mock", echo, Arc::clone(&service), 30);

    assert_eq!(tool.name(), "mcp__mock__echo");
    assert_eq!(tool.name(), qualified_tool_name("mock", "echo"));
    assert!(tool.description().contains("Echo"));
    let schema = tool.input_schema();
    assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));

    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = tool
        .call(serde_json::json!({"message": "hi"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.content, "echo: hi");
    assert!(!out.is_error);
}

#[tokio::test]
async fn tool_level_error_maps_to_is_error_with_full_text() {
    let (service, defs) = connect_mock().await;
    let fail = defs.iter().find(|t| t.name == "fail").expect("fail tool");
    let tool = parrot_mcp::McpTool::new("mock", fail, service, 30);
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = tool.call(serde_json::json!({}), &ctx).await.unwrap();
    assert!(out.is_error, "server isError=true 必须映射 is_error");
    assert_eq!(out.content, "mock failure detail", "错误全文原样");
}

#[tokio::test]
async fn unknown_tool_name_yields_protocol_error_with_code() {
    let (service, _defs) = connect_mock().await;
    let t = defs_placeholder_tool(&service);
    let out = t.call_for_test(serde_json::json!({})).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("code=-32602")
            || out.content.contains("code=-32603")
            || out.content.contains("MCP 协议错误"),
        "保真: {}",
        out.content
    );
}

fn defs_placeholder_tool(service: &McpService) -> parrot_mcp::McpTool {
    let def = rmcp::model::Tool::new(
        "no_such_tool",
        "phantom",
        rmcp::model::JsonObject::default(),
    );
    parrot_mcp::McpTool::new("mock", &def, Arc::clone(service), 30)
}
