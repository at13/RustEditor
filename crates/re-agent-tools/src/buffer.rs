//! `read_buffer` and `edit_buffer`: the open document, including unsaved
//! edits. Edits are applied by the host as one undo step after approval.

use re_agent::{
    Action, Approval, Prepared, Remember, Risk, Tool, ToolContext, ToolSpec,
    diff::{DiffKind, DiffLine, line_diff},
    tools::BufferEdit,
};
use ropey::Rope;
use serde_json::{Value, json};

use crate::{MAX_EDIT_BYTES, numbered};

pub struct ReadBuffer;

impl Tool for ReadBuffer {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_buffer".into(),
            description: "Read the open file, including unsaved edits. Returns numbered lines \
                          (`  12| text`); the numbers are not part of the text. Use start_line/end_line \
                          for large files."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line (1-based)."},
                    "end_line": {"type": "integer", "minimum": 1, "description": "Last line (inclusive)."}
                },
                "additionalProperties": false
            }),
            risk: Risk::Read,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let doc = ctx
            .doc
            .clone()
            .ok_or("the file is not open in the editor")?;
        let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        Ok(Prepared::Run(Action::Worker(Box::new(move |_| {
            let rope = &doc.rope;
            let total = rope.len_lines();
            let start = start.clamp(1, total.max(1));
            let end = end.unwrap_or(total).clamp(start, total.max(start));
            let lines: Vec<String> = (start - 1..end).map(|i| rope.line(i).to_string()).collect();
            let mut text = format!(
                "{} ({} lines{}; revision {})\n",
                doc.name,
                total,
                if doc.read_only { ", read-only" } else { "" },
                doc.revision
            );
            text.push_str(&numbered(lines.iter().map(String::as_str), start, total));
            re_agent::ToolOutput::ok(text)
        }))))
    }
}

pub struct EditBuffer;

/// Finds each edit's `old_text` exactly once in `text` and returns char
/// ranges. Plain `\n` in the model's text also matches `\r\n` in CRLF files.
pub fn locate(
    text: &str,
    edits: &[(String, String)],
    line_ending: &str,
) -> Result<Vec<BufferEdit>, String> {
    let mut located = Vec::new();
    for (index, (old, new)) in edits.iter().enumerate() {
        let n = index + 1;
        if old.is_empty() {
            if !text.is_empty() || edits.len() != 1 {
                return Err(format!(
                    "edit {n}: old_text is empty; it may only be empty to fill an empty file"
                ));
            }
            located.push(BufferEdit {
                range: 0..0,
                old_text: String::new(),
                new_text: adapt(new, line_ending),
            });
            continue;
        }
        let mut candidates = vec![(old.clone(), new.clone())];
        if line_ending == "\r\n" && !old.contains("\r\n") && old.contains('\n') {
            candidates.push((adapt(old, "\r\n"), adapt(new, "\r\n")));
        }
        let mut found = None;
        for (old, new) in candidates {
            let count = text.matches(old.as_str()).take(2).count();
            if count > 1 {
                return Err(format!(
                    "edit {n}: old_text appears more than once; include more surrounding lines to make it unique"
                ));
            }
            if count == 1 {
                let byte = text.find(old.as_str()).expect("found");
                found = Some((byte, old, new));
                break;
            }
        }
        let Some((byte, old, new)) = found else {
            return Err(format!(
                "edit {n}: old_text was not found. Read the file again and copy the text exactly \
                 (without line numbers)"
            ));
        };
        let start = text[..byte].chars().count();
        located.push(BufferEdit {
            range: start..start + old.chars().count(),
            old_text: old,
            new_text: new,
        });
    }
    located.sort_by_key(|e| e.range.start);
    for pair in located.windows(2) {
        if pair[1].range.start < pair[0].range.end {
            return Err("two edits overlap; combine them into one".into());
        }
    }
    Ok(located)
}

/// Uses the document's line ending for new text.
fn adapt(text: &str, line_ending: &str) -> String {
    if line_ending == "\r\n" {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        text.to_owned()
    }
}

/// A diff of each edit with a few lines of context.
pub fn edits_diff(rope: &Rope, edits: &[BufferEdit]) -> Vec<DiffLine> {
    let mut out = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let first = rope.char_to_line(edit.range.start);
        let last = rope.char_to_line(edit.range.end.min(rope.len_chars()));
        let from = first.saturating_sub(2);
        let to = (last + 3).min(rope.len_lines());
        let window_start = rope.line_to_char(from);
        let window_end = rope.line_to_char(to);
        let old: String = rope.slice(window_start..window_end).to_string();
        let mut new = String::new();
        new.push_str(&rope.slice(window_start..edit.range.start).to_string());
        new.push_str(&edit.new_text);
        new.push_str(&rope.slice(edit.range.end..window_end).to_string());
        if i > 0 {
            out.push(DiffLine {
                kind: DiffKind::Skip,
                line: None,
                text: "...".into(),
            });
        }
        out.extend(line_diff(&old, &new, 2, from + 1));
    }
    out
}

