//! Multi-cursor selection model in char offsets. [`crate::editor::EditorState`]
//! holds one [`Selections`] per document.

use std::ops::Range;

use crate::change::{Assoc, ChangeSet};

/// A cursor (`anchor == head`) or a selected range. `head` is the moving end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Visual column to aim for on vertical moves; cleared by other motions.
    pub goal: Option<usize>,
}

impl Selection {
    pub fn cursor(pos: usize) -> Self {
        Self::new(pos, pos)
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal: None,
        }
    }

    pub fn with_goal(mut self, goal: Option<usize>) -> Self {
        self.goal = goal;
        self
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_cursor(&self) -> bool {
        self.anchor == self.head
    }

    fn map(self, changes: &ChangeSet) -> Self {
        if self.is_cursor() {
            // Cursors move past text inserted at them (typing behaviour).
            return Self::cursor(changes.map_pos(self.head, Assoc::After));
        }
        // Ranges do not grow to include text inserted at their edges.
        let forward = self.anchor <= self.head;
        let (start, end) = (self.range().start, self.range().end);
        let start = changes.map_pos(start, Assoc::After);
        let end = changes.map_pos(end, Assoc::Before).max(start);
        if forward {
            Self::new(start, end)
        } else {
            Self::new(end, start)
        }
    }
}

/// A non-empty, sorted set of non-overlapping selections with one primary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selections {
    ranges: Vec<Selection>,
    primary: usize,
}

impl Selections {
    pub fn single(selection: Selection) -> Self {
        Self {
            ranges: vec![selection],
            primary: 0,
        }
    }

    /// Builds a set, sorting and merging overlapping or touching selections.
    /// Returns `None` if `ranges` is empty.
    pub fn new(ranges: Vec<Selection>, primary: usize) -> Option<Self> {
        let primary_sel = *ranges.get(primary)?;
        let mut set = Self { ranges, primary: 0 };
        set.normalize(primary_sel);
        Some(set)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.ranges.iter()
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn primary(&self) -> Selection {
        self.ranges[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn as_slice(&self) -> &[Selection] {
        &self.ranges
    }

    /// Replaces every selection; `primary` indexes into `ranges`. Returns
    /// `false` (leaving `self` unchanged) if `ranges` is empty.
    pub fn set_all(&mut self, ranges: Vec<Selection>, primary: usize) -> bool {
        let Some(&primary_sel) = ranges.get(primary.min(ranges.len().saturating_sub(1))) else {
            return false;
        };
        self.ranges = ranges;
        self.normalize(primary_sel);
        true
    }

    /// Maps every selection through an edit, then re-normalizes.
    pub fn map(&self, changes: &ChangeSet) -> Self {
        let primary = self.primary().map(changes);
        let mut set = Self {
            ranges: self.ranges.iter().map(|sel| sel.map(changes)).collect(),
            primary: 0,
        };
        set.normalize(primary);
        set
    }

    /// Clamps every selection to a document of `len_chars` characters.
    pub fn clamp(&self, len_chars: usize) -> Self {
        let clamp =
            |sel: &Selection| Selection::new(sel.anchor.min(len_chars), sel.head.min(len_chars));
        let primary = clamp(&self.primary());
        let mut set = Self {
            ranges: self.ranges.iter().map(clamp).collect(),
            primary: 0,
        };
        set.normalize(primary);
        set
    }

    fn normalize(&mut self, primary: Selection) {
        self.ranges
            .sort_by_key(|sel| (sel.range().start, sel.range().end));
        let mut merged: Vec<Selection> = Vec::with_capacity(self.ranges.len());
        for sel in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if touches(last, &sel) => {
                    let start = last.range().start.min(sel.range().start);
                    let end = last.range().end.max(sel.range().end);
                    let forward = if sel.is_cursor() {
                        last.anchor <= last.head
                    } else {
                        sel.anchor <= sel.head
                    };
                    let goal = last.goal.or(sel.goal);
                    *last = if forward {
                        Selection::new(start, end)
                    } else {
                        Selection::new(end, start)
                    }
                    .with_goal(goal);
                }
                _ => merged.push(sel),
            }
        }
        self.ranges = merged;
        let target = primary.head;
        self.primary = self
            .ranges
            .iter()
            .position(|sel| {
                let range = sel.range();
                range.start <= target && target <= range.end
            })
            .unwrap_or(0);
    }
}

fn touches(a: &Selection, b: &Selection) -> bool {
    let (a, b) = (a.range(), b.range());
    // Overlapping ranges merge; two cursors at one spot merge; a cursor at
    // the edge of a range merges into it. Two ranges that only touch stay apart.
    b.start < a.end || a == b || (b.start == a.end && (a.is_empty() || b.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, ChangeSet};

    #[test]
    fn normalizes_sorts_and_merges_overlaps() {
        let set = Selections::new(
            vec![
                Selection::new(10, 12),
                Selection::cursor(3),
                Selection::new(1, 4),
                Selection::cursor(3),
                Selection::new(12, 14),
            ],
            0,
        )
        .unwrap();
        let ranges: Vec<_> = set.iter().map(Selection::range).collect();
        assert_eq!(ranges, vec![1..4, 10..12, 12..14]);
        assert_eq!(set.primary().range(), 10..12);
        assert!(Selections::new(Vec::new(), 0).is_none());
    }

    #[test]
    fn cursors_follow_typed_text_and_ranges_do_not_grow() {
        let set = Selections::new(vec![Selection::cursor(2), Selection::new(5, 8)], 0).unwrap();
        // Insert "😀" at each cursor/range start.
        let changes = ChangeSet::new([Change::new(2..2, "😀"), Change::new(5..5, "X")]).unwrap();
        let mapped = set.map(&changes);
        let got: Vec<_> = mapped.iter().copied().collect();
        assert_eq!(got, vec![Selection::cursor(3), Selection::new(7, 10)]);
        assert_eq!(mapped.primary(), Selection::cursor(3));
    }

    #[test]
    fn deleting_across_selections_merges_them() {
        let set = Selections::new(vec![Selection::cursor(2), Selection::new(9, 6)], 1).unwrap();
        let changes = ChangeSet::single(1..7, "").unwrap();
        let mapped = set.map(&changes);
        let got: Vec<_> = mapped.iter().copied().collect();
        assert_eq!(got, vec![Selection::new(3, 1)]);
        assert_eq!(mapped.len(), 1);
    }

    #[test]
    fn clamp_limits_to_document_length() {
        let set = Selections::new(vec![Selection::new(2, 50), Selection::cursor(80)], 0).unwrap();
        let clamped = set.clamp(10);
        let got: Vec<_> = clamped.iter().copied().collect();
        assert_eq!(got, vec![Selection::new(2, 10)]);
    }
}
