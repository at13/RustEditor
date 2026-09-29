//! Platform-neutral keystroke syntax.
//!
//! Keymaps use `mod` for the platform's primary modifier: Ctrl on Windows and
//! Linux, Cmd on macOS. [`normalize`] turns a user-written keystroke such as
//! `Shift+Mod+S` or `mod-shift-s` into the canonical frontend form
//! (`ctrl-shift-s` / `shift-cmd-s`), and [`label`] renders it for display.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    /// Linux and other Unix desktops.
    Linux,
}

impl Platform {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    /// Name used in keymap section suffixes (`[editor.macos]`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::MacOs => "macos",
            Self::Linux => "linux",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::MacOs),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }
}

const NAMED_KEYS: &[&str] = &[
    "enter",
    "escape",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
];

#[derive(Default)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    function: bool,
}

/// Normalizes a keystroke sequence (space-separated chords). Modifiers may be
/// joined with `-` or `+`, in any order and case.
pub fn normalize(spec: &str, platform: Platform) -> Result<String, String> {
    let chords: Vec<_> = spec.split_whitespace().collect();
    if chords.is_empty() {
        return Err("empty keystroke".into());
    }
    chords
        .into_iter()
        .map(|chord| normalize_chord(chord, platform))
        .collect::<Result<Vec<_>, _>>()
        .map(|chords| chords.join(" "))
}

