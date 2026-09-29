//! Keymap files and resolution.
//!
//! Format (a small, valid subset of TOML so no TOML crate is needed):
//!
//! ```toml
//! # Comments start with '#'.
//! [global]              # active everywhere
//! "mod-s" = "file.save"
//!
//! [editor]              # active while the text input has focus
//! "enter" = "editor.newline"
//! "mod-d" = ""          # empty string removes a default binding
//! ```
//!
//! Keys and values must be quoted. A section may be limited to one OS with a
//! suffix: `[global.macos]`, `[editor.windows]`, `[editor.linux]`; those
//! entries apply after the plain sections of the same file.
//!
//! The user keymap is layered over the default keymap: an entry with the
//! same context and keystroke replaces it.

use std::collections::HashMap;

use crate::{
    Registry,
    keystroke::{self, Platform},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Context {
    Global,
    Editor,
}

impl Context {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "global" => Some(Self::Global),
            "editor" => Some(Self::Editor),
            _ => None,
        }
    }

    /// The frontend key-context predicate, if any.
    pub fn predicate(self) -> Option<&'static str> {
        match self {
            Self::Global => None,
            Self::Editor => Some("Editor"),
        }
    }
}

/// One line of a keymap file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub line: usize,
    pub context: Context,
    /// Only for this OS (`[editor.macos]`); `None` = every OS.
    pub os: Option<Platform>,
    pub keys: String,
    /// `None` unbinds the keystroke.
    pub command: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

pub fn parse(text: &str) -> Result<Vec<Entry>, ParseError> {
    let mut context = None;
    let mut os = None;
    let mut entries = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let err = |message: String| ParseError { line, message };
        let rest = raw.trim();
        if rest.is_empty() || rest.starts_with('#') {
            continue;
        }
        if let Some(header) = rest.strip_prefix('[') {
            let (name, after) = header
                .split_once(']')
                .ok_or_else(|| err("missing `]`".into()))?;
            expect_end(after).map_err(err)?;
            let name = name.trim();
            let (base, suffix) = match name.split_once('.') {
                Some((base, suffix)) => (base, Some(suffix)),
                None => (name, None),
            };
            context = Some(Context::parse(base).ok_or_else(|| {
                err(format!(
                    "unknown section `[{name}]`; use [global] or [editor], optionally .windows/.macos/.linux"
                ))
            })?);
            os = match suffix {
                None => None,
                Some(suffix) => Some(Platform::from_name(suffix).ok_or_else(|| {
                    err(format!(
                        "unknown OS `{suffix}`; use windows, macos or linux"
                    ))
                })?),
            };
            continue;
        }
        let context =
            context.ok_or_else(|| err("binding before any [global] or [editor] section".into()))?;
        let (keys, after) = parse_string(rest)
            .ok_or_else(|| err("keystroke must be a quoted string, e.g. \"mod-s\"".into()))?;
        let after = after
            .trim_start()
            .strip_prefix('=')
            .ok_or_else(|| err("expected `=` after the keystroke".into()))?;
        let (command, after) = parse_string(after.trim_start())
            .ok_or_else(|| err("command must be a quoted string, e.g. \"file.save\"".into()))?;
        expect_end(after).map_err(err)?;
        entries.push(Entry {
            line,
            context,
            os,
            keys,
            command: (!command.is_empty()).then_some(command),
        });
    }
    Ok(entries)
}

fn expect_end(rest: &str) -> Result<(), String> {
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(())
    } else {
        Err(format!("unexpected text `{rest}`"))
    }
}

