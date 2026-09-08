//! file_edit：字符串精准匹配替换。
//!
//! 三重护栏：read-before-edit（见 call）、唯一性（Ambiguous）、
//! CRLF 归一匹配（file_read 输出把 CRLF 归一成 LF，模型抄出的
//! old_string 必然是 LF）。

use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MatchOutcome {
    Replaced { content: String, count: usize },
    Ambiguous(usize),
    NotFound,
}

/// 精确匹配替换。命中则返回 (新内容, 次数)；未命中 NotFound；
/// 命中多次且未 replace_all 时 Ambiguous。
fn replace_exact(original: &str, old: &str, new: &str, replace_all: bool) -> MatchOutcome {
    let count = original.matches(old).count();
    if count == 0 {
        return MatchOutcome::NotFound;
    }
    if count > 1 && !replace_all {
        return MatchOutcome::Ambiguous(count);
    }
    MatchOutcome::Replaced {
        content: original.replace(old, new),
        count,
    }
}

/// CRLF 归一匹配：haystack 与 needle 均 `\r\n → \n` 后定位替换，再按
/// 原文件含 \r\n 与否统一恢复行尾（混合行尾文件统一化，属可接受行为）。
/// 仅当原文件确实含 \r\n 时才可能命中。
fn replace_crlf_normalized(
    original: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> MatchOutcome {
    if !original.contains('\r') {
        return MatchOutcome::NotFound;
    }
    let norm_original = original.replace("\r\n", "\n");
    let norm_old = old.replace("\r\n", "\n");
    let norm_new = new.replace("\r\n", "\n");
    let count = norm_original.matches(&norm_old).count();
    if count == 0 {
        return MatchOutcome::NotFound;
    }
    if count > 1 && !replace_all {
        return MatchOutcome::Ambiguous(count);
    }
    let updated_lf = norm_original.replace(&norm_old, &norm_new);
    MatchOutcome::Replaced {
        content: updated_lf.replace('\n', "\r\n"),
        count,
    }
}

/// not-found 自纠线索：取 old_string 首行 trim 后在文件中做行级查找，
/// 命中则返回该行附近 ±2 行的原文片段（带行号），帮模型发现行号前缀 /
/// 空白差异导致的抄错。只提示，不自动替换。
fn nearest_context_hint(original: &str, old: &str) -> Option<String> {
    let needle = old.lines().next()?.trim();
    if needle.is_empty() {
        return None;
    }
    let lines: Vec<&str> = original.lines().collect();
    let hit = lines.iter().position(|l| l.trim() == needle)?;
    let start = hit.saturating_sub(2);
    let end = (hit + 3).min(lines.len());
    let mut out = String::from("\nClosest match near:\n");
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("  {:>4}: {}\n", start + i + 1, line));
    }
    Some(out)
}

#[derive(Default)]
pub struct FileEditTool;

