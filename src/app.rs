use std::{
    collections::BTreeSet,
    ops::Range,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Align, Color32, FontFamily, FontId, Frame, Key, Layout, Margin, Panel, RichText,
    ScrollArea, Stroke, TextEdit, TextStyle, Vec2, containers::scroll_area::ScrollSource,
};
use rfd::FileDialog;

use crate::{
    ai::{AiRequest, ProviderKind, SourceDocument},
    document::Document,
    paste,
    search::{self, SearchMatch, SearchQuery, SearchResults},
    transforms::{self, CaseTransform},
};

const EDITOR_BG: Color32 = Color32::from_rgb(255, 255, 255);
const GUTTER_BG: Color32 = Color32::from_rgb(242, 242, 242);
const GUTTER_TEXT: Color32 = Color32::from_rgb(115, 115, 115);
const BORDER: Color32 = Color32::from_rgb(205, 205, 205);
const ACTIVE_LINE: Color32 = Color32::from_rgb(255, 252, 224);
const DOCUMENT_SELECTION: Color32 = Color32::from_rgb(205, 226, 255);
const ROW_HEIGHT: f32 = 22.0;
const INACTIVE_LINE_PREVIEW_CHARS: usize = 1_024;
const LONG_LINE_CHUNK_CHARS: usize = 4_096;
const MAX_INLINE_EDIT_CHARS: usize = 16_384;
const MAX_SEARCH_MATCHES: usize = 100_000;
const LARGE_PASTE_BYTES: usize = 1024 * 1024;
static NEXT_EDITOR_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq)]
enum TransformScope {
    Selection,
    Document,
}

struct JsonViewer {
    text: String,
    source: String,
}

enum SearchWorkerResult {
    Found {
        query: SearchQuery,
        revision: u64,
        results: SearchResults,
    },
    Replaced {
        revision: u64,
        rope: ropey::Rope,
        replacements: usize,
    },
}

struct SearchMessage {
    request_id: u64,
    result: std::result::Result<SearchWorkerResult, String>,
}

struct SearchPanelState {
    visible: bool,
    replace_visible: bool,
    request_focus: bool,
    query: String,
    replacement: String,
    case_sensitive: bool,
    regex: bool,
    matches: Vec<SearchMatch>,
    selected_match: Option<usize>,
    result_query: Option<SearchQuery>,
    result_revision: u64,
    truncated: bool,
    status: String,
    next_request_id: u64,
    active_request: Option<u64>,
    sender: Sender<SearchMessage>,
    receiver: Receiver<SearchMessage>,
}

impl Default for SearchPanelState {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            visible: false,
            replace_visible: false,
            request_focus: false,
            query: String::new(),
            replacement: String::new(),
            case_sensitive: false,
            regex: false,
            matches: Vec::new(),
            selected_match: None,
            result_query: None,
            result_revision: 0,
            truncated: false,
            status: String::new(),
            next_request_id: 1,
            active_request: None,
            sender,
            receiver,
        }
    }
}

struct PasteMessage {
    request_id: u64,
    revision: u64,
    result: std::result::Result<paste::PasteResult, String>,
}

struct PasteState {
    next_request_id: u64,
    active_request: Option<u64>,
    sender: Sender<PasteMessage>,
    receiver: Receiver<PasteMessage>,
}

impl Default for PasteState {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            next_request_id: 1,
            active_request: None,
            sender,
            receiver,
        }
    }
}

struct AiMessage {
    request_id: u64,
    result: std::result::Result<String, String>,
}

struct AiPanelState {
    visible: bool,
    request_focus: bool,
    provider: ProviderKind,
    question: String,
    response: String,
    status: String,
    next_request_id: u64,
    active_request: Option<u64>,
    sender: Sender<AiMessage>,
    receiver: Receiver<AiMessage>,
}

impl Default for AiPanelState {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            visible: false,
            request_focus: false,
            provider: ProviderKind::CopilotCli,
            question: String::new(),
            response: String::new(),
            status: String::new(),
            next_request_id: 1,
            active_request: None,
            sender,
            receiver,
        }
    }
}

#[derive(Default)]
struct CalculatorState {
    visible: bool,
    request_focus: bool,
    expression: String,
    result: String,
    status: String,
}

pub struct RustEditorApp {
    editor_instance_id: u64,
    document: Document,
    active_line: usize,
    line_buffer: String,
    active_line_total_chars: usize,
    active_line_chunk_start: usize,
    active_line_source_chars: usize,
    active_line_chunked: bool,
    selection: Option<Range<usize>>,
    document_selected: bool,
    transform_scope: TransformScope,
    json_viewer: Option<JsonViewer>,
    search: SearchPanelState,
    pending_search_selection: Option<Range<usize>>,
    pending_scroll_line: Option<usize>,
    paste: PasteState,
    ai: AiPanelState,
    calculator: CalculatorState,
    bookmarks: BTreeSet<usize>,
    status: String,
}

impl RustEditorApp {
    pub fn new(document: Document) -> Self {
        let mut app = Self {
            editor_instance_id: NEXT_EDITOR_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            document,
            active_line: 0,
            line_buffer: String::new(),
            active_line_total_chars: 0,
            active_line_chunk_start: 0,
            active_line_source_chars: 0,
            active_line_chunked: false,
            selection: None,
            document_selected: false,
            transform_scope: TransformScope::Selection,
            json_viewer: None,
            search: SearchPanelState::default(),
            pending_search_selection: None,
            pending_scroll_line: None,
            paste: PasteState::default(),
            ai: AiPanelState::default(),
            calculator: CalculatorState::default(),
            bookmarks: BTreeSet::new(),
            status: "Ready".to_owned(),
        };
        app.load_active_line();
        app.status = if app.document.is_read_only() {
            "Opened read-only: the file is locked or not writable".to_owned()
        } else {
            "File opened".to_owned()
        };
        app
    }

    pub fn blank() -> Self {
        let mut editor = Self::new(Document::default());
        editor.status = "Ready".to_owned();
        editor
    }

    pub fn save(&mut self) {
        self.commit_active_line();
        if self.document.path().is_none() {
            self.save_as();
            return;
        }

        match self.document.save() {
            Ok(()) => self.status = "File saved".to_owned(),
            Err(error) => self.status = format!("Save failed: {error:#}"),
        }
    }

    pub fn save_as(&mut self) {
        self.commit_active_line();
        let Some(path) = FileDialog::new().set_title("Save file").save_file() else {
            return;
        };

        match self.document.save_as(&path) {
            Ok(()) => self.status = "File saved".to_owned(),
            Err(error) => self.status = format!("Save failed: {error:#}"),
        }
    }

    fn commit_active_line(&mut self) {
        if self.document.is_read_only() {
            return;
        }

        if self.active_line_chunked {
            let replacement_chars = self.line_buffer.chars().count();
            if self.document.replace_line_range(
                self.active_line,
                self.active_line_chunk_start,
                self.active_line_chunk_start + self.active_line_source_chars,
                &self.line_buffer,
            ) {
                self.active_line_total_chars = self
                    .active_line_total_chars
                    .saturating_sub(self.active_line_source_chars)
                    + replacement_chars;
                self.active_line_source_chars = replacement_chars;
            }
        } else {
            self.document
                .replace_line(self.active_line, &self.line_buffer);
            self.active_line_total_chars = self.line_buffer.chars().count();
            self.active_line_source_chars = self.active_line_total_chars;
        }
    }

    fn activate_line(&mut self, line_index: usize) {
        self.document_selected = false;
        self.selection = None;
        if line_index == self.active_line {
            return;
        }
        self.commit_active_line();
        self.active_line = line_index;
        self.load_active_line();
        if self.active_line_chunked {
            self.status = format!(
                "Long line: editing {} characters at a time",
                LONG_LINE_CHUNK_CHARS
            );
        }
    }

    fn load_active_line(&mut self) {
        self.active_line_total_chars = self.document.line_len_chars(self.active_line);
        self.active_line_chunked = self.active_line_total_chars > MAX_INLINE_EDIT_CHARS;
        self.active_line_chunk_start = 0;
        self.load_active_line_buffer();
    }

    fn load_active_line_buffer(&mut self) {
        let max_chars = if self.active_line_chunked {
            LONG_LINE_CHUNK_CHARS
        } else {
            MAX_INLINE_EDIT_CHARS
        };
        let window =
            self.document
                .line_window(self.active_line, self.active_line_chunk_start, max_chars);
        self.line_buffer = window.text;
        self.active_line_total_chars = window.total_chars;
        self.active_line_chunk_start = window.start_char;
        self.active_line_source_chars = window.end_char - window.start_char;
        self.selection = None;
        self.document_selected = false;
    }

    fn active_page_is_fully_selected(&self) -> bool {
        self.selection.as_ref() == Some(&(0..self.line_buffer.chars().count()))
    }

