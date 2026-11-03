//! Tool steps stored as ordinary turns with role `tool_call` / `tool_result`, so
//! the phone can show them and the next request can replay them.
//!
//! Port of the Go `session/trail.go`.

use aixodia_provider::Message;
use serde::{Deserialize, Serialize};

use crate::block::{text_block, tool_block};
use crate::types::Turn;
use crate::TurnInput;

/// One call (or one result) as JSON in `text`. The assistant text that came
/// before a group of calls rides on its own narration row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallRow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// What the next request has to send back is the assistant turn that made
    /// the call, so the reasoning rides on the call row.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolResultRow {
    #[serde(default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub content: String,
}

/// Converts the tool steps of one reply into storable turns.
pub fn trail_turns(trail: &[Message]) -> Vec<TurnInput> {
    let mut out = Vec::new();
    let mut narration = 0;

    for message in trail {
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            // The model's words before a tool call are its own turn, not part of
            // the call. Hiding them inside the call row put sentences the user
            // wrote to the model inside a tool block.
            let text = message.content.trim();
            if !text.is_empty() {
                narration += 1;
                out.push(TurnInput {
                    role: "assistant".to_string(),
                    text: text.to_string(),
                    block: text_block(&format!("n{narration}"), text).json(),
                    ..TurnInput::default()
                });
            }
            for call in &message.tool_calls {
                let row = ToolCallRow {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    content: String::new(),
                    reasoning: message.reasoning_content.clone(),
                };
                out.push(TurnInput {
                    role: "tool_call".to_string(),
                    text: serde_json::to_string(&row).unwrap_or_else(|_| "{}".to_string()),
                    block: tool_block(&call.id, &call.name, &call.arguments, "", false, 0, false)
                        .json(),
                    ..TurnInput::default()
                });
            }
        } else if message.role == "tool" {
            let row = ToolResultRow {
                tool_call_id: message.tool_call_id.clone(),
                name: message.tool_name.clone(),
                content: message.content.clone(),
            };
            out.push(TurnInput {
                role: "tool_result".to_string(),
                text: serde_json::to_string(&row).unwrap_or_else(|_| "{}".to_string()),
                block: tool_block(
                    &message.tool_call_id,
                    &message.tool_name,
                    "",
                    &message.content,
                    is_error_output(&message.content),
                    0,
                    true,
                )
                .json(),
                ..TurnInput::default()
            });
        }
    }
    out
}

/// A failed execution is a `{"error":...}` envelope, not genuine command output.
pub fn is_error_output(output: &str) -> bool {
    let trimmed = output.trim();
    trimmed.starts_with("{\"error\":") || trimmed.starts_with("{\"error\" :")
}

/// Rebuilds the conversation a model needs from stored turns. Rows that cannot
/// be decoded are skipped rather than failing the whole chat.
pub fn messages_from_turns(turns: &[Turn]) -> Vec<Message> {
    let mut messages = Vec::with_capacity(turns.len());
    // A retry after a lost D1 response used to store one tool round twice.
    // Replaying that history sends the provider two function_call items with the
    // same call id, which it rejects. Keep the first call and its first result.
    let mut seen_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_results: std::collections::HashSet<String> = std::collections::HashSet::new();

    for turn in turns {
        match turn.role.as_str() {
            "system" | "user" | "assistant" => messages.push(Message {
                role: turn.role.clone(),
                content: turn.text.clone(),
                reasoning_content: turn.reasoning_content.clone(),
                ..Message::default()
            }),
            "tool_call" => {
                let Ok(row) = serde_json::from_str::<ToolCallRow>(&turn.text) else {
                    continue;
                };
                if row.id.is_empty() || !seen_calls.insert(row.id.clone()) {
                    continue;
                }
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: row.content.clone(),
                    tool_calls: vec![aixodia_provider::ToolCall {
                        id: row.id,
                        name: row.name,
                        arguments: row.arguments,
                    }],
                    reasoning_content: row.reasoning,
                    ..Message::default()
                });
            }
            "tool_result" => {
                let Ok(row) = serde_json::from_str::<ToolResultRow>(&turn.text) else {
                    continue;
                };
                if row.tool_call_id.is_empty() || !seen_results.insert(row.tool_call_id.clone()) {
                    continue;
                }
                messages.push(Message {
                    role: "tool".to_string(),
                    tool_call_id: row.tool_call_id,
                    tool_name: row.name,
                    content: row.content,
                    ..Message::default()
                });
            }
            _ => {}
        }
    }
    messages
}

/// Whether a message exists only because of tool use.
pub fn is_tool_message(message: &Message) -> bool {
    message.role == "tool" || !message.tool_calls.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, BLOCK_TOOL};

    fn call(id: &str, name: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: "let me look".to_string(),
            tool_calls: vec![aixodia_provider::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: r#"{"reason":"checking"}"#.to_string(),
            }],
            reasoning_content: "because".to_string(),
            ..Message::default()
        }
    }

    #[test]
    fn trail_becomes_rows_and_round_trips() {
        let trail = vec![
            call("call_1", "shell"),
            Message {
                role: "tool".to_string(),
                tool_call_id: "call_1".to_string(),
                tool_name: "shell".to_string(),
                content: "hi".to_string(),
                ..Message::default()
            },
        ];
        let rows = trail_turns(&trail);
        assert_eq!(rows.len(), 3, "narration, call, result");
        assert_eq!(rows[0].role, "assistant");
        assert_eq!(rows[1].role, "tool_call");
        assert_eq!(rows[2].role, "tool_result");

        let block = Block::parse(&rows[1].block).expect("call block");
        assert_eq!(block.kind, BLOCK_TOOL);
        assert_eq!(block.name, "shell");

        let turns: Vec<Turn> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| crate::types::stored_turn("s", i as i64 + 1, 0, row))
            .collect();
        let messages = messages_from_turns(&turns);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].tool_calls[0].id, "call_1");
        assert_eq!(messages[1].reasoning_content, "because");
        assert_eq!(messages[2].content, "hi");
    }

    #[test]
    fn duplicate_tool_rows_are_dropped_from_the_replay() {
        let input = TurnInput {
            role: "tool_call".to_string(),
            text: r#"{"id":"call_1","name":"shell","arguments":"{}"}"#.to_string(),
            ..TurnInput::default()
        };
        let turns = vec![
            crate::types::stored_turn("s", 1, 0, &input),
            crate::types::stored_turn("s", 2, 0, &input),
        ];
        assert_eq!(messages_from_turns(&turns).len(), 1);
    }

    #[test]
    fn error_output_is_recognised() {
        assert!(is_error_output("{\"error\":\"boom\"}"));
        assert!(is_error_output(" {\"error\" : \"boom\"}"));
        assert!(!is_error_output("ok"));
    }
}
