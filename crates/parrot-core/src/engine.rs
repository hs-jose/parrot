use crate::tool::{ToolContext, ToolRegistry};
use crate::provider::ProviderRegistry;
use crate::types::{ChatMessage, ChatRole, GenerateConfig};
use crate::context::ContextManager;
use crate::event_log::{EventLog, EventLogEntry, StreamEvent};
use crate::error::AgentError;
use parrot_protocol::types::{StopReason, Usage};
use tokio::sync::mpsc;
use std::sync::Arc;

pub const MAX_REACT_ITERATIONS: u32 = 20;

pub struct ReActEngine {
    tool_registry: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    config: GenerateConfig,
    data_dir: std::path::PathBuf,
    working_dir: std::path::PathBuf,
}

impl ReActEngine {
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        provider_registry: Arc<ProviderRegistry>,
        config: GenerateConfig,
        data_dir: std::path::PathBuf,
        working_dir: std::path::PathBuf,
    ) -> Self {
        Self { tool_registry, provider_registry, config, data_dir, working_dir }
    }

    pub async fn run(
        self,
        mut cmd_rx: mpsc::Receiver<crate::session::SessionCmd>,
        event_tx: mpsc::Sender<StreamEvent>,
    ) {
        let mut context: Vec<ChatMessage> = Vec::new();
        let mut event_log = EventLog::new(self.data_dir.clone());

        let context_manager = ContextManager::new(100_000, 10);

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                crate::session::SessionCmd::Chat { message } => {
                    let ctx_len = context.len();
                    if let Err(e) = self.handle_chat(
                        &message,
                        &mut context,
                        &context_manager,
                        &event_tx,
                        &mut event_log,
                    ).await {
                        tracing::error!("Chat handling error: {}", e);
                        context.truncate(ctx_len);
                        let _ = event_tx.send(StreamEvent::TextDelta {
                            delta: format!("\nError: {}\n", e),
                        }).await;
                        let _ = event_tx.send(StreamEvent::Finish {
                            stop_reason: StopReason::Aborted,
                            usage: Usage { input_tokens: 0, output_tokens: 0 },
                        }).await;
                    }
                }
                crate::session::SessionCmd::Abort => {
                    let _ = event_tx.send(StreamEvent::Finish {
                        stop_reason: StopReason::Aborted,
                        usage: Usage { input_tokens: 0, output_tokens: 0 },
                    }).await;
                    break;
                }
            }
        }
    }

    async fn handle_chat(
        &self,
        message: &str,
        context: &mut Vec<ChatMessage>,
        context_manager: &ContextManager,
        event_tx: &mpsc::Sender<StreamEvent>,
        event_log: &mut EventLog,
    ) -> Result<(), AgentError> {
        // 1. Add user message to context
        context.push(ChatMessage {
            role: ChatRole::User,
            content: message.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });

        let _ = event_log.append(EventLogEntry::UserMessage {
            content: message.to_string(),
        });

        // 2. Get tool definitions
        let tool_defs = self.tool_registry.list_definitions().await;

        // 3. ReAct loop
        for _iteration in 0..MAX_REACT_ITERATIONS {
            // Prune context
            context_manager.prune(context);

            // 3. Call provider's chat_stream
            let provider = self.provider_registry.resolve(&self.config.model)
                .await
                .ok_or_else(|| AgentError::Config(
                    format!("No provider found for model: {}", self.config.model)
                ))?;

            let mut stream = provider.chat_stream(
                &self.config.model,
                context,
                &tool_defs,
                &self.config,
            ).await.map_err(AgentError::Provider)?;

            // 4. Collect stream events
            let mut accumulated_text = String::new();
            let mut tool_calls: Vec<PendingToolCall> = Vec::new();
            let mut stop_reason = StopReason::EndTurn;
            let mut usage = Usage { input_tokens: 0, output_tokens: 0 };

            while let Some(event) = stream.inner.recv().await {
                match event {
                    StreamEvent::TextDelta { delta } => {
                        accumulated_text.push_str(&delta);
                        let _ = event_tx.send(StreamEvent::TextDelta { delta }).await;
                    }
                    StreamEvent::ToolCallStart { id, name } => {
                        tool_calls.push(PendingToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                            arguments_json: None,
                        });
                        let _ = event_tx.send(StreamEvent::ToolCallStart { id, name }).await;
                    }
                    StreamEvent::ToolCallDelta { id, args_delta } => {
                        if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                            tc.arguments.push_str(&args_delta);
                        }
                        let _ = event_tx.send(StreamEvent::ToolCallDelta { id, args_delta }).await;
                    }
                    StreamEvent::ToolCallEnd { id, arguments: _ } => {
                        // Parse accumulated delta string as JSON (provider may send empty args)
                        let parsed_args = if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                            let parsed = serde_json::from_str(&tc.arguments)
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            tc.arguments_json = Some(parsed.clone());
                            parsed
                        } else {
                            serde_json::Value::Object(Default::default())
                        };
                        let _ = event_tx.send(StreamEvent::ToolCallEnd {
                            id,
                            arguments: parsed_args,
                        }).await;
                    }
                    StreamEvent::Finish { stop_reason: sr, usage: u } => {
                        stop_reason = sr;
                        usage = u;
                        break;
                    }
                    StreamEvent::ToolResult { .. } => {
                        // ToolResult from provider stream isn't expected here
                    }
                }
            }

            // 5. Handle tool calls if stop reason is ToolUse
            if stop_reason == StopReason::ToolUse && !tool_calls.is_empty() {
                // Send ToolUse finish to signal tool execution phase
                let _ = event_tx.send(StreamEvent::Finish {
                    stop_reason: StopReason::ToolUse,
                    usage: usage.clone(),
                }).await;

                // Build tool call info for the assistant message
                let tc_info: Vec<crate::types::ToolCallInfo> = tool_calls
                    .iter()
                    .map(|tc| crate::types::ToolCallInfo {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                    })
                    .collect();

                // Add assistant message with text + tool_use blocks
                context.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: accumulated_text.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: if tc_info.is_empty() { None } else { Some(tc_info) },
                });

                if !accumulated_text.is_empty() {
                    let _ = event_log.append(EventLogEntry::AssistantText {
                        content: accumulated_text.clone(),
                    });
                }

                // Process each tool call
                for tc in &tool_calls {
                    let tool_ctx = ToolContext {
                        working_dir: self.working_dir.clone(),
                        max_file_size_bytes: 10 * 1024 * 1024,
                    };

                    let _ = event_log.append(EventLogEntry::ToolCall {
                        tool_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                    });

                    let result = match self.tool_registry.get(&tc.name).await {
                        Some(tool) => {
                            let args = tc.arguments_json.clone().unwrap_or(serde_json::Value::Null);
                            tool.call(args, &tool_ctx).await
                        }
                        None => Err(AgentError::ToolExecution {
                            tool: tc.name.clone(),
                            message: "Tool not found".to_string(),
                        }),
                    };

                    let output = result.unwrap_or_else(|e| crate::tool::ToolOutput {
                        content: format!("Error: {}", e),
                        is_error: true,
                    });

                    let _ = event_tx.send(StreamEvent::ToolResult {
                        id: tc.id.clone(),
                        result: output.clone(),
                    }).await;

                    let _ = event_log.append(EventLogEntry::ToolResult {
                        tool_id: tc.id.clone(),
                        output: output.clone(),
                    });

                    // Add tool result to context
                    context.push(ChatMessage {
                        role: ChatRole::Tool,
                        content: output.content,
                        tool_call_id: Some(tc.id.clone()),
                        tool_name: Some(tc.name.clone()),
                        tool_calls: None,
                    });
                }
                // Continue the loop to get the next response
                continue;
            }

            // 7. End of turn — push any accumulated text as assistant message
            if !accumulated_text.is_empty() {
                context.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: accumulated_text.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                });
                let _ = event_log.append(EventLogEntry::AssistantText {
                    content: accumulated_text,
                });
            }

            let _ = event_tx.send(StreamEvent::Finish {
                stop_reason: stop_reason.clone(),
                usage: usage.clone(),
            }).await;

            let _ = event_log.append(EventLogEntry::Finish {
                stop_reason,
                usage,
            });

            return Ok(());
        }

        // Max iterations reached
        let _ = event_tx.send(StreamEvent::Finish {
            stop_reason: StopReason::MaxTokens,
            usage: Usage { input_tokens: 0, output_tokens: 0 },
        }).await;

        Err(AgentError::Config("Max ReAct iterations reached".to_string()))
    }
}

struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
    arguments_json: Option<serde_json::Value>,
}