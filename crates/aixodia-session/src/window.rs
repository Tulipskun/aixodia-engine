//! Token estimation and the context window.
//!
//! Port of the Go `session/ctx_window.go`. The estimate prices text by character
//! class rather than `len/4`: a byte-pair tokenizer learns merges per script, so
//! Thai and CJK cost close to a token per character while English merges about
//! four characters to a token.

use aixodia_provider::{Message, ToolCall};
use unicode_general_category::{get_general_category, GeneralCategory};

use crate::types::Turn;

/// The budget a request is compacted to. It sits below the context window of the
/// models the engine talks to, so the estimate has room to be wrong.
pub const DEFAULT_MAX_TOKENS: i64 = 156_000;

/// What one attached picture is counted as.
const IMAGE_TOKENS: i64 = 1_100;

const CLASS_NONE: u8 = 0;
const CLASS_WORD: u8 = 1;
const CLASS_WIDE: u8 = 2;
const CLASS_MARK: u8 = 3;
const CLASS_SPACE: u8 = 4;
const CLASS_BREAK: u8 = 5;
const CLASS_PUNCT: u8 = 6;

fn class_of(c: char) -> u8 {
    if c == '\n' {
        return CLASS_BREAK;
    }
    if matches!(c, ' ' | '\t' | '\r' | '\u{0b}' | '\u{0c}') {
        return CLASS_SPACE;
    }
    if (c as u32) < 0x80 {
        return if c.is_ascii_alphanumeric() {
            CLASS_WORD
        } else {
            CLASS_PUNCT
        };
    }
    match get_general_category(c) {
        GeneralCategory::NonspacingMark
        | GeneralCategory::SpacingMark
        | GeneralCategory::EnclosingMark => CLASS_MARK,
        GeneralCategory::UppercaseLetter
        | GeneralCategory::LowercaseLetter
        | GeneralCategory::TitlecaseLetter
        | GeneralCategory::ModifierLetter
        | GeneralCategory::OtherLetter
        | GeneralCategory::DecimalNumber
        | GeneralCategory::LetterNumber
        | GeneralCategory::OtherNumber => CLASS_WIDE,
        _ => CLASS_PUNCT,
    }
}

fn run_tokens(class: u8, count: i64) -> i64 {
    match class {
        // English merges about four characters to a token, but a short word is
        // still one token, never a fraction of one.
        CLASS_WORD => ((count + 2) / 4).max(1),
        // A script the vocabularies learned less of costs close to a token per
        // character.
        CLASS_WIDE => count,
        // Punctuation mostly stands alone; runs like "==" or "..." are single
        // tokens often enough to price two to a token.
        CLASS_PUNCT => ((count + 1) / 2).max(1),
        // One space rides on the word after it. Only a long indent costs.
        CLASS_SPACE => count / 4,
        CLASS_BREAK => count,
        _ => count,
    }
}

/// Counts text the way a byte-pair tokenizer does, as closely as possible
/// without the model's vocabulary.
pub fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let mut total = 0;
    let mut class = CLASS_NONE;
    let mut count = 0;
    for c in text.chars() {
        let k = class_of(c);
        if k == CLASS_MARK {
            // A combining mark rides on the character before it and is not a
            // token of its own.
            continue;
        }
        if k != class {
            if class != CLASS_NONE {
                total += run_tokens(class, count);
            }
            class = k;
            count = 0;
        }
        count += 1;
    }
    total += run_tokens(class, count);
    total
}

pub fn message_tokens(message: &Message) -> i64 {
    let mut total = estimate_tokens(&message.content) + 4;
    // A reasoning model's earlier thoughts are replayed with its message, so
    // they occupy the prompt again on the next round.
    total += estimate_tokens(&message.reasoning_content);
    for call in &message.tool_calls {
        total += estimate_tokens(&call.name) + estimate_tokens(&call.arguments) + 8;
    }
    total += IMAGE_TOKENS * message.images.len() as i64;
    total
}

pub fn tokens(messages: &[Message]) -> i64 {
    messages.iter().map(message_tokens).sum()
}

/// The room a conversation has in the model's context. When a provider has
/// reported the size of a prompt, that count is exact and covers the first
/// `messages` messages; only what was appended after them has to be estimated.
#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    pub max: i64,
    /// Tokens the provider reported for the first `messages` messages.
    pub usage: i64,
    /// How many leading messages that count covers.
    pub messages: usize,
}

impl Window {
    pub fn new(max: i64) -> Self {
        Self {
            max,
            ..Self::default()
        }
    }

