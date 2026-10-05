//! `search`: find text or a regular expression in files under a folder.

use std::path::{Path, PathBuf};

use re_agent::{Prepared, Risk, Tool, ToolContext, ToolOutput, ToolSpec};
use re_search::{SearchQuery, find_in_lines};
use re_tasks::CancelToken;
use serde_json::{Value, json};

use crate::{MAX_EDIT_BYTES, cap, scope};

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".history",
    ".venv",
    "__pycache__",
];
const MAX_FILES: usize = 5000;

pub struct Search;

impl Tool for Search {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search".into(),
            description:
                "Search files under a folder (default: the conversation's folder) for text or a \
                          regular expression. Returns `path:line: text` lines."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1},
                    "regex": {"type": "boolean"},
                    "case_sensitive": {"type": "boolean"},
                    "path": {"type": "string", "description": "Folder or file to search."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            risk: Risk::Read,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        let resolved = scope::resolve(ctx.scope.as_deref(), raw)?;
        let query = SearchQuery {
            text: args["pattern"].as_str().unwrap_or_default().to_owned(),
            regex: args.get("regex").and_then(Value::as_bool).unwrap_or(false),
            case_sensitive: args
                .get("case_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        // Compile errors are reported now, not after the walk.
        find_in_lines(
            std::iter::empty::<Result<(usize, String), String>>(),
            &query,
            1,
            None,
        )
        .map_err(|e| format!("{e:#}"))?;
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(100) as usize;
        let root = resolved.path.clone();
        let action =
            re_agent::Action::Worker(Box::new(move |cancel| run(&root, &query, max, cancel)));
        Ok(if resolved.inside {
            Prepared::Run(action)
        } else {
            Prepared::Ask(
                re_agent::Approval {
                    title: format!(
                        "Allow searching outside this folder: {}",
                        resolved.display.display()
                    ),
                    detail: String::new(),
                    diff: Vec::new(),
                    risk: Risk::ReadOutsideScope,
                    remember: re_agent::Remember::Conversation(format!(
                        "read:{}",
                        resolved.display.display()
                    )),
                },
                action,
            )
        })
    }
}

fn files(root: &Path, cancel: &CancelToken) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() || out.len() >= MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries.into_iter().rev() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(t) if t.is_dir() && !SKIP_DIRS.contains(&name.as_str()) => stack.push(path),
                Ok(t) if t.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

fn is_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 4096];
    std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .is_ok_and(|n| {
            head[..n].contains(&0)
                && !head.starts_with(&[0xFF, 0xFE])
                && !head.starts_with(&[0xFE, 0xFF])
        })
}

fn run(root: &Path, query: &SearchQuery, max: usize, cancel: &CancelToken) -> ToolOutput {
    let mut out = String::new();
    let mut found = 0;
    let candidates = files(root, cancel);
    let base = if root.is_file() {
        root.parent().unwrap_or(root)
    } else {
        root
    };
    for path in &candidates {
        if cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }
        if std::fs::metadata(path).map_or(true, |m| m.len() as usize > MAX_EDIT_BYTES)
            || is_binary(path)
        {
            continue;
        }
        let Ok(lines) = re_core::huge::HugeFile::lines(path, 64 * 1024) else {
            continue;
        };
        let mut texts = Vec::new();
        let lines = lines.inspect(|item| {
            if let Ok((_, text)) = item {
                texts.push(text.clone());
            }
        });
        let Ok(results) = find_in_lines(lines, query, max - found, Some(cancel.flag())) else {
            continue;
        };
        let shown = path.strip_prefix(base).unwrap_or(path);
        for m in &results.matches {
            let text: String = texts
                .get(m.line_index)
                .map(|t| t.trim().chars().take(200).collect())
                .unwrap_or_default();
            out.push_str(&format!(
                "{}:{}: {text}\n",
                shown.display(),
                m.line_index + 1
            ));
        }
        found += results.matches.len();
        if found >= max {
            out.push_str(&format!("[stopped at {max} results]\n"));
            break;
        }
    }
    if found == 0 {
        out = format!("No matches in {} files.", candidates.len());
    }
    ToolOutput::ok(cap(out, "narrow the search"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{run as run_tool, temp_dir};

    #[test]
    fn finds_matches_skips_build_folders_and_binaries() {
        let root = temp_dir("search");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn main() {}\n// TODO: fix\n").unwrap();
        std::fs::write(root.join("b.txt"), "todo later\n").unwrap();
        std::fs::write(root.join("target/c.rs"), "TODO hidden\n").unwrap();
        std::fs::write(root.join("bin.dat"), b"TODO\0\0").unwrap();
        let c = ToolContext {
            scope: Some(root.clone()),
            ..Default::default()
        };
        let out = run_tool(Search.prepare(&c, &json!({"pattern": "todo"})).unwrap()).content;
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(out.contains("b.txt:1: todo later"));
        assert!(out.contains(&format!(
            "src{}a.rs:2: // TODO: fix",
            std::path::MAIN_SEPARATOR
        )));
        let none = run_tool(
            Search
                .prepare(
                    &c,
                    &json!({"pattern": "TODO", "case_sensitive": true, "path": "b.txt"}),
                )
                .unwrap(),
        );
        assert!(none.content.starts_with("No matches"));
        assert!(
            Search
                .prepare(&c, &json!({"pattern": "(", "regex": true}))
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