    fn select_entire_document(&mut self, ctx: &egui::Context) {
        self.commit_active_line();
        self.document_selected = true;
        self.selection = None;
        self.transform_scope = TransformScope::Document;
        ctx.memory_mut(|memory| memory.surrender_focus(self.line_editor_id()));
        self.status = format!(
            "Entire document selected ({} lines)",
            self.document.line_count()
        );
    }

    fn copy_entire_document(&mut self, ctx: &egui::Context) {
        self.commit_active_line();
        ctx.copy_text(self.document.text());
        self.status = format!(
            "Copied entire document ({} lines)",
            self.document.line_count()
        );
    }

    fn move_long_line_page(&mut self, forward: bool) {
        if !self.active_line_chunked {
            return;
        }

        self.commit_active_line();
        let new_start = if forward {
            (self.active_line_chunk_start + self.active_line_source_chars)
                .min(self.active_line_total_chars.saturating_sub(1))
        } else {
            self.active_line_chunk_start
                .saturating_sub(LONG_LINE_CHUNK_CHARS)
        };
        self.active_line_chunk_start = new_start;
        self.load_active_line_buffer();
    }

    fn split_active_line(&mut self, selection: Range<usize>) {
        if self.document.is_read_only() {
            return;
        }

        let split_at = self.active_line_chunk_start + selection.start;
        if !selection.is_empty()
            && let Err(error) = replace_char_range(&mut self.line_buffer, selection, "")
        {
            self.status = format!("Cannot split line: {error:#}");
            return;
        }

        self.commit_active_line();
        if !self.document.split_line(self.active_line, split_at) {
            self.status = "Cannot split line at the current cursor position".to_owned();
            return;
        }

        self.shift_bookmarks_after_insert(self.active_line + 1);
        self.active_line += 1;
        self.load_active_line();
        self.pending_search_selection = Some(0..0);
        self.pending_scroll_line = Some(self.active_line);
        self.status = "Line split".to_owned();
    }

    fn delete_active_selection(&mut self, selection: Range<usize>) {
        if self.document.is_read_only() || selection.is_empty() {
            return;
        }

        let cursor = selection.start;
        if let Err(error) = replace_char_range(&mut self.line_buffer, selection, "") {
            self.status = format!("Cannot delete selection: {error:#}");
            return;
        }
        self.commit_active_line();
        self.selection = None;
        self.pending_search_selection = Some(cursor..cursor);
        self.status = "Deleted selected text".to_owned();
    }

    fn delete_entire_document(&mut self) {
        if self.document.is_read_only() {
            self.status = "Cannot delete selection: the file is read-only".to_owned();
            return;
        }
        self.commit_active_line();
        self.document.replace_all("");
        self.document_selected = false;
        self.bookmarks.clear();
        self.active_line = 0;
        self.load_active_line();
        self.pending_search_selection = Some(0..0);
        self.status = "Deleted entire document selection".to_owned();
    }

    fn line_editor_id(&self) -> egui::Id {
        egui::Id::new((
            "document-line-editor",
            self.editor_instance_id,
            self.active_line,
            self.active_line_chunk_start,
        ))
    }