    /// The conversation's size in tokens, anchored on the provider's count while
    /// that count still describes a prefix of the conversation.
    pub fn size(&self, messages: &[Message]) -> i64 {
        if self.messages > 0 && self.messages <= messages.len() {
            self.usage + tokens(&messages[self.messages..])
        } else {
            tokens(messages)
        }
    }
}

/// What a compaction removed. The request conversation loses whole turns, but
/// the user can be told what fell out of the window and how much was kept.
#[derive(Debug, Clone, Default)]
pub struct Compacted {
    pub kept_turns: usize,
    pub dropped_turns: usize,
    /// A one-line digest of each dropped turn, oldest first.
    pub summary: Vec<String>,
}

/// Compacts a conversation that has reached the window: it keeps the newer turns
/// that fit and hands the kept rounds back as the reason the model gave for them,
/// not as the result text the call produced.
///
/// A turn starts at a user message and runs until the next one, so an assistant
/// message and the tool results answering its calls always move together.
pub fn compact(messages: &[Message], window: Window) -> (Vec<Message>, Option<Compacted>) {
    let max = if window.max <= 0 {
        DEFAULT_MAX_TOKENS
    } else {
        window.max
    };

    if window.size(messages) <= max {
        return (messages.to_vec(), None);
    }

    let mut system = 0;
    while system < messages.len() && messages[system].role == "system" {
        system += 1;
    }
    let body = &messages[system..];

    let budget = max - tokens(&messages[..system]);
    if budget <= 0 {
        return (messages.to_vec(), None);
    }

    let turns = split_turns(body);

    // Walk the window in from the newest turn. The newest turn is kept even if
    // it alone is over budget: a request with nothing left to answer is worse
    // than one that is a little too long.
    let mut start = turns.len();
    let mut used = 0;
    while start > 0 {
        let turn_tokens = tokens(&turns[start - 1]);
        if start < turns.len() && used + turn_tokens > budget {
            break;
        }
        used += turn_tokens;
        start -= 1;
    }

    // The body does not always open on a user message. Such a turn cannot lead
    // the request, so drop it.
    if start < turns.len() && turns[start][0].role != "user" {
        start += 1;
    }

    let dropped = &turns[..start];
    let report = if dropped.is_empty() {
        None
    } else {
        Some(Compacted {
            kept_turns: turns.len() - start,
            dropped_turns: dropped.len(),
            summary: dropped
                .iter()
                .take(30)
                .filter_map(|turn| {
                    let question = turn.iter().find(|m| m.role == "user")?;
                    let text = digest(&question.content);
                    (!text.is_empty()).then_some(text)
                })
                .collect(),
        })
    };

    let mut compacted: Vec<Message> = messages[..system].to_vec();
    for turn in &turns[start..] {
        compacted.extend(turn.iter().cloned());
    }

    // A kept round answers its call with the reason the model wrote for it, not
    // with the full output the call produced.
    let calls = tool_calls_index(&compacted);
    for message in &mut compacted {
        if message.role != "tool" {
            continue;
        }
        if let Some(call) = calls.get(&message.tool_call_id) {
            if let Some(reason) = tool_reason(call) {
                message.content = reason;
            }
        }
    }
    (compacted, report)
}

