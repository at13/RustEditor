//! View layer: soft wrap and the display map (UI-independent).
//!
//! Wrapping uses a monospace cell model ([`cell_width`]): the same function
//! decides row breaks and horizontal positions, so row counts are exact for
//! the renderer. Word breaks happen after whitespace; a word longer than a row
//! is split. Lines longer than [`HARD_WRAP_CHARS`] wrap at fixed character
//! counts so they cost O(1), not a scan of the whole line.

use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use re_core::{
    document::line_len_chars,
    editor::{LineEdit, VerticalLayout},
};
use ropey::Rope;

/// Lines longer than this wrap every `columns` chars (no word scan).
pub const HARD_WRAP_CHARS: usize = 64 * 1024;
const BLOCK: usize = 1024;

/// Display cells for `ch`: 2 for East Asian wide/fullwidth and emoji, 4 for
/// tab, otherwise 1.
pub fn cell_width(ch: char) -> usize {
    match ch {
        '\t' => 4,
        '\u{1100}'..='\u{115F}'
        | '\u{2E80}'..='\u{303E}'
        | '\u{3041}'..='\u{33FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{A000}'..='\u{A4CF}'
        | '\u{AC00}'..='\u{D7A3}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FE30}'..='\u{FE4F}'
        | '\u{FF00}'..='\u{FF60}'
        | '\u{FFE0}'..='\u{FFE6}'
        | '\u{1F300}'..='\u{1F64F}'
        | '\u{1F900}'..='\u{1F9FF}'
        | '\u{20000}'..='\u{3FFFD}' => 2,
        _ => 1,
    }
}

/// Start offsets (chars, relative to the line) of each wrapped segment of a
/// line. Always starts with `0`.
pub fn segment_starts(rope: &Rope, line: usize, columns: usize) -> Vec<usize> {
    let columns = columns.max(1);
    let len = line_len_chars(rope, line);
    if len <= columns / 2 {
        return vec![0];
    }
    if len > HARD_WRAP_CHARS {
        return (0..len.div_ceil(columns)).map(|k| k * columns).collect();
    }
    let chars = rope.line(line).chars().take(len);
    let mut starts = vec![0];
    let mut seg_start = 0; // char index where the current segment starts
    let mut cells = 0; // cells used by chars in [seg_start, i)
    let mut break_at = None; // char index just after the last whitespace
    let mut widths = Vec::with_capacity(len);
    for (i, ch) in chars.enumerate() {
        let width = cell_width(ch);
        widths.push(width);
        if cells + width > columns && i > seg_start {
            let next = match break_at {
                Some(b) if b > seg_start && b <= i => b,
                _ => i,
            };
            starts.push(next);
            // Cells already used by chars moved to the new segment.
            cells = widths[next..i].iter().sum();
            seg_start = next;
            break_at = None;
            // A word longer than a row still has to be split.
            while cells + width > columns && i > seg_start {
                let mut used = 0;
                let mut split = seg_start;
                while split < i && used + widths[split] <= columns {
                    used += widths[split];
                    split += 1;
                }
                let split = split.max(seg_start + 1);
                starts.push(split);
                cells = widths[split..i].iter().sum();
                seg_start = split;
            }
        }
        cells += width;
        if ch.is_whitespace() {
            break_at = Some(i + 1);
        }
    }
    starts
}

/// Number of display rows for a line.
pub fn row_count(rope: &Rope, line: usize, columns: usize) -> usize {
    let columns = columns.max(1);
    let len = line_len_chars(rope, line);
    if len <= columns / 2 {
        1
    } else if len > HARD_WRAP_CHARS {
        len.div_ceil(columns).max(1)
    } else {
        segment_starts(rope, line, columns).len()
    }
}

/// Char ranges (relative to the line) of each segment.
pub fn segments(rope: &Rope, line: usize, columns: usize) -> Vec<Range<usize>> {
    let starts = segment_starts(rope, line, columns);
    let len = line_len_chars(rope, line);
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| start..starts.get(i + 1).copied().unwrap_or(len))
        .collect()
}