    fn handle_paste(&mut self, pasted_text: String, selection: Range<usize>, ctx: &egui::Context) {
        if self.paste.active_request.is_some() {
            self.status = "A large paste is already being applied".to_owned();
            return;
        }

        self.commit_active_line();
        let line_offset = self.active_line_chunk_start + selection.start;
        let line_end_offset = self.active_line_chunk_start + selection.end;
        let Some(global_start) = self
            .document
            .global_char_for_line_offset(self.active_line, line_offset)
        else {
            self.status = "Paste failed: cursor position is no longer valid".to_owned();
            return;
        };
        let Some(global_end) = self
            .document
            .global_char_for_line_offset(self.active_line, line_end_offset)
        else {
            self.status = "Paste failed: selection is no longer valid".to_owned();
            return;
        };

        if pasted_text.len() < LARGE_PASTE_BYTES {
            let inserted_chars = pasted_text.chars().count();
            let revision = self.document.revision();
            let previous_line_count = self.document.line_count();
            if self.document.replace_global_range_if_revision(
                global_start,
                global_end,
                &pasted_text,
                revision,
            ) {
                if self.document.line_count() != previous_line_count {
                    self.bookmarks.clear();
                }
                self.finish_paste(global_start + inserted_chars);
                self.status = format!("Pasted {inserted_chars} characters");
            } else {
                self.status = "Paste failed because the document changed".to_owned();
            }
            return;
        }

        let (rope, revision) = self.document.search_snapshot();
        let request_id = self.paste.next_request_id;
        self.paste.next_request_id += 1;
        self.paste.active_request = Some(request_id);
        self.status = format!(
            "Pasting {:.1} MiB in the background...",
            pasted_text.len() as f64 / (1024.0 * 1024.0)
        );

        let sender = self.paste.sender.clone();
        let repaint = ctx.clone();
        let spawn_result = thread::Builder::new()
            .name(format!("rusteditor-paste-{request_id}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    paste::apply(rope, global_start, global_end, &pasted_text)
                }))
                .map_err(|_| "the paste worker panicked".to_owned())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
                let _ = sender.send(PasteMessage {
                    request_id,
                    revision,
                    result,
                });
                repaint.request_repaint();
            });

        if let Err(error) = spawn_result {
            self.paste.active_request = None;
            self.status = format!("Cannot start paste worker: {error}");
        }
    }

    fn receive_paste_results(&mut self) {
        while let Ok(message) = self.paste.receiver.try_recv() {
            if self.paste.active_request != Some(message.request_id) {
                continue;
            }
            self.paste.active_request = None;
            match message.result {
                Ok(result) => {
                    let line_count_changed = result.line_count_changed;
                    if self
                        .document
                        .replace_rope_if_revision(result.rope, message.revision)
                    {
                        if line_count_changed {
                            self.bookmarks.clear();
                        }
                        self.finish_paste(result.cursor_char);
                        self.status = "Large paste completed".to_owned();
                    } else {
                        self.status =
                            "Document changed while pasting; paste was not applied".to_owned();
                    }
                }
                Err(error) => self.status = format!("Paste failed: {error}"),
            }
        }
    }

    fn finish_paste(&mut self, cursor_char: usize) {
        let Some((line_index, line_char)) = self.document.char_location(cursor_char) else {
            self.status =
                "Paste completed, but the cursor position could not be restored".to_owned();
            return;
        };

        self.active_line = line_index;
        self.active_line_total_chars = self.document.line_len_chars(line_index);
        self.active_line_chunked = self.active_line_total_chars > MAX_INLINE_EDIT_CHARS;
        let anchor = if line_char == self.active_line_total_chars {
            line_char.saturating_sub(1)
        } else {
            line_char
        };
        self.active_line_chunk_start = if self.active_line_chunked {
            (anchor / LONG_LINE_CHUNK_CHARS) * LONG_LINE_CHUNK_CHARS
        } else {
            0
        };
        self.load_active_line_buffer();
        let relative_cursor = line_char
            .saturating_sub(self.active_line_chunk_start)
            .min(self.line_buffer.chars().count());
        self.pending_search_selection = Some(relative_cursor..relative_cursor);
        self.pending_scroll_line = Some(line_index);
    }

    fn current_search_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.search.query.clone(),
            case_sensitive: self.search.case_sensitive,
            regex: self.search.regex,
        }
    }

    fn search_results_are_current(&self) -> bool {
        self.search.result_query.as_ref() == Some(&self.current_search_query())
            && self.search.result_revision == self.document.revision()
    }

    fn start_search(&mut self, ctx: &egui::Context, replace_all: bool) {
        self.commit_active_line();
        let query = self.current_search_query();
        if query.text.is_empty() {
            self.search.status = "Enter text to search for".to_owned();
            return;
        }
        if replace_all && self.document.is_read_only() {
            self.search.status = "The file is read-only".to_owned();
            return;
        }

        let (rope, revision) = self.document.search_snapshot();
        let replacement = self.search.replacement.clone();
        let request_id = self.search.next_request_id;
        self.search.next_request_id += 1;
        self.search.active_request = Some(request_id);
        self.search.status = if replace_all {
            "Replacing in background...".to_owned()
        } else {
            "Searching in background...".to_owned()
        };

        let sender = self.search.sender.clone();
        let repaint = ctx.clone();
        let spawn_result = thread::Builder::new()
            .name(format!("rusteditor-search-{request_id}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if replace_all {
                        search::replace_all(&rope, &query, &replacement).map(
                            |(rope, replacements)| SearchWorkerResult::Replaced {
                                revision,
                                rope,
                                replacements,
                            },
                        )
                    } else {
                        search::find_all(&rope, &query, MAX_SEARCH_MATCHES).map(|results| {
                            SearchWorkerResult::Found {
                                query,
                                revision,
                                results,
                            }
                        })
                    }
                }))
                .map_err(|_| "the search worker panicked".to_owned())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
                let _ = sender.send(SearchMessage { request_id, result });
                repaint.request_repaint();
            });

        if let Err(error) = spawn_result {
            self.search.active_request = None;
            self.search.status = format!("Cannot start search: {error}");
        }
    }

    fn receive_search_results(&mut self) {
        while let Ok(message) = self.search.receiver.try_recv() {
            if self.search.active_request != Some(message.request_id) {
                continue;
            }
            self.search.active_request = None;

            match message.result {
                Ok(SearchWorkerResult::Found {
                    query,
                    revision,
                    results,
                }) => {
                    let count = results.matches.len();
                    self.search.matches = results.matches;
                    self.search.truncated = results.truncated;
                    self.search.result_query = Some(query);
                    self.search.result_revision = revision;
                    self.search.selected_match = None;
                    self.search.status = if self.search.truncated {
                        format!("Showing first {count} matches")
                    } else {
                        format!("{count} matches")
                    };
                    if count > 0 {
                        self.navigate_search_match(0);
                    }
                }
                Ok(SearchWorkerResult::Replaced {
                    revision,
                    rope,
                    replacements,
                }) => {
                    if self.document.replace_rope_if_revision(rope, revision) {
                        self.bookmarks.clear();
                        self.active_line = self
                            .active_line
                            .min(self.document.line_count().saturating_sub(1));
                        self.load_active_line();
                        self.search.matches.clear();
                        self.search.selected_match = None;
                        self.search.result_query = None;
                        self.search.status = format!("Replaced {replacements} matches");
                    } else {
                        self.search.status =
                            "Document changed while replacing; run Replace All again".to_owned();
                    }
                }
                Err(error) => {
                    self.search.status = format!("Search failed: {error}");
                }
            }
        }
    }

    fn navigate_search_match(&mut self, index: usize) {
        let Some(found) = self.search.matches.get(index).cloned() else {
            return;
        };
        if self.search.result_revision != self.document.revision() {
            self.search.status = "Search results are stale; search again".to_owned();
            return;
        }

        self.commit_active_line();
        self.active_line = found.line_index;
        self.active_line_total_chars = self.document.line_len_chars(self.active_line);
        self.active_line_chunked = self.active_line_total_chars > MAX_INLINE_EDIT_CHARS;
        self.active_line_chunk_start = if self.active_line_chunked {
            (found.start_char / LONG_LINE_CHUNK_CHARS) * LONG_LINE_CHUNK_CHARS
        } else {
            0
        };
        self.load_active_line_buffer();

        let relative_start = found
            .start_char
            .saturating_sub(self.active_line_chunk_start);
        let relative_end = found
            .end_char
            .saturating_sub(self.active_line_chunk_start)
            .min(self.line_buffer.chars().count());
        self.selection = Some(relative_start..relative_end);
        self.pending_search_selection = Some(relative_start..relative_end);
        self.pending_scroll_line = Some(found.line_index);
        self.search.selected_match = Some(index);
        self.search.status = format!(
            "Match {} of {} at line {}",
            index + 1,
            self.search.matches.len(),
            found.line_index + 1
        );
    }

    fn navigate_search(&mut self, ctx: &egui::Context, forward: bool) {
        if !self.search_results_are_current() || self.search.matches.is_empty() {
            self.start_search(ctx, false);
            return;
        }

        let count = self.search.matches.len();
        let current = self
            .search
            .selected_match
            .unwrap_or(if forward { count - 1 } else { 0 });
        let next = if forward {
            (current + 1) % count
        } else {
            current.checked_sub(1).unwrap_or(count - 1)
        };
        self.navigate_search_match(next);
    }

    fn bookmark_current_search_match(&mut self) {
        if !self.search_results_are_current() {
            self.search.status = "Search results are stale; search again".to_owned();
            return;
        }
        let Some(index) = self.search.selected_match else {
            self.search.status = "Select a search match first".to_owned();
            return;
        };
        let Some(found) = self.search.matches.get(index) else {
            return;
        };
        self.bookmarks.insert(found.line_index);
        self.search.status = format!(
            "Bookmarked line {} ({} bookmarked lines)",
            found.line_index + 1,
            self.bookmarks.len()
        );
    }

    fn bookmark_all_search_matches(&mut self) {
        if !self.search_results_are_current() {
            self.search.status = "Search results are stale; search again".to_owned();
            return;
        }
        let previous_count = self.bookmarks.len();
        self.bookmarks
            .extend(self.search.matches.iter().map(|found| found.line_index));
        let added = self.bookmarks.len() - previous_count;
        self.search.status = format!(
            "Bookmarked {added} new matching lines ({} total)",
            self.bookmarks.len()
        );
    }

    fn toggle_bookmark(&mut self, line_index: usize) {
        if !self.bookmarks.remove(&line_index) {
            self.bookmarks.insert(line_index);
        }
        self.status = format!("{} bookmarked lines", self.bookmarks.len());
    }

    fn bookmarked_line_indices(&self) -> Vec<usize> {
        self.bookmarks
            .iter()
            .copied()
            .filter(|&line_index| line_index < self.document.line_count())
            .collect()
    }

    fn copy_bookmarked_lines(&mut self, ctx: &egui::Context) -> bool {
        self.commit_active_line();
        let line_indices = self.bookmarked_line_indices();
        if line_indices.is_empty() {
            self.status = "No lines are bookmarked".to_owned();
            return false;
        }
        ctx.copy_text(self.document.lines_text(&line_indices));
        self.status = format!("Copied {} bookmarked lines", line_indices.len());
        true
    }

    fn delete_bookmarked_lines(&mut self) {
        if self.document.is_read_only() {
            self.status = "Cannot delete bookmarks: the file is read-only".to_owned();
            return;
        }
        self.commit_active_line();
        let line_indices = self.bookmarked_line_indices();
        if line_indices.is_empty() {
            self.status = "No lines are bookmarked".to_owned();
            return;
        }

        let first_deleted = line_indices[0];
        let deleted = self.document.delete_lines(&line_indices);
        self.bookmarks.clear();
        self.active_line = first_deleted.min(self.document.line_count().saturating_sub(1));
        self.load_active_line();
        self.pending_scroll_line = Some(self.active_line);
        self.search.matches.clear();
        self.search.selected_match = None;
        self.search.result_query = None;
        self.status = format!("Deleted {deleted} bookmarked lines");
    }

    fn cut_bookmarked_lines(&mut self, ctx: &egui::Context) {
        if self.document.is_read_only() {
            self.status = "Cannot cut bookmarks: the file is read-only".to_owned();
            return;
        }
        if self.copy_bookmarked_lines(ctx) {
            self.delete_bookmarked_lines();
            self.status = "Cut bookmarked lines".to_owned();
        }
    }

    fn shift_bookmarks_after_insert(&mut self, inserted_line: usize) {
        self.bookmarks = self
            .bookmarks
            .iter()
            .map(|&line_index| {
                if line_index >= inserted_line {
                    line_index + 1
                } else {
                    line_index
                }
            })
            .collect();
    }

    fn shift_bookmarks_after_delete(&mut self, deleted_line: usize) {
        self.bookmarks = self
            .bookmarks
            .iter()
            .filter_map(|&line_index| {
                if line_index == deleted_line {
                    None
                } else if line_index > deleted_line {
                    Some(line_index - 1)
                } else {
                    Some(line_index)
                }
            })
            .collect();
    }

    fn replace_current_search_match(&mut self) {
        if self.document.is_read_only() {
            self.search.status = "The file is read-only".to_owned();
            return;
        }
        if !self.search_results_are_current() {
            self.search.status = "Search results are stale; search again".to_owned();
            return;
        }

        let Some(index) = self.search.selected_match else {
            self.search.status = "Select a search match first".to_owned();
            return;
        };
        let Some(found) = self.search.matches.get(index).cloned() else {
            return;
        };
        let Some(matched_text) = self
            .document
            .text_range(found.global_start_char, found.global_end_char)
        else {
            self.search.status = "The search match is no longer valid".to_owned();
            return;
        };
        let query = self.current_search_query();
        let replacement =
            match search::replacement_for_match(&matched_text, &query, &self.search.replacement) {
                Ok(replacement) => replacement,
                Err(error) => {
                    self.search.status = format!("Replace failed: {error:#}");
                    return;
                }
            };

        if self.document.replace_global_range_if_revision(
            found.global_start_char,
            found.global_end_char,
            &replacement,
            self.search.result_revision,
        ) {
            if replacement.contains('\n') {
                self.bookmarks.clear();
            }
            self.active_line = found
                .line_index
                .min(self.document.line_count().saturating_sub(1));
            self.load_active_line();
            self.search.matches.clear();
            self.search.selected_match = None;
            self.search.result_query = None;
            self.search.status =
                "Replaced current match; search again for updated results".to_owned();
        } else {
            self.search.status = "Document changed; search again before replacing".to_owned();
        }
    }

    fn target_text(&mut self) -> Result<String> {
        match self.transform_scope {
            TransformScope::Selection => {
                let range = self
                    .selection
                    .clone()
                    .context("select text in the active line first")?;
                char_slice(&self.line_buffer, range)
            }
            TransformScope::Document => {
                self.commit_active_line();
                Ok(self.document.text())
            }
        }
    }

    fn replace_target(&mut self, replacement: &str) -> Result<()> {
        let previous_line_count = self.document.line_count();
        match self.transform_scope {
            TransformScope::Selection => {
                let range = self
                    .selection
                    .clone()
                    .context("select text in the active line first")?;
                let start = range.start;
                replace_char_range(&mut self.line_buffer, range, replacement)?;
                self.commit_active_line();
                self.selection = Some(start..start + replacement.chars().count());
            }
            TransformScope::Document => {
                self.commit_active_line();
                self.document.replace_all(replacement);
                self.bookmarks.clear();
                self.active_line = self
                    .active_line
                    .min(self.document.line_count().saturating_sub(1));
                self.load_active_line();
            }
        }
        if self.document.line_count() != previous_line_count {
            self.bookmarks.clear();
        }
        Ok(())
    }

    fn apply_case(&mut self, transform: CaseTransform, label: &str) {
        if self.document.is_read_only() {
            self.status = "Transform failed: the file is read-only".to_owned();
            return;
        }

        let result = self
            .target_text()
            .map(|text| transforms::convert_case(&text, transform))
            .and_then(|converted| self.replace_target(&converted));
        self.finish_tool(result, label);
    }

    fn decode_base64(&mut self) {
        if self.document.is_read_only() {
            self.status = "Base64 decode failed: the file is read-only".to_owned();
            return;
        }

        let result = self
            .target_text()
            .and_then(|text| transforms::decode_base64(&text))
            .and_then(|decoded| self.replace_target(&decoded));
        self.finish_tool(result, "Base64 decoded");
    }

    fn format_json(&mut self) {
        if self.document.is_read_only() {
            self.status = "JSON formatting failed: the file is read-only".to_owned();
            return;
        }

        let result = self
            .target_text()
            .and_then(|text| transforms::pretty_json(&text))
            .and_then(|formatted| self.replace_target(&formatted));
        self.finish_tool(result, "JSON formatted");
    }

    fn view_json(&mut self) {
        let result = self
            .target_text()
            .and_then(|text| transforms::pretty_json(&text));
        match result {
            Ok(text) => {
                let source = match self.transform_scope {
                    TransformScope::Selection => "Selected text",
                    TransformScope::Document => "Entire document",
                };
                self.json_viewer = Some(JsonViewer {
                    text,
                    source: source.to_owned(),
                });
                self.status = "JSON parsed successfully".to_owned();
            }
            Err(error) => self.status = format!("JSON viewer failed: {error:#}"),
        }
    }

    fn finish_tool(&mut self, result: Result<()>, success: &str) {
        self.status = match result {
            Ok(()) => format!(
                "{success} ({})",
                match self.transform_scope {
                    TransformScope::Selection => "selection",
                    TransformScope::Document => "entire document",
                }
            ),
            Err(error) => format!("{success} failed: {error:#}"),
        };
    }

    fn start_ai_request(&mut self, ctx: &egui::Context) {
        if self.ai.active_request.is_some() {
            return;
        }
        let question = self.ai.question.trim().to_owned();
        if question.is_empty() {
            self.ai.status = "Enter a question first".to_owned();
            return;
        }

        self.commit_active_line();
        let selected_text = match self.transform_scope {
            TransformScope::Selection => {
                if let Some(range) = self.selection.clone() {
                    match char_slice(&self.line_buffer, range) {
                        Ok(text) => Some(text),
                        Err(error) => {
                            self.ai.status = format!("Cannot read selection: {error:#}");
                            return;
                        }
                    }
                } else {
                    None
                }
            }
            TransformScope::Document => None,
        };
        let focus = match self.transform_scope {
            TransformScope::Selection => {
                if let Some(range) = &self.selection {
                    let start = self.active_line_chunk_start + range.start + 1;
                    let end = self.active_line_chunk_start + range.end;
                    format!(
                        "selected text on line {}, columns {start}-{end}",
                        self.active_line + 1
                    )
                } else {
                    format!("active line {}", self.active_line + 1)
                }
            }
            TransformScope::Document => "entire file".to_owned(),
        };
        let (rope, _) = self.document.search_snapshot();
        let source = SourceDocument::Snapshot {
            rope,
            suggested_name: self.document.display_name(),
        };

        let request = AiRequest {
            question,
            source,
            focus,
            selected_text,
        };
        let provider = self.ai.provider;
        let request_id = self.ai.next_request_id;
        self.ai.next_request_id += 1;
        self.ai.active_request = Some(request_id);
        self.ai.response.clear();
        self.ai.status = format!("Asking {}...", provider.display_name());

        let sender = self.ai.sender.clone();
        let repaint = ctx.clone();
        let spawn_result = thread::Builder::new()
            .name(format!("rusteditor-ai-{request_id}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| provider.ask(request)))
                    .map_err(|_| "the AI provider worker panicked".to_owned())
                    .and_then(|result| result.map_err(|error| format!("{error:#}")));
                let _ = sender.send(AiMessage { request_id, result });
                repaint.request_repaint();
            });

        if let Err(error) = spawn_result {
            self.ai.active_request = None;
            self.ai.status = format!("Cannot start AI provider: {error}");
        }
    }

    fn receive_ai_results(&mut self) {
        while let Ok(message) = self.ai.receiver.try_recv() {
            if self.ai.active_request != Some(message.request_id) {
                continue;
            }
            self.ai.active_request = None;
            match message.result {
                Ok(response) => {
                    self.ai.response = response;
                    self.ai.status = "Response received".to_owned();
                    self.status = "Copilot response received".to_owned();
                }
                Err(error) => {
                    self.ai.status = format!("Request failed: {error}");
                    self.status = "Copilot request failed".to_owned();
                }
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let save = ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::S));
        let find = ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::F));
        let replace = ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::H));
        let next = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::F3));
        let previous = ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, Key::F3));
        let select_document = ctx.memory(|memory| memory.focused().is_none())
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::A));
        let copy_document = self.document_selected
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::C));
        let delete_document = self.document_selected
            && ctx.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, Key::Delete)
                    || input.consume_key(egui::Modifiers::NONE, Key::Backspace)
            });

        if save {
            self.save();
        }
        if find {
            self.search.visible = true;
            self.search.replace_visible = false;
            self.search.request_focus = true;
        }
        if replace {
            self.search.visible = true;
            self.search.replace_visible = true;
            self.search.request_focus = true;
        }
        if next {
            self.navigate_search(ctx, true);
        }
        if previous {
            self.navigate_search(ctx, false);
        }
        if select_document {
            self.select_entire_document(ctx);
        }
        if copy_document {
            self.copy_entire_document(ctx);
        }
        if delete_document {
            self.delete_entire_document();
        }
    }

    pub fn tools_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Tools", |ui| {
            if ui.button("Find...\tCtrl+F").clicked() {
                ui.close();
                self.search.visible = true;
                self.search.replace_visible = false;
                self.search.request_focus = true;
            }
            if ui.button("Replace...\tCtrl+H").clicked() {
                ui.close();
                self.search.visible = true;
                self.search.replace_visible = true;
                self.search.request_focus = true;
            }
            ui.separator();
            ui.menu_button("Apply tools to", |ui| {
                ui.radio_value(
                    &mut self.transform_scope,
                    TransformScope::Selection,
                    "Selected text",
                );
                ui.radio_value(
                    &mut self.transform_scope,
                    TransformScope::Document,
                    "Entire document",
                );
            });
            ui.separator();

            if ui.button("JSON Viewer").clicked() {
                ui.close();
                self.view_json();
            }
            if ui
                .add_enabled(
                    !self.document.is_read_only(),
                    egui::Button::new("Pretty-print JSON"),
                )
                .clicked()
            {
                ui.close();
                self.format_json();
            }
            if ui
                .add_enabled(
                    !self.document.is_read_only(),
                    egui::Button::new("Decode Base64"),
                )
                .clicked()
            {
                ui.close();
                self.decode_base64();
            }

            ui.separator();
            ui.menu_button("Convert case", |ui| {
                let enabled = !self.document.is_read_only();
                if ui
                    .add_enabled(enabled, egui::Button::new("lowercase"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Lower, "Converted to lowercase");
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("UPPERCASE"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Upper, "Converted to uppercase");
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Proper Case"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Proper, "Converted to proper case");
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("camelCase"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Camel, "Converted to camelCase");
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("PascalCase"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Pascal, "Converted to PascalCase");
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("snake_case"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Snake, "Converted to snake_case");
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("kebab-case"))
                    .clicked()
                {
                    ui.close();
                    self.apply_case(CaseTransform::Kebab, "Converted to kebab-case");
                }
            });
        });
    }

    fn ai_window(&mut self, ctx: &egui::Context) {
        if !self.ai.visible {
            return;
        }

        let mut open = true;
        let mut ask = false;
        egui::Window::new("AI Assistant")
            .open(&mut open)
            .default_size([720.0, 560.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Provider:");
                    egui::ComboBox::from_id_salt(("ai-provider", self.editor_instance_id))
                        .selected_text(self.ai.provider.display_name())
                        .show_ui(ui, |ui| {
                            for provider in ProviderKind::ALL {
                                ui.selectable_value(
                                    &mut self.ai.provider,
                                    provider,
                                    provider.display_name(),
                                );
                            }
                        });
                    ui.separator();
                    ui.label("Context:");
                    ui.selectable_value(
                        &mut self.transform_scope,
                        TransformScope::Selection,
                        "Selection / active line",
                    );
                    ui.selectable_value(
                        &mut self.transform_scope,
                        TransformScope::Document,
                        "Entire file",
                    );
                });
                ui.separator();
                ui.label("Question:");
                let response = ui.add(
                    TextEdit::multiline(&mut self.ai.question)
                        .desired_width(f32::INFINITY)
                        .desired_rows(3)
                        .char_limit(MAX_INLINE_EDIT_CHARS)
                        .hint_text("Ask Copilot about the selected text, active line, or file"),
                );
                if self.ai.request_focus {
                    response.request_focus();
                    self.ai.request_focus = false;
                }

                ui.horizontal(|ui| {
                    ask = ui
                        .add_enabled(self.ai.active_request.is_none(), egui::Button::new("Ask"))
                        .clicked();
                    if self.ai.active_request.is_some() {
                        ui.spinner();
                    }
                    ui.label(&self.ai.status);
                });

                if !self.ai.response.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Response").strong());
                    ScrollArea::vertical()
                        .id_salt(("ai-response", self.editor_instance_id))
                        .max_height(340.0)
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut self.ai.response)
                                    .desired_width(f32::INFINITY)
                                    .interactive(false)
                                    .code_editor(),
                            );
                        });
                }
            });

        if !open {
            self.ai.visible = false;
        } else if ask {
            self.start_ai_request(ctx);
        }
    }

    fn calculator_window(&mut self, ctx: &egui::Context) {
        if !self.calculator.visible {
            return;
        }

        let mut open = true;
        let mut evaluate = false;
        egui::Window::new("Calculator")
            .open(&mut open)
            .resizable(false)
            .default_width(300.0)
            .show(ctx, |ui| {
                let response = ui.add(
                    TextEdit::singleline(&mut self.calculator.expression)
                        .desired_width(f32::INFINITY)
                        .hint_text("(12.5 + 3) * 2"),
                );
                if self.calculator.request_focus {
                    response.request_focus();
                    self.calculator.request_focus = false;
                }
                evaluate |=
                    response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));

                if !self.calculator.result.is_empty() {
                    ui.label(
                        RichText::new(format!("= {}", self.calculator.result))
                            .monospace()
                            .strong(),
                    );
                }
                if !self.calculator.status.is_empty() {
                    ui.label(
                        RichText::new(&self.calculator.status)
                            .color(Color32::from_rgb(180, 35, 35)),
                    );
                }

                ui.separator();
                egui::Grid::new(("calculator-grid", self.editor_instance_id))
                    .spacing([6.0, 6.0])
                    .show(ui, |ui| {
                        for row in [
                            ["7", "8", "9", "/"],
                            ["4", "5", "6", "*"],
                            ["1", "2", "3", "-"],
                            ["0", ".", "(", "+"],
                        ] {
                            for label in row {
                                if ui
                                    .add_sized([58.0, 32.0], egui::Button::new(label))
                                    .clicked()
                                {
                                    self.calculator.expression.push_str(label);
                                }
                            }
                            ui.end_row();
                        }
                        if ui.add_sized([58.0, 32.0], egui::Button::new(")")).clicked() {
                            self.calculator.expression.push(')');
                        }
                        if ui.add_sized([58.0, 32.0], egui::Button::new("±")).clicked() {
                            self.calculator.expression.insert(0, '-');
                        }
                        if ui.add_sized([58.0, 32.0], egui::Button::new("⌫")).clicked() {
                            self.calculator.expression.pop();
                        }
                        evaluate |= ui.add_sized([58.0, 32.0], egui::Button::new("=")).clicked();
                    });
                ui.horizontal(|ui| {
                    if ui.button("Clear").clicked() {
                        self.calculator.expression.clear();
                        self.calculator.result.clear();
                        self.calculator.status.clear();
                    }
                    if ui.button("Use result").clicked() && !self.calculator.result.is_empty() {
                        self.calculator.expression = self.calculator.result.clone();
                    }
                });
            });

        if evaluate {
            match calculate_expression(&self.calculator.expression) {
                Ok(value) => {
                    self.calculator.result = format_calculator_result(value);
                    self.calculator.status.clear();
                }
                Err(error) => {
                    self.calculator.result.clear();
                    self.calculator.status = format!("{error:#}");
                }
            }
        }
        if !open {
            self.calculator.visible = false;
        }
    }

    fn search_panel(&mut self, root_ui: &mut egui::Ui, ctx: &egui::Context) {
        if !self.search.visible {
            return;
        }

        let mut find_all = false;
        let mut previous = false;
        let mut next = false;
        let mut replace_current = false;
        let mut replace_all = false;
        let mut bookmark_current = false;
        let mut bookmark_all = false;
        let mut copy_bookmarks = false;
        let mut cut_bookmarks = false;
        let mut delete_bookmarks = false;
        let mut clear_bookmarks = false;
        let mut close = false;

        Panel::top("search_panel")
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(250, 250, 250))
                    .inner_margin(Margin::symmetric(8, 5))
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Find:");
                    let response = ui.add(
                        TextEdit::singleline(&mut self.search.query)
                            .desired_width(260.0)
                            .hint_text(if self.search.regex {
                                "Regular expression"
                            } else {
                                "Text"
                            }),
                    );
                    if self.search.request_focus {
                        response.request_focus();
                        self.search.request_focus = false;
                    }
                    if response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                        next = true;
                    }

                    find_all = ui.button("Find All").clicked();
                    previous = ui.button("Previous").clicked();
                    next |= ui.button("Next").clicked();
                    if self.search.active_request.is_some() {
                        ui.spinner();
                    }
                    if ui.button("X").on_hover_text("Close search").clicked() {
                        close = true;
                    }
                });

                if self.search.replace_visible {
                    ui.horizontal(|ui| {
                        ui.label("Replace:");
                        ui.add(
                            TextEdit::singleline(&mut self.search.replacement).desired_width(260.0),
                        );
                        replace_current = ui
                            .add_enabled(
                                !self.document.is_read_only(),
                                egui::Button::new("Replace"),
                            )
                            .clicked();
                        replace_all = ui
                            .add_enabled(
                                !self.document.is_read_only(),
                                egui::Button::new("Replace All"),
                            )
                            .clicked();
                    });
                }

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.search.case_sensitive, "Match case");
                    ui.checkbox(&mut self.search.regex, "Regular expression");
                    ui.separator();
                    ui.label(&self.search.status);
                });
                ui.horizontal(|ui| {
                    bookmark_current = ui.button("Bookmark current match").clicked();
                    bookmark_all = ui.button("Bookmark all matching lines").clicked();
                    ui.separator();
                    copy_bookmarks = ui
                        .add_enabled(
                            !self.bookmarks.is_empty(),
                            egui::Button::new("Copy bookmarked lines"),
                        )
                        .clicked();
                    cut_bookmarks = ui
                        .add_enabled(
                            !self.bookmarks.is_empty() && !self.document.is_read_only(),
                            egui::Button::new("Cut"),
                        )
                        .clicked();
                    delete_bookmarks = ui
                        .add_enabled(
                            !self.bookmarks.is_empty() && !self.document.is_read_only(),
                            egui::Button::new("Delete"),
                        )
                        .clicked();
                    clear_bookmarks = ui
                        .add_enabled(
                            !self.bookmarks.is_empty(),
                            egui::Button::new("Clear bookmarks"),
                        )
                        .clicked();
                    ui.label(format!("{} bookmarked", self.bookmarks.len()));
                });
            });

        if close {
            self.search.visible = false;
        } else if clear_bookmarks {
            self.bookmarks.clear();
            self.search.status = "Bookmarks cleared".to_owned();
        } else if delete_bookmarks {
            self.delete_bookmarked_lines();
        } else if cut_bookmarks {
            self.cut_bookmarked_lines(ctx);
        } else if copy_bookmarks {
            self.copy_bookmarked_lines(ctx);
        } else if bookmark_all {
            self.bookmark_all_search_matches();
        } else if bookmark_current {
            self.bookmark_current_search_match();
        } else if replace_all {
            self.start_search(ctx, true);
        } else if replace_current {
            self.replace_current_search_match();
        } else if find_all {
            self.start_search(ctx, false);
        } else if previous {
            self.navigate_search(ctx, false);
        } else if next {
            self.navigate_search(ctx, true);
        }
    }

    fn toolbar(&mut self, root_ui: &mut egui::Ui) {
        Panel::top("toolbar")
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(238, 238, 238))
                    .inner_margin(Margin::symmetric(6, 4))
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.document.is_read_only(), egui::Button::new("Save"))
                        .clicked()
                    {
                        self.save();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.document.is_read_only(),
                            egui::Button::new("Insert line"),
                        )
                        .clicked()
                    {
                        self.commit_active_line();
                        self.document.insert_line_below(self.active_line);
                        self.shift_bookmarks_after_insert(self.active_line + 1);
                        self.activate_line(self.active_line + 1);
                    }
                    if ui
                        .add_enabled(
                            !self.document.is_read_only(),
                            egui::Button::new("Delete line"),
                        )
                        .clicked()
                    {
                        let deleted_line = self.active_line;
                        self.document.delete_line(self.active_line);
                        self.shift_bookmarks_after_delete(deleted_line);
                        self.active_line = self
                            .active_line
                            .min(self.document.line_count().saturating_sub(1));
                        self.load_active_line();
                    }
                    ui.separator();
                    ui.label("Tool scope:");
                    ui.selectable_value(
                        &mut self.transform_scope,
                        TransformScope::Selection,
                        "Selection",
                    );
                    ui.selectable_value(
                        &mut self.transform_scope,
                        TransformScope::Document,
                        "Entire file",
                    );
                    ui.separator();
                    if ui.button("Copilot").clicked() {
                        self.ai.visible = true;
                        self.ai.request_focus = true;
                    }
                    if ui.button("Calculator").clicked() {
                        self.calculator.visible = true;
                        self.calculator.request_focus = true;
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.bookmarks.is_empty(),
                            egui::Button::new(format!("Bookmarks ({})", self.bookmarks.len())),
                        )
                        .clicked()
                    {
                        self.search.visible = true;
                    }
                });
            });
    }

    fn editor(&mut self, root_ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(EDITOR_BG))
            .show(root_ui, |ui| {
                let line_count = self.document.line_count();
                let gutter_width = ((line_count.max(1).ilog10() + 1) as f32 * 9.0 + 20.0).max(48.0);

                let mut scroll_area = ScrollArea::both()
                    .auto_shrink([false, false])
                    .scroll_source(ScrollSource::ALL)
                    .on_hover_cursor(egui::CursorIcon::Grab)
                    .on_drag_cursor(egui::CursorIcon::Grabbing);
                if let Some(line_index) = self.pending_scroll_line.take() {
                    scroll_area =
                        scroll_area.vertical_scroll_offset(line_index as f32 * ROW_HEIGHT);
                }

                let spacing_y = ui.spacing().item_spacing.y;
                let row_stride = ROW_HEIGHT + spacing_y;
                scroll_area.show_viewport(ui, |ui, viewport| {
                    ui.set_height((row_stride * line_count as f32 - spacing_y).max(0.0));

                    let mut min_row = (viewport.min.y / row_stride).floor() as usize;
                    let mut max_row = (viewport.max.y / row_stride).ceil() as usize + 1;
                    if max_row > line_count {
                        let visible_count = max_row.saturating_sub(min_row);
                        max_row = line_count;
                        min_row = line_count.saturating_sub(visible_count);
                    }

                    let y_min = ui.max_rect().top() + min_row as f32 * row_stride;
                    let y_max = ui.max_rect().top() + max_row as f32 * row_stride;
                    let visible_rect =
                        egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);

                    ui.scope_builder(egui::UiBuilder::new().max_rect(visible_rect), |ui| {
                        ui.skip_ahead_auto_ids(min_row);
                        ui.set_min_width(ui.available_width().max(900.0));

                        for line_index in min_row..max_row {
                            let is_active = line_index == self.active_line;
                            let is_bookmarked = self.bookmarks.contains(&line_index);
                            let row_fill = if self.document_selected {
                                DOCUMENT_SELECTION
                            } else if is_active {
                                ACTIVE_LINE
                            } else {
                                EDITOR_BG
                            };
                            let mut toggle_bookmark = false;

                            let gutter_rect = Frame::new()
                                .fill(row_fill)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let (gutter_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(gutter_width, ROW_HEIGHT),
                                            egui::Sense::hover(),
                                        );

                                        if is_active && self.active_line_chunked {
                                            let page_start = self.active_line_chunk_start + 1;
                                            let page_end = self.active_line_chunk_start
                                                + self.active_line_source_chars;
                                            let previous = ui
                                                .add_enabled(
                                                    self.active_line_chunk_start > 0,
                                                    egui::Button::new("<"),
                                                )
                                                .on_hover_text("Previous long-line page")
                                                .clicked();
                                            let next = ui
                                                .add_enabled(
                                                    page_end < self.active_line_total_chars,
                                                    egui::Button::new(">"),
                                                )
                                                .on_hover_text("Next long-line page")
                                                .clicked();
                                            ui.label(
                                                RichText::new(format!(
                                                    "{page_start}-{page_end}/{}",
                                                    self.active_line_total_chars
                                                ))
                                                .small()
                                                .color(GUTTER_TEXT),
                                            );
                                            if previous {
                                                self.move_long_line_page(false);
                                            } else if next {
                                                self.move_long_line_page(true);
                                            }
                                        }

                                        if is_active
                                            && !self.document.is_read_only()
                                            && !self.document_selected
                                        {
                                            let editor_id = self.line_editor_id();
                                            let select_all = take_select_all_event(ui, editor_id);
                                            let select_document =
                                                select_all && self.active_page_is_fully_selected();
                                            if select_document {
                                                self.select_entire_document(ui.ctx());
                                            }
                                            let existing_selection = text_edit_selection(
                                                ui.ctx(),
                                                editor_id,
                                                self.line_buffer.chars().count(),
                                            );
                                            let delete_selection = !select_all
                                                && !existing_selection.is_empty()
                                                && take_delete_selection_event(ui, editor_id);
                                            let enter_selection = (!select_all
                                                && !delete_selection
                                                && take_enter_event(ui, editor_id))
                                            .then(|| {
                                                text_edit_selection(
                                                    ui.ctx(),
                                                    editor_id,
                                                    self.line_buffer.chars().count(),
                                                )
                                            });
                                            let pasted_text = if enter_selection.is_none() {
                                                if delete_selection {
                                                    None
                                                } else {
                                                    take_paste_event(ui, editor_id)
                                                }
                                            } else {
                                                None
                                            };
                                            let input_handled = if select_document {
                                                true
                                            } else if delete_selection {
                                                self.delete_active_selection(existing_selection);
                                                true
                                            } else if let Some(selection) = enter_selection {
                                                self.split_active_line(selection);
                                                true
                                            } else if let Some(pasted_text) = pasted_text {
                                                let selection = text_edit_selection(
                                                    ui.ctx(),
                                                    editor_id,
                                                    self.line_buffer.chars().count(),
                                                );
                                                self.handle_paste(pasted_text, selection, ui.ctx());
                                                true
                                            } else {
                                                false
                                            };

                                            if input_handled {
                                                ui.label(
                                                    RichText::new(" ")
                                                        .color(GUTTER_TEXT)
                                                        .monospace(),
                                                );
                                            } else {
                                                let mut output =
                                                    TextEdit::singleline(&mut self.line_buffer)
                                                        .id(editor_id)
                                                        .font(TextStyle::Monospace)
                                                        .desired_width(f32::INFINITY)
                                                        .char_limit(MAX_INLINE_EDIT_CHARS)
                                                        .interactive(
                                                            self.paste.active_request.is_none(),
                                                        )
                                                        .margin(Vec2::new(5.0, 2.0))
                                                        .frame(Frame::NONE)
                                                        .show(ui);
                                                let requested_selection =
                                                    if select_all && !select_document {
                                                        let end = self.line_buffer.chars().count();
                                                        Some(0..end)
                                                    } else {
                                                        self.pending_search_selection.take()
                                                    };
                                                if let Some(range) = requested_selection {
                                                    let cursor_range =
                                                        egui::text::CCursorRange::two(
                                                            egui::text::CCursor::new(range.start),
                                                            egui::text::CCursor::new(range.end),
                                                        );
                                                    output
                                                        .state
                                                        .cursor
                                                        .set_char_range(Some(cursor_range));
                                                    output
                                                        .state
                                                        .store(ui.ctx(), output.response.id);
                                                    output.response.request_focus();
                                                    self.selection =
                                                        (!range.is_empty()).then_some(range);
                                                } else {
                                                    let cursor_selection =
                                                        output.cursor_range.map(|cursor_range| {
                                                            let range =
                                                                cursor_range.as_sorted_char_range();
                                                            range.start.0..range.end.0
                                                        });
                                                    if let Some(range) = &cursor_selection {
                                                        self.selection = (!range.is_empty())
                                                            .then_some(range.clone());
                                                    }
                                                }
                                                if output.response.changed() {
                                                    self.commit_active_line();
                                                }
                                            }
                                        } else {
                                            let (text, truncated) = if is_active {
                                                (self.line_buffer.clone(), self.active_line_chunked)
                                            } else {
                                                let window = self.document.line_window(
                                                    line_index,
                                                    0,
                                                    INACTIVE_LINE_PREVIEW_CHARS,
                                                );
                                                (window.text, window.end_char < window.total_chars)
                                            };
                                            let display_text = if truncated {
                                                format!(
                                                    "{} ... [{} chars]",
                                                    text,
                                                    self.document.line_len_chars(line_index)
                                                )
                                            } else if text.is_empty() {
                                                " ".to_owned()
                                            } else {
                                                text
                                            };
                                            let response = ui.add(
                                                egui::Label::new(
                                                    RichText::new(display_text)
                                                        .font(FontId::monospace(14.0))
                                                        .color(Color32::from_rgb(25, 25, 25)),
                                                )
                                                .sense(egui::Sense::click()),
                                            );
                                            if response.clicked() {
                                                self.activate_line(line_index);
                                            }
                                        }
                                        gutter_rect
                                    })
                                    .inner
                                })
                                .inner;

                            let pinned_gutter_rect =
                                gutter_rect.translate(egui::vec2(viewport.min.x, 0.0));
                            let mut gutter_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .id_salt(("pinned-gutter", line_index))
                                    .max_rect(pinned_gutter_rect),
                            );
                            gutter_ui
                                .set_clip_rect(pinned_gutter_rect.intersect(gutter_ui.clip_rect()));
                            Frame::new().fill(GUTTER_BG).show(&mut gutter_ui, |ui| {
                                ui.set_min_width(gutter_width);
                                ui.set_max_width(gutter_width);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new((line_index + 1).to_string())
                                            .color(GUTTER_TEXT)
                                            .monospace(),
                                    );
                                    if ui
                                        .selectable_label(is_bookmarked, "B")
                                        .on_hover_text("Toggle line bookmark")
                                        .clicked()
                                    {
                                        toggle_bookmark = true;
                                    }
                                });
                            });
                            if toggle_bookmark {
                                self.toggle_bookmark(line_index);
                            }
                        }
                    });
                });
            });
    }

    fn status_bar(&mut self, root_ui: &mut egui::Ui) {
        Panel::bottom("status_bar")
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(240, 240, 240))
                    .stroke(Stroke::new(1.0, BORDER))
                    .inner_margin(Margin::symmetric(8, 3)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&self.status);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(if self.document.is_read_only() {
                            "READ ONLY"
                        } else {
                            "EDIT"
                        });
                        ui.separator();
                        ui.label("UTF-8");
                        ui.separator();
                        ui.label(format!(
                            "Ln {}, Col page {}-{}, Line chars {}, Lines {}",
                            self.active_line + 1,
                            self.active_line_chunk_start + 1,
                            self.active_line_chunk_start + self.active_line_source_chars,
                            self.active_line_total_chars,
                            self.document.line_count()
                        ));
                        if let Some(selection) = &self.selection {
                            ui.separator();
                            ui.label(format!("Selected {} chars", selection.len()));
                        } else if self.document_selected {
                            ui.separator();
                            ui.label("Entire document selected");
                        }
                    });
                });
            });
    }

    fn json_viewer_window(&mut self, ctx: &egui::Context) {
        let Some(viewer) = &mut self.json_viewer else {
            return;
        };

        let mut open = true;
        egui::Window::new("JSON Viewer")
            .open(&mut open)
            .default_size([720.0, 600.0])
            .show(ctx, |ui| {
                ui.label(format!("Source: {}", viewer.source));
                ui.separator();
                ui.add(
                    TextEdit::multiline(&mut viewer.text)
                        .code_editor()
                        .interactive(false)
                        .desired_width(f32::INFINITY)
                        .desired_rows(30),
                );
            });
        if !open {
            self.json_viewer = None;
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.receive_paste_results();
        self.receive_search_results();
        self.receive_ai_results();
        self.handle_shortcuts(&ctx);
        self.toolbar(ui);
        self.search_panel(ui, &ctx);
        self.status_bar(ui);
        self.editor(ui);
        self.json_viewer_window(&ctx);
        self.ai_window(&ctx);
        self.calculator_window(&ctx);
    }

    pub fn display_name(&self) -> String {
        self.document.display_name()
    }

    pub fn is_dirty(&self) -> bool {
        self.document.is_dirty()
    }

    pub fn is_read_only(&self) -> bool {
        self.document.is_read_only()
    }
}

