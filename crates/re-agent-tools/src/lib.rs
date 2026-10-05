//! Built-in tools for agents.
//!
//! | Tool | Risk |
//! |------|------|
//! | `read_buffer`, `read_file`, `list_dir`, `search` | read (outside the scope: asks once per folder) |
//! | `edit_buffer`, `write_file`, `create_file`, `delete_file` | write (always asks, diff shown) |
//! | `run_command` | process (always asks) |
//! | `ext_*` | Extension Builder only; installing always asks |
//!
//! Plugin-contributed tools are wrapped by [`plugin::plugin_tools`] and
//! named `<plugin-id>__<tool>` (model APIs do not allow `.` in names).

pub mod buffer;
pub mod ext;
pub mod files;
pub mod plugin;
pub mod process;
pub mod scope;
pub mod search;

use std::sync::Arc;

use re_agent::ToolRegistry;

/// Most text one tool returns to the model.
pub const MAX_OUTPUT: usize = 64 * 1024;
/// Largest document or file the tools edit or read whole.
pub const MAX_EDIT_BYTES: usize = 32 * 1024 * 1024;

/// Every built-in tool. Agents only see those in their allowlist.
pub fn builtin_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(buffer::ReadBuffer));
    registry.register(Arc::new(buffer::EditBuffer));
    registry.register(Arc::new(files::ReadFile));
    registry.register(Arc::new(files::ListDir));
    registry.register(Arc::new(files::WriteFile { create: false }));
    registry.register(Arc::new(files::WriteFile { create: true }));
    registry.register(Arc::new(files::DeleteFile));
    registry.register(Arc::new(search::Search));
    registry.register(Arc::new(process::RunCommand));
    for tool in ext::tools() {
        registry.register(tool);
    }
    registry
}

/// Cuts `text` to [`MAX_OUTPUT`] bytes at a character boundary.
pub fn cap(mut text: String, note: &str) -> String {
    if text.len() > MAX_OUTPUT {
        let mut end = MAX_OUTPUT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(&format!("\n[... output truncated; {note}]"));
    }
    text
}

/// Lines `start..=end` (1-based) as `  12| text`, capped.
pub fn numbered<'a>(lines: impl Iterator<Item = &'a str>, start: usize, total: usize) -> String {
    let mut out = String::new();
    let mut last = start.saturating_sub(1);
    for (i, line) in lines.enumerate() {
        let n = start + i;
        let line = line.trim_end_matches(['\n', '\r']);
        if out.len() + line.len() > MAX_OUTPUT {
            out.push_str(&format!(
                "[... stopped at line {last} of {total}; read from line {} to continue]\n",
                last + 1
            ));
            return out;
        }
        out.push_str(&format!("{n:>5}| {line}\n"));
        last = n;
    }
    out
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "re-agent-tools-{name}-{}-{}",
            std::process::id(),
            re_agent::conversation::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs a worker action synchronously.
    pub fn run(prepared: re_agent::Prepared) -> re_agent::ToolOutput {
        let action = match prepared {
            re_agent::Prepared::Run(a) | re_agent::Prepared::Ask(_, a) => a,
        };
        match action {
            re_agent::Action::Worker(f) => re_agent::tools::run_worker(f, &Default::default()),
            other => panic!("expected a worker action, got {other:?}"),
        }
    }
}