impl FileEditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. The file must be read with file_read first. \
         Copy old_string exactly from the file_read output WITHOUT the line-number prefixes; \
         it must be unique in the file unless replace_all is true."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace; must be unique in the file unless replace_all is true"
                },
                "new_string": {
                    "type": "string",
                    "description": "Text to replace old_string with"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique match (optional, default false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let tool = "file_edit";
        let path_str = super::str_arg(&arguments, "path", tool)?;
        let old_string = super::str_arg(&arguments, "old_string", tool)?;
        let new_string = super::str_arg(&arguments, "new_string", tool)?;
        let replace_all = arguments
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string.is_empty() {
            return Ok(ToolOutput {
                content: "old_string must not be empty".to_string(),
                is_error: true,
            });
        }
        if old_string == new_string {
            return Ok(ToolOutput {
                content: "old_string and new_string are identical; nothing to do".to_string(),
                is_error: true,
            });
        }

        let path = super::resolve_arg_path(path_str, &ctx.working_dir);

        // read-before-edit 护栏：没读过就编辑，大概率是凭空构造 old_string。
        if !ctx.files_read.lock().unwrap().contains(&path) {
            return Ok(ToolOutput {
                content: format!("File not read yet: {:?}. Call file_read first.", path),
                is_error: true,
            });
        }

        let metadata = std::fs::metadata(&path).map_err(|e| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Cannot access file {:?}: {}", path, e),
        })?;
        if metadata.len() > ctx.max_file_size_bytes {
            return Ok(ToolOutput {
                content: format!(
                    "File too large: {} bytes (max: {} bytes)",
                    metadata.len(),
                    ctx.max_file_size_bytes
                ),
                is_error: true,
            });
        }

        let original = std::fs::read_to_string(&path).map_err(|e| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Cannot read file {:?}: {}", path, e),
        })?;

        // 先精确匹配；失败再 CRLF 归一匹配。
        let outcome = match replace_exact(&original, old_string, new_string, replace_all) {
            MatchOutcome::NotFound => {
                replace_crlf_normalized(&original, old_string, new_string, replace_all)
            }
            other => other,
        };

        let (content, count) = match outcome {
            MatchOutcome::Replaced { content, count } => (content, count),
            MatchOutcome::Ambiguous(n) => {
                return Ok(ToolOutput {
                    content: format!(
                        "old_string matches {n} locations in {:?}. Add surrounding context to make it unique, or set replace_all=true.",
                        path
                    ),
                    is_error: true,
                });
            }
            MatchOutcome::NotFound => {
                let msg = match nearest_context_hint(&original, old_string) {
                    Some(hint) => format!("old_string not found in {:?}.{hint}", path),
                    None => format!(
                        "old_string not found in {:?}. Re-read the file and copy the text exactly.",
                        path
                    ),
                };
                return Ok(ToolOutput {
                    content: msg,
                    is_error: true,
                });
            }
        };

        std::fs::write(&path, content).map_err(|e| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Cannot write file {:?}: {}", path, e),
        })?;

        // 编辑成功后保留已读状态，允许对同一文件连续编辑（陈旧性由精确
        // 匹配失败兜底）。
        ctx.files_read.lock().unwrap().insert(path.clone());

        Ok(ToolOutput {
            content: format!(
                "Successfully replaced {} occurrence(s) in {:?}",
                count, path
            ),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_exact_replaces_unique() {
        let out = replace_exact("a b c", "b", "X", false);
        assert_eq!(
            out,
            MatchOutcome::Replaced {
                content: "a X c".to_string(),
                count: 1
            }
        );
    }

    #[test]
    fn replace_exact_replace_all_counts_every_occurrence() {
        let out = replace_exact("a b b c", "b", "X", true);
        assert_eq!(
            out,
            MatchOutcome::Replaced {
                content: "a X X c".to_string(),
                count: 2
            }
        );
    }

    #[test]
    fn replace_exact_multiple_without_replace_all_is_ambiguous() {
        assert_eq!(
            replace_exact("a b b c", "b", "X", false),
            MatchOutcome::Ambiguous(2)
        );
    }

    #[test]
    fn replace_exact_not_found() {
        assert_eq!(
            replace_exact("abc", "zzz", "X", false),
            MatchOutcome::NotFound
        );
    }

    #[test]
    fn crlf_normalized_matches_lf_needle_and_restores_crlf() {
        let original = "fn main() {\r\n    println!(\"hello\");\r\n}\r\n";
        let out = replace_crlf_normalized(
            original,
            "println!(\"hello\");",
            "println!(\"edited\");",
            false,
        );
        match out {
            MatchOutcome::Replaced { content, count } => {
                assert_eq!(count, 1);
                assert_eq!(
                    content, "fn main() {\r\n    println!(\"edited\");\r\n}\r\n",
                    "写回必须保留 CRLF 行尾"
                );
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn crlf_normalized_skips_lf_only_file() {
        assert_eq!(
            replace_crlf_normalized("a\nb\n", "b", "X", false),
            MatchOutcome::NotFound
        );
    }

    #[test]
    fn crlf_normalized_ambiguous_propagates() {
        let original = "x\r\ny\r\nx\r\n";
        assert_eq!(
            replace_crlf_normalized(original, "x", "X", false),
            MatchOutcome::Ambiguous(2)
        );
    }

    #[test]
    fn context_hint_shows_neighborhood_of_first_line_match() {
        let original = "fn a() {}\nfn b() {\n    todo!()\n}\n";
        // 模型抄出的 old_string 首行与文件一致但后续行有偏差
        let hint = nearest_context_hint(original, "fn b() {\n    todo !()\n}");
        let hint = hint.expect("应给出线索");
        assert!(hint.contains("fn b() {"));
        assert!(hint.contains("todo!()"));
        assert!(hint.contains('2'), "片段应带行号");
    }

    #[test]
    fn context_hint_none_when_first_line_absent() {
        assert!(nearest_context_hint("a\nb\n", "zzz\nx").is_none());
    }

    // ---- Tool 集成测试（追加到上方 tests 模块内）----
    // FileEditTool / Tool / ToolContext / ToolOutput 均经 `use super::*;`
    // 继承自本文件顶部与 impl，勿重复引入。
    use serde_json::json;

    fn fixture_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    async fn run_edit(
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> parrot_protocol::types::ToolOutput {
        FileEditTool.call(args, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn edit_replaces_unique_occurrence() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "println!(\"hello\");", "new_string": "println!(\"edited\");"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Successfully replaced 1 occurrence"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    println!(\"edited\");\n}\n"
        );
    }

    #[tokio::test]
    async fn edit_requires_read_first() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "hello", "new_string": "X"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("File not read yet"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_ambiguous_match_errors() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\nx\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("2 locations"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_not_found_includes_hint() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "fn a() {}\nfn b() {\n    todo!()\n}\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "fn b() {\n    todo !()\n}", "new_string": "X"}),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("Closest match near"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn edit_crlf_file_with_lf_needle_succeeds_and_preserves_crlf() {
        let dir = fixture_dir();
        let path = dir.path().join("win.rs");
        std::fs::write(&path, "fn main() {\r\n    println!(\"hello\");\r\n}\r\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "win.rs", "old_string": "println!(\"hello\");", "new_string": "println!(\"edited\");"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\r\n    println!(\"edited\");\r\n}\r\n"
        );
    }

    #[tokio::test]
    async fn edit_identical_strings_is_noop_error() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "same", "new_string": "same"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("identical"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_empty_old_string_is_error() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "", "new_string": "y"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("must not be empty"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_replace_all_rewrites_every_occurrence() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\ny\nx\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "x", "new_string": "z", "replace_all": true}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("2 occurrence"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "z\ny\nz\n");
    }
}
