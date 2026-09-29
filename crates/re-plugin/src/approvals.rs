//! Remembered user approvals (plugins, and data leaving the machine).
//!
//! One key per line in a small text file in the user's config folder. A key
//! identifies exactly what was approved (see `Plugin::approval_key`), so an
//! updated plugin asks again.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct Approvals {
    path: Option<PathBuf>,
    keys: BTreeSet<String>,
}

impl Approvals {
    /// Loads approvals from `path` (missing file = nothing approved).
    pub fn load(path: Option<PathBuf>) -> Self {
        let keys = path
            .as_deref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self { path, keys }
    }

    pub fn is_approved(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// Approves `key` and saves the file.
    pub fn approve(&mut self, key: &str) -> io::Result<()> {
        self.keys.insert(key.to_owned());
        self.save()
    }

    /// Forgets every approval.
    pub fn clear(&mut self) -> io::Result<()> {
        self.keys.clear();
        self.save()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = String::from("# RustEditor approvals. Delete a line to be asked again.\n");
        for key in &self.keys {
            text.push_str(key);
            text.push('\n');
        }
        write_replace(path, &text)
    }
}

fn write_replace(path: &Path, text: &str) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, text)?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_persist_and_can_be_cleared() {
        let path = std::env::temp_dir()
            .join(format!("re-plugin-approvals-{}", std::process::id()))
            .join("approvals.txt");
        let _ = fs::remove_file(&path);
        let mut approvals = Approvals::load(Some(path.clone()));
        assert!(!approvals.is_approved("plugin:x@1:abc"));
        approvals.approve("plugin:x@1:abc").unwrap();
        let reloaded = Approvals::load(Some(path.clone()));
        assert!(reloaded.is_approved("plugin:x@1:abc"));
        assert_eq!(reloaded.len(), 1);
        let mut reloaded = reloaded;
        reloaded.clear().unwrap();
        assert!(Approvals::load(Some(path.clone())).is_empty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        // Without a path, approvals live only in memory.
        let mut memory = Approvals::load(None);
        memory.approve("k").unwrap();
        assert!(memory.is_approved("k"));
    }
}