/// Parses a `"basic"` (with `\"` and `\\` escapes) or `'literal'` string.
fn parse_string(input: &str) -> Option<(String, &str)> {
    let mut chars = input.char_indices();
    let quote = match chars.next()? {
        (_, q @ ('"' | '\'')) => q,
        _ => return None,
    };
    let mut out = String::new();
    let mut escaped = false;
    for (i, c) in chars {
        if escaped {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if c == '\\' && quote == '"' {
            escaped = true;
        } else if c == quote {
            return Some((out, &input[i + 1..]));
        } else {
            out.push(c);
        }
    }
    None
}

/// A keymap resolved for one platform.
#[derive(Debug, Default)]
pub struct Resolved {
    /// `(context, normalized keystrokes, command id)` in binding order.
    pub bindings: Vec<(Context, String, String)>,
    /// Problems found in the keymap; the offending entries are skipped.
    pub errors: Vec<String>,
}

impl Resolved {
    /// Display labels for a command's keystrokes (global bindings first).
    pub fn labels(&self, command: &str, platform: Platform) -> Vec<String> {
        let mut found: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, _, id)| id == command)
            .collect();
        found.sort_by_key(|(context, _, _)| *context != Context::Global);
        found
            .into_iter()
            .map(|(_, keys, _)| keystroke::label(keys, platform))
            .collect()
    }
}

