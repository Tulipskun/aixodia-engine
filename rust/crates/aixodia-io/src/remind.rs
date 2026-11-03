//! The transient goal and plan reminder.
//!
//! Port of the Go `io/gateway/remind.go`. The reminder is a transient user
//! message: it is never stored in chat history or in the turn trail; it exists
//! only in the request conversation, so the model sees the current lists without
//! the phone showing an extra user bubble.

use aixodia_goals::{Item, Kind};
use aixodia_provider::Message;
use aixodia_session::{compact, message_tokens, Compacted, Window, DEFAULT_MAX_TOKENS};

use crate::gateway::Gateway;

pub const REMINDER_OPEN: &str = "<system-remind>";
pub const REMINDER_CLOSE: &str = "</system-remind>";

/// Per-turn bookkeeping. `dirty` is set when a goal or plan tool succeeded;
/// `checked_empty` remembers that this turn already looked at an empty store, so
/// a long tool loop does not query D1 every round for nothing. `text` is the
/// reminder currently in force for this turn.
#[derive(Debug, Default, Clone)]
pub struct ReminderState {
    pub dirty: bool,
    pub checked_empty: bool,
    pub text: String,
}

pub fn is_reminder(message: &Message) -> bool {
    message.role == "user" && message.content.trim_start().starts_with(REMINDER_OPEN)
}

pub fn has_reminder(messages: &[Message]) -> bool {
    messages.iter().any(is_reminder)
}

/// Removes every earlier reminder from the local conversation. They are
/// transient by design: keeping the old ones would accumulate one reminder per
/// tool round and waste the window on stale lists.
pub fn strip_reminders(conversation: &[Message]) -> Vec<Message> {
    conversation
        .iter()
        .filter(|message| !is_reminder(message))
        .cloned()
        .collect()
}

/// Puts one fresh reminder at the very end of the request, after room for it is
/// left. The tail is the cache-friendly position: the next request keeps every
/// earlier message as a prefix, so replacing the reminder only invalidates the
/// last message instead of the user's question. The reminder is sized separately
/// from the conversation so the newest turn always survives even when the window
/// is tight.
pub fn append_reminder(
    conversation: &[Message],
    text: &str,
    window: Window,
) -> (Vec<Message>, bool, Option<Compacted>) {
    let conversation = strip_reminders(conversation);
    let reminder = Message {
        role: "user".to_string(),
        content: text.to_string(),
        ..Message::default()
    };
    let mut window = window;
    if window.max <= 0 {
        window.max = DEFAULT_MAX_TOKENS;
    }
    window.max -= message_tokens(&reminder);
    if window.max < 1 {
        window.max = 1;
    }
    let (compacted, report) = compact(&conversation, window);
    let whole = compacted.len() == conversation.len();
    let mut out = compacted;
    out.push(reminder);
    (out, whole, report)
}

impl Gateway {
    /// Compacts the conversation and, when needed, injects the active goals and
    /// plans as a `<system-remind>` user message at the tail. A reminder is
    /// injected when a goal or plan tool changed something, or when this turn has
    /// not looked at the store yet. It is not injected for a chat that has no
    /// active goals or plans and did not just change them.
    pub(crate) async fn prepare_messages(
        &self,
        conversation: &[Message],
        window: Window,
        session_id: &str,
        state: &mut ReminderState,
    ) -> (Vec<Message>, bool, Option<Compacted>) {
        let mut window = window;
        if window.max <= 0 {
            window.max = DEFAULT_MAX_TOKENS;
        }
        // Older reminders are transient too. Strip them before compacting so the
        // newest turn is the user's question, not a stale reminder from the last
        // round.
        let base = strip_reminders(conversation);
        if base.len() != conversation.len() {
            // A provider count from the previous round covered the reminder that
            // was just removed, so it no longer describes this conversation's
            // prefix. Size the compacted window from the estimate instead.
            window = Window::new(window.max);
        }
        let need = state.dirty || (!state.checked_empty && state.text.is_empty());
        if need {
            match self.objective_reminder(session_id).await {
                Err(err) => {
                    // A failed reminder must not fail the turn. The next round
                    // retries because text stays empty or dirty stays set.
                    tracing::warn!("gateway: objective reminder: {err}");
                }
                Ok((text, active)) => {
                    if !text.is_empty() && (active || state.dirty) {
                        state.text = text;
                        state.checked_empty = !active;
                        state.dirty = false;
                    } else if !active {
                        state.text = String::new();
                        state.checked_empty = true;
                        state.dirty = false;
                    }
                }
            }
        }

        if !state.text.is_empty() {
            let (out, whole_conversation, report) = append_reminder(&base, &state.text, window);
            let whole = whole_conversation && base.len() == conversation.len();
            return (out, whole, report);
        }

        let (messages, report) = compact(&base, window);
        let whole = messages.len() == base.len() && base.len() == conversation.len();
        (messages, whole, report)
    }

