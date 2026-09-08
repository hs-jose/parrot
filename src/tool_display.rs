//! 工具调用参数/文本的展示辅助，TUI 与 CLI 共用。
use serde_json::Value;

/// 将工具参数压缩成单行展示串：单字段对象直接取值（如 `{"path":"x"}` → `x`），
/// 多字段对象退化为紧凑 JSON，由调用方再截断。
pub(crate) fn compact_args(args: &Value) -> String {
    if args.is_null() {
        return String::new();
    }
    if let Some(obj) = args.as_object() {
        if obj.len() == 1 {
            if let Some(v) = obj.values().next() {
                if let Some(s) = v.as_str() {
                    return s.to_string();
                }
            }
        }
    }
    serde_json::to_string(args).unwrap_or_default()
}

/// 按字符数截断（超出补 `…`）。ui 渲染、confirm modal 与 CLI 流式输出共用。
pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