/// Resolves the default keymap with an optional user keymap layered on top.
/// Invalid entries are reported in [`Resolved::errors`] and skipped.
pub fn resolve(
    default: &str,
    user: Option<&str>,
    registry: &Registry,
    platform: Platform,
) -> Resolved {
    let mut resolved = Resolved::default();
    let mut order: Vec<(Context, String)> = Vec::new();
    let mut map: HashMap<(Context, String), Option<String>> = HashMap::new();

    for (source, text) in [("default keymap", Some(default)), ("user keymap", user)] {
        let Some(text) = text else { continue };
        let entries = match parse(text) {
            Ok(entries) => entries,
            Err(error) => {
                resolved.errors.push(format!("{source}: {error}"));
                continue;
            }
        };
        // Plain sections first, then this OS's sections; other OSes skipped.
        let (plain, specific): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|entry| entry.os.is_none());
        let specific = specific
            .into_iter()
            .filter(|entry| entry.os == Some(platform));
        for entry in plain.into_iter().chain(specific) {
            let keys = match keystroke::normalize(&entry.keys, platform) {
                Ok(keys) => keys,
                Err(error) => {
                    resolved
                        .errors
                        .push(format!("{source}: line {}: {error}", entry.line));
                    continue;
                }
            };
            if let Some(command) = &entry.command
                && registry.get(command).is_none()
            {
                resolved.errors.push(format!(
                    "{source}: line {}: unknown command `{command}`",
                    entry.line
                ));
                continue;
            }
            let slot = (entry.context, keys);
            if !map.contains_key(&slot) {
                order.push(slot.clone());
            }
            map.insert(slot, entry.command);
        }
    }

    for slot in order {
        if let Some(Some(command)) = map.remove(&slot) {
            resolved.bindings.push((slot.0, slot.1, command));
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = r#"
# Defaults
[global]
"mod-s" = "file.save"
"mod-z" = "edit.undo"   # trailing comment
"mod-shift-z" = 'edit.redo'

[editor]
"enter" = "editor.newline"
"#;

    #[test]
    fn parses_sections_strings_and_comments() {
        let entries = parse(DEFAULT).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].context, Context::Global);
        assert_eq!(entries[0].keys, "mod-s");
        assert_eq!(entries[2].command.as_deref(), Some("edit.redo"));
        assert_eq!(entries[3].context, Context::Editor);
        assert_eq!(parse("[editor]\n\"x\" = \"\"").unwrap()[0].command, None);
    }

    #[test]
    fn parse_errors_report_line_numbers() {
        let cases = [
            ("\"mod-s\" = \"file.save\"", 1, "before any"),
            ("[global]\nmod-s = \"file.save\"", 2, "quoted"),
            ("[global]\n\"mod-s\" \"file.save\"", 2, "expected `=`"),
            ("[global]\n\"mod-s\" = \"file.save\" extra", 2, "unexpected"),
            ("[keys]", 1, "unknown section"),
            ("[global]\n\"mod-s\" = \"unterminated", 2, "quoted"),
        ];
        for (text, line, needle) in cases {
            let error = parse(text).unwrap_err();
            assert_eq!(error.line, line, "{text}");
            assert!(error.message.contains(needle), "{text}: {}", error.message);
        }
    }

    #[test]
    fn user_keymap_overrides_unbinds_and_adds() {
        let user = r#"
[global]
"ctrl-s" = "file.save_as"   # same keystroke as mod-s on Windows/Linux
"mod-z" = ""
"f5" = "edit.undo"
"#;
        let registry = Registry::builtin();
        let resolved = resolve(DEFAULT, Some(user), &registry, Platform::Windows);
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        let bindings: Vec<_> = resolved
            .bindings
            .iter()
            .map(|(c, k, id)| (*c, k.as_str(), id.as_str()))
            .collect();
        assert_eq!(
            bindings,
            vec![
                (Context::Global, "ctrl-s", "file.save_as"),
                (Context::Global, "ctrl-shift-z", "edit.redo"),
                (Context::Editor, "enter", "editor.newline"),
                (Context::Global, "f5", "edit.undo"),
            ]
        );
        assert_eq!(resolved.labels("edit.undo", Platform::Windows), vec!["F5"]);
    }

    #[test]
    fn mod_resolves_per_platform() {
        let registry = Registry::builtin();
        let mac = resolve(DEFAULT, None, &registry, Platform::MacOs);
        assert_eq!(mac.bindings[0].1, "cmd-s");
        assert_eq!(mac.labels("edit.redo", Platform::MacOs), vec!["⇧⌘Z"]);
        let win = resolve(DEFAULT, None, &registry, Platform::Windows);
        assert_eq!(win.bindings[0].1, "ctrl-s");
    }

    #[test]
    fn os_sections_apply_only_on_their_os_and_after_plain_sections() {
        let text = r#"
[editor.macos]
"mod-left" = "editor.home"
[editor]
"mod-left" = "editor.word_left"
[editor.windows]
"f9" = "editor.home"
"#;
        let registry = Registry::builtin();
        let pairs = |r: &Resolved| -> Vec<(String, String)> {
            r.bindings
                .iter()
                .map(|(_, k, c)| (k.clone(), c.clone()))
                .collect()
        };
        let mac = resolve(text, None, &registry, Platform::MacOs);
        assert!(mac.errors.is_empty(), "{:?}", mac.errors);
        assert_eq!(pairs(&mac), vec![("cmd-left".into(), "editor.home".into())]);
        let win = resolve(text, None, &registry, Platform::Windows);
        assert_eq!(
            pairs(&win),
            vec![
                ("ctrl-left".into(), "editor.word_left".into()),
                ("f9".into(), "editor.home".into())
            ]
        );
        let linux = resolve(text, None, &registry, Platform::Linux);
        assert_eq!(linux.bindings.len(), 1);
        let error = parse("[editor.beos]").unwrap_err();
        assert!(error.message.contains("unknown OS"), "{}", error.message);
    }

    #[test]
    fn bad_user_entries_are_reported_and_skipped() {
        let user = "[global]\n\"hyper-q\" = \"file.save\"\n\"mod-q\" = \"no.such\"\n\"mod-n\" = \"file.new\"";
        let resolved = resolve(DEFAULT, Some(user), &Registry::builtin(), Platform::Windows);
        assert_eq!(resolved.errors.len(), 2, "{:?}", resolved.errors);
        assert!(resolved.errors[0].contains("line 2"));
        assert!(resolved.errors[1].contains("unknown command `no.such`"));
        assert!(
            resolved
                .bindings
                .iter()
                .any(|(_, k, id)| k == "ctrl-n" && id == "file.new")
        );
        // A syntax error in the user keymap keeps the defaults.
        let broken = resolve(
            DEFAULT,
            Some("oops"),
            &Registry::builtin(),
            Platform::Windows,
        );
        assert_eq!(broken.errors.len(), 1);
        assert_eq!(broken.bindings.len(), 4);
    }
}
