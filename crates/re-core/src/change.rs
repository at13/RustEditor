//! Atomic multi-range edits over a rope.
//!
//! A [`ChangeSet`] is a sorted list of non-overlapping replacements expressed
//! in Unicode **char** offsets of the document it applies to. Applying it
//! returns the inverse change set, which restores the original text exactly.

use std::ops::Range;

use ropey::Rope;

/// Replace `range` (char offsets in the pre-edit document) with `insert`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub range: Range<usize>,
    pub insert: String,
}

impl Change {
    pub fn new(range: Range<usize>, insert: impl Into<String>) -> Self {
        Self {
            range,
            insert: insert.into(),
        }
    }

    fn insert_chars(&self) -> usize {
        self.insert.chars().count()
    }
}

/// Which side a position sticks to when text is inserted exactly at it, or
/// when the range around it is replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assoc {
    Before,
    After,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChangeError {
    /// A range has `start > end`.
    InvalidRange,
    /// Two changes overlap, or two insertions share a position.
    Overlap,
    /// A range ends past the end of the document.
    OutOfBounds,
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRange => "change range start is after its end",
            Self::Overlap => "changes overlap",
            Self::OutOfBounds => "change range is outside the document",
        })
    }
}

impl std::error::Error for ChangeError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    changes: Vec<Change>,
}

