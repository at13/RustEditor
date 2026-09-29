//! Whole-document editing state: multiple selections, cursor motion, edits.
//!
//! All positions are Unicode **char** offsets into the document rope. UTF-16
//! (used by platform IME APIs) appears only in the `*_utf16` helpers. A
//! cursor never sits between the `\r` and `\n` of a CRLF pair.
//!
//! Vertical motion goes through [`VerticalLayout`], so the same code moves by
//! logical lines ([`LogicalLines`]) or by wrapped display rows (provided by
//! the view layer).

use std::ops::Range;

use ropey::Rope;

use crate::{
    change::{Change, ChangeSet},
    document::{Document, line_len_chars},
    history::EditKind,
    selection::{Selection, Selections},
};

/// The largest text window handed to an IME in one query.
pub const IME_WINDOW_CHARS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    /// Move vertically by this many rows.
    PageUp(usize),
    PageDown(usize),
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

/// Maps positions to visual columns and moves between visual rows.
pub trait VerticalLayout {
    /// Visual row index of `pos` in the document.
    fn row(&self, rope: &Rope, pos: usize) -> usize;
    /// Visual column of `pos` within its row.
    fn column(&self, rope: &Rope, pos: usize) -> usize;
    /// Position `rows` rows away (negative = up) at visual column `goal`,
    /// clamped to the document. Never returns a position inside CRLF.
    fn offset(&self, rope: &Rope, pos: usize, rows: isize, goal: usize) -> usize;
}

/// One row per logical line (no wrapping).
pub struct LogicalLines;

impl VerticalLayout for LogicalLines {
    fn row(&self, rope: &Rope, pos: usize) -> usize {
        rope.char_to_line(pos)
    }

    fn column(&self, rope: &Rope, pos: usize) -> usize {
        pos - rope.line_to_char(rope.char_to_line(pos))
    }

    fn offset(&self, rope: &Rope, pos: usize, rows: isize, goal: usize) -> usize {
        let line = rope.char_to_line(pos) as isize + rows;
        if line < 0 {
            return 0;
        }
        let line = line as usize;
        if line >= rope.len_lines() {
            return rope.len_chars();
        }
        rope.line_to_char(line) + goal.min(line_len_chars(rope, line))
    }
}

/// Lines `old` (pre-edit numbering) were replaced by lines `new` (post-edit
/// numbering). Used to update line-anchored state (bookmarks, wrap map).
/// Half-open ranges. In an [`EditOutcome`] they are sorted and disjoint, so
/// splicing them in reverse order at `old.start` yields the new line array.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Result of an edit, for adjusting line-anchored UI state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditOutcome {
    /// Sorted, non-overlapping; apply in reverse order (see [`shift_lines`]).
    pub lines: Vec<LineEdit>,
}

impl EditOutcome {
    pub fn line_delta(&self) -> isize {
        self.lines
            .iter()
            .map(|e| e.new.len() as isize - e.old.len() as isize)
            .sum()
    }
}

