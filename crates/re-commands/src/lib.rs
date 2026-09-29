//! Command registry, platform-neutral keymaps, and command-palette matching.
//!
//! Everything user-facing is a command with a stable id (e.g. `file.save`).
//! Keybindings, the palette, toolbar buttons and (later) plugins all refer
//! to commands by id. This crate is UI-independent: the frontend maps ids to
//! handlers and resolved keystrokes to its own key-binding type.

pub mod builtin;
pub mod keymap;
pub mod keystroke;
pub mod menus;
pub mod palette;

use std::borrow::Cow;

pub use keymap::{Context, Resolved};
pub use keystroke::Platform;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    /// Stable, dotted identifier used by keymaps and plugins.
    pub id: Cow<'static, str>,
    /// Human-readable title shown in the palette.
    pub title: Cow<'static, str>,
    pub category: Cow<'static, str>,
    /// Bindable but not listed in the palette (e.g. cursor movement).
    pub hidden: bool,
}

impl CommandInfo {
    pub const fn builtin(
        id: &'static str,
        title: &'static str,
        category: &'static str,
        hidden: bool,
    ) -> Self {
        Self {
            id: Cow::Borrowed(id),
            title: Cow::Borrowed(title),
            category: Cow::Borrowed(category),
            hidden,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    commands: Vec<CommandInfo>,
}

impl Registry {
    /// A registry holding the editor's built-in commands.
    pub fn builtin() -> Self {
        Self {
            commands: builtin::COMMANDS.to_vec(),
        }
    }

    /// Adds a command. Ids must be unique.
    pub fn register(&mut self, command: CommandInfo) -> Result<(), String> {
        if self.get(&command.id).is_some() {
            return Err(format!("command `{}` is already registered", command.id));
        }
        self.commands.push(command);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|command| command.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CommandInfo> {
        self.commands.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique_and_dotted() {
        let registry = Registry::builtin();
        let mut ids: Vec<_> = registry.iter().map(|c| c.id.as_ref()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate builtin command ids");
        assert!(ids.iter().all(|id| id.contains('.') && !id.contains(' ')));
    }

    #[test]
    fn register_rejects_duplicates() {
        let mut registry = Registry::builtin();
        let dup = CommandInfo::builtin("file.save", "Save again", "File", false);
        assert!(registry.register(dup).is_err());
        let new = CommandInfo {
            id: "plugin.hello".to_owned().into(),
            title: "Hello".to_owned().into(),
            category: "Plugin".into(),
            hidden: false,
        };
        registry.register(new).unwrap();
        assert_eq!(registry.get("plugin.hello").unwrap().title, "Hello");
    }
}
