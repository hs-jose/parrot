use crate::types::{ChatMessage, ChatRole};

pub struct ContextManager {
    pub max_tokens: u32,
    pub keep_recent_turns: u32,
}

impl ContextManager {
    pub fn new(max_tokens: u32, keep_recent_turns: u32) -> Self {
        Self { max_tokens, keep_recent_turns }
    }

    fn estimate_tokens(msg: &ChatMessage) -> u32 {
        (msg.content.len() as u32) / 4 + 1
    }

    /// Prune the context by removing complete logical turns oldest-first.
    ///
    /// A logical turn starts at a ChatRole::User message and spans all messages
    /// up to (not including) the next ChatRole::User message. Removing complete
    /// turns preserves the Anthropic API invariant that every tool_result user
    /// message is preceded by the assistant message containing its tool_use blocks.
    pub fn prune(&self, messages: &mut Vec<ChatMessage>) {
        if messages.is_empty() {
            return;
        }

        let total_tokens: u32 = messages.iter().map(Self::estimate_tokens).sum();
        if total_tokens <= self.max_tokens {
            return;
        }

        loop {
            let current_tokens: u32 = messages.iter().map(Self::estimate_tokens).sum();
            if current_tokens <= self.max_tokens {
                break;
            }

            // Indices of ChatRole::User messages (conversation turns, not tool results)
            let user_turns: Vec<usize> = messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m.role == ChatRole::User)
                .map(|(i, _)| i)
                .collect();

            // Stop when only keep_recent_turns turns remain
            if user_turns.len() <= self.keep_recent_turns as usize {
                break;
            }

            // Remove oldest complete turn: [user_turns[0], user_turns[1])
            // This keeps assistant + tool_result + assistant intact for each turn.
            let remove_start = user_turns[0];
            let remove_end = user_turns[1];
            messages.drain(remove_start..remove_end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages(count: usize, chars_each: usize) -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage {
            role: ChatRole::System,
            content: "You are a helpful assistant.".to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        }];
        for _i in 0..count {
            msgs.push(ChatMessage { role: ChatRole::User, content: "a".repeat(chars_each), tool_call_id: None, tool_name: None, tool_calls: None });
            msgs.push(ChatMessage { role: ChatRole::Assistant, content: "b".repeat(chars_each), tool_call_id: None, tool_name: None, tool_calls: None });
        }
        msgs
    }

    #[test]
    fn prune_removes_old_messages() {
        let mgr = ContextManager::new(500, 2);
        let mut msgs = make_messages(10, 100);
        let original_len = msgs.len();
        mgr.prune(&mut msgs);
        assert!(msgs.len() < original_len);
        assert_eq!(msgs[0].role, ChatRole::System);
    }

    #[test]
    fn prune_keeps_system_message() {
        let mgr = ContextManager::new(10000, 6);
        let mut msgs = make_messages(3, 50);
        mgr.prune(&mut msgs);
        assert_eq!(msgs[0].role, ChatRole::System);
    }

    #[test]
    fn prune_stops_at_minimum_window() {
        let mgr = ContextManager::new(10, 2);
        let mut msgs = make_messages(2, 100);
        mgr.prune(&mut msgs);
        // 2 turns remain: system(1) + 2×(user+assistant) = 5 messages
        assert!(msgs.len() >= 5);
    }

    #[test]
    fn prune_preserves_tool_call_pairs() {
        // Build a context with a tool-call turn: User → Assistant{tool_calls} → Tool → Assistant
        // followed by a second User turn. Pruning should remove the complete first turn.
        let mgr = ContextManager::new(10, 1);

        let mut msgs = vec![
            ChatMessage { role: ChatRole::System,    content: "sys".to_string(),         tool_call_id: None, tool_name: None, tool_calls: None },
            // Turn 1
            ChatMessage { role: ChatRole::User,      content: "question1".to_string(),   tool_call_id: None, tool_name: None, tool_calls: None },
            ChatMessage { role: ChatRole::Assistant, content: "".to_string(),             tool_call_id: None, tool_name: None, tool_calls: Some(vec![]) },
            ChatMessage { role: ChatRole::Tool,      content: "result".to_string(),       tool_call_id: Some("id1".to_string()), tool_name: Some("file_read".to_string()), tool_calls: None },
            ChatMessage { role: ChatRole::Assistant, content: "answer1".to_string(),      tool_call_id: None, tool_name: None, tool_calls: None },
            // Turn 2
            ChatMessage { role: ChatRole::User,      content: "question2".to_string(),   tool_call_id: None, tool_name: None, tool_calls: None },
            ChatMessage { role: ChatRole::Assistant, content: "answer2".to_string(),      tool_call_id: None, tool_name: None, tool_calls: None },
        ];

        mgr.prune(&mut msgs);

        // Turn 1 (messages 1-4) should be removed; system + turn 2 remain
        assert_eq!(msgs.len(), 3, "expected system + turn2 user + turn2 assistant");
        assert_eq!(msgs[0].role, ChatRole::System);
        assert_eq!(msgs[1].role, ChatRole::User);
        assert_eq!(msgs[1].content, "question2");
        // No orphaned Tool messages remain
        assert!(!msgs.iter().any(|m| m.role == ChatRole::Tool));
    }
}