fn calculate_expression(expression: &str) -> Result<f64> {
    let mut parser = CalculatorParser {
        input: expression.as_bytes(),
        position: 0,
    };
    let value = parser.parse_expression()?;
    parser.skip_whitespace();
    if parser.position != parser.input.len() {
        anyhow::bail!(
            "unexpected character at position {}",
            parser.position.saturating_add(1)
        );
    }
    if !value.is_finite() {
        anyhow::bail!("result is not a finite number");
    }
    Ok(value)
}

fn format_calculator_result(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.abs() >= 1_000_000_000_000.0 || value.abs() < 0.000_000_001 {
        return format!("{value:.10e}");
    }
    let formatted = format!("{value:.12}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

struct CalculatorParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl CalculatorParser<'_> {
    fn parse_expression(&mut self) -> Result<f64> {
        let mut value = self.parse_term()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.position += 1;
                    value += self.parse_term()?;
                }
                Some(b'-') => {
                    self.position += 1;
                    value -= self.parse_term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_term(&mut self) -> Result<f64> {
        let mut value = self.parse_factor()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.position += 1;
                    value *= self.parse_factor()?;
                }
                Some(b'/') => {
                    self.position += 1;
                    let divisor = self.parse_factor()?;
                    if divisor == 0.0 {
                        anyhow::bail!("division by zero");
                    }
                    value /= divisor;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_factor(&mut self) -> Result<f64> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'+') => {
                self.position += 1;
                self.parse_factor()
            }
            Some(b'-') => {
                self.position += 1;
                Ok(-self.parse_factor()?)
            }
            Some(b'(') => {
                self.position += 1;
                let value = self.parse_expression()?;
                self.skip_whitespace();
                if self.peek() != Some(b')') {
                    anyhow::bail!("missing closing parenthesis");
                }
                self.position += 1;
                Ok(value)
            }
            _ => self.parse_number(),
        }
    }

    fn parse_number(&mut self) -> Result<f64> {
        self.skip_whitespace();
        let start = self.position;
        let mut decimal_seen = false;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                self.position += 1;
            } else if byte == b'.' && !decimal_seen {
                decimal_seen = true;
                self.position += 1;
            } else {
                break;
            }
        }
        if start == self.position {
            anyhow::bail!(
                "expected a number at position {}",
                self.position.saturating_add(1)
            );
        }
        let text = std::str::from_utf8(&self.input[start..self.position])?;
        text.parse::<f64>()
            .with_context(|| format!("invalid number `{text}`"))
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}

