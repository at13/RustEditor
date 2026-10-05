//! `read_file`, `list_dir`, `write_file`, `create_file`, `delete_file`.

use std::path::Path;

use re_agent::{
    Action, Approval, Prepared, Remember, Risk, Tool, ToolContext, ToolOutput, ToolSpec,
    diff::line_diff, tools::HostEffect,
};
use re_core::text_io::{self, Encoding};
use serde_json::{Value, json};

use crate::{MAX_EDIT_BYTES, cap, numbered, scope};

/// Read approval for a path outside the scope: once per folder per
/// conversation.
fn read_access(resolved: &scope::Resolved, action: Action, what: &str) -> Prepared {
    if resolved.inside {
        return Prepared::Run(action);
    }
    let folder = resolved
        .display
        .parent()
        .unwrap_or(&resolved.display)
        .to_path_buf();
    Prepared::Ask(
        Approval {
            title: format!("Allow reading outside this folder: {}", folder.display()),
            detail: format!("The agent wants to {what} {}.", resolved.display.display()),
            diff: Vec::new(),
            risk: Risk::ReadOutsideScope,
            remember: Remember::Conversation(format!("read:{}", folder.display())),
        },
        action,
    )
}

fn path_arg(args: &Value) -> &str {
    args.get("path").and_then(Value::as_str).unwrap_or_default()
}

pub struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description:
                "Read a file from disk (relative to the conversation's folder, or absolute). \
                          Returns numbered lines; use start_line/end_line for large files."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            risk: Risk::Read,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let resolved = scope::resolve(ctx.scope.as_deref(), path_arg(args))?;
        let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let path = resolved.path.clone();
        let shown = resolved.display.clone();
        let action = Action::Worker(Box::new(move |_| match read_lines(&path, start, end) {
            Ok(text) => ToolOutput::ok(format!("{}\n{text}", shown.display())),
            Err(error) => ToolOutput::error(error),
        }));
        Ok(read_access(&resolved, action, "read"))
    }
}

fn read_lines(path: &Path, start: usize, end: Option<usize>) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder; use list_dir", path.display()));
    }
    if meta.len() as usize > MAX_EDIT_BYTES {
        // Huge files: stream only the requested lines.
        let start = start.max(1);
        let end = end.unwrap_or(start + 199);
        let mut lines = Vec::new();
        for item in re_core::huge::HugeFile::lines(path, 4096).map_err(|e| e.to_string())? {
            let (n, text) = item.map_err(|e| e.to_string())?;
            if n + 1 > end {
                break;
            }
            if n + 1 >= start {
                lines.push(text);
            }
        }
        return Ok(format!(
            "(large file, {} bytes; showing lines {start}-{end})\n{}",
            meta.len(),
            numbered(lines.iter().map(String::as_str), start, end)
        ));
    }
    let loaded = text_io::load(path).map_err(|e| format!("{e:#}"))?;
    let rope = loaded.rope;
    let total = rope.len_lines();
    let start = start.clamp(1, total.max(1));
    let end = end.unwrap_or(total).clamp(start, total.max(start));
    let lines: Vec<String> = (start - 1..end).map(|i| rope.line(i).to_string()).collect();
    Ok(format!(
        "({total} lines, {})\n{}",
        loaded.encoding.label(),
        numbered(lines.iter().map(String::as_str), start, total)
    ))
}

pub struct ListDir;

impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description:
                "List a folder (default: the conversation's folder). Folders end with `/`.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "additionalProperties": false
            }),
            risk: Risk::Read,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let raw = match path_arg(args) {
            "" => ".",
            other => other,
        };
        let resolved = scope::resolve(ctx.scope.as_deref(), raw)?;
        let path = resolved.path.clone();
        let action = Action::Worker(Box::new(move |_| {
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(e) => return ToolOutput::error(format!("cannot list {}: {e}", path.display())),
            };
            let mut items: Vec<(bool, String, u64)> = entries
                .flatten()
                .map(|e| {
                    let meta = e.metadata().ok();
                    (
                        meta.as_ref().is_some_and(|m| m.is_dir()),
                        e.file_name().to_string_lossy().into_owned(),
                        meta.map_or(0, |m| m.len()),
                    )
                })
                .collect();
            items.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            });
            let total = items.len();
            let mut out = String::new();
            for (dir, name, size) in items.into_iter().take(500) {
                if dir {
                    out.push_str(&format!("{name}/\n"));
                } else {
                    out.push_str(&format!("{name}  ({size} bytes)\n"));
                }
            }
            if total > 500 {
                out.push_str(&format!("[... {} more entries]\n", total - 500));
            }
            ToolOutput::ok(cap(out, "list a subfolder"))
        }));
        Ok(read_access(&resolved, action, "list"))
    }
}

