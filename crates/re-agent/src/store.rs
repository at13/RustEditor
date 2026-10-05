//! Conversation storage: one JSON file per conversation in a private
//! per-user folder, grouped by subject (a hash of the file's canonical path,
//! or `global`). Untitled subjects are never written.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::conversation::{Conversation, Subject};

/// A conversation in a list (without its messages).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub updated: u64,
    pub messages: usize,
}

#[derive(Serialize, Deserialize)]
struct SubjectFile {
    subject: Subject,
}

#[derive(Clone, Debug)]
pub struct ConversationStore {
    root: PathBuf,
}

/// Stable hash for folder names.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The path as the store keys it (canonical when possible; case-insensitive
/// on Windows).
pub fn canonical_key(path: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl ConversationStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn folder(&self, subject: &Subject) -> Option<PathBuf> {
        match subject {
            Subject::File { path } => Some(
                self.root
                    .join("files")
                    .join(format!("{:016x}", fnv1a(&canonical_key(path)))),
            ),
            Subject::Global => Some(self.root.join("global")),
            Subject::Untitled { .. } => None,
        }
    }

    /// Conversations about `subject`, newest first. Unreadable files are
    /// skipped.
    pub fn list(&self, subject: &Subject) -> Vec<Summary> {
        let Some(folder) = self.folder(subject) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(&folder) else {
            return Vec::new();
        };
        let mut list: Vec<Summary> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .filter(|e| e.file_name() != "subject.json")
            .filter_map(|e| fs::read(e.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<Conversation>(&bytes).ok())
            .map(|c| Summary {
                id: c.id,
                agent: c.agent,
                title: c.title,
                updated: c.updated,
                messages: c.messages.len(),
            })
            .collect();
        list.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
        list
    }

    pub fn load(&self, subject: &Subject, id: &str) -> io::Result<Conversation> {
        let folder = self.folder(subject).ok_or(io::ErrorKind::NotFound)?;
        if !valid_id(id) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let bytes = fs::read(folder.join(format!("{id}.json")))?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    /// Saves (atomically). Untitled conversations are not saved.
    pub fn save(&self, conversation: &Conversation) -> io::Result<()> {
        let Some(folder) = self.folder(&conversation.subject) else {
            return Ok(());
        };
        if !valid_id(&conversation.id) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        fs::create_dir_all(&folder)?;
        let marker = folder.join("subject.json");
        if !marker.exists() {
            let subject = serde_json::to_vec_pretty(&SubjectFile {
                subject: conversation.subject.clone(),
            })
            .map_err(io::Error::other)?;
            write_atomic(&marker, &subject)?;
        }
        let bytes = serde_json::to_vec(conversation).map_err(io::Error::other)?;
        write_atomic(&folder.join(format!("{}.json", conversation.id)), &bytes)
    }

    pub fn delete(&self, subject: &Subject, id: &str) -> io::Result<()> {
        let folder = self.folder(subject).ok_or(io::ErrorKind::NotFound)?;
        if !valid_id(id) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        match fs::remove_file(folder.join(format!("{id}.json"))) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Moves `conversations` (e.g. an untitled tab's) to a new subject and
    /// saves them there (after Save As).
    pub fn adopt(&self, conversations: &mut [Conversation], subject: &Subject) -> io::Result<()> {
        for conversation in conversations {
            conversation.subject = subject.clone();
            self.save(conversation)?;
        }
        Ok(())
    }

    /// Moves every saved conversation from one file to another (rename).
    pub fn rename(&self, from: &Subject, to: &Subject) -> io::Result<()> {
        for summary in self.list(from) {
            let mut conversation = self.load(from, &summary.id)?;
            conversation.subject = to.clone();
            self.save(&conversation)?;
            self.delete(from, &summary.id)?;
        }
        Ok(())
    }

    /// Deletes every stored conversation.
    pub fn delete_all(&self) -> io::Result<()> {
        for part in ["files", "global"] {
            match fs::remove_dir_all(self.root.join(part)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Message;

    fn temp_store(name: &str) -> ConversationStore {
        let root = std::env::temp_dir().join(format!(
            "re-agent-store-{name}-{}-{}",
            std::process::id(),
            crate::conversation::new_id()
        ));
        ConversationStore::new(root)
    }

    #[test]
    fn saves_lists_loads_and_deletes_per_file() {
        let store = temp_store("basic");
        let file = std::env::temp_dir().join("re-agent-some-file.rs");
        let subject = Subject::File { path: file.clone() };
        let mut first = Conversation::new("file-agent", subject.clone());
        first.push(Message::User {
            text: "first".into(),
        });
        store.save(&first).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second = Conversation::new("file-agent", subject.clone());
        second.push(Message::User {
            text: "second".into(),
        });
        store.save(&second).unwrap();
        let list = store.list(&subject);
        assert_eq!(
            list.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            ["second", "first"]
        );
        assert_eq!(store.load(&subject, &first.id).unwrap(), first);
        // Another file has its own list.
        assert!(
            store
                .list(&Subject::File {
                    path: "/other".into()
                })
                .is_empty()
        );
        store.delete(&subject, &first.id).unwrap();
        assert_eq!(store.list(&subject).len(), 1);
        // Ids cannot escape the folder.
        assert!(store.load(&subject, "../x").is_err());
        store.delete_all().unwrap();
        assert!(store.list(&subject).is_empty());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn untitled_conversations_are_not_saved_until_adopted_and_rename_moves() {
        let store = temp_store("untitled");
        let mut c = Conversation::new("file-agent", Subject::Untitled { tab: 3 });
        c.push(Message::User {
            text: "draft".into(),
        });
        store.save(&c).unwrap();
        assert!(!store.root().exists());
        let a = Subject::File {
            path: "/a.txt".into(),
        };
        store.adopt(std::slice::from_mut(&mut c), &a).unwrap();
        assert_eq!(store.list(&a).len(), 1);
        let b = Subject::File {
            path: "/b.txt".into(),
        };
        store.rename(&a, &b).unwrap();
        assert!(store.list(&a).is_empty());
        assert_eq!(store.load(&b, &c.id).unwrap().subject, b);
        let _ = fs::remove_dir_all(store.root());
    }
}