fn take_paste_event(ui: &mut egui::Ui, editor_id: egui::Id) -> Option<String> {
    if !ui.memory(|memory| memory.has_focus(editor_id)) {
        return None;
    }

    ui.input_mut(|input| {
        let events = std::mem::take(&mut input.events);
        let mut pasted_text = String::new();
        for event in events {
            match event {
                egui::Event::Paste(text) => pasted_text.push_str(&text),
                event => input.events.push(event),
            }
        }
        (!pasted_text.is_empty()).then_some(pasted_text)
    })
}

fn take_enter_event(ui: &mut egui::Ui, editor_id: egui::Id) -> bool {
    if !ui.memory(|memory| memory.has_focus(editor_id)) {
        return false;
    }

    ui.input_mut(|input| {
        let events = std::mem::take(&mut input.events);
        let mut enter_pressed = false;
        for event in events {
            match event {
                egui::Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                } => enter_pressed = true,
                event => input.events.push(event),
            }
        }
        enter_pressed
    })
}

fn take_select_all_event(ui: &mut egui::Ui, editor_id: egui::Id) -> bool {
    if !ui.memory(|memory| memory.has_focus(editor_id)) {
        return false;
    }

    ui.input_mut(|input| {
        let events = std::mem::take(&mut input.events);
        let mut select_all = false;
        for event in events {
            match event {
                egui::Event::Key {
                    key: Key::A,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.ctrl && !modifiers.alt => select_all = true,
                event => input.events.push(event),
            }
        }
        select_all
    })
}

fn take_delete_selection_event(ui: &mut egui::Ui, editor_id: egui::Id) -> bool {
    if !ui.memory(|memory| memory.has_focus(editor_id)) {
        return false;
    }

    ui.input_mut(|input| {
        let events = std::mem::take(&mut input.events);
        let mut delete_pressed = false;
        for event in events {
            match event {
                egui::Event::Key {
                    key: Key::Delete | Key::Backspace,
                    pressed: true,
                    modifiers,
                    ..
                } if !modifiers.ctrl && !modifiers.alt && !modifiers.command => {
                    delete_pressed = true;
                }
                event => input.events.push(event),
            }
        }
        delete_pressed
    })
}

fn text_edit_selection(
    ctx: &egui::Context,
    editor_id: egui::Id,
    buffer_chars: usize,
) -> Range<usize> {
    let Some(cursor_range) = egui::text_edit::TextEditState::load(ctx, editor_id)
        .and_then(|state| state.cursor.char_range())
    else {
        return buffer_chars..buffer_chars;
    };
    let range = cursor_range.as_sorted_char_range();
    range.start.0.min(buffer_chars)..range.end.0.min(buffer_chars)
}

fn char_slice(text: &str, range: Range<usize>) -> Result<String> {
    let start = char_to_byte_index(text, range.start).context("selection start is invalid")?;
    let end = char_to_byte_index(text, range.end).context("selection end is invalid")?;
    Ok(text[start..end].to_owned())
}

fn replace_char_range(text: &mut String, range: Range<usize>, replacement: &str) -> Result<()> {
    let start = char_to_byte_index(text, range.start).context("selection start is invalid")?;
    let end = char_to_byte_index(text, range.end).context("selection end is invalid")?;
    text.replace_range(start..end, replacement);
    Ok(())
}

fn char_to_byte_index(text: &str, char_index: usize) -> Option<usize> {
    if char_index == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
}

pub fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = Color32::from_rgb(245, 245, 245);
    style.visuals.window_fill = Color32::from_rgb(250, 250, 250);
    style.visuals.selection.bg_fill = Color32::from_rgb(51, 153, 255);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(230, 240, 255);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(232, 240, 248);
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(14.0, FontFamily::Monospace),
    );
    ctx.set_style_of(egui::Theme::Light, style);
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::{
        RustEditorApp, calculate_expression, char_slice, format_calculator_result,
        replace_char_range, take_delete_selection_event, take_enter_event, take_paste_event,
        take_select_all_event,
    };

    #[test]
    fn selection_helpers_use_character_not_byte_indices() {
        let mut text = "aé日z".to_owned();
        assert_eq!(char_slice(&text, 1..3).expect("valid range"), "é日");

        replace_char_range(&mut text, 1..3, "OK").expect("valid replacement");

        assert_eq!(text, "aOKz");
    }

    #[test]
    fn large_paste_is_intercepted_before_text_edit_layout() {
        let context = egui::Context::default();
        let editor_id = egui::Id::new("paste-test-editor");
        let mut line_buffer = String::new();

        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui)
                .response
                .request_focus();
        });
        output.textures_delta.clear();

        let pasted = "large line\n".repeat(200_000);
        let input = egui::RawInput {
            events: vec![egui::Event::Paste(pasted.clone())],
            ..Default::default()
        };
        let mut intercepted = None;
        let mut output = context.run_ui(input, |ui| {
            intercepted = take_paste_event(ui, editor_id);
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui);
        });
        output.textures_delta.clear();

        assert_eq!(intercepted.as_deref(), Some(pasted.as_str()));
        assert!(line_buffer.is_empty());
    }

    #[test]
    fn enter_is_intercepted_before_single_line_text_edit_consumes_it() {
        let context = egui::Context::default();
        let editor_id = egui::Id::new("enter-test-editor");
        let mut line_buffer = "left right".to_owned();

        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui)
                .response
                .request_focus();
        });
        output.textures_delta.clear();

        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut intercepted = false;
        let mut output = context.run_ui(input, |ui| {
            intercepted = take_enter_event(ui, editor_id);
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui);
        });
        output.textures_delta.clear();

        assert!(intercepted);
        assert_eq!(line_buffer, "left right");
    }

    #[test]
    fn ctrl_a_is_intercepted_for_the_active_line_editor() {
        let context = egui::Context::default();
        let editor_id = egui::Id::new("select-all-test-editor");
        let mut line_buffer = "select all".to_owned();

        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui)
                .response
                .request_focus();
        });
        output.textures_delta.clear();

        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::A,
                physical_key: Some(egui::Key::A),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            }],
            ..Default::default()
        };
        let mut intercepted = false;
        let mut output = context.run_ui(input, |ui| {
            intercepted = take_select_all_event(ui, editor_id);
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui);
        });
        output.textures_delta.clear();

        assert!(intercepted);
        assert_eq!(line_buffer, "select all");
    }

    #[test]
    fn delete_is_intercepted_when_the_line_editor_has_a_selection() {
        let context = egui::Context::default();
        let editor_id = egui::Id::new("delete-selection-test-editor");
        let mut line_buffer = "selected".to_owned();

        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            let mut output = egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui);
            output
                .state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(8),
                )));
            output.state.store(ui.ctx(), editor_id);
            output.response.request_focus();
        });
        output.textures_delta.clear();

        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Delete,
                physical_key: Some(egui::Key::Delete),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut intercepted = false;
        let mut output = context.run_ui(input, |ui| {
            intercepted = take_delete_selection_event(ui, editor_id);
            egui::TextEdit::singleline(&mut line_buffer)
                .id(editor_id)
                .show(ui);
        });
        output.textures_delta.clear();

        assert!(intercepted);
        assert_eq!(line_buffer, "selected");
    }

    #[test]
    fn deleting_active_selection_updates_the_document() {
        let mut app = RustEditorApp::blank();
        app.line_buffer = "before selected after".to_owned();

        app.delete_active_selection(7..15);

        assert_eq!(app.document.text(), "before  after");
        assert_eq!(app.line_buffer, "before  after");
        assert!(app.selection.is_none());
    }

    #[test]
    fn calculator_supports_precedence_parentheses_and_unary_signs() {
        assert_eq!(
            calculate_expression("(12.5 + 3) * -2").expect("expression should parse"),
            -31.0
        );
        assert_eq!(
            format_calculator_result(
                calculate_expression("10 / 4").expect("expression should parse")
            ),
            "2.5"
        );
        assert!(calculate_expression("1 / 0").is_err());
    }

    #[test]
    fn full_active_page_can_be_promoted_to_document_selection() {
        let mut app = RustEditorApp::blank();
        app.line_buffer = "select all".to_owned();
        app.selection = Some(0..10);
        assert!(app.active_page_is_fully_selected());

        let context = egui::Context::default();
        app.select_entire_document(&context);

        assert!(app.document_selected);
        assert!(app.selection.is_none());
    }

    #[test]
    fn enter_splits_the_line_at_a_unicode_character_cursor() {
        let mut app = RustEditorApp::blank();
        app.line_buffer = "ab日cd".to_owned();

        app.split_active_line(3..3);

        assert_eq!(app.document.text(), "ab日\ncd");
        assert_eq!(app.active_line, 1);
        assert_eq!(app.line_buffer, "cd");
    }

    #[test]
    fn enter_replaces_selected_text_with_a_line_break() {
        let mut app = RustEditorApp::blank();
        app.line_buffer = "before selected after".to_owned();

        app.split_active_line(7..15);

        assert_eq!(app.document.text(), "before \n after");
        assert_eq!(app.line_buffer, " after");
    }

    #[test]
    fn bookmarks_follow_single_line_insertions_and_deletions() {
        let mut app = RustEditorApp::blank();
        app.bookmarks.extend([1, 3]);

        app.shift_bookmarks_after_insert(2);
        assert_eq!(app.bookmarks.iter().copied().collect::<Vec<_>>(), [1, 4]);

        app.shift_bookmarks_after_delete(1);
        assert_eq!(app.bookmarks.iter().copied().collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn deleting_bookmarked_lines_updates_the_document_and_clears_bookmarks() {
        let mut app = RustEditorApp::blank();
        app.document.replace_all("zero\none\ntwo\nthree");
        app.load_active_line();
        app.bookmarks.extend([1, 3]);

        app.delete_bookmarked_lines();

        assert_eq!(app.document.text(), "zero\ntwo");
        assert!(app.bookmarks.is_empty());
        assert_eq!(app.active_line, 1);
    }
}
