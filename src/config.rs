//! User configuration content. Paths come from `re_platform::paths`.

/// Written when the user first opens their keymap.
pub const USER_KEYMAP_TEMPLATE: &str = r#"# RustEditor user keymap.
#
# Entries here override the defaults. After saving, run "Reload Keymap" from
# the Command Palette (Ctrl/Cmd+Shift+P). `mod` is Ctrl on Windows/Linux and
# Cmd on macOS. An empty command ("") removes a default binding.
#
# Examples:
# [global]
# "mod-shift-f" = "transform.json_format"
# "mod-h" = ""
#
# [editor]
# "mod-d" = "edit.delete_line"

[global]

[editor]
"#;