impl ChangeSet {
    /// Builds a change set. Changes may be given in any order; no-op changes
    /// (empty range and empty insert) are dropped.
    pub fn new(changes: impl IntoIterator<Item = Change>) -> Result<Self, ChangeError> {
        let mut changes: Vec<Change> = changes.into_iter().collect();
        if changes
            .iter()
            .any(|change| change.range.start > change.range.end)
        {
            return Err(ChangeError::InvalidRange);
        }
        changes.retain(|change| !(change.range.is_empty() && change.insert.is_empty()));
        changes.sort_by_key(|change| (change.range.start, change.range.end));
        for pair in changes.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            // Adjacent is fine; overlap, or two changes at one point, is not.
            if b.range.start < a.range.end || b.range.start == a.range.start {
                return Err(ChangeError::Overlap);
            }
        }
        Ok(Self { changes })
    }

    pub fn single(range: Range<usize>, insert: impl Into<String>) -> Result<Self, ChangeError> {
        Self::new([Change::new(range, insert)])
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Approximate heap bytes held (inserted text).
    pub fn heap_bytes(&self) -> usize {
        self.changes.iter().map(|change| change.insert.len()).sum()
    }

    /// Applies the changes to `rope` and returns the inverse change set.
    /// On error the rope is left unchanged.
    pub fn apply(&self, rope: &mut Rope) -> Result<ChangeSet, ChangeError> {
        if self
            .changes
            .last()
            .is_some_and(|change| change.range.end > rope.len_chars())
        {
            return Err(ChangeError::OutOfBounds);
        }

        // Inverse ranges are in post-edit coordinates: shift each start by the
        // net length change of all earlier edits.
        let mut inverse = Vec::with_capacity(self.changes.len());
        let mut delta: isize = 0;
        for change in &self.changes {
            let removed = rope.slice(change.range.clone()).to_string();
            let new_start = change.range.start.wrapping_add_signed(delta);
            let inserted = change.insert_chars();
            inverse.push(Change::new(new_start..new_start + inserted, removed));
            delta += inserted as isize - change.range.len() as isize;
        }

        // Edit back to front so earlier offsets stay valid.
        for change in self.changes.iter().rev() {
            if !change.range.is_empty() {
                rope.remove(change.range.clone());
            }
            if !change.insert.is_empty() {
                rope.insert(change.range.start, &change.insert);
            }
        }
        Ok(ChangeSet { changes: inverse })
    }

    /// Applies the changes without computing the inverse (no removed text
    /// is copied). Used for huge edits that are undone via rope snapshots.
    pub fn apply_forward(&self, rope: &mut Rope) -> Result<(), ChangeError> {
        if self
            .changes
            .last()
            .is_some_and(|change| change.range.end > rope.len_chars())
        {
            return Err(ChangeError::OutOfBounds);
        }
        for change in self.changes.iter().rev() {
            if !change.range.is_empty() {
                rope.remove(change.range.clone());
            }
            if !change.insert.is_empty() {
                rope.insert(change.range.start, &change.insert);
            }
        }
        Ok(())
    }

    /// Total chars removed plus inserted; a cheap measure of edit size.
    pub fn size_chars(&self) -> usize {
        self.changes
            .iter()
            .map(|change| change.range.len() + change.insert.len())
            .sum()
    }

    /// Maps a char position in the pre-edit document to the post-edit one.
    pub fn map_pos(&self, pos: usize, assoc: Assoc) -> usize {
        let mut delta: isize = 0;
        for change in &self.changes {
            let inserted = change.insert_chars();
            let start = change.range.start.wrapping_add_signed(delta);
            if pos < change.range.start {
                break;
            }
            let at_point = pos == change.range.start && change.range.is_empty();
            if at_point || pos < change.range.end {
                return match assoc {
                    Assoc::Before => start,
                    Assoc::After => start + inserted,
                };
            }
            delta += inserted as isize - change.range.len() as isize;
        }
        pos.wrapping_add_signed(delta)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Small deterministic PRNG so property tests need no extra dependency.
    pub(crate) struct Rng(u64);

    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }
        pub(crate) fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        pub(crate) fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as usize
            }
        }
        pub(crate) fn text(&mut self) -> String {
            const PIECES: &[&str] = &["a", "é", "😀", "\n", "\r\n", "日本", "xyz", ""];
            (0..self.below(4))
                .map(|_| PIECES[self.below(PIECES.len())])
                .collect()
        }
    }

    pub(crate) fn random_change_set(rng: &mut Rng, len: usize) -> ChangeSet {
        let mut changes = Vec::new();
        let mut pos = 0;
        while pos <= len && changes.len() < 4 {
            let start = pos + rng.below(len - pos + 1);
            let end = start + rng.below(len - start + 1).min(3);
            changes.push(Change::new(start..end, rng.text()));
            // Leave a gap so insertions never share a position.
            pos = end + 1;
            if rng.below(3) == 0 {
                break;
            }
        }
        ChangeSet::new(changes).expect("generated changes are disjoint")
    }

    #[test]
    fn apply_then_inverse_restores_the_original_with_non_bmp_text() {
        let mut rng = Rng::new(0x5eed);
        for _ in 0..2_000 {
            let original =
                Rope::from_str(&(0..rng.below(6)).map(|_| rng.text()).collect::<String>());
            let changes = random_change_set(&mut rng, original.len_chars());
            let mut rope = original.clone();
            let inverse = changes.apply(&mut rope).unwrap();
            let redo = inverse.apply(&mut rope).unwrap();
            assert_eq!(rope, original);
            redo.apply(&mut rope).unwrap();
            let mut expected = original.clone();
            changes.apply(&mut expected).unwrap();
            assert_eq!(rope, expected);
        }
    }

    #[test]
    fn multi_range_changes_use_pre_edit_offsets() {
        let mut rope = Rope::from_str("a😀b\ncd");
        let changes = ChangeSet::new([
            Change::new(5..6, "DD"),
            Change::new(1..2, ""),
            Change::new(3..3, "\r\n"),
        ])
        .unwrap();
        let inverse = changes.apply(&mut rope).unwrap();
        assert_eq!(rope.to_string(), "ab\r\n\ncDD");
        inverse.apply(&mut rope).unwrap();
        assert_eq!(rope.to_string(), "a😀b\ncd");
    }

    #[test]
    fn rejects_invalid_overlapping_and_out_of_bounds_changes() {
        assert_eq!(
            ChangeSet::new([Change::new(1..3, ""), Change::new(2..4, "")]),
            Err(ChangeError::Overlap)
        );
        assert_eq!(
            ChangeSet::new([Change::new(1..1, "a"), Change::new(1..1, "b")]),
            Err(ChangeError::Overlap)
        );
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = Change::new(3..1, "");
        assert_eq!(ChangeSet::new([reversed]), Err(ChangeError::InvalidRange));
        let mut rope = Rope::from_str("ab");
        let changes = ChangeSet::single(1..5, "x").unwrap();
        assert_eq!(changes.apply(&mut rope), Err(ChangeError::OutOfBounds));
        assert_eq!(rope.to_string(), "ab");
        assert!(ChangeSet::single(1..1, "").unwrap().is_empty());
    }

    #[test]
    fn map_pos_respects_association() {
        // "hello world" -> insert "big " at 6, replace "hello" with "hi".
        let changes = ChangeSet::new([Change::new(6..6, "big "), Change::new(0..5, "hi")]).unwrap();
        assert_eq!(changes.map_pos(0, Assoc::Before), 0);
        assert_eq!(changes.map_pos(3, Assoc::Before), 0);
        assert_eq!(changes.map_pos(3, Assoc::After), 2);
        assert_eq!(changes.map_pos(5, Assoc::After), 2);
        assert_eq!(changes.map_pos(6, Assoc::Before), 3);
        assert_eq!(changes.map_pos(6, Assoc::After), 7);
        assert_eq!(changes.map_pos(11, Assoc::After), 12);
    }

    #[test]
    fn map_pos_agrees_with_applied_text() {
        // A position outside every change keeps pointing at the same char.
        let mut rng = Rng::new(42);
        for _ in 0..2_000 {
            let text: String = (0..rng.below(6)).map(|_| rng.text()).collect();
            let original = Rope::from_str(&text);
            let changes = random_change_set(&mut rng, original.len_chars());
            let mut rope = original.clone();
            changes.apply(&mut rope).unwrap();
            for pos in 0..original.len_chars() {
                let touched = changes.changes().iter().any(|c| {
                    c.range.contains(&pos) || (c.range.is_empty() && c.range.start == pos)
                });
                if !touched {
                    let mapped = changes.map_pos(pos, Assoc::After);
                    assert_eq!(rope.char(mapped), original.char(pos));
                }
            }
        }
    }
}
