//! The application menu bar, as data: every item is a registered command,
//! so menus, the palette, keymaps and plugins stay in sync.

/// One entry in a menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    /// A command; `label` overrides the command's title in the menu.
    Command {
        id: &'static str,
        label: Option<&'static str>,
    },
    Separator,
    /// The commands contributed by plugins (filled in by the frontend).
    PluginCommands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Menu {
    pub title: &'static str,
    /// Opens the menu with Alt+key on Windows/Linux (underlined letter).
    pub mnemonic: char,
    pub items: &'static [MenuItem],
}

impl Menu {
    /// Command id that opens this menu (`app.menu.file`, ...).
    pub fn open_command(&self) -> String {
        format!("app.menu.{}", self.title.to_ascii_lowercase())
    }
}

const fn cmd(id: &'static str) -> MenuItem {
    MenuItem::Command { id, label: None }
}

const fn named(id: &'static str, label: &'static str) -> MenuItem {
    MenuItem::Command {
        id,
        label: Some(label),
    }
}

const SEP: MenuItem = MenuItem::Separator;

pub static MENUS: &[Menu] = &[
    Menu {
        title: "File",
        mnemonic: 'f',
        items: &[
            cmd("file.new"),
            cmd("file.open"),
            SEP,
            cmd("file.save"),
            cmd("file.save_as"),
            cmd("file.reload"),
            SEP,
            named("file.encoding_utf8", "Encoding: UTF-8"),
            named("file.encoding_utf8_bom", "Encoding: UTF-8 with BOM"),
            named("file.encoding_utf16le", "Encoding: UTF-16 LE"),
            named("file.encoding_utf16be", "Encoding: UTF-16 BE"),
            named("file.encoding_windows1252", "Encoding: Windows-1252"),
            SEP,
            named("file.line_endings_lf", "Line Endings: LF"),
            named("file.line_endings_crlf", "Line Endings: CRLF"),
            SEP,
            named("file.open_fully", "Open Huge File for Editing"),
        ],
    },
    Menu {
        title: "Edit",
        mnemonic: 'e',
        items: &[
            cmd("edit.undo"),
            cmd("edit.redo"),
            SEP,
            cmd("edit.cut"),
            cmd("edit.copy"),
            cmd("edit.paste"),
            cmd("edit.select_all"),
            SEP,
            cmd("edit.add_cursor_above"),
            cmd("edit.add_cursor_below"),
            SEP,
            cmd("edit.insert_line"),
            cmd("edit.delete_line"),
            cmd("edit.go_to_line"),
        ],
    },
    Menu {
        title: "Search",
        mnemonic: 's',
        items: &[
            cmd("search.find"),
            cmd("search.next"),
            cmd("search.previous"),
            SEP,
            cmd("search.replace"),
            cmd("search.replace_all"),
            SEP,
            named("search.toggle_case", "Match Case"),
            named("search.toggle_regex", "Regular Expression"),
        ],
    },
    Menu {
        title: "View",
        mnemonic: 'v',
        items: &[
            named("view.toggle_word_wrap", "Word Wrap"),
            SEP,
            cmd("app.command_palette"),
            SEP,
            cmd("view.close_viewer"),
        ],
    },
    Menu {
        title: "Transform",
        mnemonic: 't',
        items: &[
            named("transform.toggle_scope", "Selection Only"),
            SEP,
            cmd("transform.json_format"),
            cmd("transform.json_view"),
            cmd("transform.base64_decode"),
            SEP,
            cmd("transform.lower"),
            cmd("transform.upper"),
            cmd("transform.proper"),
            cmd("transform.camel"),
            cmd("transform.pascal"),
            cmd("transform.snake"),
            cmd("transform.kebab"),
        ],
    },
    Menu {
        title: "Bookmarks",
        mnemonic: 'b',
        items: &[
            cmd("bookmark.match"),
            cmd("bookmark.all_matches"),
            SEP,
            cmd("bookmark.copy"),
            cmd("bookmark.cut"),
            cmd("bookmark.delete"),
            SEP,
            cmd("bookmark.clear"),
        ],
    },
    Menu {
        title: "Tools",
        mnemonic: 'o',
        items: &[
            cmd("tools.calculator"),
            cmd("tools.ask_ai"),
            SEP,
            cmd("app.open_keymap"),
            cmd("app.reload_keymap"),
        ],
    },
    Menu {
        title: "Plugins",
        mnemonic: 'p',
        items: &[
            MenuItem::PluginCommands,
            SEP,
            cmd("plugins.reload"),
            cmd("plugins.forget_approvals"),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;

    #[test]
    fn every_menu_item_is_a_registered_command() {
        let registry = Registry::builtin();
        for menu in MENUS {
            for item in menu.items {
                if let MenuItem::Command { id, .. } = item {
                    let command = registry
                        .get(id)
                        .unwrap_or_else(|| panic!("{id} not registered"));
                    assert!(!command.hidden, "{id} is hidden but in a menu");
                }
            }
            assert!(
                registry.get(&menu.open_command()).is_some(),
                "{} has no open command",
                menu.open_command()
            );
        }
    }

    #[test]
    fn every_visible_command_is_in_a_menu_once() {
        let mut seen = std::collections::HashMap::new();
        for menu in MENUS {
            for item in menu.items {
                if let MenuItem::Command { id, .. } = item {
                    *seen.entry(*id).or_insert(0) += 1;
                }
            }
        }
        for command in Registry::builtin().iter().filter(|c| !c.hidden) {
            assert_eq!(
                seen.get(command.id.as_ref()).copied().unwrap_or(0),
                1,
                "{} should appear in exactly one menu",
                command.id
            );
        }
    }

    #[test]
    fn mnemonics_are_unique_letters_in_the_title() {
        let mut keys: Vec<char> = MENUS.iter().map(|m| m.mnemonic).collect();
        for menu in MENUS {
            assert!(
                menu.title.to_ascii_lowercase().contains(menu.mnemonic),
                "{}",
                menu.title
            );
        }
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), MENUS.len());
    }
}