/// `write_file` (replace or create) and `create_file` (never overwrites).
pub struct WriteFile {
    pub create: bool,
}

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        let (name, description) = if self.create {
            (
                "create_file",
                "Create a new file with the given content (fails if it exists). The user approves first.",
            )
        } else {
            (
                "write_file",
                "Replace a file's whole content (or create it). Not for the open file: use edit_buffer. The user sees a diff and approves first.",
            )
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            risk: Risk::Write,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let resolved = scope::resolve(ctx.scope.as_deref(), path_arg(args))?;
        let content = args["content"].as_str().unwrap_or_default().to_owned();
        if content.len() > MAX_EDIT_BYTES {
            return Err("the content is too large".into());
        }
        if resolved.path.is_dir() {
            return Err(format!("{} is a folder", resolved.display.display()));
        }
        let open = ctx
            .doc
            .as_ref()
            .and_then(|d| d.path.as_deref())
            .and_then(|p| std::fs::canonicalize(p).ok());
        if open.is_some_and(|p| p == resolved.path) {
            return Err("this is the open file; use edit_buffer so the change is undoable".into());
        }
        let exists = resolved.path.exists();
        if self.create && exists {
            return Err(format!(
                "{} already exists; use write_file to replace it",
                resolved.display.display()
            ));
        }
        let (old, encoding) = if exists {
            let loaded = text_io::load(&resolved.path).map_err(|e| format!("{e:#}"))?;
            (loaded.rope.to_string(), loaded.encoding)
        } else {
            (String::new(), Encoding::Utf8)
        };
        let diff = line_diff(&old, &content, 3, 1);
        let path = resolved.path.clone();
        let shown = resolved.display.clone();
        let action = Action::Worker(Box::new(move |_| {
            if let Some(parent) = path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return ToolOutput::error(format!("cannot create {}: {e}", parent.display()));
            }
            let rope = ropey::Rope::from_str(&content);
            match text_io::atomic_write(&path, |out| text_io::encode(&rope, encoding, out)) {
                Ok(_) => ToolOutput {
                    content: format!("Wrote {} bytes to {}", content.len(), shown.display()),
                    is_error: false,
                    effects: vec![HostEffect::FileChanged(path.clone())],
                },
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }));
        let verb = if exists { "Replace" } else { "Create" };
        Ok(Prepared::Ask(
            Approval {
                title: format!("{verb} {}", resolved.display.display()),
                detail: if resolved.inside {
                    String::new()
                } else {
                    "This file is outside the conversation's folder.".into()
                },
                diff,
                risk: Risk::Write,
                remember: Remember::Once,
            },
            action,
        ))
    }
}

/// `delete_file`: off by default (only agents that list it can use it).
pub struct DeleteFile;

