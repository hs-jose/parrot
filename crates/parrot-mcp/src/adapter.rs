use rmcp::model::ContentBlock;
use rmcp::service::ServiceError;

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
