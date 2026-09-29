use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use ropey::Rope;

use crate::{
    change::ChangeSet,
    history::{Edit, EditKind, Group, History},
    text_io::{self, DiskState, Encoding, LineEnding, Loaded, WriteMode},
};

/// Saving was refused because the file changed on disk since it was loaded
/// or last saved. Save again with `force` to overwrite.
#[derive(Debug)]
pub struct SaveConflict;

impl std::fmt::Display for SaveConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the file changed on disk since it was opened")
    }
}

impl std::error::Error for SaveConflict {}

/// How the file on disk compares to what the document last read or wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskStatus {
    /// No file (untitled) or unchanged.
    Unchanged,
    /// Another program changed the file.
    Modified,
    /// The file was deleted or moved.
    Missing,
}

/// Ranged edits at least this large (chars removed + inserted) are recorded
/// as O(1) rope snapshots instead of copying the text into undo history.
pub const SNAPSHOT_EDIT_CHARS: usize = 256 * 1024;

/// What an undo or redo changed, so the UI can move the cursor and adjust
/// line-anchored state such as bookmarks.
#[derive(Debug, PartialEq, Eq)]
pub struct Applied {
    /// Char offset at the (first) edited place.
    pub cursor: usize,
    /// `Some((first_line, line_delta))`: lines after `first_line` shifted by
    /// `line_delta`. `None`: the whole document was replaced, so
    /// line-anchored state is invalid.
    pub lines: Option<(usize, isize)>,
}

pub struct LineWindow {
    pub text: String,
    pub total_chars: usize,
    pub start_char: usize,
    pub end_char: usize,
}

pub struct Document {
    path: Option<PathBuf>,
    rope: Rope,
    /// The file is not writable (read-only attribute/permission). No lock is
    /// held: other programs can read and write the file; changes are
    /// detected through `disk` instead.
    read_only: bool,
    encoding: Encoding,
    line_ending: LineEnding,
    /// Loaded text had both LF and CRLF line breaks.
    mixed_line_endings: bool,
    /// File state when last read or written.
    disk: Option<DiskState>,
    /// Encoding or line-ending setting changed since the last save.
    meta_dirty: bool,
    revision: u64,
    history: History,
    /// Identifies the current text for dirty tracking; every new edit gets a
    /// fresh id, and undo/redo restore earlier ids.
    state: u64,
    saved_state: u64,
    next_state: u64,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            path: None,
            rope: Rope::new(),
            read_only: false,
            encoding: Encoding::Utf8,
            line_ending: LineEnding::native(),
            mixed_line_endings: false,
            disk: None,
            meta_dirty: false,
            revision: 0,
            history: History::default(),
            state: 0,
            saved_state: 0,
            next_state: 1,
        }
    }
}

impl Document {
    /// Reads and decodes a file (see [`text_io::load`]). No lock is taken.
    pub fn open(path: &Path) -> Result<Self> {
        let loaded = text_io::load(path)?;
        let mut document = Self {
            path: Some(path.to_path_buf()),
            ..Self::default()
        };
        document.install(loaded);
        Ok(document)
    }

    fn install(&mut self, loaded: Loaded) {
        self.rope = loaded.rope;
        self.encoding = loaded.encoding;
        self.line_ending = loaded
            .line_endings
            .dominant()
            .unwrap_or_else(LineEnding::native);
        self.mixed_line_endings = loaded.line_endings.is_mixed();
        self.read_only = loaded.read_only;
        self.disk = Some(loaded.disk);
        self.meta_dirty = false;
    }

    /// Replaces the text with a fresh read of the file (undoable) and marks
    /// the document saved. Only if `expected_revision` still matches.
    pub fn reload_if_revision(&mut self, loaded: Loaded, expected_revision: u64) -> bool {
        if self.revision != expected_revision {
            return false;
        }
        let before = self.rope.clone();
        self.install(loaded);
        if self.rope != before {
            self.record_snapshot(before);
        } else {
            self.revision += 1;
        }
        self.mark_saved();
        true
    }

