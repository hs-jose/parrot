use crate::tui::app::PendingConfirmation;

/// 格式化工具二次确认 modal 文本：
/// 第 1 行：工具名
/// 后续：JSON pretty arguments（最多 8 行 / 400 字符截断）
pub(crate) fn format_confirmation(p: &PendingConfirmation) -> String {
    let pretty =
        serde_json::to_string_pretty(&p.arguments).unwrap_or_else(|_| "<unprintable>".into());
    let mut lines: Vec<&str> = pretty.lines().collect();
    if lines.len() > 8 {
        lines.truncate(8);
        lines.push("    ...");
    }
    let body = lines.join("\n");
    let truncated_body = if body.chars().count() > 400 {
        let t: String = body.chars().take(400).collect();
        format!("{}…", t)
    } else {
        body
    };
    format!(
        "Approve tool call?\n\nTool: {}\nArguments:\n{}",
        p.tool_name, truncated_body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pc(name: &str, args: serde_json::Value) -> PendingConfirmation {
        PendingConfirmation {
            tool_call_id: "x".into(),
            tool_name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn short_args_render_full() {
        let p = pc("file_read", json!({"path": "src/lib.rs"}));
        let s = format_confirmation(&p);
        assert!(s.contains("Tool: file_read"));
        assert!(s.contains(r#""path": "src/lib.rs""#));
    }

    #[test]
    fn large_args_truncated_body() {
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(format!("k{i}"), json!(format!("v{}", "x".repeat(40))));
        }
        let p = pc("shell_exec", json!(obj));
        let s = format_confirmation(&p);
        assert!(s.contains("…") || s.contains("..."));
    }
}
