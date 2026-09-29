//! Linear undo/redo history.
//!
//! Each undo step is a [`Group`] of entries applied atomically. Consecutive
//! small insertions that continue where the previous one ended, within
//! [`TYPING_GROUP_WINDOW`], merge into one group so undo removes a typed run
//! instead of one character at a time.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use ropey::Rope;

use crate::change::ChangeSet;

pub const TYPING_GROUP_WINDOW: Duration = Duration::from_secs(1);
const MAX_GROUPS: usize = 10_000;
/// Budget for text held by changes. Snapshots share structure with the live
/// rope and are not counted.
const MAX_HEAP_BYTES: usize = 64 * 1024 * 1024;

/// How an edit should be grouped for undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// A small typed insertion; may merge with the previous typed insertion.
    Typing,
    /// Any other edit; always its own undo step.
    Other,
}

pub(crate) enum Edit {
    Changes {
        forward: ChangeSet,
        inverse: ChangeSet,
    },
    /// Rope replacement (whole document, or a huge ranged edit). Rope clones
    /// are O(1) and share structure, so no text is copied.
    Snapshot {
        before: Rope,
        after: Rope,
        /// Start of the edited range for ranged edits; `None` means the whole
        /// document was replaced and line-anchored state is invalid.
        at: Option<usize>,
    },
}

impl Edit {
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Changes { forward, inverse } => forward.heap_bytes() + inverse.heap_bytes(),
            Self::Snapshot { .. } => 0,
        }
    }
}

pub(crate) struct Group {
    pub(crate) entries: Vec<Edit>,
    /// Document state ids before and after the whole group.
    pub(crate) before: u64,
    pub(crate) after: u64,
    /// Where each cursor's typed run ended (post-edit offsets), if the
    /// group is a typing run that later typing may extend.
    typing_end: Option<Vec<usize>>,
    last_edit: Instant,
}

impl Group {
    fn heap_bytes(&self) -> usize {
        self.entries.iter().map(Edit::heap_bytes).sum()
    }
}

#[derive(Default)]
pub(crate) struct History {
    undo: VecDeque<Group>,
    redo: Vec<Group>,
    heap_bytes: usize,
    /// While > 0, every record joins the newest group.
    open_groups: usize,
    joining: bool,
}

impl History {
    pub(crate) fn record(
        &mut self,
        edit: Edit,
        kind: EditKind,
        before: u64,
        after: u64,
        now: Instant,
    ) {
        for group in self.redo.drain(..) {
            self.heap_bytes = self.heap_bytes.saturating_sub(group.heap_bytes());
        }
        self.heap_bytes += edit.heap_bytes();
        let typing_end = typing_end(&edit, kind);

        if let Some(last) = self.undo.back_mut()
            && last.after == before
            && (self.joining
                || (typing_end.is_some()
                    && last.typing_end.is_some()
                    && typing_starts(&edit) == last.typing_end
                    && now.saturating_duration_since(last.last_edit) <= TYPING_GROUP_WINDOW))
        {
            last.entries.push(edit);
            last.after = after;
            last.last_edit = now;
            last.typing_end = if self.joining { None } else { typing_end };
            self.trim();
            return;
        }

        self.undo.push_back(Group {
            entries: vec![edit],
            before,
            after,
            typing_end: if self.open_groups > 0 {
                None
            } else {
                typing_end
            },
            last_edit: now,
        });
        if self.open_groups > 0 {
            self.joining = true;
        }
        self.trim();
    }

    /// Starts a compound edit: every record until the matching
    /// [`end_group`](Self::end_group) forms one undo step.
    pub(crate) fn begin_group(&mut self) {
        self.open_groups += 1;
    }

    pub(crate) fn end_group(&mut self) {
        self.open_groups = self.open_groups.saturating_sub(1);
        if self.open_groups == 0 {
            self.joining = false;
        }
    }

    /// Prevents the next edit from merging into the current group (e.g. after
    /// saving, so undo can return exactly to the saved state).
    pub(crate) fn seal(&mut self) {
        if let Some(last) = self.undo.back_mut() {
            last.typing_end = None;
        }
    }

    pub(crate) fn pop_undo(&mut self) -> Option<Group> {
        let group = self.undo.pop_back()?;
        self.heap_bytes = self.heap_bytes.saturating_sub(group.heap_bytes());
        Some(group)
    }

    pub(crate) fn pop_redo(&mut self) -> Option<Group> {
        let group = self.redo.pop()?;
        self.heap_bytes = self.heap_bytes.saturating_sub(group.heap_bytes());
        Some(group)
    }

    pub(crate) fn push_undo(&mut self, mut group: Group) {
        group.typing_end = None;
        self.heap_bytes += group.heap_bytes();
        self.undo.push_back(group);
    }

    pub(crate) fn push_redo(&mut self, group: Group) {
        self.heap_bytes += group.heap_bytes();
        self.redo.push(group);
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    fn trim(&mut self) {
        while self.undo.len() > 1
            && (self.undo.len() > MAX_GROUPS || self.heap_bytes > MAX_HEAP_BYTES)
        {
            if let Some(group) = self.undo.pop_front() {
                self.heap_bytes = self.heap_bytes.saturating_sub(group.heap_bytes());
            }
        }
    }
}

/// For typed pure insertions (one per cursor, no line breaks), returns where
/// each ends in post-edit offsets.
fn typing_end(edit: &Edit, kind: EditKind) -> Option<Vec<usize>> {
    if kind != EditKind::Typing {
        return None;
    }
    let Edit::Changes { forward, .. } = edit else {
        return None;
    };
    let mut shift = 0usize;
    let mut ends = Vec::with_capacity(forward.changes().len());
    for change in forward.changes() {
        if !change.range.is_empty() || change.insert.contains(['\r', '\n']) {
            return None;
        }
        let inserted = change.insert.chars().count();
        shift += inserted;
        ends.push(change.range.start + shift);
    }
    (!ends.is_empty()).then_some(ends)
}

/// Where each change starts (pre-edit offsets).
fn typing_starts(edit: &Edit) -> Option<Vec<usize>> {
    match edit {
        Edit::Changes { forward, .. } => {
            Some(forward.changes().iter().map(|c| c.range.start).collect())
        }
        Edit::Snapshot { .. } => None,
    }
}