    /// Compares the file on disk with the last read or write.
    pub fn disk_status(&self) -> DiskStatus {
        let (Some(path), Some(known)) = (&self.path, self.disk) else {
            return DiskStatus::Unchanged;
        };
        match DiskState::of(path) {
            Ok(now) if now == known => DiskStatus::Unchanged,
            Ok(_) => DiskStatus::Modified,
            Err(_) => DiskStatus::Missing,
        }
    }

    /// Saves to the document's path (crash-safe, see
    /// [`text_io::atomic_write`]). Fails with [`SaveConflict`] if the file
    /// changed on disk, unless `force` is set.
    pub fn save_checked(&mut self, force: bool) -> Result<WriteMode> {
        let path = self.path.clone().context("no file is open; use Save As")?;
        if self.read_only {
            bail!("{} is read-only", path.display());
        }
        if !force && self.disk_status() == DiskStatus::Modified {
            return Err(SaveConflict.into());
        }
        self.write_to(&path)
    }

    /// Saves, refusing to overwrite changes made by other programs.
    pub fn save(&mut self) -> Result<()> {
        self.save_checked(false).map(|_| ())
    }

    /// Writes to `path` and makes it the document's file.
    pub fn save_as(&mut self, path: &Path) -> Result<()> {
        self.write_to(path)?;
        self.path = Some(path.to_path_buf());
        self.read_only = false;
        Ok(())
    }

    fn write_to(&mut self, path: &Path) -> Result<WriteMode> {
        let rope = &self.rope;
        let encoding = self.encoding;
        let mode = text_io::atomic_write(path, |out| text_io::encode(rope, encoding, out))?;
        self.disk = DiskState::of(path).ok();
        self.mark_saved();
        Ok(mode)
    }

    /// An empty untitled document whose new lines use `ending` (the default
    /// is the platform's native ending).
    pub fn with_line_ending(ending: LineEnding) -> Self {
        Self {
            line_ending: ending,
            ..Self::default()
        }
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Changes the encoding used when saving (marks the document modified).
    pub fn set_encoding(&mut self, encoding: Encoding) {
        if self.encoding != encoding && !self.read_only {
            self.encoding = encoding;
            self.meta_dirty = true;
        }
    }

    /// Line break inserted by Enter.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn has_mixed_line_endings(&self) -> bool {
        self.mixed_line_endings
    }

