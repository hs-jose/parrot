pub const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// Cap tool result content at MAX_TOOL_OUTPUT_BYTES with a truncation
/// marker. The cut point always lands on a UTF-8 char boundary.
pub fn truncate_tool_content(content: &str) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_string();
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}... (truncated, total {} bytes)",
        &content[..end],
        content.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_tool_content_noop_under_cap() {
        let content = "x".repeat(100);
        assert_eq!(truncate_tool_content(&content), content);
    }

    #[test]
    fn truncate_tool_content_caps_and_marks() {
        let content = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 1000);
        let out = truncate_tool_content(&content);
        assert!(
            out.len() <= MAX_TOOL_OUTPUT_BYTES + 64,
            "capped output must stay within cap + marker, got {}",
            out.len()
        );
        assert_eq!(
            out,
            format!(
                "{}... (truncated, total {} bytes)",
                "x".repeat(MAX_TOOL_OUTPUT_BYTES),
                content.len()
            )
        );
    }

    #[test]
    fn truncate_tool_content_cuts_on_char_boundary() {
        let content = "你".repeat(MAX_TOOL_OUTPUT_BYTES / 3 + 10);
        let out = truncate_tool_content(&content);
        assert!(out.starts_with('你'));
        assert!(out.contains("(truncated"));
        assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES + 64);
    }
}