impl Tool for DeleteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delete_file".into(),
            description: "Delete one file (not a folder). The user approves first.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "minLength": 1}},
                "required": ["path"],
                "additionalProperties": false
            }),
            risk: Risk::Write,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let resolved = scope::resolve(ctx.scope.as_deref(), path_arg(args))?;
        if !resolved.path.is_file() {
            return Err(format!("{} is not a file", resolved.display.display()));
        }
        let path = resolved.path.clone();
        Ok(Prepared::Ask(
            Approval {
                title: format!("Delete {}", resolved.display.display()),
                detail: "The file is deleted permanently (not moved to the recycle bin).".into(),
                diff: Vec::new(),
                risk: Risk::Write,
                remember: Remember::Once,
            },
            Action::Worker(Box::new(move |_| match std::fs::remove_file(&path) {
                Ok(()) => ToolOutput {
                    content: format!("Deleted {}", path.display()),
                    is_error: false,
                    effects: vec![HostEffect::FileChanged(path.clone())],
                },
                Err(e) => ToolOutput::error(e.to_string()),
            })),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{run, temp_dir};

    fn ctx(scope: &Path) -> ToolContext {
        ToolContext {
            scope: Some(scope.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn read_and_list_inside_the_scope_without_asking() {
        let root = temp_dir("files");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "x\r\ny\r\n").unwrap();
        let c = ctx(&root);
        let prepared = ReadFile.prepare(&c, &json!({"path": "a.txt"})).unwrap();
        assert!(matches!(prepared, Prepared::Run(_)));
        let out = run(prepared).content;
        assert!(out.contains("    2| y"), "{out}");
        let out = run(ListDir.prepare(&c, &json!({})).unwrap()).content;
        assert_eq!(out.lines().next(), Some("sub/"));
        assert!(out.contains("a.txt  (6 bytes)"));
        let missing = run(ReadFile.prepare(&c, &json!({"path": "nope.txt"})).unwrap());
        assert!(missing.is_error);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reading_outside_asks_once_per_folder() {
        let root = temp_dir("inside");
        let other = temp_dir("outside");
        std::fs::write(other.join("secret.txt"), "s").unwrap();
        let prepared = ReadFile
            .prepare(
                &ctx(&root),
                &json!({"path": other.join("secret.txt").to_string_lossy()}),
            )
            .unwrap();
        let Prepared::Ask(approval, _) = &prepared else {
            panic!("ask");
        };
        assert_eq!(approval.risk, Risk::ReadOutsideScope);
        assert!(matches!(&approval.remember, Remember::Conversation(k) if k.starts_with("read:")));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn writes_always_ask_show_a_diff_and_keep_the_encoding() {
        let root = temp_dir("write");
        std::fs::write(root.join("b.txt"), b"caf\xe9\n").unwrap(); // Windows-1252
        let c = ctx(&root);
        let prepared = WriteFile { create: false }
            .prepare(
                &c,
                &json!({"path": "b.txt", "content": "caf\u{e9} au lait\n"}),
            )
            .unwrap();
        let Prepared::Ask(approval, _) = &prepared else {
            panic!("ask");
        };
        assert_eq!(
            approval.title,
            format!("Replace {}", root.join("b.txt").display())
        );
        assert!(!approval.diff.is_empty());
        let out = run(prepared);
        assert!(!out.is_error, "{}", out.content);
        assert!(matches!(&out.effects[0], HostEffect::FileChanged(_)));
        assert_eq!(
            std::fs::read(root.join("b.txt")).unwrap(),
            b"caf\xe9 au lait\n"
        );
        // create_file refuses to overwrite; creates folders as needed.
        assert!(
            WriteFile { create: true }
                .prepare(&c, &json!({"path": "b.txt", "content": ""}))
                .is_err()
        );
        run(WriteFile { create: true }
            .prepare(&c, &json!({"path": "new/c.txt", "content": "hi"}))
            .unwrap());
        assert_eq!(
            std::fs::read_to_string(root.join("new/c.txt")).unwrap(),
            "hi"
        );
        run(DeleteFile
            .prepare(&c, &json!({"path": "new/c.txt"}))
            .unwrap());
        assert!(!root.join("new/c.txt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_open_file_must_be_edited_through_the_buffer() {
        let root = temp_dir("open");
        std::fs::write(root.join("open.rs"), "x").unwrap();
        let mut c = ctx(&root);
        c.doc = Some(re_agent::DocSnapshot {
            path: Some(root.join("open.rs")),
            name: "open.rs".into(),
            rope: ropey::Rope::from_str("x"),
            revision: 0,
            read_only: false,
            line_ending: "\n",
        });
        let error = WriteFile { create: false }
            .prepare(&c, &json!({"path": "open.rs", "content": "y"}))
            .unwrap_err();
        assert!(error.contains("edit_buffer"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