fn normalize_chord(chord: &str, platform: Platform) -> Result<String, String> {
    let lower = chord.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let is_sep = |b: u8| b == b'-' || b == b'+';
    // The key is everything after the last separator; a separator doubled at
    // the end (`mod--`) means the key itself is `-` or `+`.
    let (mods_part, key) = if lower.chars().count() == 1 {
        ("", lower.as_str())
    } else if bytes.len() >= 2 && is_sep(bytes[bytes.len() - 1]) && is_sep(bytes[bytes.len() - 2]) {
        (&lower[..lower.len() - 2], &lower[lower.len() - 1..])
    } else {
        match lower.rfind(['-', '+']) {
            Some(i) => (&lower[..i], &lower[i + 1..]),
            None => ("", lower.as_str()),
        }
    };
    if key.is_empty() {
        return Err(format!("missing key in `{chord}`"));
    }
    let mut mods = Mods::default();
    for part in mods_part.split(['-', '+']).filter(|part| !part.is_empty()) {
        let flag = match (part, platform) {
            ("mod" | "secondary", Platform::MacOs) => &mut mods.cmd,
            ("mod" | "secondary", Platform::Windows | Platform::Linux) => &mut mods.ctrl,
            ("ctrl" | "control", _) => &mut mods.ctrl,
            ("alt" | "option" | "opt", _) => &mut mods.alt,
            ("shift", _) => &mut mods.shift,
            ("cmd" | "command" | "super" | "win" | "meta", _) => &mut mods.cmd,
            ("fn", _) => &mut mods.function,
            _ => return Err(format!("unknown modifier `{part}` in `{chord}`")),
        };
        if *flag {
            return Err(format!("modifier `{part}` repeated in `{chord}`"));
        }
        *flag = true;
    }
    let key = match key {
        "esc" => "escape",
        "return" => "enter",
        "del" => "delete",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        other => other,
    };
    let valid = NAMED_KEYS.contains(&key)
        || key
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n))
        || (key.chars().count() == 1 && !key.starts_with(char::is_whitespace));
    if !valid {
        return Err(format!("unknown key `{key}` in `{chord}`"));
    }
    let mut out = String::new();
    for (on, name) in [
        (mods.ctrl, "ctrl-"),
        (mods.alt, "alt-"),
        (mods.shift, "shift-"),
        (mods.cmd, "cmd-"),
        (mods.function, "fn-"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(key);
    Ok(out)
}

/// Renders a normalized keystroke for display, e.g. `Ctrl+Shift+S` or `⇧⌘S`.
pub fn label(normalized: &str, platform: Platform) -> String {
    normalized
        .split(' ')
        .map(|chord| {
            let (mods, key) = match chord.strip_suffix("--") {
                Some(mods) => (mods, "-"),
                None => chord.rsplit_once('-').unwrap_or(("", chord)),
            };
            let mods: Vec<&str> = mods.split('-').filter(|m| !m.is_empty()).collect();
            let key = key_label(key);
            match platform {
                Platform::MacOs => {
                    let mut out = String::new();
                    for (name, symbol) in [
                        ("ctrl", "⌃"),
                        ("alt", "⌥"),
                        ("shift", "⇧"),
                        ("cmd", "⌘"),
                        ("fn", "fn "),
                    ] {
                        if mods.contains(&name) {
                            out.push_str(symbol);
                        }
                    }
                    out + &key
                }
                Platform::Windows | Platform::Linux => {
                    let mut parts: Vec<String> = mods
                        .iter()
                        .map(|m| match *m {
                            "ctrl" => "Ctrl".to_owned(),
                            "alt" => "Alt".to_owned(),
                            "shift" => "Shift".to_owned(),
                            "cmd" if platform == Platform::Windows => "Win".to_owned(),
                            "cmd" => "Super".to_owned(),
                            _ => "Fn".to_owned(),
                        })
                        .collect();
                    parts.push(key);
                    parts.join("+")
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn key_label(key: &str) -> String {
    match key {
        "escape" => "Esc".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        _ if key.chars().count() == 1 => key.to_uppercase(),
        _ => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_maps_to_ctrl_or_cmd() {
        assert_eq!(normalize("mod-s", Platform::Windows).unwrap(), "ctrl-s");
        assert_eq!(normalize("mod-s", Platform::MacOs).unwrap(), "cmd-s");
        assert_eq!(
            normalize("Shift+Mod+S", Platform::Windows).unwrap(),
            "ctrl-shift-s"
        );
        assert_eq!(
            normalize("shift-mod-s", Platform::MacOs).unwrap(),
            "shift-cmd-s"
        );
        // Literal ctrl stays ctrl on macOS.
        assert_eq!(normalize("ctrl-a", Platform::MacOs).unwrap(), "ctrl-a");
    }

    #[test]
    fn normalizes_aliases_sequences_and_punctuation() {
        assert_eq!(normalize("Esc", Platform::Windows).unwrap(), "escape");
        assert_eq!(
            normalize("shift-F3", Platform::Windows).unwrap(),
            "shift-f3"
        );
        assert_eq!(
            normalize("mod-k  mod-c", Platform::Windows).unwrap(),
            "ctrl-k ctrl-c"
        );
        assert_eq!(normalize("mod--", Platform::Windows).unwrap(), "ctrl--");
        assert_eq!(normalize("mod-+", Platform::Windows).unwrap(), "ctrl-+");
        assert_eq!(normalize("mod-/", Platform::Windows).unwrap(), "ctrl-/");
        assert_eq!(normalize("-", Platform::Windows).unwrap(), "-");
    }

    #[test]
    fn rejects_bad_keystrokes() {
        assert!(normalize("", Platform::Windows).is_err());
        assert!(normalize("hyper-s", Platform::Windows).is_err());
        assert!(normalize("ctrl-ctrl-s", Platform::Windows).is_err());
        assert!(normalize("mod-ctrl-s", Platform::Windows).is_err());
        assert!(normalize("ctrl-nope", Platform::Windows).is_err());
        assert!(normalize("f25", Platform::Windows).is_err());
    }

    #[test]
    fn labels_follow_platform_conventions() {
        assert_eq!(label("ctrl-shift-s", Platform::Windows), "Ctrl+Shift+S");
        assert_eq!(label("shift-cmd-s", Platform::MacOs), "⇧⌘S");
        assert_eq!(label("shift-f3", Platform::Windows), "Shift+F3");
        assert_eq!(label("escape", Platform::Windows), "Esc");
        assert_eq!(label("ctrl--", Platform::Windows), "Ctrl+-");
        assert_eq!(label("ctrl-k ctrl-c", Platform::Windows), "Ctrl+K Ctrl+C");
        assert_eq!(label("cmd-e", Platform::Windows), "Win+E");
        assert_eq!(label("cmd-e", Platform::Linux), "Super+E");
        assert_eq!(normalize("mod-s", Platform::Linux).unwrap(), "ctrl-s");
    }
}
