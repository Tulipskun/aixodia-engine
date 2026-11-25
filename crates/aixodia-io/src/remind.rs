//! The goal and plan reminder.
//!
//! Port of the Go `io/gateway/remind.go`. The reminder is a user message that
//! lives in the request conversation only: it is never stored in chat history or
//! in the turn trail. It used to be a tail note that was stripped and re-written
//! every round; it is now placed once, at the head of the chat, and kept there
//! for the whole conversation, so every round reads the same lists.

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

/// Puts the reminder at the head of the chat, after the identity block, and
/// leaves it there for the whole conversation.
///
/// It used to be re-appended to the tail every round, which meant the model saw
/// the goals and plans only from the round that wrote them on: an older copy was
/// stripped away each time. A chat's goals are not a per-round note, they are
/// what the chat is about, so the text stays where it is and every round reads
/// it. The head is also a stable position — the prefix before it never moves, so
/// the provider's prompt cache still covers the identity block.
pub fn place_reminder(
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
    // After the identity block, before the first turn: the shortest prefix that
    // can hold it, and the one that does not move when the thread grows.
    let at = compacted
        .iter()
        .position(|message| message.role != "system")
        .unwrap_or(compacted.len());
    let mut out = compacted;
    out.insert(at, reminder);
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
            let (out, whole_conversation, report) = place_reminder(&base, &state.text, window);
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
        // Every item, whatever its status: a goal that was completed or dropped
        // is still part of what this chat did, and removing it from the list
        // made the model (and the reader) forget the work that had been done.
        let items = store.all(session_id).await?;
        // A chat whose goals are all done still has goals: the list is shown for
        // as long as the chat has anything in it, because nothing is removed.
        let wanted = !items.is_empty();
        Ok((format_reminder(&items), wanted))
    }
}

pub fn format_reminder(items: &[Item]) -> String {
    let mut goals_list: Vec<String> = Vec::new();
    let mut plans_list: Vec<String> = Vec::new();
    for item in items {
        // The status is on every line: "done" and "dropped" are how the work
        // reads back, and they are kept rather than removed from the list.
        let mark = match item.status.as_str() {
            aixodia_goals::STATUS_DONE => "done",
            aixodia_goals::STATUS_DROPPED => "dropped",
            _ => "open",
        };
        let line = format!("- [{}] {} · {}", item.id, mark, item.content);
        if item.kind == Kind::Plan.as_str() {
            plans_list.push(line);
        } else {
            goals_list.push(line);
        }
    }

    let mut out = String::new();
    out.push_str(REMINDER_OPEN);
    out.push_str(
        "\nThis chat's goals and plans, with the status of each. Keep them in mind; call goal_* or plan_* when one changes. ",
    );
    out.push_str(
        "Nothing is removed from this list: completed and dropped items keep their place.\n",
    );
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
        let (out, whole, report) = place_reminder(&conversation, "REM", Window::new(0));
        assert!(whole);
        assert!(report.is_none());
        assert_eq!(out.len(), 3);
        // The reminder leads the conversation: it is what the chat is about, and
        // it stays for every round instead of being re-appended to the tail.
        assert_eq!(out.first().expect("reminder").content, "REM");
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
        assert!(text.contains("Goals:\n- [g1] open · ship it\n"));
        assert!(text.contains("Plans:\n- [p1] open · write tests\n"));
    }

    #[test]
    fn finished_items_keep_their_place_with_their_status() {
        // The point of the change: a completed or dropped item is not deleted
        // from the reminder, it is marked.
        let items = vec![
            Item {
                id: "g1".to_string(),
                kind: "goal".to_string(),
                content: "ship it".to_string(),
                status: aixodia_goals::STATUS_DONE.to_string(),
            },
            Item {
                id: "p1".to_string(),
                kind: "plan".to_string(),
                content: "write tests".to_string(),
                status: aixodia_goals::STATUS_DROPPED.to_string(),
            },
        ];
        let text = format_reminder(&items);
        assert!(text.contains("- [g1] done · ship it"), "{text}");
        assert!(text.contains("- [p1] dropped · write tests"), "{text}");
        assert!(text.contains("Nothing is removed"), "{text}");
    }

    #[test]
    fn the_reminder_stays_at_the_head_and_is_not_duplicated() {
        // Two rounds in a row: the text is placed once, at the head, and the
        // earlier copy is replaced rather than kept beside the new one.
        let note = |text: &str| format!("{REMINDER_OPEN}\n{text}\n{REMINDER_CLOSE}");
        let first = vec![user("hello"), user("again")];
        let (one, _, _) = place_reminder(&first, &note("REM-1"), Window::new(0));
        let (two, _, _) = place_reminder(&one, &note("REM-2"), Window::new(0));
        assert_eq!(two.len(), 3);
        assert_eq!(two.iter().filter(|m| is_reminder(m)).count(), 1);
        assert_eq!(two[0].content, note("REM-2"));
        assert_eq!(two[1].content, "hello");
    }

    #[test]
    fn empty_lists_say_none() {
        let text = format_reminder(&[]);
        assert!(text.contains("Goals:\n- none\n"));
        assert!(text.contains("Plans:\n- none\n"));
    }
}