/// Moves line-anchored marks (pre-edit line numbers) through `edits`. A mark
/// inside an edited span keeps its offset within the span, clamped to the
/// replacement's length.
pub fn shift_lines(marks: impl IntoIterator<Item = usize>, edits: &[LineEdit]) -> Vec<usize> {
    let mut out: Vec<usize> = marks
        .into_iter()
        .map(|mark| {
            // The last edit starting at or before the mark.
            let index = edits.partition_point(|edit| edit.old.start <= mark);
            let Some(edit) = index.checked_sub(1).map(|i| &edits[i]) else {
                return mark;
            };
            if mark < edit.old.end {
                let offset = mark - edit.old.start;
                edit.new.start + offset.min(edit.new.len().saturating_sub(1))
            } else {
                // `new` is post-edit, so this includes every earlier delta.
                mark.saturating_add(edit.new.end)
                    .saturating_sub(edit.old.end)
            }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorState {
    selections: Selections,
    /// IME composition range, if any (always with a single selection).
    marked: Option<Range<usize>>,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            selections: Selections::single(Selection::cursor(0)),
            marked: None,
        }
    }
}

impl EditorState {
    // ----- Primary selection accessors -----

    pub fn primary(&self) -> Selection {
        self.selections.primary()
    }

    pub fn head(&self) -> usize {
        self.primary().head
    }

    pub fn anchor(&self) -> usize {
        self.primary().anchor
    }

    /// The primary selected range (`start <= end`).
    pub fn range(&self) -> Range<usize> {
        self.primary().range()
    }

    /// Whether the primary selection is empty.
    pub fn is_empty(&self) -> bool {
        self.primary().is_cursor()
    }

    /// Whether every selection is empty.
    pub fn all_empty(&self) -> bool {
        self.selections.iter().all(Selection::is_cursor)
    }

    /// Whether the primary head is before its anchor.
    pub fn reversed(&self) -> bool {
        self.primary().head < self.primary().anchor
    }

    /// All selections, sorted by position.
    pub fn selections(&self) -> &[Selection] {
        self.selections.as_slice()
    }

    pub fn cursor_count(&self) -> usize {
        self.selections.len()
    }

    /// Index of the primary selection in [`selections`](Self::selections).
    pub fn primary_index(&self) -> usize {
        self.selections.primary_index()
    }

    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    pub fn unmark(&mut self) {
        self.marked = None;
    }

    // ----- Setting selections -----

    fn set_single(&mut self, selection: Selection) {
        self.selections = Selections::single(selection);
        self.marked = None;
    }

    /// Places a single cursor at `pos`.
    pub fn set_cursor(&mut self, doc: &Document, pos: usize) {
        self.set_single(Selection::cursor(snap(doc.rope(), pos)));
    }

    /// Selects `range` (single selection), with the head at `range.end`.
    pub fn select(&mut self, doc: &Document, range: Range<usize>) {
        let rope = doc.rope();
        self.set_single(Selection::new(
            snap(rope, range.start),
            snap(rope, range.end),
        ));
    }

    /// Moves the primary head to `pos` (single selection); keeps the anchor
    /// when `extend` is set.
    pub fn move_head_to(&mut self, doc: &Document, pos: usize, extend: bool) {
        let pos = snap(doc.rope(), pos);
        let anchor = if extend { self.anchor() } else { pos };
        self.set_single(Selection::new(anchor, pos));
    }

    pub fn select_all(&mut self, doc: &Document) {
        self.set_single(Selection::new(0, doc.rope().len_chars()));
    }

    /// Selects the word (or whitespace/punctuation run) around `pos`.
    pub fn select_word_at(&mut self, doc: &Document, pos: usize) {
        let rope = doc.rope();
        let pos = snap(rope, pos);
        let len = rope.len_chars();
        let class = if pos < len {
            char_class(rope.char(pos))
        } else if pos > 0 {
            char_class(rope.char(pos - 1))
        } else {
            return self.set_cursor(doc, 0);
        };
        let mut start = pos;
        while start > 0 && char_class(rope.char(start - 1)) == class && class != Class::Newline {
            start -= 1;
        }
        let mut end = pos;
        while end < len && char_class(rope.char(end)) == class && class != Class::Newline {
            end += 1;
        }
        self.select(doc, start..end);
    }

    /// Adds a cursor at `pos` and makes it primary (e.g. Alt+click).
    pub fn add_cursor(&mut self, doc: &Document, pos: usize) {
        let pos = snap(doc.rope(), pos);
        let mut all = self.selections().to_vec();
        all.push(Selection::cursor(pos));
        let primary = all.len() - 1;
        self.selections.set_all(all, primary);
        self.marked = None;
    }

    /// Adds a cursor one row above/below the primary, at its goal column.
    pub fn add_cursor_vertical(&mut self, doc: &Document, layout: &dyn VerticalLayout, down: bool) {
        let rope = doc.rope();
        let primary = self.primary();
        let goal = primary
            .goal
            .unwrap_or_else(|| layout.column(rope, primary.head));
        let pos = layout.offset(rope, primary.head, if down { 1 } else { -1 }, goal);
        if layout.row(rope, pos) == layout.row(rope, primary.head) {
            return; // first/last row
        }
        let mut all = self.selections().to_vec();
        all.push(Selection::cursor(pos).with_goal(Some(goal)));
        let primary = all.len() - 1;
        self.selections.set_all(all, primary);
        self.marked = None;
    }

    /// Collapses to the primary selection. Returns whether there were more.
    pub fn collapse_to_primary(&mut self) -> bool {
        let multiple = self.cursor_count() > 1;
        let primary = self.primary();
        self.set_single(primary);
        multiple
    }

    /// Keeps positions valid after the document changed underneath.
    pub fn clamp(&mut self, doc: &Document) {
        let rope = doc.rope();
        let primary = self.selections.primary_index();
        let all = self
            .selections()
            .iter()
            .map(|s| Selection::new(snap(rope, s.anchor), snap(rope, s.head)))
            .collect();
        self.selections.set_all(all, primary);
        let len = rope.len_chars();
        self.marked = self
            .marked
            .take()
            .map(|m| m.start.min(len)..m.end.min(len))
            .filter(|m| !m.is_empty());
    }

    // ----- Motion -----

    /// Moves every selection by logical lines.
    pub fn move_by(&mut self, doc: &Document, motion: Motion, extend: bool) {
        self.move_by_in(doc, &LogicalLines, motion, extend);
    }

    /// Moves every selection; vertical motion uses `layout`.
    pub fn move_by_in(
        &mut self,
        doc: &Document,
        layout: &dyn VerticalLayout,
        motion: Motion,
        extend: bool,
    ) {
        let rope = doc.rope();
        self.marked = None;
        let primary = self.selections.primary_index();
        let moved = self
            .selections()
            .iter()
            .map(|&sel| move_one(rope, doc, layout, sel, motion, extend))
            .collect();
        self.selections.set_all(moved, primary);
    }

    // ----- Editing -----

    /// Replaces each `(range, text)` (sorted, disjoint after merging) in one
    /// undoable edit and puts a cursor after each insertion (or selects the
    /// insertion when `select_inserted`). Returns `None` if nothing changed.
    fn edit_ranges(
        &mut self,
        doc: &mut Document,
        mut edits: Vec<(Range<usize>, String)>,
        select_inserted: bool,
    ) -> Option<EditOutcome> {
        let len = doc.rope().len_chars();
        if doc.is_read_only()
            || edits
                .iter()
                .any(|(range, _)| range.start > range.end || range.end > len)
        {
            return None;
        }
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        // Merge overlapping ranges (e.g. two cursors deleting the same char).
        let mut merged: Vec<(Range<usize>, String)> = Vec::with_capacity(edits.len());
        for (range, text) in edits {
            match merged.last_mut() {
                Some((last, last_text))
                    if range.start < last.end
                        || (range.start == last.start && !range.is_empty()) =>
                {
                    last.end = last.end.max(range.end);
                    last_text.push_str(&text);
                }
                Some((last, last_text)) if range.start == last.start => {
                    // Two insertions at one point: keep one.
                    if last_text.is_empty() {
                        *last_text = text;
                    }
                    last.end = last.end.max(range.end);
                }
                _ => merged.push((range, text)),
            }
        }
        merged.retain(|(range, text)| !(range.is_empty() && text.is_empty()));
        if merged.is_empty() {
            return None;
        }
        let rope = doc.rope();
        let old_lines: Vec<Range<usize>> = merged
            .iter()
            .map(|(range, _)| rope.char_to_line(range.start)..rope.char_to_line(range.end) + 1)
            .collect();
        let typing = merged
            .iter()
            .all(|(range, text)| range.is_empty() && text.chars().count() <= 8);
        let changes = ChangeSet::new(
            merged
                .iter()
                .map(|(range, text)| Change::new(range.clone(), text.as_str())),
        )
        .ok()?;
        let primary_start = self.range().start;
        let kind = if typing {
            EditKind::Typing
        } else {
            EditKind::Other
        };
        if !doc.apply_changes(&changes, kind) {
            return None;
        }
        // New positions: each change shifts later ones by its length delta.
        let rope = doc.rope();
        let mut shift: isize = 0;
        let mut selections = Vec::with_capacity(merged.len());
        let mut lines: Vec<LineEdit> = Vec::with_capacity(merged.len());
        let mut primary = merged.len() - 1;
        for (index, ((range, text), old)) in merged.iter().zip(old_lines).enumerate() {
            let start = range.start.wrapping_add_signed(shift);
            let end = start + text.chars().count();
            shift += text.chars().count() as isize - range.len() as isize;
            if range.start <= primary_start && primary_start <= range.end.max(range.start) {
                primary = index;
            }
            selections.push(if select_inserted {
                Selection::new(start, end)
            } else {
                Selection::cursor(end)
            });
            let new = rope.char_to_line(start)..rope.char_to_line(end) + 1;
            match lines.last_mut() {
                Some(last) if old.start < last.old.end => {
                    last.old.end = last.old.end.max(old.end);
                    last.new.end = last.new.end.max(new.end);
                }
                _ => lines.push(LineEdit { old, new }),
            }
        }
        self.selections.set_all(selections, primary);
        self.marked = None;
        Some(EditOutcome { lines })
    }

    /// Replaces `range` with `text` (single selection afterwards).
    pub fn replace_range(
        &mut self,
        doc: &mut Document,
        range: Range<usize>,
        text: &str,
    ) -> Option<EditOutcome> {
        self.edit_ranges(doc, vec![(range, text.to_owned())], false)
    }

    /// Replaces every selection (or the IME composition) with `text`.
    pub fn insert(&mut self, doc: &mut Document, text: &str) -> Option<EditOutcome> {
        if let Some(marked) = self.marked.clone() {
            return self.replace_range(doc, marked, text);
        }
        let edits = self
            .selections()
            .iter()
            .map(|sel| (sel.range(), text.to_owned()))
            .collect();
        self.edit_ranges(doc, edits, false)
    }

    /// Replaces selection `i` with `texts[i]`. Requires one text per
    /// selection; when `select_inserted`, the insertions stay selected.
    pub fn insert_each(
        &mut self,
        doc: &mut Document,
        texts: Vec<String>,
        select_inserted: bool,
    ) -> Option<EditOutcome> {
        if texts.len() != self.cursor_count() {
            return None;
        }
        let edits = self
            .selections()
            .iter()
            .zip(texts)
            .map(|(sel, text)| (sel.range(), text))
            .collect();
        self.edit_ranges(doc, edits, select_inserted)
    }

    /// Deletes each selection, or the character (CRLF pair) before each cursor.
    pub fn backspace(&mut self, doc: &mut Document) -> Option<EditOutcome> {
        let rope = doc.rope();
        let edits = self
            .selections()
            .iter()
            .map(|sel| {
                let range = if sel.is_cursor() {
                    prev_pos(rope, sel.head)..sel.head
                } else {
                    sel.range()
                };
                (range, String::new())
            })
            .collect();
        self.edit_ranges(doc, edits, false)
    }

    /// Deletes each selection, or the character (CRLF pair) after each cursor.
    pub fn delete_forward(&mut self, doc: &mut Document) -> Option<EditOutcome> {
        let rope = doc.rope();
        let edits = self
            .selections()
            .iter()
            .map(|sel| {
                let range = if sel.is_cursor() {
                    sel.head..next_pos(rope, sel.head)
                } else {
                    sel.range()
                };
                (range, String::new())
            })
            .collect();
        self.edit_ranges(doc, edits, false)
    }

    /// The primary selected text.
    pub fn selected_text(&self, doc: &Document) -> String {
        doc.rope().slice(self.range()).to_string()
    }

    /// Text of every selection, in order. Materializes the selections.
    pub fn selected_texts(&self, doc: &Document) -> Vec<String> {
        self.selections()
            .iter()
            .map(|sel| doc.rope().slice(sel.range()).to_string())
            .collect()
    }

    /// Deletes the primary cursor's line, including its line break.
    pub fn delete_line(&mut self, doc: &mut Document) -> Option<EditOutcome> {
        self.collapse_to_primary();
        let rope = doc.rope();
        let line = rope.char_to_line(self.head().min(rope.len_chars()));
        let mut start = rope.line_to_char(line);
        let end = if line + 1 < rope.len_lines() {
            rope.line_to_char(line + 1)
        } else {
            // Last line: remove the preceding line break instead.
            if start > 0 && rope.char(start - 1) == '\n' {
                start -= 1;
                if start > 0 && rope.char(start - 1) == '\r' {
                    start -= 1;
                }
            }
            rope.len_chars()
        };
        self.replace_range(doc, start..end, "")
    }

    /// Inserts an empty line below the primary cursor's line.
    pub fn insert_line_below(&mut self, doc: &mut Document) -> Option<EditOutcome> {
        self.collapse_to_primary();
        self.move_by(doc, Motion::LineEnd, false);
        let head = self.head();
        self.replace_range(doc, head..head, "\n")
    }

    // ----- IME (UTF-16) boundary -----

    pub fn to_utf16(&self, doc: &Document, range: Range<usize>) -> Range<usize> {
        let rope = doc.rope();
        let len = rope.len_chars();
        rope.char_to_utf16_cu(range.start.min(len))..rope.char_to_utf16_cu(range.end.min(len))
    }

    pub fn from_utf16(&self, doc: &Document, range: Range<usize>) -> Range<usize> {
        let rope = doc.rope();
        let len = rope.len_utf16_cu();
        let start = rope.utf16_cu_to_char(range.start.min(len));
        let end = rope.utf16_cu_to_char(range.end.min(len));
        start.min(end)..end.max(start)
    }

    /// Text for an IME query, limited to a window around the primary cursor.
    /// Returns the text and the UTF-16 range actually covered.
    pub fn text_for_utf16(&self, doc: &Document, range: Range<usize>) -> (String, Range<usize>) {
        let chars = self.from_utf16(doc, range);
        let head = self.head();
        let lo = head.saturating_sub(IME_WINDOW_CHARS / 2);
        let hi = head.saturating_add(IME_WINDOW_CHARS / 2);
        let start = chars.start.clamp(lo, hi);
        let end = chars.end.clamp(start, hi.max(start));
        let end = end.min(doc.rope().len_chars());
        let start = start.min(end);
        (
            doc.rope().slice(start..end).to_string(),
            self.to_utf16(doc, start..end),
        )
    }

    /// Applies an IME composition update at the primary selection: replaces
    /// `range` (UTF-16, or the current composition/selection) with `text`,
    /// marks it, and selects `selection` (UTF-16, relative to `text`).
    pub fn replace_and_mark(
        &mut self,
        doc: &mut Document,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
    ) -> Option<EditOutcome> {
        let range = match range {
            Some(range) => self.from_utf16(doc, range),
            None => self.marked.clone().unwrap_or_else(|| self.range()),
        };
        let start = range.start;
        let outcome = if range.is_empty() && text.is_empty() {
            self.set_cursor(doc, start);
            None
        } else {
            self.replace_range(doc, range, text)
        };
        if text.is_empty() {
            self.marked = None;
            return outcome;
        }
        let end = start + text.chars().count();
        if let Some(sel) = selection {
            let to_char = |units: usize| {
                let mut count = 0;
                let mut chars = 0;
                for ch in text.chars() {
                    if count >= units {
                        break;
                    }
                    count += ch.len_utf16();
                    chars += 1;
                }
                start + chars
            };
            self.selections =
                Selections::single(Selection::new(to_char(sel.start), to_char(sel.end)));
        }
        self.marked = Some(start..end);
        outcome
    }
}

/// Applies `motion` to one selection.
fn move_one(
    rope: &Rope,
    doc: &Document,
    layout: &dyn VerticalLayout,
    sel: Selection,
    motion: Motion,
    extend: bool,
) -> Selection {
    if !extend && !sel.is_cursor() {
        match motion {
            Motion::Left => return Selection::cursor(sel.range().start),
            Motion::Right => return Selection::cursor(sel.range().end),
            _ => {}
        }
    }
    let head = sel.head;
    let rows = match motion {
        Motion::Up => Some(-1),
        Motion::Down => Some(1),
        Motion::PageUp(n) => Some(-(n.max(1) as isize)),
        Motion::PageDown(n) => Some(n.max(1) as isize),
        _ => None,
    };
    let (new_head, goal) = match rows {
        Some(rows) => {
            let goal = sel.goal.unwrap_or_else(|| layout.column(rope, head));
            (layout.offset(rope, head, rows, goal), Some(goal))
        }
        None => {
            let pos = match motion {
                Motion::Left => prev_pos(rope, head),
                Motion::Right => next_pos(rope, head),
                Motion::WordLeft => word_left(rope, head),
                Motion::WordRight => word_right(rope, head),
                Motion::LineStart => rope.line_to_char(rope.char_to_line(head)),
                Motion::LineEnd => {
                    let line = rope.char_to_line(head);
                    rope.line_to_char(line) + doc.line_len_chars(line)
                }
                Motion::DocStart => 0,
                _ => rope.len_chars(),
            };
            (pos, None)
        }
    };
    let anchor = if extend { sel.anchor } else { new_head };
    Selection::new(anchor, new_head).with_goal(goal)
}

/// Moves `pos` into the document and off the middle of a CRLF pair.
fn snap(rope: &Rope, pos: usize) -> usize {
    let pos = pos.min(rope.len_chars());
    if pos > 0 && pos < rope.len_chars() && rope.char(pos - 1) == '\r' && rope.char(pos) == '\n' {
        pos - 1
    } else {
        pos
    }
}

fn prev_pos(rope: &Rope, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    if pos >= 2 && rope.char(pos - 1) == '\n' && rope.char(pos - 2) == '\r' {
        pos - 2
    } else {
        pos - 1
    }
}

fn next_pos(rope: &Rope, pos: usize) -> usize {
    let len = rope.len_chars();
    if pos >= len {
        return len;
    }
    if pos + 1 < len && rope.char(pos) == '\r' && rope.char(pos + 1) == '\n' {
        pos + 2
    } else {
        pos + 1
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Newline,
    Punct,
}

fn char_class(ch: char) -> Class {
    if ch == '\n' || ch == '\r' {
        Class::Newline
    } else if ch.is_whitespace() {
        Class::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Start of the previous word: skip spaces, then a run of one class. A line
/// break is its own stop.
fn word_left(rope: &Rope, pos: usize) -> usize {
    let mut pos = pos;
    while pos > 0 && char_class(rope.char(pos - 1)) == Class::Space {
        pos -= 1;
    }
    if pos == 0 {
        return 0;
    }
    let class = char_class(rope.char(pos - 1));
    if class == Class::Newline {
        return prev_pos(rope, pos);
    }
    while pos > 0 && char_class(rope.char(pos - 1)) == class {
        pos -= 1;
    }
    pos
}

/// End of the next word: skip spaces, then a run of one class.
fn word_right(rope: &Rope, pos: usize) -> usize {
    let len = rope.len_chars();
    let mut pos = pos;
    while pos < len && char_class(rope.char(pos)) == Class::Space {
        pos += 1;
    }
    if pos >= len {
        return len;
    }
    let class = char_class(rope.char(pos));
    if class == Class::Newline {
        return next_pos(rope, pos);
    }
    while pos < len && char_class(rope.char(pos)) == class {
        pos += 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        let mut doc = Document::default();
        doc.replace_all(text);
        doc
    }

    fn at(doc: &Document, pos: usize) -> EditorState {
        let mut editor = EditorState::default();
        editor.set_cursor(doc, pos);
        editor
    }

    #[test]
    fn horizontal_motion_treats_crlf_and_emoji_as_one_step() {
        let d = doc("a😀\r\nb");
        let mut e = at(&d, 0);
        e.move_by(&d, Motion::Right, false);
        assert_eq!(e.head(), 1);
        e.move_by(&d, Motion::Right, false);
        assert_eq!(e.head(), 2); // after the emoji (one char)
        e.move_by(&d, Motion::Right, false);
        assert_eq!(e.head(), 4); // skipped the whole CRLF
        e.move_by(&d, Motion::Left, false);
        assert_eq!(e.head(), 2);
        // Positions inside a CRLF snap to its start.
        e.set_cursor(&d, 3);
        assert_eq!(e.head(), 2);
    }

    #[test]
    fn vertical_motion_keeps_the_goal_column() {
        let d = doc("abcdef\nxy\nlonger line");
        let mut e = at(&d, 5);
        e.move_by(&d, Motion::Down, false);
        assert_eq!(e.head(), 9); // end of "xy"
        e.move_by(&d, Motion::Down, false);
        assert_eq!(e.head(), 15); // column 5 of "longer line"
        e.move_by(&d, Motion::Up, false);
        e.move_by(&d, Motion::Up, false);
        assert_eq!(e.head(), 5);
        e.move_by(&d, Motion::Up, false);
        assert_eq!(e.head(), 0);
        e.move_by(&d, Motion::PageDown(10), false);
        assert_eq!(e.head(), d.rope().len_chars());
        // Horizontal motion resets the goal.
        e.set_cursor(&d, 5);
        e.move_by(&d, Motion::Down, false);
        e.move_by(&d, Motion::Left, false);
        e.move_by(&d, Motion::Up, false);
        assert_eq!(e.head(), 1);
    }

    #[test]
    fn line_and_document_boundaries() {
        let d = doc("one\r\ntwo two\nthree");
        let mut e = at(&d, 7);
        e.move_by(&d, Motion::LineStart, false);
        assert_eq!(e.head(), 5);
        e.move_by(&d, Motion::LineEnd, false);
        assert_eq!(e.head(), 12);
        e.move_by(&d, Motion::DocStart, true);
        assert_eq!(e.range(), 0..12);
        e.move_by(&d, Motion::DocEnd, false);
        assert_eq!(e.head(), d.rope().len_chars());
        let mut first = at(&d, 1);
        first.move_by(&d, Motion::LineEnd, false);
        assert_eq!(first.head(), 3); // before the CRLF
    }

    #[test]
    fn word_motion_and_word_selection() {
        let d = doc("let foo_bar = 1;\nnext");
        let mut e = at(&d, 0);
        e.move_by(&d, Motion::WordRight, false);
        assert_eq!(e.head(), 3);
        e.move_by(&d, Motion::WordRight, false);
        assert_eq!(e.head(), 11);
        e.move_by(&d, Motion::WordRight, false);
        assert_eq!(e.head(), 13); // "="
        e.set_cursor(&d, 16);
        e.move_by(&d, Motion::WordRight, false);
        assert_eq!(e.head(), 17); // line break is its own stop
        e.move_by(&d, Motion::WordLeft, false);
        assert_eq!(e.head(), 16);
        e.move_by(&d, Motion::WordLeft, false);
        assert_eq!(e.head(), 15);
        e.select_word_at(&d, 6);
        assert_eq!(e.range(), 4..11);
    }

    #[test]
    fn selection_extends_across_lines_and_collapses() {
        let d = doc("ab\ncd\nef");
        let mut e = at(&d, 1);
        e.move_by(&d, Motion::Down, true);
        e.move_by(&d, Motion::Down, true);
        assert_eq!(e.range(), 1..7);
        assert!(!e.reversed());
        e.move_by(&d, Motion::Left, false);
        assert_eq!((e.head(), e.is_empty()), (1, true));
        e.move_by(&d, Motion::DocEnd, true);
        e.move_by(&d, Motion::Right, false);
        assert_eq!(e.head(), 8);
        e.move_by(&d, Motion::DocStart, true);
        assert!(e.reversed());
    }

    #[test]
    fn edits_join_lines_and_delete_crlf_as_a_unit() {
        let mut d = doc("ab\r\ncd");
        let mut e = at(&d, 4);
        let outcome = e.backspace(&mut d).unwrap();
        assert_eq!(d.text(), "abcd");
        assert_eq!(e.head(), 2);
        assert_eq!(outcome.line_delta(), -1);
        assert_eq!(
            outcome.lines,
            vec![LineEdit {
                old: 0..2,
                new: 0..1
            }]
        );
        e.insert(&mut d, "\r\n").unwrap();
        e.move_by(&d, Motion::Left, false);
        e.delete_forward(&mut d).unwrap();
        assert_eq!(d.text(), "abcd");
        assert!(at(&d, 0).backspace(&mut d).is_none());
        assert!(at(&d, 4).delete_forward(&mut d).is_none());
    }

    #[test]
    fn multi_line_selection_replace_is_one_undo_step() {
        let mut d = doc("one\ntwo\nthree");
        let mut e = EditorState::default();
        e.select(&d, 2..9);
        let outcome = e.insert(&mut d, "X").unwrap();
        assert_eq!(d.text(), "onXhree");
        assert_eq!(e.head(), 3);
        assert_eq!(
            outcome.lines,
            vec![LineEdit {
                old: 0..3,
                new: 0..1
            }]
        );
        d.undo().unwrap();
        assert_eq!(d.text(), "one\ntwo\nthree");
    }

    #[test]
    fn select_all_and_delete_a_large_document_is_undoable() {
        let text = "line of text\n".repeat(40_000);
        let mut d = doc(&text);
        let mut e = EditorState::default();
        e.select_all(&d);
        e.backspace(&mut d).unwrap();
        assert_eq!(d.text(), "");
        assert_eq!(d.undo().unwrap().lines, Some((0, 40_000)));
        assert_eq!(d.rope().len_chars(), text.chars().count());
    }

    #[test]
    fn read_only_documents_are_not_edited() {
        let mut d = Document::default();
        // A default document is writable; simulate read-only through a
        // locked file is covered in document tests. Here: no-op edits.
        let mut e = EditorState::default();
        assert!(e.replace_range(&mut d, 0..0, "").is_none());
        assert!(e.replace_range(&mut d, 0..5, "x").is_none());
    }

    #[test]
    fn utf16_mapping_uses_surrogate_pairs_for_non_bmp() {
        let d = doc("a😀\nb😀c");
        let e = at(&d, 0);
        assert_eq!(e.to_utf16(&d, 0..2), 0..3);
        assert_eq!(e.to_utf16(&d, 3..6), 4..8);
        assert_eq!(e.from_utf16(&d, 4..8), 3..6);
        let (text, actual) = e.text_for_utf16(&d, 1..7);
        assert_eq!(text, "😀\nb😀");
        assert_eq!(actual, 1..7);
        assert_eq!(e.from_utf16(&d, 0..2), 0..1);
    }

    #[test]
    fn ime_composition_replaces_marked_text_and_commits() {
        let mut d = doc("x\ny");
        let mut e = at(&d, 2);
        // Start composing "に" then update to "日本", selecting nothing.
        e.replace_and_mark(&mut d, None, "に", Some(1..1));
        assert_eq!(d.text(), "x\nにy");
        assert_eq!(e.marked(), Some(2..3));
        e.replace_and_mark(&mut d, None, "日本😀", Some(4..4));
        assert_eq!(d.text(), "x\n日本😀y");
        assert_eq!(e.marked(), Some(2..5));
        assert_eq!(e.head(), 5);
        // Commit replaces the composition.
        e.insert(&mut d, "日本");
        assert_eq!(d.text(), "x\n日本y");
        assert_eq!(e.marked(), None);
        assert_eq!(e.head(), 4);
    }

    #[test]
    fn delete_and_insert_lines() {
        let mut d = doc("one\r\ntwo\nthree");
        let mut e = at(&d, 6);
        e.delete_line(&mut d).unwrap();
        assert_eq!(d.text(), "one\r\nthree");
        assert_eq!(e.head(), 5);
        e.delete_line(&mut d).unwrap();
        assert_eq!(d.text(), "one");
        e.insert_line_below(&mut d).unwrap();
        assert_eq!((d.text().as_str(), e.head()), ("one\n", 4));
    }

    #[test]
    fn multi_cursor_typing_is_one_undo_step_and_merges_runs() {
        let mut d = doc("a\nb\nc");
        let mut e = at(&d, 1);
        e.add_cursor(&d, 3);
        e.add_cursor(&d, 5);
        assert_eq!(e.cursor_count(), 3);
        e.insert(&mut d, "x").unwrap();
        e.insert(&mut d, "😀").unwrap();
        assert_eq!(d.text(), "ax😀\nbx😀\ncx😀");
        let heads: Vec<_> = e.selections().iter().map(|s| s.head).collect();
        assert_eq!(heads, vec![3, 7, 11]);
        // Both keystrokes merged into one undo step across all cursors.
        d.undo().unwrap();
        assert_eq!(d.text(), "a\nb\nc");
    }

    #[test]
    fn multi_cursor_backspace_and_overlap_merging() {
        let mut d = doc("abc");
        let mut e = at(&d, 1);
        e.add_cursor(&d, 2);
        e.backspace(&mut d).unwrap();
        assert_eq!(d.text(), "c");
        assert_eq!(e.cursor_count(), 1); // both cursors now at 0: merged
        let mut d = doc("one two three");
        let mut e = EditorState::default();
        e.select(&d, 0..3);
        let mut all = e.selections().to_vec();
        all.push(Selection::new(4, 7));
        e.selections.set_all(all, 0);
        e.insert_each(&mut d, vec!["1".into(), "2".into()], true)
            .unwrap();
        assert_eq!(d.text(), "1 2 three");
        let ranges: Vec<_> = e.selections().iter().map(Selection::range).collect();
        assert_eq!(ranges, vec![0..1, 2..3]);
        assert_eq!(e.selected_texts(&d), vec!["1", "2"]);
    }

    #[test]
    fn multi_cursor_motion_and_add_cursor_vertical() {
        let d = doc("abcd\nxy\nlonger");
        let mut e = at(&d, 3);
        e.add_cursor_vertical(&d, &LogicalLines, true);
        e.add_cursor_vertical(&d, &LogicalLines, true);
        let heads: Vec<_> = e.selections().iter().map(|s| s.head).collect();
        // Goal column 3 kept even though "xy" is shorter.
        assert_eq!(heads, vec![3, 7, 11]);
        e.add_cursor_vertical(&d, &LogicalLines, true); // last line: no-op
        assert_eq!(e.cursor_count(), 3);
        e.move_by(&d, Motion::LineStart, true);
        let ranges: Vec<_> = e.selections().iter().map(Selection::range).collect();
        assert_eq!(ranges, vec![0..3, 5..7, 8..11]);
        assert!(e.collapse_to_primary());
        assert_eq!(e.head(), 8);
        assert!(!e.collapse_to_primary());
    }

    #[test]
    fn line_edits_describe_multi_range_changes_and_shift_marks() {
        let mut d = doc("l0\nl1\nl2\nl3\nl4");
        let mut e = at(&d, 2); // end of l0
        e.add_cursor(&d, 11); // end of l3
        let outcome = e.insert(&mut d, "\n").unwrap();
        assert_eq!(d.text(), "l0\n\nl1\nl2\nl3\n\nl4");
        assert_eq!(
            outcome.lines,
            vec![
                LineEdit {
                    old: 0..1,
                    new: 0..2
                },
                LineEdit {
                    old: 3..4,
                    new: 4..6
                },
            ]
        );
        assert_eq!(shift_lines([0, 1, 3, 4], &outcome.lines), vec![0, 2, 4, 6]);
        // Deleting lines 1..=2 (incl. breaks) merges marks inside.
        let lines = vec![LineEdit {
            old: 1..4,
            new: 1..2,
        }];
        assert_eq!(shift_lines([0, 1, 2, 3, 5], &lines), vec![0, 1, 3]);
    }

    #[test]
    fn random_multi_cursor_edits_report_consistent_line_edits() {
        use crate::change::tests::Rng;
        let mut rng = Rng::new(99);
        for _ in 0..500 {
            let text: String = (0..rng.below(8)).map(|_| rng.text()).collect();
            let mut d = doc(&text);
            let len = d.rope().len_chars();
            let mut e = at(&d, rng.below(len + 1));
            for _ in 0..rng.below(3) {
                e.add_cursor(&d, rng.below(len + 1));
            }
            let before: Vec<usize> = (0..d.rope().len_lines()).map(|l| l * 7).collect();
            let old_lines = d.rope().len_lines();
            let insert = rng.text();
            let Some(outcome) = (if rng.below(2) == 0 {
                e.insert(&mut d, &insert)
            } else {
                e.backspace(&mut d)
            }) else {
                continue;
            };
            // Splicing a per-line array in reverse gives the new line count.
            let mut rows = before.clone();
            for edit in outcome.lines.iter().rev() {
                rows.splice(edit.old.clone(), edit.new.clone().map(|_| 0));
            }
            assert_eq!(rows.len(), d.rope().len_lines(), "text {text:?}");
            assert_eq!(
                old_lines as isize + outcome.line_delta(),
                d.rope().len_lines() as isize
            );
            // Unedited lines keep their content after the splice.
            for (index, value) in rows.iter().enumerate() {
                if *value != 0 {
                    let old_line = value / 7;
                    let old_text = doc(&text).rope().line(old_line).to_string();
                    let new_text = d.rope().line(index).to_string();
                    let strip = |s: &str| s.trim_end_matches(['\r', '\n']).to_owned();
                    assert_eq!(strip(&old_text), strip(&new_text), "text {text:?}");
                }
            }
        }
    }

    #[test]
    fn clamp_keeps_state_valid_after_external_changes() {
        let mut d = doc("hello world");
        let mut e = EditorState::default();
        e.select(&d, 3..11);
        d.replace_all("hi");
        e.clamp(&d);
        assert_eq!(e.range(), 2..2);
    }
}