    /// Converts every line break to `ending` (one undo step) and uses it for
    /// new lines. Materializes nothing: the rope is rebuilt chunk by chunk.
    pub fn convert_line_endings(&mut self, ending: LineEnding) -> bool {
        if self.read_only {
            return false;
        }
        let converted = text_io::convert_line_endings(&self.rope, ending);
        self.line_ending = ending;
        self.mixed_line_endings = false;
        if converted != self.rope {
            let before = std::mem::replace(&mut self.rope, converted);
            self.record_snapshot(before);
        } else {
            self.meta_dirty = true;
        }
        true
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn replace_all(&mut self, text: &str) {
        if self.read_only || self.rope == text {
            return;
        }

        let before = std::mem::replace(&mut self.rope, Rope::from_str(text));
        self.record_snapshot(before);
    }

    pub fn line_len_chars(&self, line_index: usize) -> usize {
        line_len_chars(&self.rope, line_index)
    }

    pub fn line_window(
        &self,
        line_index: usize,
        start_char: usize,
        max_chars: usize,
    ) -> LineWindow {
        let line = self.rope.line(line_index);
        let total_chars = line.len_chars() - line_ending_chars(line);
        let start_char = start_char.min(total_chars);
        let end_char = start_char.saturating_add(max_chars).min(total_chars);

        LineWindow {
            text: line.slice(start_char..end_char).to_string(),
            total_chars,
            start_char,
            end_char,
        }
    }

    #[allow(dead_code)]
    pub fn replace_line(&mut self, line_index: usize, text: &str) {
        if self.read_only || line_index >= self.line_count() {
            return;
        }

        let start = self.rope.line_to_char(line_index);
        let raw_line = self.rope.line(line_index);
        let ending_chars = line_ending_chars(raw_line);
        let end = start + raw_line.len_chars() - ending_chars;

        if self.rope.slice(start..end) != text {
            self.commit_range(start..end, text, EditKind::Other);
        }
    }

    #[allow(dead_code)]
    pub fn replace_line_range(
        &mut self,
        line_index: usize,
        start_char: usize,
        end_char: usize,
        text: &str,
    ) -> bool {
        if self.read_only || line_index >= self.line_count() {
            return false;
        }

        let line_len = self.line_len_chars(line_index);
        if start_char > end_char || end_char > line_len {
            return false;
        }

        let line_start = self.rope.line_to_char(line_index);
        let start = line_start + start_char;
        let end = line_start + end_char;
        if self.rope.slice(start..end) == text {
            return false;
        }

        self.commit_range(start..end, text, EditKind::Other)
    }

    pub fn insert_line_below(&mut self, line_index: usize) {
        if self.read_only {
            return;
        }

        let insertion = if line_index + 1 < self.line_count() {
            self.rope.line_to_char(line_index + 1)
        } else {
            self.rope.len_chars()
        };
        let ending = self.line_ending.as_str();
        self.commit_range(insertion..insertion, ending, EditKind::Other);
    }

    #[allow(dead_code)]
    pub fn split_line(&mut self, line_index: usize, char_offset: usize) -> bool {
        if self.read_only
            || line_index >= self.line_count()
            || char_offset > self.line_len_chars(line_index)
        {
            return false;
        }

        let line = self.rope.line(line_index);
        let line_ending = if line.len_chars() >= 2
            && line.char(line.len_chars() - 2) == '\r'
            && line.char(line.len_chars() - 1) == '\n'
        {
            "\r\n"
        } else {
            "\n"
        };
        let insertion = self.rope.line_to_char(line_index) + char_offset;
        self.commit_range(insertion..insertion, line_ending, EditKind::Other)
    }

    pub fn delete_line(&mut self, line_index: usize) {
        if self.read_only || line_index >= self.line_count() {
            return;
        }

        let mut start = self.rope.line_to_char(line_index);
        let end = if line_index + 1 < self.line_count() {
            self.rope.line_to_char(line_index + 1)
        } else {
            self.rope.len_chars()
        };

        if line_index > 0 && line_index + 1 == self.line_count() {
            let previous_line = self.rope.line(line_index - 1);
            let previous_len = previous_line.len_chars();
            if previous_len > 0 && previous_line.char(previous_len - 1) == '\n' {
                start = start.saturating_sub(1);
                if previous_len > 1 && previous_line.char(previous_len - 2) == '\r' {
                    start = start.saturating_sub(1);
                }
            }
        }

        if start != end {
            self.commit_range(start..end, "", EditKind::Other);
        }
    }

    pub fn lines_text(&self, line_indices: &[usize]) -> String {
        let mut text = String::new();
        for &line_index in line_indices {
            if line_index < self.line_count() {
                for chunk in self.rope.line(line_index).chunks() {
                    text.push_str(chunk);
                }
            }
        }
        text
    }

    pub fn delete_lines(&mut self, line_indices: &[usize]) -> usize {
        if self.read_only {
            return 0;
        }

        let mut unique = line_indices
            .iter()
            .copied()
            .filter(|&line_index| line_index < self.line_count())
            .collect::<Vec<_>>();
        unique.sort_unstable();
        unique.dedup();
        let deleted = unique.len();
        // One undo step for the whole batch.
        self.history.begin_group();
        for line_index in unique.into_iter().rev() {
            self.delete_line(line_index);
        }
        self.history.end_group();
        deleted
    }

    /// Read access to the text (O(log n) queries; never copy it on the UI
    /// thread for normal operations).
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn search_snapshot(&self) -> (Rope, u64) {
        (self.rope.clone(), self.revision)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn text_range(&self, start_char: usize, end_char: usize) -> Option<String> {
        if start_char > end_char || end_char > self.rope.len_chars() {
            return None;
        }
        Some(self.rope.slice(start_char..end_char).to_string())
    }

    pub fn global_char_for_line_offset(
        &self,
        line_index: usize,
        line_char_offset: usize,
    ) -> Option<usize> {
        if line_index >= self.line_count() || line_char_offset > self.line_len_chars(line_index) {
            return None;
        }
        Some(self.rope.line_to_char(line_index) + line_char_offset)
    }

    pub fn char_location(&self, global_char: usize) -> Option<(usize, usize)> {
        if global_char > self.rope.len_chars() {
            return None;
        }
        let line_index = self.rope.char_to_line(global_char);
        Some((
            line_index,
            global_char.saturating_sub(self.rope.line_to_char(line_index)),
        ))
    }

    pub fn replace_global_range_if_revision(
        &mut self,
        start_char: usize,
        end_char: usize,
        text: &str,
        expected_revision: u64,
    ) -> bool {
        self.edit_global_range_if_revision(
            start_char,
            end_char,
            text,
            expected_revision,
            EditKind::Other,
        )
    }

    /// Like [`replace_global_range_if_revision`](Self::replace_global_range_if_revision)
    /// with an explicit undo grouping kind (use [`EditKind::Typing`] for
    /// characters typed by the user).
    pub fn edit_global_range_if_revision(
        &mut self,
        start_char: usize,
        end_char: usize,
        text: &str,
        expected_revision: u64,
        kind: EditKind,
    ) -> bool {
        if self.read_only
            || self.revision != expected_revision
            || start_char > end_char
            || end_char > self.rope.len_chars()
        {
            return false;
        }

        self.commit_range(start_char..end_char, text, kind)
    }

    pub fn replace_rope_if_revision(&mut self, rope: Rope, expected_revision: u64) -> bool {
        if self.read_only || self.revision != expected_revision {
            return false;
        }

        let before = std::mem::replace(&mut self.rope, rope);
        self.record_snapshot(before);
        true
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned())
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_dirty(&self) -> bool {
        self.state != self.saved_state || self.meta_dirty
    }

    pub fn can_undo(&self) -> bool {
        !self.read_only && self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        !self.read_only && self.history.can_redo()
    }

    /// Reverts the most recent undo step. Returns `None` if there is nothing
    /// to undo or the document is read-only.
    pub fn undo(&mut self) -> Option<Applied> {
        if self.read_only {
            return None;
        }
        let group = self.history.pop_undo()?;
        let applied = self.apply_group(&group, false)?;
        self.history.push_redo(group);
        Some(applied)
    }

    /// Re-applies the most recently undone step.
    pub fn redo(&mut self) -> Option<Applied> {
        if self.read_only {
            return None;
        }
        let group = self.history.pop_redo()?;
        let applied = self.apply_group(&group, true)?;
        self.history.push_undo(group);
        Some(applied)
    }

    /// Applies a change set as one edit: one undo step, unless `kind` is
    /// [`EditKind::Typing`] and it continues the previous typed run.
    pub fn apply_changes(&mut self, changes: &ChangeSet, kind: EditKind) -> bool {
        if self.read_only || changes.is_empty() {
            return false;
        }
        if changes.size_chars() >= SNAPSHOT_EDIT_CHARS {
            // Huge edit (e.g. select-all + delete on a large file): keep an
            // O(1) snapshot instead of copying the removed text.
            let before = self.rope.clone();
            if changes.apply_forward(&mut self.rope).is_err() {
                return false;
            }
            let at = changes.changes().first().map(|change| change.range.start);
            self.record_snapshot_at(before, at);
            return true;
        }
        let Ok(inverse) = changes.apply(&mut self.rope) else {
            return false;
        };
        let (before, after) = self.advance_state();
        self.history.record(
            Edit::Changes {
                forward: changes.clone(),
                inverse,
            },
            kind,
            before,
            after,
            Instant::now(),
        );
        true
    }

    fn commit_range(&mut self, range: Range<usize>, text: &str, kind: EditKind) -> bool {
        match ChangeSet::single(range, text) {
            Ok(changes) => self.apply_changes(&changes, kind),
            Err(_) => false,
        }
    }

    fn record_snapshot(&mut self, before: Rope) {
        self.record_snapshot_at(before, None);
    }

    fn record_snapshot_at(&mut self, before: Rope, at: Option<usize>) {
        let (before_state, after_state) = self.advance_state();
        self.history.record(
            Edit::Snapshot {
                before,
                after: self.rope.clone(),
                at,
            },
            EditKind::Other,
            before_state,
            after_state,
            Instant::now(),
        );
    }

    fn advance_state(&mut self) -> (u64, u64) {
        let before = self.state;
        self.state = self.next_state;
        self.next_state += 1;
        self.revision += 1;
        (before, self.state)
    }

    fn mark_saved(&mut self) {
        self.saved_state = self.state;
        self.meta_dirty = false;
        self.history.seal();
    }

    fn apply_group(&mut self, group: &Group, forward: bool) -> Option<Applied> {
        let lines_before = self.rope.len_lines();
        let mut whole_document = false;
        let mut cursor = 0;
        let mut first_char = usize::MAX;
        let mut apply = |entry: &Edit, rope: &mut Rope| -> bool {
            match entry {
                Edit::Changes {
                    forward: redo,
                    inverse,
                } => {
                    let changes = if forward { redo } else { inverse };
                    // History was recorded against these exact texts, so this
                    // fails only if the rope was mutated outside the history.
                    let Ok(applied) = changes.apply(rope) else {
                        return false;
                    };
                    // `applied` holds the new ranges in post-edit offsets.
                    if let Some(first) = applied.changes().first() {
                        cursor = first.range.end;
                        first_char = first_char.min(first.range.start);
                    }
                }
                Edit::Snapshot { before, after, at } => {
                    *rope = if forward { after } else { before }.clone();
                    match at {
                        Some(at) => {
                            cursor = *at;
                            first_char = first_char.min(*at);
                        }
                        None => whole_document = true,
                    }
                }
            }
            true
        };
        let ok = if forward {
            group
                .entries
                .iter()
                .all(|entry| apply(entry, &mut self.rope))
        } else {
            group
                .entries
                .iter()
                .rev()
                .all(|entry| apply(entry, &mut self.rope))
        };
        self.revision += 1;
        if !ok {
            // Unreachable while all edits go through the history; never panic
            // on user data, just drop history that no longer matches.
            self.history.clear();
            self.state = self.next_state;
            self.next_state += 1;
            return None;
        }
        self.state = if forward { group.after } else { group.before };
        let len = self.rope.len_chars();
        if whole_document {
            return Some(Applied {
                cursor: 0,
                lines: None,
            });
        }
        Some(Applied {
            cursor: cursor.min(len),
            lines: Some((
                self.rope.char_to_line(first_char.min(len)),
                self.rope.len_lines() as isize - lines_before as isize,
            )),
        })
    }
}

/// Length of `line_index` in chars, excluding its `\n` or `\r\n` ending.
pub fn line_len_chars(rope: &Rope, line_index: usize) -> usize {
    let line = rope.line(line_index);
    line.len_chars() - line_ending_chars(line)
}

fn line_ending_chars(line: ropey::RopeSlice<'_>) -> usize {
    let len = line.len_chars();
    match len {
        0 => 0,
        _ if line.char(len - 1) != '\n' => 0,
        1 => 1,
        _ if line.char(len - 2) == '\r' => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Applied, DiskStatus, Document, LineEnding, SNAPSHOT_EDIT_CHARS, SaveConflict};
    use crate::text_io::Encoding;
    use crate::{
        change::{
            ChangeSet,
            tests::{Rng, random_change_set},
        },
        history::EditKind,
    };

    fn temp_file(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("rusteditor-{name}-{nonce}.txt"))
    }

    #[test]
    fn edits_and_saves_without_building_a_full_output_string() {
        let path = temp_file("save");
        fs::write(&path, "alpha\nbeta\ngamma").expect("test file should be created");

        {
            let mut document = Document::open(&path).expect("file should open");
            document.replace_line(1, "changed");
            document.insert_line_below(1);
            document.replace_line(2, "inserted");
            document.save().expect("file should save");
        }

        assert_eq!(
            fs::read_to_string(&path).expect("saved file should be readable"),
            "alpha\nchanged\ninserted\ngamma"
        );
        fs::remove_file(path).expect("test file should be removed");
    }

    #[test]
    fn deleting_the_last_line_removes_its_line_separator() {
        let mut document = Document {
            rope: ropey::Rope::from_str("first\r\nsecond"),
            ..Document::default()
        };

        document.delete_line(1);

        assert_eq!(document.rope.to_string(), "first");
        assert_eq!(document.line_count(), 1);
    }

    #[test]
    fn splitting_a_line_moves_the_remaining_text_to_the_next_line() {
        let mut document = Document {
            rope: ropey::Rope::from_str("alpha beta\r\nnext"),
            ..Document::default()
        };

        assert!(document.split_line(0, 5));

        assert_eq!(document.text(), "alpha\r\n beta\r\nnext");
        assert_eq!(document.line_count(), 3);
    }

    #[test]
    fn bookmarked_lines_can_be_copied_and_deleted_as_a_batch() {
        let mut document = Document {
            rope: ropey::Rope::from_str("zero\r\none\r\ntwo\r\nthree"),
            ..Document::default()
        };

        assert_eq!(document.lines_text(&[1, 3]), "one\r\nthree");
        assert_eq!(document.delete_lines(&[1, 3, 3]), 2);
        assert_eq!(document.text(), "zero\r\ntwo");
    }

    #[test]
    fn replacing_the_document_marks_it_dirty() {
        let mut document = Document::default();

        document.replace_all("one\ntwo");

        assert_eq!(document.text(), "one\ntwo");
        assert!(document.is_dirty());
    }

    #[test]
    fn windows_and_edits_long_lines_without_copying_the_whole_line() {
        let mut document = Document::default();
        let long_line = "a".repeat(100_000) + "日END\r\nnext";
        document.rope = ropey::Rope::from_str(&long_line);

        let window = document.line_window(0, 99_998, 8);
        assert_eq!(window.text, "aa日END");
        assert_eq!(window.total_chars, 100_004);
        assert_eq!(window.start_char, 99_998);
        assert_eq!(window.end_char, 100_004);

        assert!(document.replace_line_range(0, 100_000, 100_001, "OK"));
        assert_eq!(document.line_window(0, 99_998, 8).text, "aaOKEND");
        assert_eq!(document.line_window(1, 0, 8).text, "next");
    }

    #[test]
    fn stale_search_replacements_are_rejected() {
        let mut document = Document::default();
        document.replace_all("alpha beta");
        let (_, revision) = document.search_snapshot();
        document.replace_line(0, "changed");

        assert!(!document.replace_global_range_if_revision(0, 5, "ALPHA", revision));
        assert_eq!(document.text(), "changed");
    }

    fn typed(document: &mut Document, at: usize, text: &str) {
        let revision = document.revision();
        assert!(document.edit_global_range_if_revision(at, at, text, revision, EditKind::Typing));
    }

    #[test]
    fn consecutive_typing_is_one_undo_step_and_redo_restores_it() {
        let mut document = Document::default();
        typed(&mut document, 0, "h");
        typed(&mut document, 1, "i");
        typed(&mut document, 2, "😀");
        assert_eq!(document.text(), "hi😀");

        assert_eq!(
            document.undo(),
            Some(Applied {
                cursor: 0,
                lines: Some((0, 0))
            })
        );
        assert_eq!(document.text(), "");
        assert!(!document.is_dirty());
        assert!(document.undo().is_none());

        assert_eq!(
            document.redo(),
            Some(Applied {
                cursor: 3,
                lines: Some((0, 0))
            })
        );
        assert_eq!(document.text(), "hi😀");
        assert!(document.redo().is_none());
    }

    #[test]
    fn typing_elsewhere_or_non_typing_edits_start_new_undo_steps() {
        let mut document = Document::default();
        typed(&mut document, 0, "ab");
        typed(&mut document, 0, "X"); // not where the run ended
        document.replace_line(0, "line"); // not typing
        typed(&mut document, 4, "!");
        assert_eq!(document.text(), "line!");
        document.undo();
        assert_eq!(document.text(), "line");
        document.undo();
        assert_eq!(document.text(), "Xab");
        document.undo();
        assert_eq!(document.text(), "ab");
        document.undo();
        assert_eq!(document.text(), "");
    }

    #[test]
    fn a_new_edit_after_undo_discards_redo() {
        let mut document = Document::default();
        document.replace_all("one");
        document.replace_line(0, "two");
        document.undo();
        assert!(document.can_redo());
        document.replace_line(0, "three");
        assert!(!document.can_redo());
        assert!(document.redo().is_none());
        assert_eq!(document.text(), "three");
    }

    #[test]
    fn undo_reports_line_changes_for_bookmarks() {
        let mut document = Document::with_line_ending(LineEnding::Lf);
        document.replace_all("a\nb\nc");
        document.insert_line_below(0);
        assert_eq!(document.text(), "a\n\nb\nc");
        assert_eq!(
            document.undo(),
            Some(Applied {
                cursor: 2,
                lines: Some((1, -1))
            })
        );
        assert_eq!(document.text(), "a\nb\nc");
        assert_eq!(document.undo().unwrap().lines, None);
        assert_eq!(document.text(), "");
    }

    #[test]
    fn batch_line_deletion_is_one_undo_step() {
        let mut document = Document::default();
        document.replace_all("zero\r\none\r\ntwo\r\nthree");
        assert_eq!(document.delete_lines(&[1, 3]), 2);
        assert_eq!(document.text(), "zero\r\ntwo");
        document.undo();
        assert_eq!(document.text(), "zero\r\none\r\ntwo\r\nthree");
        document.redo();
        assert_eq!(document.text(), "zero\r\ntwo");
    }

    #[test]
    fn open_does_not_lock_and_other_programs_can_read_and_write() {
        let path = temp_file("nolock");
        fs::write(&path, "one\r\ntwo\r\n").unwrap();
        let mut document = Document::open(&path).unwrap();
        assert!(!document.is_read_only());
        assert_eq!(document.line_ending(), LineEnding::CrLf);
        // Another program can read and replace the file while it is open.
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\ntwo\r\n");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, "changed elsewhere\n").unwrap();
        assert_eq!(document.disk_status(), DiskStatus::Modified);
        // Saving refuses to overwrite the external change...
        document.insert_line_below(0);
        let error = document.save().unwrap_err();
        assert!(error.downcast_ref::<SaveConflict>().is_some(), "{error:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed elsewhere\n");
        // ...unless forced.
        document.save_checked(true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\n\r\ntwo\r\n");
        assert_eq!(document.disk_status(), DiskStatus::Unchanged);
        fs::remove_file(&path).unwrap();
        assert_eq!(document.disk_status(), DiskStatus::Missing);
    }

    #[test]
    fn reload_is_undoable_and_marks_the_document_saved() {
        let path = temp_file("reload");
        fs::write(&path, "original").unwrap();
        let mut document = Document::open(&path).unwrap();
        fs::write(&path, "from disk").unwrap();
        let loaded = crate::text_io::load(&path).unwrap();
        let revision = document.revision();
        assert!(document.reload_if_revision(loaded, revision));
        assert_eq!(document.text(), "from disk");
        assert!(!document.is_dirty());
        assert_eq!(document.disk_status(), DiskStatus::Unchanged);
        document.undo().unwrap();
        assert_eq!(document.text(), "original");
        assert!(document.is_dirty());
        // A stale reload (document changed meanwhile) is not applied.
        let loaded = crate::text_io::load(&path).unwrap();
        assert!(!document.reload_if_revision(loaded, revision));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn encoding_is_kept_on_save_and_can_be_changed() {
        let path = temp_file("encoding");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "hé😀\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        let mut document = Document::open(&path).unwrap();
        assert_eq!(document.encoding(), Encoding::Utf16Le);
        assert_eq!(document.text(), "hé😀\r\n");
        document.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        document.set_encoding(Encoding::Utf8);
        assert!(document.is_dirty());
        document.save().unwrap();
        assert!(!document.is_dirty());
        assert_eq!(fs::read(&path).unwrap(), "hé😀\r\n".as_bytes());
        // Unrepresentable text: the save fails and the file is untouched.
        document.set_encoding(Encoding::Windows1252);
        assert!(document.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), "hé😀\r\n".as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn line_endings_convert_as_one_undo_step() {
        let mut document = Document::default();
        document.replace_all("a\r\nb\nc");
        assert!(document.convert_line_endings(LineEnding::Lf));
        assert_eq!(document.text(), "a\nb\nc");
        assert_eq!(document.line_ending(), LineEnding::Lf);
        document.insert_line_below(0);
        assert_eq!(document.text(), "a\n\nb\nc");
        document.undo().unwrap();
        document.undo().unwrap();
        assert_eq!(document.text(), "a\r\nb\nc");
    }

    #[test]
    fn read_only_files_open_read_only() {
        let path = temp_file("readonly");
        fs::write(&path, "locked").unwrap();
        let writable = fs::metadata(&path).unwrap().permissions();
        let mut read_only = writable.clone();
        read_only.set_readonly(true);
        fs::set_permissions(&path, read_only).unwrap();
        let mut document = Document::open(&path).unwrap();
        assert!(document.is_read_only());
        assert!(document.save().is_err());
        fs::set_permissions(&path, writable).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dirty_tracks_the_saved_state_through_undo_and_redo() {
        let path = temp_file("undo-dirty");
        fs::write(&path, "base").unwrap();
        {
            let mut document = Document::open(&path).unwrap();
            typed(&mut document, 4, "1");
            document.save().unwrap();
            assert!(!document.is_dirty());
            // Saving seals the typed run, so this is a separate step.
            typed(&mut document, 5, "2");
            assert!(document.is_dirty());
            document.undo();
            assert_eq!(document.text(), "base1");
            assert!(!document.is_dirty());
            document.undo();
            assert_eq!(document.text(), "base");
            assert!(document.is_dirty());
            document.redo();
            assert!(!document.is_dirty());
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn background_rope_replacement_is_undoable() {
        let mut document = Document::default();
        document.replace_all("keep");
        let (mut rope, revision) = document.search_snapshot();
        rope.insert(4, " pasted");
        assert!(document.replace_rope_if_revision(rope, revision));
        assert_eq!(document.undo().unwrap().lines, None);
        assert_eq!(document.text(), "keep");
    }

    #[test]
    fn huge_ranged_edits_are_undoable_snapshots_without_copying_text() {
        let mut document = Document::default();
        let big = "0123456789\n".repeat(SNAPSHOT_EDIT_CHARS / 10);
        document.replace_all(&format!("head\n{big}tail"));
        let len = document.rope().len_chars();
        // Delete everything except "head\n" and "tail".
        let revision = document.revision();
        assert!(document.replace_global_range_if_revision(5, len - 4, "", revision));
        assert_eq!(document.text(), "head\ntail");
        let applied = document.undo().unwrap();
        assert_eq!(applied.cursor, 5);
        let (first_line, delta) = applied.lines.unwrap();
        assert_eq!(first_line, 1);
        assert_eq!(delta, (SNAPSHOT_EDIT_CHARS / 10) as isize);
        assert_eq!(document.rope().len_chars(), len);
        document.redo().unwrap();
        assert_eq!(document.text(), "head\ntail");
    }

    #[test]
    fn read_only_documents_cannot_undo() {
        let mut document = Document::default();
        document.replace_all("x");
        document.read_only = true;
        assert!(!document.can_undo());
        assert!(document.undo().is_none());
        assert_eq!(document.text(), "x");
    }

    #[test]
    fn undo_and_redo_bump_the_revision_so_stale_results_are_dropped() {
        let mut document = Document::default();
        document.replace_all("abc");
        let (_, revision) = document.search_snapshot();
        document.undo();
        assert!(!document.replace_global_range_if_revision(0, 0, "x", revision));
    }

    #[test]
    fn random_edit_sequences_undo_and_redo_exactly() {
        let mut rng = Rng::new(7);
        for _ in 0..200 {
            let mut document = Document::default();
            let mut states = vec![document.text()];
            for _ in 0..rng.below(12) + 1 {
                let len = document.rope.len_chars();
                let changes: ChangeSet = random_change_set(&mut rng, len);
                if document.apply_changes(&changes, EditKind::Other) {
                    states.push(document.text());
                }
            }
            for expected in states.iter().rev().skip(1) {
                document.undo().unwrap();
                assert_eq!(&document.text(), expected);
            }
            assert!(!document.is_dirty());
            for expected in states.iter().skip(1) {
                document.redo().unwrap();
                assert_eq!(&document.text(), expected);
            }
        }
    }
}
