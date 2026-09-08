//! file_edit：字符串精准匹配替换。
//!
//! 三重护栏：read-before-edit（见 call）、唯一性（Ambiguous）、
//! CRLF 归一匹配（file_read 输出把 CRLF 归一成 LF，模型抄出的
//! old_string 必然是 LF）。

// 本模块的纯函数暂无 crate 内消费者（Task 5 的 impl Tool 接入后删除以下豁免）。
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MatchOutcome {
    Replaced { content: String, count: usize },
    Ambiguous(usize),
    NotFound,
}

/// 精确匹配替换。命中则返回 (新内容, 次数)；未命中 NotFound；
/// 命中多次且未 replace_all 时 Ambiguous。
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
}
