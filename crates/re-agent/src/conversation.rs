//! Conversations: what the user and an agent said, and the tool calls made.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a conversation is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    /// A file on disk (conversations are saved).
    File { path: PathBuf },
    /// Not tied to a file (e.g. the Extension Builder; saved).
    Global,
    /// An untitled tab (kept in memory until it is saved as a file).
    Untitled { tab: u64 },
}

impl Subject {
    pub fn is_persistent(&self) -> bool {
        !matches!(self, Self::Untitled { .. })
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::File { path } => Some(path),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        text: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// Shown to the user only (errors, "stopped"); never sent to the model.
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    /// Agent definition id (`file-agent`, `extension-builder`, custom).
    pub agent: String,
    pub subject: Subject,
    pub title: String,
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    pub updated: u64,
    #[serde(default)]
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new(agent: &str, subject: Subject) -> Self {
        let now = now_millis();
        Self {
            id: new_id(),
            agent: agent.to_owned(),
            subject,
            title: "New conversation".to_owned(),
            created: now,
            updated: now,
            messages: Vec::new(),
        }
    }

    pub fn push(&mut self, message: Message) {
        if let Message::User { text } = &message
            && !self
                .messages
                .iter()
                .any(|m| matches!(m, Message::User { .. }))
        {
            self.title = title_from(text);
        }
        self.messages.push(message);
        self.updated = now_millis();
    }
}

/// A short title from the first user message.
pub fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    if title.is_empty() {
        "New conversation".to_owned()
    } else {
        title
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 128 random bits as hex (OS-seeded hasher keys; no RNG dependency).
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut id = String::new();
    for salt in 0..2u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.write_u64(now_millis());
        hasher.write_u64(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_the_first_user_message() {
        let mut c = Conversation::new("file-agent", Subject::Global);
        c.push(Message::User {
            text: "\n  Explain this function please  \nmore".into(),
        });
        c.push(Message::User {
            text: "second".into(),
        });
        assert_eq!(c.title, "Explain this function please");
        assert_eq!(title_from(&"x".repeat(80)).chars().count(), 61);
    }

    #[test]
    fn conversations_round_trip_as_json() {
        let mut c = Conversation::new(
            "a",
            Subject::File {
                path: "/x/y.rs".into(),
            },
        );
        c.push(Message::User {
            text: "hi 😀".into(),
        });
        c.push(Message::Assistant {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_buffer".into(),
                arguments: serde_json::json!({"start_line": 1}),
            }],
        });
        c.push(Message::Tool {
            call_id: "c1".into(),
            name: "read_buffer".into(),
            content: "1| fn main".into(),
            is_error: false,
        });
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Conversation>(&json).unwrap(), c);
        assert_ne!(new_id(), new_id());
        assert_eq!(new_id().len(), 32);
    }
}
