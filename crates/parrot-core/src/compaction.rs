use crate::context::estimate_tokens;
use crate::types::{ChatMessage, ChatRole};

pub const SUMMARY_MARKER: &str = "[CONVERSATION SUMMARY]";

/// 摘要请求的 system prompt(spec §6,固定内嵌常量)。
pub const SUMMARIZATION_PROMPT: &str = "你是压缩助手。将以下对话历史压缩为结构化摘要,供后续对话参考。\n输出格式(纯文本,不调用任何工具):\n<summary>\n## 目标与任务\n## 关键事实与决定(文件路径、命令、命名、约束)\n## 未完成事项与下一步\n## 关键文件/代码位置\n</summary>";

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub threshold: f32,
    pub keep_recent_tokens: u32,
    pub summary_max_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.9,
            keep_recent_tokens: 20_000,
            summary_max_tokens: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextLimits {
    pub max_history_tokens: u32,
    pub keep_recent_turns: u32,
    pub compaction: CompactionConfig,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_history_tokens: 100_000,
            keep_recent_turns: 10,
            compaction: CompactionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan {
    /// 待摘要区(含旧摘要,不含 system)起点下标。
    pub summarize_start: usize,
    /// 保留区起点下标(指向某个 User 消息,tool 配对安全)。
    pub cut_index: usize,
}

pub fn is_summary_message(msg: &ChatMessage) -> bool {
    msg.role == ChatRole::User && msg.content.starts_with(SUMMARY_MARKER)
}

pub fn serialize_conversation(messages: &[ChatMessage]) -> String {
    let mut s = String::from("<conversation>\n");
    for m in messages {
        let role = match m.role {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::Tool => "tool",
        };
        s.push_str(&format!("[{}]\n{}\n\n", role, m.content));
    }
    s.push_str("</conversation>");
    s
}

/// 决定是否压缩以及在哪儿切。返回 `None` 表示本轮不压缩:
/// 禁用 / 未超阈值 / 不足两条完整 turn / 待摘要区为空。
///
/// 切点规则(spec §3.1):从最新 turn 往回累计估算 token 直到
/// ≥ `keep_recent_tokens`,切点对齐到 User 消息(turn 边界),
/// Tool 消息绝不孤立。若回退累计吞掉全部 turn(keep 预算 ≥ 历史
/// 总量但总量已超阈值),强制只保留最新 1 个完整 turn(spec §7)。
pub fn plan_compaction(
    context: &[ChatMessage],
    budget_tokens: u32,
    config: &CompactionConfig,
) -> Option<CompactionPlan> {
    if !config.enabled || context.is_empty() {
        return None;
    }

    let total: u32 = context.iter().map(estimate_tokens).sum();
    if (total as f32) <= budget_tokens as f32 * config.threshold {
        return None;
    }

    // system(若有)不参与压缩。
    let body_start = match context.first() {
        Some(m) if m.role == ChatRole::System => 1,
        _ => 0,
    };

    // 真实对话 turn 的起点:User 消息,排除旧摘要消息。
    let turn_starts: Vec<usize> = context
        .iter()
        .enumerate()
        .filter(|(i, m)| *i >= body_start && m.role == ChatRole::User && !is_summary_message(m))
        .map(|(i, _)| i)
        .collect();
    if turn_starts.len() < 2 {
        return None;
    }

    // 从最新 turn 往回累计,至少保留 1 个完整 turn。
    let turn_end = |k: usize| turn_starts.get(k + 1).copied().unwrap_or(context.len());
    let mut kept_turns = 0usize;
    let mut acc: u32 = 0;
    for k in (0..turn_starts.len()).rev() {
        if kept_turns >= 1 && acc >= config.keep_recent_tokens {
            break;
        }
        let start = turn_starts[k];
        acc += context[start..turn_end(k)]
            .iter()
            .map(estimate_tokens)
            .sum::<u32>();
        kept_turns += 1;
    }
    // 回退吞掉全部 turn ⇒ 强制只留最新 1 个,保证压缩有进展。
    if kept_turns == turn_starts.len() {
        kept_turns = 1;
    }

    let cut_index = turn_starts[turn_starts.len() - kept_turns];
    if cut_index <= body_start {
        return None; // 待摘要区为空,没东西可摘
    }

    Some(CompactionPlan {
        summarize_start: body_start,
        cut_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        }
    }

    /// [sys, u1, a1, u2, a2, ...] 每条 turn 的 user 消息 chars_each 字符。
    fn context_with_turns(turns: usize, chars_each: usize) -> Vec<ChatMessage> {
        let mut ctx = vec![msg(ChatRole::System, "sys")];
        for i in 0..turns {
            ctx.push(msg(
                ChatRole::User,
                &format!("q{}{}", i, "x".repeat(chars_each)),
            ));
            ctx.push(msg(ChatRole::Assistant, "done"));
        }
        ctx
    }

    fn default_cfg() -> CompactionConfig {
        CompactionConfig::default()
    }

    #[test]
    fn plan_disabled_returns_none() {
        let ctx = context_with_turns(5, 400);
        let cfg = CompactionConfig {
            enabled: false,
            ..default_cfg()
        };
        assert!(plan_compaction(&ctx, 1, &cfg).is_none());
    }

    #[test]
    fn plan_under_threshold_returns_none() {
        // 5 turns × ~100 tokens = ~500 est tokens, budget 1000 ⇒ 500 < 900.
        let ctx = context_with_turns(5, 400);
        assert!(plan_compaction(&ctx, 1000, &default_cfg()).is_none());
    }

    #[test]
    fn plan_fires_over_threshold() {
        // 3 turns × ~1000 tokens = ~3000 > 900 (budget 1000 × 0.9).
        let ctx = context_with_turns(3, 4000);
        // keep 1000 tokens ⇒ newest turn (~1001 tokens) alone ≥ keep ⇒ kept = 1 turn.
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(plan.summarize_start, 1, "system at 0 excluded");
        assert_eq!(
            plan.cut_index,
            ctx.len() - 2,
            "kept region = last turn (u3, a3)"
        );
        assert_eq!(
            ctx[plan.cut_index].role,
            ChatRole::User,
            "cut must land on a User message"
        );
    }

    #[test]
    fn plan_single_turn_returns_none() {
        let ctx = context_with_turns(1, 4000);
        assert!(plan_compaction(&ctx, 1, &default_cfg()).is_none());
    }

    #[test]
    fn plan_walk_fallback_keeps_only_newest_turn() {
        // keep_recent_tokens huge ⇒ 回退累计吞掉所有 turn ⇒ 强制只留最新 1 个 turn。
        let ctx = context_with_turns(3, 4000);
        let cfg = CompactionConfig {
            keep_recent_tokens: 1_000_000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("must make progress");
        assert_eq!(
            plan.cut_index,
            ctx.len() - 2,
            "fallback keeps only newest turn"
        );
        assert!(
            plan.cut_index > plan.summarize_start,
            "summarize region non-empty"
        );
    }

    #[test]
    fn plan_includes_old_summary_in_summarize_region() {
        let mut ctx = context_with_turns(3, 4000);
        ctx.insert(
            1,
            msg(ChatRole::User, &format!("{SUMMARY_MARKER}\nold summary")),
        );
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(
            plan.summarize_start, 1,
            "old summary at index 1 included in region"
        );
        assert!(is_summary_message(&ctx[plan.summarize_start]));
        assert!(plan.cut_index > 2, "cut is past the old summary");
    }

    #[test]
    fn plan_without_system_prompt_uses_body_start_zero() {
        let mut ctx = context_with_turns(3, 4000);
        ctx.remove(0); // drop system
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(plan.summarize_start, 0);
    }

    #[test]
    fn plan_cut_never_lands_on_tool_message() {
        // turn with tool calls: User, Assistant(tool_calls), Tool, Assistant
        let mut ctx = vec![msg(ChatRole::System, "sys")];
        for i in 0..4 {
            ctx.push(msg(ChatRole::User, &format!("q{}{}", i, "x".repeat(4000))));
            let mut a = msg(ChatRole::Assistant, "");
            a.tool_calls = Some(vec![]);
            ctx.push(a);
            ctx.push(msg(ChatRole::Tool, "result"));
            ctx.push(msg(ChatRole::Assistant, "done"));
        }
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(ctx[plan.cut_index].role, ChatRole::User);
    }

    #[test]
    fn serialize_conversation_renders_roles() {
        let ctx = vec![msg(ChatRole::User, "hi"), msg(ChatRole::Assistant, "hello")];
        let s = serialize_conversation(&ctx);
        assert!(s.starts_with("<conversation>"));
        assert!(s.ends_with("</conversation>"));
        assert!(s.contains("[user]\nhi"));
        assert!(s.contains("[assistant]\nhello"));
    }

    #[test]
    fn is_summary_message_requires_marker_and_user_role() {
        assert!(is_summary_message(&msg(
            ChatRole::User,
            "[CONVERSATION SUMMARY]\nabc"
        )));
        assert!(!is_summary_message(&msg(
            ChatRole::Assistant,
            "[CONVERSATION SUMMARY]\nabc"
        )));
        assert!(!is_summary_message(&msg(ChatRole::User, "plain")));
    }
}