/// Maps logical lines to display rows for one wrap width. Row counts are
/// kept per line with per-block sums, so row lookups are O(lines / 1024 +
/// 1024) and edits that do not change the line count are O(1024).
#[derive(Clone, Debug)]
pub struct DisplayMap {
    columns: usize,
    rows: Vec<u32>,
    blocks: Vec<u64>,
    total: u64,
}

impl DisplayMap {
    /// Builds the map. Returns `None` if `cancel` was set during the build.
    pub fn build(rope: &Rope, columns: usize, cancel: Option<&AtomicBool>) -> Option<Self> {
        let columns = columns.max(1);
        let lines = rope.len_lines();
        let mut rows = Vec::with_capacity(lines);
        for line in 0..lines {
            if line % 4096 == 0 && cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return None;
            }
            rows.push(row_count(rope, line, columns) as u32);
        }
        let mut map = Self {
            columns,
            rows,
            blocks: Vec::new(),
            total: 0,
        };
        map.recompute_blocks(0);
        Some(map)
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn line_count(&self) -> usize {
        self.rows.len()
    }

    pub fn total_rows(&self) -> usize {
        self.total as usize
    }

    pub fn rows_in(&self, line: usize) -> usize {
        self.rows.get(line).copied().unwrap_or(1) as usize
    }

    /// First display row of `line`.
    pub fn line_to_row(&self, line: usize) -> usize {
        let line = line.min(self.rows.len());
        let block = line / BLOCK;
        let before: u64 = self.blocks[..block.min(self.blocks.len())].iter().sum();
        let within: u64 = self.rows[block * BLOCK..line]
            .iter()
            .map(|&r| r as u64)
            .sum();
        (before + within) as usize
    }

    /// The line containing display `row` and the segment index within it.
    pub fn row_to_line(&self, row: usize) -> (usize, usize) {
        if self.rows.is_empty() {
            return (0, 0);
        }
        let mut remaining = (row as u64).min(self.total.saturating_sub(1));
        let mut line = 0;
        for &block in &self.blocks {
            if remaining < block {
                break;
            }
            remaining -= block;
            line += BLOCK;
        }
        while line < self.rows.len() {
            let rows = self.rows[line] as u64;
            if remaining < rows {
                return (line, remaining as usize);
            }
            remaining -= rows;
            line += 1;
        }
        let last = self.rows.len() - 1;
        (last, self.rows[last] as usize - 1)
    }

    /// Updates the map for an edit. `rope` is the post-edit text. Falls back
    /// to a full rebuild if the edit does not match the map's line count.
    pub fn apply(&mut self, rope: &Rope, edits: &[LineEdit]) {
        let expected: isize = edits
            .iter()
            .map(|e| e.new.len() as isize - e.old.len() as isize)
            .sum();
        let consistent = self.rows.len() as isize + expected == rope.len_lines() as isize
            && edits.iter().all(|e| e.old.end <= self.rows.len());
        if !consistent {
            if let Some(map) = Self::build(rope, self.columns, None) {
                *self = map;
            }
            return;
        }
        let mut first_dirty = usize::MAX;
        for edit in edits.iter().rev() {
            let counts: Vec<u32> = edit
                .new
                .clone()
                .map(|line| row_count(rope, line, self.columns) as u32)
                .collect();
            if counts.len() == edit.old.len() {
                self.rows[edit.old.clone()].copy_from_slice(&counts);
            } else {
                self.rows.splice(edit.old.clone(), counts);
            }
            first_dirty = first_dirty.min(edit.old.start);
        }
        let same_shape = edits.iter().all(|e| e.new.len() == e.old.len());
        if same_shape {
            for edit in edits {
                self.recompute_block_range(edit.old.start, edit.old.end.max(edit.old.start + 1));
            }
        } else if first_dirty != usize::MAX {
            self.recompute_blocks(first_dirty);
        }
    }

    fn recompute_block_range(&mut self, start: usize, end: usize) {
        for block in start / BLOCK..=(end.saturating_sub(1)) / BLOCK {
            if block >= self.blocks.len() {
                break;
            }
            let lo = block * BLOCK;
            let hi = ((block + 1) * BLOCK).min(self.rows.len());
            let sum: u64 = self.rows[lo..hi].iter().map(|&r| r as u64).sum();
            self.total = self.total - self.blocks[block] + sum;
            self.blocks[block] = sum;
        }
    }