    pub(crate) async fn objective_reminder(
        &self,
        session_id: &str,
    ) -> anyhow::Result<(String, bool)> {
        let Some(store) = self.objective_store() else {
            return Ok((String::new(), false));
        };
        if session_id.trim().is_empty() {
            return Ok((String::new(), false));
        }
        let items = store.open(session_id).await?;
        let active = !items.is_empty();
        Ok((format_reminder(&items), active))
    }
}

pub fn format_reminder(items: &[Item]) -> String {
    let mut goals_list: Vec<String> = Vec::new();
    let mut plans_list: Vec<String> = Vec::new();
    for item in items {
        let line = format!("- [{}] {}", item.id, item.content);
        if item.kind == Kind::Plan.as_str() {
            plans_list.push(line);
        } else {
            goals_list.push(line);
        }
    }

    let mut out = String::new();
    out.push_str(REMINDER_OPEN);
    out.push_str(
        "\nActive goals and plans in this chat. Keep them in mind; call goal_* or plan_* when one changes. ",
    );
    out.push_str("Completed and dropped items are removed from this list.\n");
    out.push_str("\nGoals:\n");
    if goals_list.is_empty() {
        out.push_str("- none\n");
    } else {
        for line in &goals_list {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("\nPlans:\n");
    if plans_list.is_empty() {
        out.push_str("- none\n");
    } else {
        for line in &plans_list {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(REMINDER_CLOSE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aixodia_session::Window;

    fn user(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: text.to_string(),
            ..Message::default()
        }
    }

    #[test]
    fn a_reminder_is_recognised_and_stripped() {
        let reminder = user(&format!(
            "{REMINDER_OPEN}\nGoals:\n- none\n{REMINDER_CLOSE}"
        ));
        let plain = user("hello");
        assert!(is_reminder(&reminder));
        assert!(!is_reminder(&plain));
        let stripped = strip_reminders(&[plain.clone(), reminder]);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].content, "hello");
    }

    #[test]
    fn the_reminder_goes_last() {
        let conversation = vec![user("hello"), user("again")];
        let (out, whole, report) = append_reminder(&conversation, "REM", Window::new(0));
        assert!(whole);
        assert!(report.is_none());
        assert_eq!(out.len(), 3);
        assert_eq!(out.last().expect("reminder").content, "REM");
    }

    #[test]
    fn the_lists_are_formatted_with_headers() {
        let items = vec![
            Item {
                id: "g1".to_string(),
                kind: "goal".to_string(),
                content: "ship it".to_string(),
                status: aixodia_goals::STATUS_OPEN.to_string(),
            },
            Item {
                id: "p1".to_string(),
                kind: "plan".to_string(),
                content: "write tests".to_string(),
                status: aixodia_goals::STATUS_OPEN.to_string(),
            },
        ];
        let text = format_reminder(&items);
        assert!(text.starts_with(REMINDER_OPEN));
        assert!(text.ends_with(REMINDER_CLOSE));
        assert!(text.contains("Goals:\n- [g1] ship it\n"));
        assert!(text.contains("Plans:\n- [p1] write tests\n"));
    }

    #[test]
    fn empty_lists_say_none() {
        let text = format_reminder(&[]);
        assert!(text.contains("Goals:\n- none\n"));
        assert!(text.contains("Plans:\n- none\n"));
    }
}