/// One line for the report: whitespace collapsed, cut to about 120 characters.
fn digest(text: &str) -> String {
    const LIMIT: usize = 120;
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out = String::new();
    let mut kept = 0usize;
    for word in &words {
        let next = out.chars().count() + if out.is_empty() { 0 } else { 1 } + word.chars().count();
        if !out.is_empty() && next > LIMIT {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        kept += 1;
    }
    if kept < words.len() {
        out.push('…');
    }
    out
}

fn tool_calls_index(messages: &[Message]) -> std::collections::HashMap<String, ToolCall> {
    let mut calls = std::collections::HashMap::new();
    for message in messages {
        for call in &message.tool_calls {
            calls.insert(call.id.clone(), call.clone());
        }
    }
    calls
}

/// Reads the short reason a model wrote for a call out of the arguments the call
/// carried.
fn tool_reason(call: &ToolCall) -> Option<String> {
    let fields: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let reason = fields.get("reason")?;
    match reason {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Cuts the body at every user message.
pub fn split_turns(messages: &[Message]) -> Vec<Vec<Message>> {
    let mut turns: Vec<Vec<Message>> = Vec::new();
    for message in messages {
        if turns.is_empty() || message.role == "user" {
            turns.push(Vec::new());
        }
        turns.last_mut().expect("a turn").push(message.clone());
    }
    turns
}

/// Reports what the provider last measured in a stored history. A turn that ran
/// tools stores the sum of its rounds, which is not the size of any one prompt,
/// so only a turn that answered in a single round can anchor.
pub fn anchor(turns: &[Turn]) -> (usize, i64) {
    let mut messages = 0;
    let mut tokens = 0;
    let mut seen = 0usize;
    let mut tools = false;

    for turn in turns {
        match turn.role.as_str() {
            "user" => {
                tools = false;
                seen += 1;
            }
            "system" => {
                seen += 1;
            }
            "assistant" => {
                // `input_tokens` is the prompt that produced this answer, so it
                // covers the messages before it, not the answer itself.
                if !tools && turn.input_tokens > 0 {
                    messages = seen;
                    tokens = turn.input_tokens;
                }
                seen += 1;
            }
            "tool_call" => {
                let Ok(row) = serde_json::from_str::<crate::trail::ToolCallRow>(&turn.text) else {
                    continue;
                };
                if row.id.is_empty() {
                    continue;
                }
                tools = true;
                seen += 1;
            }
            "tool_result" => {
                let Ok(row) = serde_json::from_str::<crate::trail::ToolResultRow>(&turn.text)
                else {
                    continue;
                };
                if row.tool_call_id.is_empty() {
                    continue;
                }
                tools = true;
                seen += 1;
            }
            _ => {}
        }
    }
    (messages, tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: text.to_string(),
            ..Message::default()
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: text.to_string(),
            ..Message::default()
        }
    }

    #[test]
    fn english_is_about_four_characters_a_token() {
        assert_eq!(estimate_tokens(""), 0);
        let tokens = estimate_tokens("the quick brown fox jumps over the lazy dog");
        // 43 characters of words and spaces: roughly 10–12 tokens.
        assert!((9..=14).contains(&tokens), "tokens = {tokens}");
    }

    #[test]
    fn thai_costs_about_one_token_a_character() {
        let text = "สวัสดีครับวันนี้อากาศดี";
        let tokens = estimate_tokens(text);
        let chars = text.chars().count() as i64;
        // Combining marks are not separate tokens.
        assert!(tokens >= chars / 3, "tokens = {tokens}, chars = {chars}");
        assert!(tokens <= chars, "tokens = {tokens}, chars = {chars}");
    }

    #[test]
    fn punctuation_and_newlines_cost_something() {
        assert!(estimate_tokens("{}[]()") >= 3);
        assert_eq!(estimate_tokens("\n\n\n"), 3);
    }

    #[test]
    fn window_anchors_on_the_measured_prefix() {
        let messages = vec![user("hello world"), assistant("hi there")];
        let measured = Window {
            max: 1000,
            usage: 500,
            messages: 1,
        };
        assert_eq!(measured.size(&messages), 500 + message_tokens(&messages[1]));

        // A count that no longer describes a prefix is ignored.
        let stale = Window {
            max: 1000,
            usage: 500,
            messages: 5,
        };
        assert_eq!(stale.size(&messages), tokens(&messages));
    }

    #[test]
    fn compact_keeps_the_newest_turn_and_whole_tool_rounds() {
        let messages = vec![
            user(&"old question ".repeat(50)),
            assistant(&"old answer ".repeat(50)),
            user("new question"),
            Message {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    arguments: r#"{"reason":"because"}"#.to_string(),
                }],
                ..Message::default()
            },
            Message {
                role: "tool".to_string(),
                tool_call_id: "call_1".to_string(),
                content: "a very long tool output that should become the reason".to_string(),
                ..Message::default()
            },
        ];

        let (compacted, report) = compact(&messages, Window::new(60));
        assert!(
            compacted[0].role == "user",
            "the window opens on a user turn"
        );
        assert_eq!(compacted[0].content, "new question");
        // The tool result was replaced by the call's reason.
        let tool = compacted
            .iter()
            .find(|message| message.role == "tool")
            .expect("tool result kept with its call");
        assert_eq!(tool.content, "because");
        let report = report.expect("the old turn is reported");
        assert_eq!(report.dropped_turns, 1);
        assert_eq!(report.kept_turns, 1);
        assert_eq!(report.summary.len(), 1);
        assert!(report.summary[0].starts_with("old question"));
    }

    #[test]
    fn a_conversation_that_fits_is_untouched() {
        let messages = vec![user("hi"), assistant("hello")];
        let (compacted, report) = compact(&messages, Window::new(DEFAULT_MAX_TOKENS));
        assert_eq!(compacted.len(), messages.len());
        assert!(report.is_none());
    }
}