    fn recompute_blocks(&mut self, from_line: usize) {
        let first = from_line / BLOCK;
        self.blocks.truncate(first);
        let mut line = first * BLOCK;
        while line < self.rows.len() {
            let hi = (line + BLOCK).min(self.rows.len());
            self.blocks
                .push(self.rows[line..hi].iter().map(|&r| r as u64).sum());
            line = hi;
        }
        self.total = self.blocks.iter().sum();
    }
}

/// Vertical motion over wrapped display rows.
pub struct WrapLayout<'a> {
    pub map: &'a DisplayMap,
}

impl WrapLayout<'_> {
    /// (line, segment index, segment ranges) for `pos`.
    fn locate(&self, rope: &Rope, pos: usize) -> (usize, usize, Vec<Range<usize>>) {
        let pos = pos.min(rope.len_chars());
        let line = rope.char_to_line(pos);
        let column = pos - rope.line_to_char(line);
        let segments = segments(rope, line, self.map.columns());
        let index = segments
            .iter()
            .rposition(|seg| seg.start <= column)
            .unwrap_or(0);
        (line, index, segments)
    }
}

impl VerticalLayout for WrapLayout<'_> {
    fn row(&self, rope: &Rope, pos: usize) -> usize {
        let (line, index, _) = self.locate(rope, pos);
        self.map.line_to_row(line) + index
    }

    fn column(&self, rope: &Rope, pos: usize) -> usize {
        let (line, index, segments) = self.locate(rope, pos);
        pos.min(rope.len_chars()) - rope.line_to_char(line) - segments[index].start
    }

    fn offset(&self, rope: &Rope, pos: usize, rows: isize, goal: usize) -> usize {
        let row = self.row(rope, pos) as isize + rows;
        if row < 0 {
            return 0;
        }
        if row as usize >= self.map.total_rows() {
            return rope.len_chars();
        }
        let (line, index) = self.map.row_to_line(row as usize);
        let segments = segments(rope, line, self.map.columns());
        let Some(segment) = segments.get(index) else {
            return rope.line_to_char(line) + line_len_chars(rope, line);
        };
        // Stay on this row: the end of a non-final segment is the start of
        // the next one, so stop one char short.
        let last = index + 1 == segments.len();
        let max = if last {
            segment.len()
        } else {
            segment.len().saturating_sub(1)
        };
        rope.line_to_char(line) + segment.start + goal.min(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use re_core::{document::Document, editor::EditorState};

    fn rope(text: &str) -> Rope {
        Rope::from_str(text)
    }

    #[test]
    fn wraps_after_whitespace_and_splits_long_words() {
        let r = rope("the quick brown fox\nshort\nabcdefghijklmnopqrstuvwxyz");
        assert_eq!(segment_starts(&r, 0, 10), vec![0, 10]); // "the quick " | "brown fox"
        assert_eq!(segment_starts(&r, 1, 10), vec![0]);
        assert_eq!(segment_starts(&r, 2, 10), vec![0, 10, 20]);
        let segs = segments(&r, 0, 10);
        assert_eq!(segs, vec![0..10, 10..19]);
        // Every segment fits in the width.
        let r = rope("aa bbbbbbbbbbbbbbbbbbbbbbbbb c dddd eeeeeeeeeee ff");
        for seg in segments(&r, 0, 8) {
            let cells: usize = r.slice(seg.clone()).chars().map(cell_width).sum();
            assert!(cells <= 8, "{seg:?}");
            assert!(!seg.is_empty());
        }
    }

    #[test]
    fn wide_characters_use_two_cells() {
        let r = rope("日本語のテキスト");
        assert_eq!(segment_starts(&r, 0, 6), vec![0, 3, 6]);
        assert_eq!(cell_width('😀'), 2);
        assert_eq!(cell_width('a'), 1);
    }

    #[test]
    fn huge_lines_hard_wrap_without_scanning() {
        let r = rope(&"x".repeat(HARD_WRAP_CHARS + 5));
        assert_eq!(row_count(&r, 0, 100), (HARD_WRAP_CHARS + 5).div_ceil(100));
        assert_eq!(segment_starts(&r, 0, 100)[3], 300);
    }

    #[test]
    fn row_counts_match_segments_for_random_text() {
        use re_core::document::line_len_chars;
        let pieces = ["a", "bb ", "日本", "😀", " ", "long_word_here", "\t"];
        let mut seed = 17u64;
        for _ in 0..300 {
            let mut text = String::new();
            for _ in 0..(seed % 40) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push_str(pieces[(seed >> 33) as usize % pieces.len()]);
            }
            let r = rope(&text);
            for cols in [1, 3, 8, 20] {
                let segs = segments(&r, 0, cols);
                assert_eq!(segs.len(), row_count(&r, 0, cols));
                assert_eq!(segs.last().unwrap().end, line_len_chars(&r, 0));
                for pair in segs.windows(2) {
                    assert_eq!(pair[0].end, pair[1].start);
                }
            }
        }
    }

    #[test]
    fn display_map_row_lookup_across_blocks() {
        let mut text = String::new();
        for i in 0..3000 {
            if i % 7 == 0 {
                text.push_str("word word word word word\n"); // 3 rows at 10 cols
            } else {
                text.push_str("x\n");
            }
        }
        let r = rope(&text);
        let map = DisplayMap::build(&r, 10, None).unwrap();
        let mut row = 0;
        for line in 0..r.len_lines() {
            assert_eq!(map.line_to_row(line), row);
            for seg in 0..map.rows_in(line) {
                assert_eq!(map.row_to_line(row + seg), (line, seg));
            }
            row += map.rows_in(line);
        }
        assert_eq!(map.total_rows(), row);
    }

    #[test]
    fn display_map_updates_incrementally_like_a_rebuild() {
        // > 2 blocks of lines so block sums are exercised.
        let text = "alpha beta gamma delta\n".repeat(2100);
        let mut d = Document::default();
        d.replace_all(&text);
        let mut map = DisplayMap::build(d.rope(), 12, None).unwrap();
        let mut e = EditorState::default();
        let mut seed = 5u64;
        for step in 0..60 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let pos = (seed >> 20) as usize % (d.rope().len_chars() + 1);
            e.set_cursor(&d, pos);
            if step % 3 == 0 {
                e.add_cursor(&d, (pos * 7) % (d.rope().len_chars() + 1));
            }
            let insert = ["x", "\n", "longer words here ", ""][step % 4];
            let outcome = if insert.is_empty() {
                e.backspace(&mut d)
            } else {
                e.insert(&mut d, insert)
            };
            if let Some(outcome) = outcome {
                map.apply(d.rope(), &outcome.lines);
            }
            if step % 6 == 5 {
                let rebuilt = DisplayMap::build(d.rope(), 12, None).unwrap();
                assert_eq!(map.rows, rebuilt.rows, "step {step}");
                assert_eq!(map.blocks, rebuilt.blocks, "step {step}");
                assert_eq!(map.total_rows(), rebuilt.total_rows());
            }
        }
    }

    #[test]
    fn build_can_be_cancelled() {
        let r = rope(&"line\n".repeat(10_000));
        let cancel = AtomicBool::new(true);
        assert!(DisplayMap::build(&r, 10, Some(&cancel)).is_none());
    }

    #[test]
    fn wrap_layout_moves_by_display_rows() {
        let mut d = Document::default();
        d.replace_all("the quick brown fox\nend");
        let map = DisplayMap::build(d.rope(), 10, None).unwrap();
        let layout = WrapLayout { map: &map };
        let r = d.rope();
        // pos 2 ("e" in "the") -> down -> row 1 col 2 => 12.
        assert_eq!(layout.row(r, 2), 0);
        assert_eq!(layout.offset(r, 2, 1, 2), 12);
        assert_eq!(layout.row(r, 12), 1);
        assert_eq!(layout.column(r, 12), 2);
        // Down again: line "end" (row 2), clamped to its length.
        assert_eq!(layout.offset(r, 12, 1, 8), 23);
        // Up from row 1 at a large goal stays on row 0 (before the break).
        assert_eq!(layout.offset(r, 12, -1, 50), 9);
        let mut e = EditorState::default();
        e.set_cursor(&d, 2);
        e.move_by_in(&d, &layout, re_core::editor::Motion::Down, false);
        assert_eq!(e.head(), 12);
    }
}
