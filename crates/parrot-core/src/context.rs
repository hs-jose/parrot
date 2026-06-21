use crate::types::{ChatMessage, ChatRole};

pub struct ContextManager {
    pub max_tokens: u32,
    pub keep_recent_turns: u32,
}

impl ContextManager {
    pub fn new(max_tokens: u32, keep_recent_turns: u32) -> Self {
        Self { max_tokens, keep_recent_turns }
    }

    /// Estimate token count for a message (rough approximation: ~4 chars per token)
    fn estimate_tokens(msg: &ChatMessage) -> u32 {
        (msg.content.len() as u32) / 4 + 1
    }

    /// Prune messages using sliding window context pruning:
    /// - Always keep system messages
    /// - Keep the last N turn pairs (user + assistant)
    /// - Drop oldest non-system messages first
    /// - Stop at minimum window (system messages + keep_recent_turns * 2)
    pub fn prune(&self, messages: &mut Vec<ChatMessage>) {
        if messages.is_empty() {
            return;
        }

        // Calculate total tokens
        let total_tokens: u32 = messages.iter().map(Self::estimate_tokens).sum();

        if total_tokens <= self.max_tokens {
            return;
        }

        // Minimum messages we must keep: system messages + keep_recent_turns * 2 conversation messages
        let system_count = messages.iter().filter(|m| m.role == ChatRole::System).count();
        let min_messages = system_count + (self.keep_recent_turns as usize) * 2;

        // If we're already at minimum, can't prune further
        if messages.len() <= min_messages {
            return;
        }

        // Remove oldest non-system messages until we're under budget or at minimum
        loop {
            let current_tokens: u32 = messages.iter().map(Self::estimate_tokens).sum();
            if current_tokens <= self.max_tokens || messages.len() <= min_messages {
                break;
            }

            // Find the first non-system message and remove it
            let idx = messages.iter().position(|m| m.role != ChatRole::System);
            match idx {
                Some(i) => {
                    messages.remove(i);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages(count: usize, chars_each: usize) -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage { role: ChatRole::System, content: "You are a helpful assistant.".to_string(), tool_call_id: None, tool_name: None }];
        for _i in 0..count {
            msgs.push(ChatMessage { role: ChatRole::User, content: "a".repeat(chars_each), tool_call_id: None, tool_name: None });
            msgs.push(ChatMessage { role: ChatRole::Assistant, content: "b".repeat(chars_each), tool_call_id: None, tool_name: None });
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
        let min_len = 1 + 2 * 2;
        mgr.prune(&mut msgs);
        assert!(msgs.len() >= min_len);
    }
}