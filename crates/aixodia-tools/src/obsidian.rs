//! The Obsidian mirror of a chat's goals and plans.
//!
//! After every successful `goal_*`/`plan_*` mutation the tool writes the whole
//! list into `<set_directory>/aixodia-goals.md` so the repository a chat works
//! in carries a readable record of what is in progress, done, or dropped. The
//! file is Obsidian markdown: YAML frontmatter, task checkboxes, and status
//! tags. A chat with no directory set writes nothing.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};

use aixodia_goals::{Item, STATUS_DONE, STATUS_DROPPED};

pub const FILE_NAME: &str = "aixodia-goals.md";

/// Renders the whole list. Items keep their store order (oldest first).
pub fn format_goals_md(session_id: &str, items: &[Item]) -> String {
    let mut open = Vec::new();
    let mut done = Vec::new();
    let mut dropped = Vec::new();
    for item in items {
        match item.status.as_str() {
            STATUS_DONE => &mut done,
            STATUS_DROPPED => &mut dropped,
            _ => &mut open,
        }
        .push(item);
    }

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("title: Goals and plans\n");
    out.push_str(&format!("session: {session_id}\n"));
    out.push_str(&format!(
        "updated: {}\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    ));
    out.push_str("---\n\n");

    for (title, list, box_state, section) in [
        ("In progress", &open, " ", ""),
        ("Completed", &done, "x", " #done"),
        ("Dropped", &dropped, " ", " #dropped"),
    ] {
        out.push_str(&format!("## {title}\n\n"));
        if list.is_empty() {
            out.push_str("- none\n\n");
            continue;
        }
        for item in list {
            let content = if section == " #dropped" {
                format!("~~{}~~", item.content.replace("~~", ""))
            } else {
                item.content.clone()
            };
            let kind = if item.kind == "goal" { "goal" } else { "plan" };
            out.push_str(&format!(
                "- [{box_state}] {content} (`{}`) {kind}{section}\n",
                item.id
            ));
        }
        out.push('\n');
    }
    out
}

/// Writes (replaces) the mirror file inside `dir`.
pub async fn write_goals_md(dir: &Path, session_id: &str, items: &[Item]) -> Result<()> {
    let text = format_goals_md(session_id, items);
    let path = dir.join(FILE_NAME);
    tokio::fs::write(&path, text)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aixodia_goals::STATUS_OPEN;

    fn item(id: &str, kind: &str, content: &str, status: &str) -> Item {
        Item {
            id: id.to_string(),
            kind: kind.to_string(),
            content: content.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn the_three_sections_are_split_by_status() {
        let items = vec![
            item("g-1", "goal", "ship it", STATUS_OPEN),
            item("p-1", "plan", "write tests", STATUS_DONE),
            item("p-2", "plan", "drop this", STATUS_DROPPED),
        ];
        let text = format_goals_md("sess-9", &items);
        assert!(text.starts_with("---\ntitle: Goals and plans\nsession: sess-9\nupdated: "));
        assert!(text.contains("## In progress\n\n- [ ] ship it (`g-1`) goal\n"));
        assert!(text.contains("## Completed\n\n- [x] write tests (`p-1`) plan #done\n"));
        assert!(text.contains("## Dropped\n\n- [ ] ~~drop this~~ (`p-2`) plan #dropped\n"));
    }

    #[test]
    fn empty_sections_say_none() {
        let text = format_goals_md("s", &[]);
        assert_eq!(text.matches("- none").count(), 3);
    }

    #[tokio::test]
    async fn the_file_is_written() {
        let dir = std::env::temp_dir().join(format!("aix-goals-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        write_goals_md(&dir, "s", &[item("g-1", "goal", "a", STATUS_OPEN)])
            .await
            .expect("write");
        let text = tokio::fs::read_to_string(dir.join(FILE_NAME))
            .await
            .expect("read");
        assert!(text.contains("- [ ] a (`g-1`) goal"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