impl Tool for EditBuffer {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_buffer".into(),
            description: "Propose changes to the open file. Each edit replaces `old_text` (copied exactly \
                          from the file, without line numbers, and unique in the file) with `new_text`. \
                          The user sees a diff and accepts or rejects it; accepted edits are one undo step."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string"},
                                "new_text": {"type": "string"}
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    },
                    "explanation": {"type": "string", "description": "One sentence shown to the user."}
                },
                "required": ["edits"],
                "additionalProperties": false
            }),
            risk: Risk::Write,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let doc = ctx
            .doc
            .as_ref()
            .ok_or("the file is not open in the editor")?;
        if doc.read_only {
            return Err("the file is read-only".into());
        }
        if doc.rope.len_bytes() > MAX_EDIT_BYTES {
            return Err("the file is too large for agent edits".into());
        }
        let edits: Vec<(String, String)> = args["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| {
                (
                    e["old_text"].as_str().unwrap_or_default().to_owned(),
                    e["new_text"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        if edits.is_empty() {
            return Err("no edits given".into());
        }
        let text = doc.rope.to_string();
        let located = locate(&text, &edits, doc.line_ending)?;
        if located.iter().all(|e| e.old_text == e.new_text) {
            return Err("the edits change nothing".into());
        }
        let diff = edits_diff(&doc.rope, &located);
        let explanation = args
            .get("explanation")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(Prepared::Ask(
            Approval {
                title: format!("Edit {}", doc.name),
                detail: explanation.to_owned(),
                diff,
                risk: Risk::Write,
                remember: Remember::Once,
            },
            Action::EditBuffer {
                revision: doc.revision,
                edits: located,
            },
        ))
    }
}

/// For the host: re-finds edits after the user changed the document (the
/// revision moved). Fails if any `old_text` is gone or ambiguous.
pub fn rebase(
    text: &str,
    edits: &[BufferEdit],
) -> Result<Vec<(std::ops::Range<usize>, String)>, String> {
    let pairs: Vec<(String, String)> = edits
        .iter()
        .map(|e| (e.old_text.clone(), e.new_text.clone()))
        .collect();
    locate(text, &pairs, "\n")
        .map(|located| located.into_iter().map(|e| (e.range, e.new_text)).collect())
        .map_err(|_| "the document changed since it was read; read it again".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::run;
    use re_agent::{DocSnapshot, ToolContext};

    fn ctx(text: &str, line_ending: &'static str) -> ToolContext {
        ToolContext {
            doc: Some(DocSnapshot {
                path: None,
                name: "a.rs".into(),
                rope: Rope::from_str(text),
                revision: 7,
                read_only: false,
                line_ending,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reads_numbered_line_ranges() {
        let c = ctx("one\ntwo\nthree\n", "\n");
        let out = run(ReadBuffer.prepare(&c, &json!({})).unwrap()).content;
        assert!(out.starts_with("a.rs (4 lines; revision 7)"), "{out}");
        assert!(out.contains("    2| two\n"));
        let out = run(ReadBuffer
            .prepare(&c, &json!({"start_line": 2, "end_line": 2}))
            .unwrap())
        .content;
        assert!(out.contains("    2| two") && !out.contains("one"));
        let out = run(ReadBuffer.prepare(&c, &json!({"start_line": 99})).unwrap()).content;
        assert!(out.contains("    4| "));
        assert!(
            ReadBuffer
                .prepare(&ToolContext::default(), &json!({}))
                .is_err()
        );
    }

    #[test]
    fn edits_need_unique_exact_text_and_show_a_diff() {
        let c = ctx("fn a() {}\nfn b() {}\nfn a() {}\n", "\n");
        let error = EditBuffer
            .prepare(
                &c,
                &json!({"edits": [{"old_text": "fn a() {}", "new_text": "x"}]}),
            )
            .unwrap_err();
        assert!(error.contains("more than once"));
        let error = EditBuffer
            .prepare(
                &c,
                &json!({"edits": [{"old_text": "fn z", "new_text": "x"}]}),
            )
            .unwrap_err();
        assert!(error.contains("not found"));
        let Prepared::Ask(approval, Action::EditBuffer { revision, edits }) = EditBuffer
            .prepare(&c, &json!({"edits": [{"old_text": "fn b() {}", "new_text": "fn b() { 1 }"}], "explanation": "why"}))
            .unwrap()
        else {
            panic!("ask");
        };
        assert_eq!(revision, 7);
        assert_eq!(edits[0].range, 10..19);
        assert_eq!(approval.detail, "why");
        assert_eq!(approval.risk, Risk::Write);
        assert!(
            approval
                .diff
                .iter()
                .any(|l| l.kind == DiffKind::Removed && l.text == "fn b() {}")
        );
        assert!(
            approval
                .diff
                .iter()
                .any(|l| l.kind == DiffKind::Added && l.text == "fn b() { 1 }")
        );
    }

    #[test]
    fn crlf_documents_accept_lf_edits_and_keep_crlf() {
        let c = ctx("a\r\nb\r\nc\r\n", "\r\n");
        let Prepared::Ask(_, Action::EditBuffer { edits, .. }) = EditBuffer
            .prepare(
                &c,
                &json!({"edits": [{"old_text": "a\nb", "new_text": "A\nB\nb2"}]}),
            )
            .unwrap()
        else {
            panic!("ask");
        };
        assert_eq!(edits[0].old_text, "a\r\nb");
        assert_eq!(edits[0].new_text, "A\r\nB\r\nb2");
    }

    #[test]
    fn overlapping_edits_and_rebasing() {
        let c = ctx("abcdef", "\n");
        assert!(EditBuffer
            .prepare(&c, &json!({"edits": [{"old_text": "abc", "new_text": "x"}, {"old_text": "cde", "new_text": "y"}]}))
            .unwrap_err()
            .contains("overlap"));
        let edits = vec![BufferEdit {
            range: 0..3,
            old_text: "abc".into(),
            new_text: "X".into(),
        }];
        assert_eq!(
            rebase("zzabcdef", &edits).unwrap(),
            vec![(2..5, "X".to_owned())]
        );
        assert!(
            rebase("nothing here", &edits)
                .unwrap_err()
                .contains("read it again")
        );
        // Empty file: insert.
        let empty = ctx("", "\n");
        assert!(
            EditBuffer
                .prepare(
                    &empty,
                    &json!({"edits": [{"old_text": "", "new_text": "hello"}]})
                )
                .is_ok()
        );
    }
}
