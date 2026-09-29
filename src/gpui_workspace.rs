use std::{
    cell::RefCell,
    collections::BTreeSet,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, mpsc::Receiver},
};

use gpui::{
    App, Application, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, ScrollStrategy, ScrollWheelEvent, ShapedLine, Style, TextRun, UTF16Selection,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, actions, anchored, canvas,
    deferred, div, fill, point, prelude::*, px, rgb, rgba, size, uniform_list,
};
use rfd::FileDialog;

use re_core::{
    document::{DiskStatus, Document, SaveConflict},
    editor::{EditOutcome, EditorState, LogicalLines, Motion, VerticalLayout, shift_lines},
    huge::{HUGE_FILE_BYTES, HugeFile, LineIndex, SAMPLE_EVERY},
    paste,
    text_io::{Encoding, LineEnding, Loaded, WriteMode},
};
use re_ext_calculator as calculator;
use re_ext_transforms::{self as transforms, CaseTransform};
use re_search::{self as search, SearchQuery, SearchResults};
use re_view::{DisplayMap, WrapLayout};

use re_platform::instance::InstanceServer;
use re_tasks::{
    CancelToken, Latest,
    wake::{self, WakingSender},
};

use crate::ai::{AiRequest, ProviderKind, SourceDocument};

const PREVIEW_CHARS: usize = 1_024;
const ROW_HEIGHT: f32 = 26.0;
const GUTTER_WIDTH: f32 = 60.0;
const MENU_BAR_HEIGHT: f32 = 28.0;
const MENU_TITLE_HEIGHT: f32 = 22.0;
/// Most characters shaped per visible row (horizontal virtualization).
const ROW_WINDOW_CHARS: usize = 1_024;
/// Documents up to this many chars are wrapped on the UI thread; larger ones
/// on a worker (~30M chars/s in release builds).
const SYNC_WRAP_CHARS: usize = 1_000_000;
/// Cached wrapped segments (lines) kept for rendering.
const SEGMENT_CACHE_LINES: usize = 4_096;

/// Wrap width in character cells for a text area.
fn wrap_columns_for(width: Pixels, char_width: Pixels) -> usize {
    if char_width <= px(0.) {
        return 80;
    }
    ((width / char_width) as usize).saturating_sub(1).max(8)
}

actions!(
    rusteditor,
    [
        New,
        Open,
        Save,
        SaveAs,
        Enter,
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Escape
    ]
);

/// Runs a registered command by id. Every keybinding dispatches this action;
/// the palette and toolbar call [`Workspace::run_command`] directly.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = rusteditor, no_json)]
pub struct RunCommand(pub String);

const DEFAULT_KEYMAP: &str = include_str!("../assets/default-keymap.toml");

/// The resolved keymap, kept for palette labels.
#[derive(Default)]
struct KeymapState {
    resolved: re_commands::Resolved,
}

impl gpui::Global for KeymapState {}

/// Installed plugins and remembered approvals.
#[derive(Default)]
struct PluginState {
    registry: re_plugin::Registry,
    approvals: re_plugin::approvals::Approvals,
    /// Where plugins were loaded from (for messages).
    dir: Option<PathBuf>,
}

impl gpui::Global for PluginState {}

/// Approval key for sending document text to GitHub Copilot.
const COPILOT_EGRESS_KEY: &str = "builtin:copilot:send-document";

/// Loads plugins from `dir` (default: `<config>/plugins`) and approvals
/// from `approvals` (default: `<config>/approvals.txt`). Returns problems.
fn load_plugins(cx: &mut App, dir: Option<PathBuf>, approvals: Option<PathBuf>) -> Vec<String> {
    let config = re_platform::paths::config_dir();
    let dir = dir.or_else(|| config.as_ref().map(|c| c.join("plugins")));
    let approvals = approvals.or_else(|| config.map(|c| c.join("approvals.txt")));
    let registry = dir
        .as_deref()
        .map(re_plugin::Registry::load)
        .unwrap_or_default();
    let errors = registry.errors.clone();
    cx.set_global(PluginState {
        registry,
        approvals: re_plugin::approvals::Approvals::load(approvals),
        dir,
    });
    errors
}

/// Built-in commands plus every plugin command.
fn command_registry(cx: &App) -> re_commands::Registry {
    let mut registry = re_commands::Registry::builtin();
    if let Some(state) = cx.try_global::<PluginState>() {
        for plugin in &state.registry.plugins {
            for command in &plugin.manifest.commands {
                let _ = registry.register(re_commands::CommandInfo {
                    id: plugin.command_id(command).into(),
                    title: command.title.clone().into(),
                    category: format!("Plugin: {}", plugin.manifest.name).into(),
                    hidden: false,
                });
            }
        }
    }
    registry
}

/// Resolves the default + user keymaps and (re)binds all keys. `user`
/// replaces reading the user keymap file (used when it is open in a tab,
/// because the tab's lock prevents reading it from disk on Windows).
/// Returns keymap problems for the status bar.
fn bind_keymap(cx: &mut App, user: Option<String>) -> Vec<String> {
    let platform = re_commands::Platform::current();
    let mut errors = Vec::new();
    let user = user.or_else(|| {
        let path = re_platform::paths::keymap_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                errors.push(format!("cannot read {}: {error}", path.display()));
                None
            }
        }
    });
    let resolved = re_commands::keymap::resolve(
        DEFAULT_KEYMAP,
        user.as_deref(),
        &command_registry(cx),
        platform,
    );
    errors.extend(resolved.errors.iter().cloned());
    let mut bindings = Vec::with_capacity(resolved.bindings.len());
    for (context, keys, command) in &resolved.bindings {
        let predicate = context
            .predicate()
            .and_then(|p| gpui::KeyBindingContextPredicate::parse(p).ok())
            .map(std::rc::Rc::new);
        match KeyBinding::load(
            keys,
            Box::new(RunCommand(command.clone())),
            predicate,
            false,
            None,
            &gpui::DummyKeyboardMapper,
        ) {
            Ok(binding) => bindings.push(binding),
            Err(error) => errors.push(format!("keymap: {keys}: {error}")),
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
    cx.set_global(KeymapState { resolved });
    errors
}

/// Summarizes keymap problems for the status bar.
fn keymap_status(errors: &[String], ok: &str) -> String {
    match errors {
        [] => ok.to_owned(),
        [only] => format!("Keymap: {only}"),
        [first, rest @ ..] => format!("Keymap: {first} (+{} more)", rest.len()),
    }
}

type CommandHandler = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

/// Maps a command id to its handler. Every built-in command must have one
/// (checked by a test).
fn command_handler(id: &str) -> Option<CommandHandler> {
    use TransformOp::{Base64, Case, Json};
    Some(match id {
        "file.new" => |w, win, cx| w.new_action(&New, win, cx),
        "file.open" => |w, win, cx| w.open_action(&Open, win, cx),
        "file.save" => |w, win, cx| w.save_action(&Save, win, cx),
        "file.save_as" => |w, win, cx| w.save_as_action(&SaveAs, win, cx),
        "edit.undo" => |w, win, cx| w.undo_action(&Undo, win, cx),
        "edit.redo" => |w, win, cx| w.redo_action(&Redo, win, cx),
        "edit.cut" => |w, win, cx| w.cut(&Cut, win, cx),
        "edit.copy" => |w, win, cx| w.copy(&Copy, win, cx),
        "edit.paste" => |w, win, cx| w.paste(&Paste, win, cx),
        "edit.select_all" => |w, win, cx| w.select_all(&SelectAll, win, cx),
        "edit.insert_line" => |w, _, cx| w.insert_line(cx),
        "edit.delete_line" => |w, _, cx| w.delete_current_line(cx),
        "search.find" => |w, win, cx| w.start_find(win, cx),
        "search.next" => |w, _, cx| w.next_match(true, cx),
        "search.previous" => |w, _, cx| w.next_match(false, cx),
        "search.replace" => |w, win, cx| w.start_command(CommandMode::Replace, win, cx),
        "search.replace_all" => |w, win, cx| w.start_command(CommandMode::ReplaceAll, win, cx),
        "search.toggle_case" => |w, _, cx| w.toggle_search_option(false, cx),
        "search.toggle_regex" => |w, _, cx| w.toggle_search_option(true, cx),
        "bookmark.match" => |w, _, cx| w.bookmark_match(false, cx),
        "bookmark.all_matches" => |w, _, cx| w.bookmark_match(true, cx),
        "bookmark.copy" => |w, _, cx| w.copy_bookmarks(cx),
        "bookmark.cut" => |w, _, cx| w.delete_bookmarks(true, cx),
        "bookmark.delete" => |w, _, cx| w.delete_bookmarks(false, cx),
        "bookmark.clear" => |w, _, cx| w.clear_bookmarks(cx),
        "transform.toggle_scope" => |w, _, cx| w.toggle_scope(cx),
        "transform.json_view" => |w, _, cx| w.view_json(cx),
        "transform.json_format" => |w, _, cx| w.transform(Json, cx),
        "transform.base64_decode" => |w, _, cx| w.transform(Base64, cx),
        "transform.lower" => |w, _, cx| w.transform(Case(CaseTransform::Lower), cx),
        "transform.upper" => |w, _, cx| w.transform(Case(CaseTransform::Upper), cx),
        "transform.proper" => |w, _, cx| w.transform(Case(CaseTransform::Proper), cx),
        "transform.camel" => |w, _, cx| w.transform(Case(CaseTransform::Camel), cx),
        "transform.pascal" => |w, _, cx| w.transform(Case(CaseTransform::Pascal), cx),
        "transform.snake" => |w, _, cx| w.transform(Case(CaseTransform::Snake), cx),
        "transform.kebab" => |w, _, cx| w.transform(Case(CaseTransform::Kebab), cx),
        "view.close_viewer" => |w, _, cx| w.close_viewer(cx),
        "view.toggle_word_wrap" => |w, _, cx| w.toggle_word_wrap(cx),
        "file.reload" => |w, _, cx| w.reload_from_disk(cx),
        "file.encoding_utf8" => |w, _, cx| w.set_encoding(Encoding::Utf8, cx),
        "file.encoding_utf8_bom" => |w, _, cx| w.set_encoding(Encoding::Utf8Bom, cx),
        "file.encoding_utf16le" => |w, _, cx| w.set_encoding(Encoding::Utf16Le, cx),
        "file.encoding_utf16be" => |w, _, cx| w.set_encoding(Encoding::Utf16Be, cx),
        "file.encoding_windows1252" => |w, _, cx| w.set_encoding(Encoding::Windows1252, cx),
        "file.line_endings_lf" => |w, _, cx| w.set_line_endings(LineEnding::Lf, cx),
        "file.line_endings_crlf" => |w, _, cx| w.set_line_endings(LineEnding::CrLf, cx),
        "edit.add_cursor_above" => |w, _, cx| w.add_cursor_vertical(false, cx),
        "edit.add_cursor_below" => |w, _, cx| w.add_cursor_vertical(true, cx),
        "tools.calculator" => |w, win, cx| w.start_command(CommandMode::Calculator, win, cx),
        "tools.ask_ai" => |w, win, cx| w.ask_ai(win, cx),
        "plugins.reload" => |w, _, cx| w.reload_plugins(cx),
        "plugins.forget_approvals" => |w, _, cx| w.forget_approvals(cx),
        "app.command_palette" => |w, win, cx| w.start_command(CommandMode::Palette, win, cx),
        "app.open_keymap" => |w, _, cx| w.open_keymap(cx),
        "app.reload_keymap" => |w, _, cx| w.reload_keymap(cx),
        "editor.newline" => |w, win, cx| w.enter(&Enter, win, cx),
        "editor.backspace" => |w, win, cx| w.backspace(&Backspace, win, cx),
        "editor.delete" => |w, win, cx| w.delete(&Delete, win, cx),
        "editor.left" => |w, win, cx| w.left(&Left, win, cx),
        "editor.right" => |w, win, cx| w.right(&Right, win, cx),
        "editor.select_left" => |w, win, cx| w.select_left(&SelectLeft, win, cx),
        "editor.select_right" => |w, win, cx| w.select_right(&SelectRight, win, cx),
        "editor.up" => |w, _, cx| w.motion(Motion::Up, false, cx),
        "editor.down" => |w, _, cx| w.motion(Motion::Down, false, cx),
        "editor.select_up" => |w, _, cx| w.motion(Motion::Up, true, cx),
        "editor.select_down" => |w, _, cx| w.motion(Motion::Down, true, cx),
        "editor.word_left" => |w, _, cx| w.motion(Motion::WordLeft, false, cx),
        "editor.word_right" => |w, _, cx| w.motion(Motion::WordRight, false, cx),
        "editor.select_word_left" => |w, _, cx| w.motion(Motion::WordLeft, true, cx),
        "editor.select_word_right" => |w, _, cx| w.motion(Motion::WordRight, true, cx),
        "editor.page_up" => |w, _, cx| w.motion(Motion::PageUp(0), false, cx),
        "editor.page_down" => |w, _, cx| w.motion(Motion::PageDown(0), false, cx),
        "editor.select_page_up" => |w, _, cx| w.motion(Motion::PageUp(0), true, cx),
        "editor.select_page_down" => |w, _, cx| w.motion(Motion::PageDown(0), true, cx),
        "editor.home" => |w, win, cx| w.home(&Home, win, cx),
        "editor.end" => |w, win, cx| w.end(&End, win, cx),
        "editor.select_home" => |w, _, cx| w.motion(Motion::LineStart, true, cx),
        "editor.select_end" => |w, _, cx| w.motion(Motion::LineEnd, true, cx),
        "editor.doc_start" => |w, _, cx| w.motion(Motion::DocStart, false, cx),
        "editor.doc_end" => |w, _, cx| w.motion(Motion::DocEnd, false, cx),
        "editor.select_doc_start" => |w, _, cx| w.motion(Motion::DocStart, true, cx),
        "editor.select_doc_end" => |w, _, cx| w.motion(Motion::DocEnd, true, cx),
        "editor.cancel" => |w, win, cx| w.escape_action(&Escape, win, cx),
        "app.menu.file" => |w, _, cx| w.open_menu_named("File", cx),
        "app.menu.edit" => |w, _, cx| w.open_menu_named("Edit", cx),
        "app.menu.search" => |w, _, cx| w.open_menu_named("Search", cx),
        "app.menu.view" => |w, _, cx| w.open_menu_named("View", cx),
        "app.menu.transform" => |w, _, cx| w.open_menu_named("Transform", cx),
        "app.menu.bookmarks" => |w, _, cx| w.open_menu_named("Bookmarks", cx),
        "app.menu.tools" => |w, _, cx| w.open_menu_named("Tools", cx),
        "app.menu.plugins" => |w, _, cx| w.open_menu_named("Plugins", cx),
        "edit.go_to_line" => |w, win, cx| w.start_command(CommandMode::GoToLine, win, cx),
        "file.open_fully" => |w, _, cx| w.open_fully(cx),
        _ => return None,
    })
}

/// Maximum number of palette results shown.
const PALETTE_ROWS: usize = 10;

enum TabState {
    Loading,
    Ready(Document),
    /// A file too large to load: read-only view over positioned reads.
    Huge(Box<HugeView>),
    Failed(String),
}

/// Most bytes of a line shown in huge-file mode (no horizontal scroll).
const HUGE_DISPLAY_BYTES: usize = 2048;
/// Most bytes copied or searched per line in huge-file mode.
const HUGE_LINE_BYTES: usize = 1024 * 1024;
/// Most bytes put on the clipboard from a huge file.
const HUGE_COPY_BYTES: usize = 16 * 1024 * 1024;

struct HugeView {
    file: Arc<HugeFile>,
    index: LineIndex,
    /// Stops the background line scan when the tab goes away.
    cancel: CancelToken,
    error: Option<String>,
    /// The highlighted line (keyboard/mouse position).
    current_line: usize,
}

impl Drop for HugeView {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

enum LoadResult {
    Doc(Document),
    Huge(Arc<HugeFile>, CancelToken),
}

struct IndexMessage {
    tab_id: u64,
    result: anyhow::Result<LineIndex>,
}

struct Tab {
    id: u64,
    path: Option<PathBuf>,
    state: TabState,
    bookmarks: BTreeSet<usize>,
    ai_response: Option<ropey::Rope>,
    /// Cursor and selection over the whole document (char offsets).
    editor: EditorState,
    /// First visible column (chars) for horizontal scrolling.
    h_scroll: usize,
    /// Wrapped display map, valid for `WrapState::revision`.
    wrap: Option<WrapState>,
    /// Background wrap build in progress.
    wrap_job: Option<WrapJob>,
    /// The last external change we told the user about (not reloaded
    /// because the tab had unsaved edits), so it is reported once.
    reported_disk_change: Option<DiskStatus>,
    /// A save was refused because the file changed on disk; saving again
    /// overwrites it.
    confirm_overwrite: bool,
}

struct WrapState {
    map: DisplayMap,
    revision: u64,
}

struct WrapJob {
    revision: u64,
    columns: usize,
    cancel: CancelToken,
}

/// `(document revision, wrap columns, tab id)` and the cached segments of
/// each line for that key.
type SegmentCache = (
    (u64, usize, u64),
    std::collections::HashMap<usize, Vec<Range<usize>>>,
);

struct ReloadMessage {
    tab_id: u64,
    revision: u64,
    result: anyhow::Result<Loaded>,
}

struct WrapMessage {
    tab_id: u64,
    revision: u64,
    map: DisplayMap,
}

impl Tab {
    fn new(id: u64, path: Option<PathBuf>, state: TabState) -> Self {
        Self {
            id,
            path,
            state,
            bookmarks: BTreeSet::new(),
            ai_response: None,
            editor: EditorState::default(),
            h_scroll: 0,
            wrap: None,
            wrap_job: None,
            reported_disk_change: None,
            confirm_overwrite: false,
        }
    }
}

struct LoadMessage {
    id: u64,
    result: anyhow::Result<LoadResult>,
}

struct PasteMessage {
    tab_id: u64,
    revision: u64,
    whole_document: bool,
    result: anyhow::Result<paste::PasteResult>,
}

struct SearchMessage {
    tab_id: u64,
    revision: u64,
    generation: u64,
    result: anyhow::Result<SearchResults>,
}

struct ReplaceMessage {
    tab_id: u64,
    revision: u64,
    result: anyhow::Result<(ropey::Rope, usize)>,
}

struct AiMessage {
    tab_id: u64,
    result: anyhow::Result<String>,
}

struct PluginMessage {
    tab_id: u64,
    revision: u64,
    title: String,
    input: re_plugin::Input,
    result: anyhow::Result<re_plugin::Outcome>,
}

#[derive(Clone, Copy)]
enum CommandMode {
    Palette,
    GoToLine,
    Find,
    Ask,
    Calculator,
    Replace,
    ReplaceAll,
}

#[derive(Clone, Copy)]
enum TransformOp {
    Case(CaseTransform),
    Base64,
    Json,
}

/// A document row painted in the last frame, for hit-testing and IME bounds.
struct PaintedRow {
    /// Index in the list (display row when wrapping, else line).
    display_row: usize,
    line: usize,
    /// Char offset in the document of the first shaped character.
    start_char: usize,
    /// Char offset just after the last shaped character.
    end_char: usize,
    /// Whether this row ends its line (always true without wrapping).
    last_segment: bool,
    /// The shaped (visible) slice of the line.
    text: String,
    layout: ShapedLine,
    bounds: Bounds<Pixels>,
}

/// Geometry from the last paint. Filled by row elements during prepaint.
#[derive(Default)]
struct PaintCache {
    rows: Vec<PaintedRow>,
    /// Width of one character cell, for horizontal scrolling.
    char_width: Option<Pixels>,
    /// Width of the text area (excluding the gutter).
    text_width: Option<Pixels>,
}

impl PaintCache {
    /// The painted row showing document offset `pos` on `line`.
    fn row_for_pos(&self, line: usize, pos: usize) -> Option<&PaintedRow> {
        self.rows.iter().find(|row| {
            row.line == line
                && pos >= row.start_char
                && (pos < row.end_char || (row.last_segment && pos <= row.end_char))
        })
    }

    /// The document char offset under `position`, clamped to painted rows.
    /// `None` if no rows are painted or the point is left of the text.
    fn char_at(&self, position: Point<Pixels>) -> Option<usize> {
        let first = self.rows.iter().min_by_key(|row| row.display_row)?;
        let last = self.rows.iter().max_by_key(|row| row.display_row)?;
        let row = self
            .rows
            .iter()
            .find(|row| position.y >= row.bounds.top() && position.y < row.bounds.bottom())
            .unwrap_or(if position.y < first.bounds.top() {
                first
            } else {
                last
            });
        if position.x < row.bounds.left() {
            return None;
        }
        let byte = row
            .layout
            .closest_index_for_x(position.x - row.bounds.left())
            .min(row.text.len());
        let pos = row.start_char + row.text[..byte].chars().count();
        // The end of a non-final wrapped row belongs to the next row.
        if !row.last_segment && pos >= row.end_char {
            return Some(row.end_char.saturating_sub(1).max(row.start_char));
        }
        Some(pos)
    }

    /// First and last painted list rows.
    fn visible_rows(&self) -> Option<(usize, usize)> {
        let first = self.rows.iter().map(|row| row.display_row).min()?;
        let last = self.rows.iter().map(|row| row.display_row).max()?;
        Some((first, last))
    }
}

pub struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    sender: WakingSender<LoadMessage>,
    receiver: Receiver<LoadMessage>,
    paste_sender: WakingSender<PasteMessage>,
    paste_receiver: Receiver<PasteMessage>,
    search_sender: WakingSender<SearchMessage>,
    search_receiver: Receiver<SearchMessage>,
    ai_sender: WakingSender<AiMessage>,
    ai_receiver: Receiver<AiMessage>,
    replace_sender: WakingSender<ReplaceMessage>,
    replace_receiver: Receiver<ReplaceMessage>,
    incoming: Receiver<Vec<PathBuf>>,
    /// Sends paths to this window from outside (macOS "open with").
    incoming_sender: WakingSender<Vec<PathBuf>>,
    focus: FocusHandle,
    // Command bar (single-line input). The document editor state lives in
    // each `Tab::editor`.
    content: String,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    layout: Option<ShapedLine>,
    bounds: Option<Bounds<Pixels>>,
    /// Mouse drag in progress (command bar or document).
    selecting: bool,
    paint_cache: Rc<RefCell<PaintCache>>,
    /// Soft wrap on/off (all tabs).
    word_wrap: bool,
    wrap_sender: WakingSender<WrapMessage>,
    wrap_receiver: Receiver<WrapMessage>,
    reload_sender: WakingSender<ReloadMessage>,
    reload_receiver: Receiver<ReloadMessage>,
    plugin_sender: WakingSender<PluginMessage>,
    plugin_receiver: Receiver<PluginMessage>,
    index_sender: WakingSender<IndexMessage>,
    index_receiver: Receiver<IndexMessage>,
    /// Files at least this large open in huge-file mode.
    huge_threshold: u64,
    /// List rows rendered last (huge-file mode keeps the line visible).
    huge_rows: Range<usize>,
    /// A command waiting for the user to confirm an approval (run it again).
    pending_approval: Option<String>,
    /// Index into `re_commands::menus::MENUS` of the open menu.
    open_menu: Option<usize>,
    /// Highlighted row in the open menu (keyboard/mouse).
    menu_highlight: Option<usize>,
    /// Window bounds of each menu title from the last paint; dropdowns open
    /// just below them.
    menu_titles: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>>,
    /// The running search / Replace All; a new one cancels the old.
    search_job: Latest,
    replace_job: Latest,
    /// Wrapped segments per line, for `(revision, columns, tab id)`.
    segment_cache: SegmentCache,
    status: String,
    selection_scope: bool,
    json_viewer: Option<ropey::Rope>,
    command: Option<CommandMode>,
    search_query: Option<SearchQuery>,
    search_results: Option<SearchResults>,
    search_index: usize,
    search_revision: u64,
    search_generation: u64,
    case_sensitive: bool,
    regex: bool,
    replacement: String,
    list_scroll: UniformListScrollHandle,
}

impl Workspace {
    fn new(paths: Vec<PathBuf>, server: Option<InstanceServer>, cx: &mut Context<Self>) -> Self {
        let (waker, mut wakes) = wake::channel();
        let (sender, receiver) = waker.message_channel();
        let (paste_sender, paste_receiver) = waker.message_channel();
        let (search_sender, search_receiver) = waker.message_channel();
        let (ai_sender, ai_receiver) = waker.message_channel();
        let (replace_sender, replace_receiver) = waker.message_channel();
        let (incoming_sender, incoming) = waker.message_channel();
        let (wrap_sender, wrap_receiver) = waker.message_channel();
        let (reload_sender, reload_receiver) = waker.message_channel();
        let (plugin_sender, plugin_receiver) = waker.message_channel();
        let (index_sender, index_receiver) = waker.message_channel();
        let mut status = String::new();
        let server_sender = incoming_sender.clone();
        if let Some(server) = server
            && let Err(error) = server.start(move |paths| server_sender.send(paths).is_ok())
        {
            status = format!("Instance listener failed: {error}");
        }
        let mut workspace = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            sender,
            receiver,
            paste_sender,
            paste_receiver,
            search_sender,
            search_receiver,
            ai_sender,
            ai_receiver,
            replace_sender,
            replace_receiver,
            incoming,
            incoming_sender,
            focus: cx.focus_handle(),
            content: String::new(),
            selected: 0..0,
            reversed: false,
            marked: None,
            layout: None,
            bounds: None,
            selecting: false,
            paint_cache: Rc::default(),
            word_wrap: false,
            wrap_sender,
            wrap_receiver,
            reload_sender,
            reload_receiver,
            plugin_sender,
            plugin_receiver,
            index_sender,
            index_receiver,
            huge_threshold: HUGE_FILE_BYTES,
            huge_rows: 0..0,
            pending_approval: None,
            open_menu: None,
            menu_highlight: None,
            menu_titles: Rc::default(),
            search_job: Latest::default(),
            replace_job: Latest::default(),
            segment_cache: ((u64::MAX, 0, 0), Default::default()),
            status,
            selection_scope: true,
            json_viewer: None,
            command: None,
            search_query: None,
            search_results: None,
            search_index: 0,
            search_revision: 0,
            search_generation: 0,
            case_sensitive: false,
            regex: false,
            replacement: String::new(),
            list_scroll: UniformListScrollHandle::default(),
        };
        if paths.is_empty() {
            workspace.add_blank();
        } else {
            for path in paths {
                workspace.open_path(path);
            }
        }
        // Worker results wake this task directly: no polling timer, so results
        // apply immediately and an idle editor uses no CPU.
        drop(waker);
        cx.spawn(async move |this, cx| {
            while wakes.wait().await {
                if this.update(cx, |workspace, cx| workspace.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        workspace
    }

    fn add_blank(&mut self) {
        self.tabs.push(Tab::new(
            self.next_id,
            None,
            TabState::Ready(Document::default()),
        ));
        self.next_id += 1;
        self.active = self.tabs.len() - 1;
        self.paint_cache.borrow_mut().rows.clear();
    }

    fn open_path(&mut self, path: PathBuf) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.path.as_deref() == Some(path.as_path()))
        {
            self.active = index;
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.tabs
            .push(Tab::new(id, Some(path.clone()), TabState::Loading));
        self.active = self.tabs.len() - 1;
        self.paint_cache.borrow_mut().rows.clear();
        self.start_loader(self.active, path, false);
    }

    /// Loads `path` into tab `index` on a worker. Files at least
    /// `huge_threshold` bytes (unless `full`) open in huge-file mode: the tab
    /// is usable at once while the line index is built in the background.
    fn start_loader(&mut self, index: usize, path: PathBuf, full: bool) {
        let id = self.tabs[index].id;
        self.tabs[index].state = TabState::Loading;
        let sender = self.sender.clone();
        let panic_sender = self.sender.clone();
        let index_sender = self.index_sender.clone();
        let threshold = self.huge_threshold;
        let job = move || -> anyhow::Result<()> {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if full || size < threshold {
                let result = Document::open(&path).map(LoadResult::Doc);
                let _ = sender.send(LoadMessage { id, result });
                return Ok(());
            }
            let file = match HugeFile::open(&path) {
                Ok(file) => Arc::new(file),
                Err(error) => {
                    let _ = sender.send(LoadMessage {
                        id,
                        result: Err(error),
                    });
                    return Ok(());
                }
            };
            let cancel = CancelToken::new();
            let _ = sender.send(LoadMessage {
                id,
                result: Ok(LoadResult::Huge(file, cancel.clone())),
            });
            let progress = index_sender.clone();
            let result =
                re_core::huge::build_index(&path, SAMPLE_EVERY, Some(cancel.flag()), |index| {
                    let _ = progress.send(IndexMessage {
                        tab_id: id,
                        result: Ok(index.clone()),
                    });
                });
            if let Err(error) = result {
                let _ = index_sender.send(IndexMessage {
                    tab_id: id,
                    result: Err(error),
                });
            }
            Ok(())
        };
        // A panic while loading fails this tab only (see `re_tasks::spawn`).
        let started = re_tasks::spawn(
            format!("rusteditor-loader-{id}"),
            None,
            job,
            move |result| {
                if let Err(error) = result {
                    let _ = panic_sender.send(LoadMessage {
                        id,
                        result: Err(error),
                    });
                }
            },
        );
        if let Err(error) = started {
            self.tabs[index].state = TabState::Failed(format!("Cannot start loader: {error}"));
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(message) = self.receiver.try_recv() {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.id) {
                tab.state = match message.result {
                    Ok(LoadResult::Doc(document)) => TabState::Ready(document),
                    Ok(LoadResult::Huge(file, cancel)) => TabState::Huge(Box::new(HugeView {
                        file,
                        index: LineIndex::default(),
                        cancel,
                        error: None,
                        current_line: 0,
                    })),
                    Err(error) => TabState::Failed(format!("{error:#}")),
                };
                changed = true;
            }
        }
        while let Ok(message) = self.index_receiver.try_recv() {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id)
                && let TabState::Huge(view) = &mut tab.state
            {
                match message.result {
                    Ok(index) => view.index = index,
                    Err(error) => view.error = Some(format!("Indexing failed: {error:#}")),
                }
                changed = true;
            }
        }
        while let Ok(paths) = self.incoming.try_recv() {
            for path in paths {
                self.open_path(path);
            }
            changed = true;
        }
        while let Ok(message) = self.paste_receiver.try_recv() {
            let active_id = self.tabs[self.active].id;
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id) else {
                continue;
            };
            let TabState::Ready(document) = &mut tab.state else {
                continue;
            };
            match message.result {
                Ok(result) => {
                    let previous_lines = document.line_count();
                    let first_line = document
                        .rope()
                        .char_to_line(tab.editor.range().start.min(document.rope().len_chars()));
                    if document.replace_rope_if_revision(result.rope, message.revision) {
                        if message.whole_document {
                            tab.bookmarks.clear();
                            tab.editor.set_cursor(document, 0);
                        } else {
                            if result.line_count_changed {
                                let delta =
                                    document.line_count() as isize - previous_lines as isize;
                                adjust_bookmarks(&mut tab.bookmarks, first_line, delta);
                            }
                            tab.editor.set_cursor(document, result.cursor_char);
                        }
                        if active_id == message.tab_id {
                            self.search_results = None;
                            self.ensure_cursor_visible();
                        }
                        self.status = "Pasted".into();
                    } else {
                        self.status = "Paste not applied: document changed while pasting".into();
                    }
                }
                Err(error) => self.status = format!("Paste failed: {error:#}"),
            }
            changed = true;
        }
        while let Ok(message) = self.search_receiver.try_recv() {
            if self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.id == message.tab_id)
                && message.generation == self.search_generation
                && (self.huge().is_some()
                    || self
                        .document()
                        .is_some_and(|doc| doc.revision() == message.revision))
            {
                match message.result {
                    Ok(results) => {
                        self.status = format!(
                            "{} matches{}",
                            results.matches.len(),
                            if results.truncated {
                                " (truncated)"
                            } else {
                                ""
                            }
                        );
                        self.search_results = Some(results);
                        self.search_revision = message.revision;
                        self.search_index = 0;
                        self.navigate_match(0);
                    }
                    Err(error) => self.status = format!("Search failed: {error:#}"),
                }
                changed = true;
            }
        }
        while let Ok(message) = self.ai_receiver.try_recv() {
            let active_id = self.tabs[self.active].id;
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id) {
                match message.result {
                    Ok(response) => {
                        let response = ropey::Rope::from_str(&response);
                        tab.ai_response = Some(response.clone());
                        if active_id == message.tab_id {
                            self.json_viewer = Some(response);
                            self.status = "Copilot response".into();
                        }
                    }
                    Err(error) => {
                        if active_id == message.tab_id {
                            self.status = format!("Copilot failed: {error:#}");
                        }
                    }
                }
                changed = true;
            }
        }
        while let Ok(message) = self.replace_receiver.try_recv() {
            let active_id = self.tabs[self.active].id;
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id) else {
                continue;
            };
            let TabState::Ready(doc) = &mut tab.state else {
                continue;
            };
            match message.result {
                Ok((rope, count)) => {
                    let old_lines = doc.line_count();
                    if count == 0 {
                        if active_id == message.tab_id {
                            self.status = "No matches to replace".into();
                        }
                    } else if doc.replace_rope_if_revision(rope, message.revision) {
                        if doc.line_count() != old_lines {
                            tab.bookmarks.clear();
                        }
                        tab.editor.clamp(doc);
                        if active_id == message.tab_id {
                            self.search_results = None;
                            self.status = format!("Replaced {count} matches");
                        }
                    } else {
                        self.status = "Replace All not applied: document changed".into();
                    }
                }
                Err(error) => self.status = format!("Replace All failed: {error:#}"),
            }
            changed = true;
        }
        while let Ok(message) = self.plugin_receiver.try_recv() {
            self.apply_plugin_result(message);
            changed = true;
        }
        while let Ok(message) = self.reload_receiver.try_recv() {
            let active_id = self.tabs[self.active].id;
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id) else {
                continue;
            };
            let TabState::Ready(doc) = &mut tab.state else {
                continue;
            };
            let name = doc.display_name();
            match message.result {
                Ok(loaded) => {
                    if doc.reload_if_revision(loaded, message.revision) {
                        tab.bookmarks.clear();
                        tab.editor.clamp(doc);
                        tab.reported_disk_change = None;
                        tab.confirm_overwrite = false;
                        if active_id == message.tab_id {
                            self.search_results = None;
                            self.status = format!("Reloaded {name} from disk");
                        }
                    } else {
                        self.status = format!("Reload of {name} skipped: it was edited meanwhile");
                    }
                }
                Err(error) => self.status = format!("Reload failed: {error:#}"),
            }
            changed = true;
        }
        while let Ok(message) = self.wrap_receiver.try_recv() {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id)
                && let TabState::Ready(doc) = &tab.state
                && doc.revision() == message.revision
                && tab
                    .wrap_job
                    .as_ref()
                    .is_some_and(|job| job.revision == message.revision)
            {
                tab.wrap_job = None;
                tab.wrap = Some(WrapState {
                    map: message.map,
                    revision: message.revision,
                });
                if self.status == "Wrapping..." {
                    self.status.clear();
                }
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn document(&self) -> Option<&Document> {
        match &self.tabs.get(self.active)?.state {
            TabState::Ready(document) => Some(document),
            _ => None,
        }
    }

    fn document_mut(&mut self) -> Option<&mut Document> {
        match &mut self.tabs.get_mut(self.active)?.state {
            TabState::Ready(document) => Some(document),
            _ => None,
        }
    }

    /// The active tab's huge-file view, if it is one.
    fn huge(&self) -> Option<&HugeView> {
        match &self.tabs.get(self.active)?.state {
            TabState::Huge(view) => Some(view),
            _ => None,
        }
    }

    fn huge_mut(&mut self) -> Option<&mut HugeView> {
        match &mut self.tabs.get_mut(self.active)?.state {
            TabState::Huge(view) => Some(view),
            _ => None,
        }
    }

    /// Refuses edits in huge-file mode with an explanation.
    fn refuse_huge_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.huge().is_none() {
            return false;
        }
        self.status = "This file is too large to edit here; it is open read-only. \
                       Use \"Open Fully for Editing\" to load it into memory."
            .into();
        cx.notify();
        true
    }

    /// Moves the highlighted line in huge-file mode and keeps it in view.
    fn huge_jump(&mut self, line: usize) {
        let rows = self.huge_rows.clone();
        let Some(view) = self.huge_mut() else {
            return;
        };
        let last = view.index.lines().saturating_sub(1);
        let line = line.min(last);
        view.current_line = line;
        let strategy = if line < rows.start {
            ScrollStrategy::Top
        } else if line + 1 >= rows.end && rows.end > rows.start {
            ScrollStrategy::Bottom
        } else if rows.is_empty() {
            ScrollStrategy::Center
        } else {
            return;
        };
        self.list_scroll.scroll_to_item(line, strategy);
    }

    /// Text of huge-file lines, capped in total.
    fn huge_lines_text(&self, lines: &[usize]) -> anyhow::Result<String> {
        let view = self
            .huge()
            .ok_or_else(|| anyhow::anyhow!("not a huge file"))?;
        let mut text = String::new();
        for &line in lines {
            if text.len() >= HUGE_COPY_BYTES {
                break;
            }
            if let Some((line_text, _)) = view.file.line_text(&view.index, line, HUGE_LINE_BYTES)? {
                text.push_str(&line_text);
                text.push('\n');
            }
        }
        Ok(text)
    }

    /// The active tab's editor state.
    fn editor(&self) -> Option<&EditorState> {
        let tab = self.tabs.get(self.active)?;
        matches!(tab.state, TabState::Ready(_)).then_some(&tab.editor)
    }

    /// Runs `f` with the active document and its editor state.
    fn with_editor<R>(
        &mut self,
        f: impl FnOnce(&mut Document, &mut EditorState) -> R,
    ) -> Option<R> {
        let tab = self.tabs.get_mut(self.active)?;
        match &mut tab.state {
            TabState::Ready(document) => Some(f(document, &mut tab.editor)),
            _ => None,
        }
    }

    /// Updates bookmarks, the wrap map and view state after a document edit.
    /// `revision` is the document revision before the edit.
    fn after_edit(&mut self, outcome: EditOutcome, revision: u64) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let marks = std::mem::take(&mut tab.bookmarks);
            tab.bookmarks = shift_lines(marks, &outcome.lines).into_iter().collect();
            if let TabState::Ready(doc) = &tab.state
                && let Some(wrap) = &mut tab.wrap
                && wrap.revision == revision
            {
                wrap.map.apply(doc.rope(), &outcome.lines);
                wrap.revision = doc.revision();
            }
        }
        self.search_results = None;
        self.ensure_cursor_visible();
    }

    /// Edits the active document with `f`, reporting read-only and failures.
    fn edit(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Document, &mut EditorState) -> Option<EditOutcome>,
    ) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        if self.document().is_some_and(Document::is_read_only) {
            self.status = "File is read-only".into();
            cx.notify();
            return;
        }
        let revision = self.document().map_or(0, Document::revision);
        if let Some(outcome) = self.with_editor(f).flatten() {
            self.after_edit(outcome, revision);
        }
        cx.notify();
    }

    /// The active tab's wrap map, if wrapping is on and it is current.
    fn wrap_map(&self) -> Option<&DisplayMap> {
        if !self.word_wrap {
            return None;
        }
        let tab = self.tabs.get(self.active)?;
        let TabState::Ready(doc) = &tab.state else {
            return None;
        };
        tab.wrap
            .as_ref()
            .filter(|wrap| wrap.revision == doc.revision())
            .map(|wrap| &wrap.map)
    }

    /// Runs `f` with the document, editor state and the vertical layout
    /// (wrapped display rows or logical lines).
    fn with_layout<R>(
        &mut self,
        f: impl FnOnce(&Document, &mut EditorState, &dyn VerticalLayout) -> R,
    ) -> Option<R> {
        let word_wrap = self.word_wrap;
        let tab = self.tabs.get_mut(self.active)?;
        let TabState::Ready(doc) = &tab.state else {
            return None;
        };
        let wrap = tab
            .wrap
            .as_ref()
            .filter(|wrap| word_wrap && wrap.revision == doc.revision());
        Some(match wrap {
            Some(wrap) => f(doc, &mut tab.editor, &WrapLayout { map: &wrap.map }),
            None => f(doc, &mut tab.editor, &LogicalLines),
        })
    }

    /// Current wrap width in cells, from the last paint.
    fn wrap_columns(&self) -> usize {
        let cache = self.paint_cache.borrow();
        match (cache.text_width, cache.char_width) {
            (Some(width), Some(char_width)) => wrap_columns_for(width, char_width),
            _ => 80,
        }
    }

    /// Makes the active tab's wrap map current: small documents are wrapped
    /// immediately, large ones on a worker (rendering falls back to no wrap
    /// until it finishes).
    fn sync_wrap(&mut self) {
        if !self.word_wrap {
            return;
        }
        let columns = self.wrap_columns();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let TabState::Ready(doc) = &tab.state else {
            return;
        };
        let revision = doc.revision();
        if tab
            .wrap
            .as_ref()
            .is_some_and(|wrap| wrap.revision == revision && wrap.map.columns() == columns)
        {
            return;
        }
        if tab
            .wrap_job
            .as_ref()
            .is_some_and(|job| job.revision == revision && job.columns == columns)
        {
            return;
        }
        if let Some(job) = tab.wrap_job.take() {
            job.cancel.cancel();
        }
        if doc.rope().len_chars() <= SYNC_WRAP_CHARS {
            tab.wrap =
                DisplayMap::build(doc.rope(), columns, None).map(|map| WrapState { map, revision });
            return;
        }
        let rope = doc.rope().clone();
        let cancel = CancelToken::new();
        let job_cancel = cancel.clone();
        let sender = self.wrap_sender.clone();
        let tab_id = tab.id;
        let started = re_tasks::spawn(
            format!("rusteditor-wrap-{tab_id}"),
            Some(cancel.clone()),
            move || {
                DisplayMap::build(&rope, columns, Some(job_cancel.flag()))
                    .ok_or_else(|| anyhow::anyhow!("cancelled"))
            },
            move |result| {
                if let Ok(map) = result {
                    let _ = sender.send(WrapMessage {
                        tab_id,
                        revision,
                        map,
                    });
                }
            },
        );
        match started {
            Ok(()) => {
                tab.wrap_job = Some(WrapJob {
                    revision,
                    columns,
                    cancel,
                });
                self.status = "Wrapping...".into();
            }
            Err(error) => self.status = format!("Cannot start wrapping: {error}"),
        }
    }

    /// Wrapped segments of `line` (cached for the current revision/width).
    fn line_segments(&mut self, line: usize) -> Vec<Range<usize>> {
        let Some((key, rope, columns)) = self.tabs.get(self.active).and_then(|tab| {
            let TabState::Ready(doc) = &tab.state else {
                return None;
            };
            let columns = tab.wrap.as_ref()?.map.columns();
            Some(((doc.revision(), columns, tab.id), doc.rope(), columns))
        }) else {
            return std::iter::once(0..usize::MAX).collect();
        };
        let (cache_key, cache) = &mut self.segment_cache;
        if *cache_key != key || cache.len() > SEGMENT_CACHE_LINES {
            *cache_key = key;
            cache.clear();
        }
        cache
            .entry(line)
            .or_insert_with(|| re_view::segments(rope, line, columns))
            .clone()
    }

    fn toggle_word_wrap(&mut self, cx: &mut Context<Self>) {
        self.word_wrap = !self.word_wrap;
        for tab in &mut self.tabs {
            tab.h_scroll = 0;
        }
        self.status = if self.word_wrap {
            "Word wrap on"
        } else {
            "Word wrap off"
        }
        .into();
        self.paint_cache.borrow_mut().rows.clear();
        self.ensure_cursor_visible();
        cx.notify();
    }

    fn add_cursor_vertical(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.command.is_some() {
            return;
        }
        self.with_layout(|doc, editor, layout| editor.add_cursor_vertical(doc, layout, down));
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Moves the cursor in the active document.
    fn motion(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) {
        if self.command.is_some() {
            return;
        }
        if let Some(view) = self.huge() {
            let page = self.huge_rows.len().saturating_sub(1).max(1);
            let line = view.current_line;
            let target = match motion {
                Motion::Up => line.saturating_sub(1),
                Motion::Down => line + 1,
                Motion::PageUp(_) => line.saturating_sub(page),
                Motion::PageDown(_) => line + page,
                Motion::DocStart => 0,
                Motion::DocEnd => usize::MAX,
                _ => return,
            };
            self.huge_jump(target);
            cx.notify();
            return;
        }
        let motion = match motion {
            Motion::PageUp(_) => Motion::PageUp(self.page_lines()),
            Motion::PageDown(_) => Motion::PageDown(self.page_lines()),
            other => other,
        };
        self.with_layout(|doc, editor, layout| editor.move_by_in(doc, layout, motion, extend));
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Rows per page, from the last paint.
    fn page_lines(&self) -> usize {
        self.paint_cache
            .borrow()
            .visible_rows()
            .map_or(20, |(first, last)| (last - first).saturating_sub(1).max(1))
    }

    /// Places the cursor at a document char offset and scrolls to it.
    fn select_global_position(&mut self, position: usize) {
        self.with_editor(|doc, editor| editor.set_cursor(doc, position));
        self.ensure_cursor_visible();
    }

    /// Scrolls vertically and horizontally so the cursor is visible.
    fn ensure_cursor_visible(&mut self) {
        let Some((line, column)) = self.document().zip(self.editor()).map(|(doc, editor)| {
            let rope = doc.rope();
            let head = editor.head().min(rope.len_chars());
            let row = match self.wrap_map() {
                Some(map) => WrapLayout { map }.row(rope, head),
                None => rope.char_to_line(head),
            };
            (row, head - rope.line_to_char(rope.char_to_line(head)))
        }) else {
            return;
        };
        let wrapping = self.wrap_map().is_some();
        let cache = self.paint_cache.borrow();
        match cache.visible_rows() {
            Some((first, _)) if line < first => {
                self.list_scroll.scroll_to_item(line, ScrollStrategy::Top)
            }
            // `last` may be partly visible; keep the cursor above it.
            Some((first, last)) if line >= last && last > first => self
                .list_scroll
                .scroll_to_item(line, ScrollStrategy::Bottom),
            Some(_) => {}
            None => self
                .list_scroll
                .scroll_to_item(line, ScrollStrategy::Center),
        }
        let columns = match (cache.text_width, cache.char_width) {
            (Some(width), Some(char_width)) if char_width > px(0.) => {
                ((width / char_width) as usize).max(8)
            }
            _ => 80,
        };
        drop(cache);
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if wrapping {
            tab.h_scroll = 0;
            return;
        }
        let margin = (columns / 4).min(16);
        if column < tab.h_scroll {
            tab.h_scroll = column.saturating_sub(margin);
        } else if column + 2 > tab.h_scroll + columns {
            // Put the cursor `margin` columns from the right edge.
            tab.h_scroll = (column + 2 + margin).saturating_sub(columns);
        }
    }

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(paths) = FileDialog::new().set_title("Open files").pick_files() {
            for path in paths {
                self.open_path(path)
            }
            cx.notify();
        }
    }

    fn save_document(&mut self, force_path: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let TabState::Ready(document) = &mut tab.state else {
            return;
        };
        let result = if force_path || document.path().is_none() {
            let Some(path) = FileDialog::new().set_title("Save file").save_file() else {
                return;
            };
            document.save_as(&path).map(|()| WriteMode::Atomic)
        } else {
            // A second Save after a conflict warning overwrites.
            document.save_checked(tab.confirm_overwrite)
        };
        tab.confirm_overwrite = false;
        self.status = match result {
            Ok(mode) => {
                tab.reported_disk_change = None;
                if let Some(path) = document.path().map(Path::to_path_buf) {
                    tab.path = Some(path);
                }
                match mode {
                    WriteMode::Atomic => "Saved".to_owned(),
                    WriteMode::InPlace => {
                        "Saved (in place: another program has the file open)".to_owned()
                    }
                }
            }
            Err(error) if error.downcast_ref::<SaveConflict>().is_some() => {
                tab.confirm_overwrite = true;
                "The file changed on disk. Save again to overwrite it, or Reload from Disk"
                    .to_owned()
            }
            Err(error) => format!("Save failed: {error:#}"),
        };
        cx.notify();
    }

    /// Re-reads the active file on a worker; applied if not edited meanwhile.
    fn reload_from_disk(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.huge().map(|view| view.file.path().to_path_buf()) {
            self.tabs[self.active].reported_disk_change = None;
            self.search_results = None;
            self.start_loader(self.active, path, false);
            self.status = "Re-reading the file...".into();
            cx.notify();
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let TabState::Ready(doc) = &tab.state else {
            return;
        };
        let Some(path) = doc.path().map(Path::to_path_buf) else {
            self.status = "Untitled documents have no file to reload".into();
            cx.notify();
            return;
        };
        let (tab_id, revision) = (tab.id, doc.revision());
        let sender = self.reload_sender.clone();
        let started = re_tasks::spawn(
            format!("rusteditor-reload-{tab_id}"),
            None,
            move || re_core::text_io::load(&path),
            move |result| {
                let _ = sender.send(ReloadMessage {
                    tab_id,
                    revision,
                    result,
                });
            },
        );
        self.status = match started {
            Ok(()) => "Reloading...".into(),
            Err(error) => format!("Cannot start reload: {error}"),
        };
        cx.notify();
    }

    /// Checks the active file for changes made by other programs: a clean
    /// document reloads automatically; a modified one is warned about once.
    fn check_disk(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if let TabState::Huge(view) = &tab.state {
            let now = re_core::text_io::DiskState::of(view.file.path()).ok();
            let status = match now {
                Some(now) if now == view.file.disk() => DiskStatus::Unchanged,
                Some(_) => DiskStatus::Modified,
                None => DiskStatus::Missing,
            };
            if status != DiskStatus::Unchanged && tab.reported_disk_change != Some(status) {
                tab.reported_disk_change = Some(status);
                self.status = "This file changed on disk; the view may be out of date. \
                               Reload from Disk re-reads it."
                    .into();
                cx.notify();
            }
            return;
        }
        let TabState::Ready(doc) = &tab.state else {
            return;
        };
        let status = doc.disk_status();
        let name = doc.display_name();
        let dirty = doc.is_dirty();
        match status {
            DiskStatus::Unchanged => tab.reported_disk_change = None,
            _ if tab.reported_disk_change == Some(status) => {}
            DiskStatus::Modified if !dirty => {
                tab.reported_disk_change = Some(status);
                self.reload_from_disk(cx);
            }
            DiskStatus::Modified => {
                tab.reported_disk_change = Some(status);
                self.status = format!(
                    "{name} changed on disk. Reload from Disk discards your edits; Save asks before overwriting"
                );
                cx.notify();
            }
            DiskStatus::Missing => {
                tab.reported_disk_change = Some(status);
                self.status = format!("{name} was deleted or moved; Save to recreate it");
                cx.notify();
            }
        }
    }

    fn set_encoding(&mut self, encoding: Encoding, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        if let Some(doc) = self.document_mut() {
            if doc.is_read_only() {
                self.status = "File is read-only".into();
            } else {
                doc.set_encoding(encoding);
                self.status = format!("Will save as {}", encoding.label());
            }
        }
        cx.notify();
    }

    fn set_line_endings(&mut self, ending: LineEnding, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        let converted = self
            .with_editor(|doc, editor| {
                let ok = doc.convert_line_endings(ending);
                editor.clamp(doc);
                ok
            })
            .unwrap_or(false);
        self.status = if converted {
            format!("Line endings: {}", ending.label())
        } else {
            "File is read-only".into()
        };
        cx.notify();
    }

    fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.command = None;
        self.active = index;
        self.paint_cache.borrow_mut().rows.clear();
        self.json_viewer = self.tabs[index].ai_response.clone();
        self.check_disk(cx);
        cx.notify();
    }

    fn new_action(&mut self, _: &New, _: &mut Window, cx: &mut Context<Self>) {
        self.add_blank();
        cx.notify();
    }
    fn open_action(&mut self, _: &Open, _: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(cx);
    }
    fn save_action(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save_document(false, cx);
    }
    fn save_as_action(&mut self, _: &SaveAs, _: &mut Window, cx: &mut Context<Self>) {
        self.save_document(true, cx);
    }

    // ----- Command bar (single-line input) helpers -----

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }
    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = index..index;
        self.reversed = false;
        cx.notify();
    }
    fn select_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let anchor = if self.reversed {
            self.selected.end
        } else {
            self.selected.start
        };
        self.reversed = index < anchor;
        self.selected = anchor.min(index)..anchor.max(index);
        cx.notify();
    }
    fn previous(&self, index: usize) -> usize {
        self.content[..index]
            .char_indices()
            .last()
            .map_or(0, |(i, _)| i)
    }
    fn next(&self, index: usize) -> usize {
        self.content[index..]
            .chars()
            .next()
            .map_or(index, |ch| index + ch.len_utf8())
    }

    // ----- Editing actions (command bar when open, otherwise document) -----

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_none() {
            return self.motion(Motion::Left, false, cx);
        }
        let index = if self.selected.is_empty() {
            self.previous(self.cursor())
        } else {
            self.selected.start
        };
        self.move_to(index, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_none() {
            return self.motion(Motion::Right, false, cx);
        }
        let index = if self.selected.is_empty() {
            self.next(self.cursor())
        } else {
            self.selected.end
        };
        self.move_to(index, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_none() {
            return self.motion(Motion::Left, true, cx);
        }
        self.select_to(self.previous(self.cursor()), cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_none() {
            return self.motion(Motion::Right, true, cx);
        }
        self.select_to(self.next(self.cursor()), cx);
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_none() {
            return self.motion(Motion::LineStart, false, cx);
        }
        self.move_to(0, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_none() {
            return self.motion(Motion::LineEnd, false, cx);
        }
        self.move_to(self.content.len(), cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            self.selected = 0..self.content.len();
            cx.notify();
            return;
        }
        self.with_editor(|doc, editor| editor.select_all(doc));
        cx.notify();
    }
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            if self.selected.is_empty() {
                self.select_to(self.previous(self.cursor()), cx)
            }
            self.replace_text_in_range(None, "", window, cx);
            return;
        }
        self.edit(cx, |doc, editor| editor.backspace(doc));
    }
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            if self.selected.is_empty() {
                self.select_to(self.next(self.cursor()), cx)
            }
            self.replace_text_in_range(None, "", window, cx);
            return;
        }
        self.edit(cx, |doc, editor| editor.delete_forward(doc));
    }
    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            self.finish_command(window, cx);
            return;
        }
        self.edit(cx, |doc, editor| {
            let ending = doc.line_ending().as_str();
            editor.insert(doc, ending)
        });
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if self.command.is_some() {
            self.replace_text_in_range(None, &text, window, cx);
            return;
        }
        self.insert_document_text(&text, cx);
    }
    /// Inserts `text` over the selection; very large text is spliced on a
    /// worker so the UI never blocks.
    fn insert_document_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        let Some((range, len)) = self
            .document()
            .zip(self.editor())
            .map(|(doc, editor)| (editor.range(), doc.rope().len_chars()))
        else {
            return;
        };
        if self.document().is_some_and(Document::is_read_only) {
            self.status = "File is read-only".into();
        } else if text.len() > 1_048_576 {
            let whole = range.start == 0 && range.end == len;
            self.queue_paste(range.start, range.end, text.to_owned(), whole);
        } else {
            // One clipboard line per cursor when the counts match.
            let cursors = self.editor().map_or(1, EditorState::cursor_count);
            let trimmed = text.strip_suffix('\n').unwrap_or(text);
            let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
            let lines: Vec<String> = trimmed
                .split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
                .collect();
            if cursors > 1 && lines.len() == cursors {
                return self.edit(cx, |doc, editor| editor.insert_each(doc, lines, false));
            }
            return self.edit(cx, |doc, editor| editor.insert(doc, text));
        }
        cx.notify();
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            if !self.selected.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    self.content[self.selected.clone()].to_owned(),
                ));
            }
            return;
        }
        if let Some(line) = self.huge().map(|view| view.current_line) {
            // Huge-file mode copies the highlighted line.
            match self.huge_lines_text(&[line]) {
                Ok(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.status = format!("Copied line {}", line + 1);
                }
                Err(error) => self.status = format!("Copy failed: {error:#}"),
            }
            cx.notify();
            return;
        }
        // Copying a document-wide selection materializes it (documented cost).
        // Multiple selections are joined with line breaks.
        if let Some(text) = self
            .document()
            .zip(self.editor())
            .filter(|(_, editor)| !editor.all_empty())
            .map(|(doc, editor)| editor.selected_texts(doc).join("\n"))
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        let has_selection = if self.command.is_some() {
            !self.selected.is_empty()
        } else {
            self.editor().is_some_and(|editor| !editor.all_empty())
        };
        if !has_selection {
            return;
        }
        self.copy(&Copy, window, cx);
        if self.command.is_some() {
            self.replace_text_in_range(None, "", window, cx);
        } else {
            self.edit(cx, |doc, editor| editor.backspace(doc));
        }
    }

    fn queue_paste(&mut self, start: usize, end: usize, pasted: String, whole_document: bool) {
        let Some(document) = self.document() else {
            return;
        };
        let (rope, revision) = document.search_snapshot();
        let tab_id = self.tabs[self.active].id;
        let sender = self.paste_sender.clone();
        let result = re_tasks::spawn(
            format!("rusteditor-paste-{tab_id}"),
            None,
            move || paste::apply(rope, start, end, &pasted),
            move |result| {
                let _ = sender.send(PasteMessage {
                    tab_id,
                    revision,
                    whole_document,
                    result,
                });
            },
        );
        self.status = match result {
            Ok(()) => "Pasting in background...".into(),
            Err(error) => format!("Cannot start paste: {error}"),
        };
    }

    fn start_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_command(CommandMode::Find, window, cx);
    }

    fn start_command(&mut self, mode: CommandMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            return;
        }
        self.command = Some(mode);
        self.content = match mode {
            CommandMode::Find => self
                .search_query
                .as_ref()
                .map_or_else(String::new, |query| query.text.clone()),
            CommandMode::Replace | CommandMode::ReplaceAll => self.replacement.clone(),
            CommandMode::Ask
            | CommandMode::Calculator
            | CommandMode::Palette
            | CommandMode::GoToLine => String::new(),
        };
        self.selected = self.content.len()..self.content.len();
        self.reversed = false;
        self.marked = None;
        self.layout = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn cancel_command(&mut self, cx: &mut Context<Self>) {
        self.command = None;
        self.content.clear();
        self.selected = 0..0;
        self.marked = None;
        self.layout = None;
        cx.notify();
    }

    fn finish_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.command;
        let input = self.content.clone();
        if matches!(command, Some(CommandMode::Palette)) {
            let registry = command_registry(cx);
            let best = re_commands::palette::filter(&registry, &input)
                .first()
                .map(|command| command.id.to_string());
            self.cancel_command(cx);
            match best {
                Some(id) => self.run_command(&id, window, cx),
                None => {
                    self.status = format!("No command matches '{input}'");
                    cx.notify();
                }
            }
            return;
        }
        if matches!(command, Some(CommandMode::Calculator)) {
            match calculator::calculate(&input) {
                Ok(result) => {
                    self.content = result.clone();
                    self.selected = self.content.len()..self.content.len();
                    self.status = format!("{input} = {result}");
                }
                Err(error) => self.status = format!("Calculator: {error:#}"),
            }
            cx.notify();
            return;
        }
        let (saved_selection, focus_line) = self
            .document()
            .zip(self.editor())
            .map(|(doc, editor)| {
                let selection = (!editor.is_empty()).then(|| editor.selected_text(doc));
                let rope = doc.rope();
                (
                    selection,
                    Some(rope.char_to_line(editor.head().min(rope.len_chars()))),
                )
            })
            .unwrap_or((None, None));
        self.cancel_command(cx);
        match command {
            Some(CommandMode::Find) => self.run_find(input, cx),
            Some(CommandMode::Ask) => self.run_ai(input, saved_selection, focus_line, cx),
            Some(CommandMode::Replace) => {
                self.replacement = input;
                self.replace_current(cx);
            }
            Some(CommandMode::ReplaceAll) => {
                self.replacement = input;
                self.replace_all(cx);
            }
            Some(CommandMode::GoToLine) => self.go_to_line(&input, cx),
            Some(CommandMode::Calculator | CommandMode::Palette) | None => {}
        }
    }

    fn run_ai(
        &mut self,
        question: String,
        selection: Option<String>,
        line: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if question.trim().is_empty() {
            self.status = "Enter a question for Copilot".into();
            cx.notify();
            return;
        }
        let Some(doc) = self.document() else { return };
        let (rope, _) = doc.search_snapshot();
        let name = doc.display_name();
        let focus = if self.selection_scope {
            line.map_or_else(
                || "Entire file".to_owned(),
                |line| format!("line {}", line + 1),
            )
        } else {
            "Entire file".to_owned()
        };
        let request = AiRequest {
            question,
            source: SourceDocument::Snapshot {
                rope,
                suggested_name: name,
            },
            focus,
            selected_text: if self.selection_scope {
                selection
            } else {
                None
            },
        };
        let tab_id = self.tabs[self.active].id;
        let sender = self.ai_sender.clone();
        match re_tasks::spawn(
            format!("rusteditor-ai-{tab_id}"),
            None,
            move || ProviderKind::CopilotCli.ask(request),
            move |result| {
                let _ = sender.send(AiMessage { tab_id, result });
            },
        ) {
            Ok(()) => self.status = "Asking Copilot...".into(),
            Err(error) => self.status = format!("Cannot start Copilot: {error}"),
        }
        cx.notify();
    }

    fn calculator_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if key == "C" {
            self.content.clear();
            self.selected = 0..0;
        } else if key == "=" {
            self.finish_command(window, cx);
        } else if key == "Backspace" {
            if let Some((index, _)) = self.content.char_indices().last() {
                self.content.truncate(index);
            }
            self.selected = self.content.len()..self.content.len();
        } else {
            self.content.push_str(key);
            self.selected = self.content.len()..self.content.len();
        }
        cx.notify();
    }

    fn run_find(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(path) = self.huge().map(|view| view.file.path().to_path_buf()) {
            return self.run_huge_find(text, path, cx);
        }
        let Some(doc) = self.document() else { return };
        let (rope, revision) = doc.search_snapshot();
        let tab_id = self.tabs[self.active].id;
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        let query = SearchQuery {
            text,
            case_sensitive: self.case_sensitive,
            regex: self.regex,
        };
        let sender = self.search_sender.clone();
        let work_query = query.clone();
        // A new search cancels the previous one.
        let cancel = self.search_job.restart();
        let job_cancel = cancel.clone();
        match re_tasks::spawn(
            format!("rusteditor-search-{tab_id}"),
            Some(cancel),
            move || {
                search::find_all_cancellable(&rope, &work_query, 100_000, Some(job_cancel.flag()))
            },
            move |result| {
                let _ = sender.send(SearchMessage {
                    tab_id,
                    revision,
                    generation,
                    result,
                });
            },
        ) {
            Ok(()) => {
                self.search_query = Some(query);
                self.search_results = None;
                self.status = "Searching...".into();
            }
            Err(error) => self.status = format!("Cannot start search: {error}"),
        }
        cx.notify();
    }

    /// Streams a huge file through the search on a worker (cancellable).
    fn run_huge_find(&mut self, text: String, path: PathBuf, cx: &mut Context<Self>) {
        let tab_id = self.tabs[self.active].id;
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        let query = SearchQuery {
            text,
            case_sensitive: self.case_sensitive,
            regex: self.regex,
        };
        let sender = self.search_sender.clone();
        let work_query = query.clone();
        let cancel = self.search_job.restart();
        let job_cancel = cancel.clone();
        let started = re_tasks::spawn(
            format!("rusteditor-search-{tab_id}"),
            Some(cancel),
            move || {
                let lines = HugeFile::lines(&path, HUGE_LINE_BYTES)?;
                search::find_in_lines(lines, &work_query, 100_000, Some(job_cancel.flag()))
            },
            move |result| {
                let _ = sender.send(SearchMessage {
                    tab_id,
                    revision: 0,
                    generation,
                    result,
                });
            },
        );
        match started {
            Ok(()) => {
                self.search_query = Some(query);
                self.search_results = None;
                self.status = "Searching the whole file...".into();
            }
            Err(error) => self.status = format!("Cannot start search: {error}"),
        }
        cx.notify();
    }

    fn navigate_match(&mut self, index: usize) {
        let Some(found) = self
            .search_results
            .as_ref()
            .and_then(|results| results.matches.get(index))
            .cloned()
        else {
            return;
        };
        self.search_index = index;
        if self.huge().is_some() {
            self.huge_rows = 0..0; // center the match
            self.huge_jump(found.line_index);
            return;
        }
        self.with_editor(|doc, editor| {
            editor.select(doc, found.global_start_char..found.global_end_char)
        });
        self.ensure_cursor_visible();
    }

    fn next_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(results) = &self.search_results else {
            return;
        };
        if results.matches.is_empty() {
            return;
        }
        let next = if forward {
            (self.search_index + 1) % results.matches.len()
        } else {
            (self.search_index + results.matches.len() - 1) % results.matches.len()
        };
        self.navigate_match(next);
        cx.notify();
    }

    fn replace_current(&mut self, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        let Some(query) = self.search_query.clone() else {
            self.status = "Find text before replacing".into();
            cx.notify();
            return;
        };
        let Some(found) = self
            .search_results
            .as_ref()
            .and_then(|results| results.matches.get(self.search_index))
            .cloned()
        else {
            self.status = "No current search match".into();
            cx.notify();
            return;
        };
        let Some(doc) = self.document() else { return };
        if doc.revision() != self.search_revision {
            self.status = "Search results are stale; search again".into();
            cx.notify();
            return;
        }
        let Some(matched) = doc.text_range(found.global_start_char, found.global_end_char) else {
            return;
        };
        let replacement = match search::replacement_for_match(&matched, &query, &self.replacement) {
            Ok(value) => value,
            Err(error) => {
                self.status = format!("Replace failed: {error:#}");
                cx.notify();
                return;
            }
        };
        let revision = self.search_revision;
        let Some(doc) = self.document_mut() else {
            return;
        };
        let old_lines = doc.line_count();
        if !doc.replace_global_range_if_revision(
            found.global_start_char,
            found.global_end_char,
            &replacement,
            revision,
        ) {
            self.status = "Cannot replace read-only or changed document".into();
        } else {
            let delta = doc.line_count() as isize - old_lines as isize;
            self.adjust_bookmarks(found.line_index, delta);
            self.select_global_position(found.global_start_char + replacement.chars().count());
            self.search_results = None;
            self.status = "Replaced match; searching again...".into();
            self.run_find(query.text, cx);
        }
        cx.notify();
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        let Some(query) = self.search_query.clone() else {
            self.status = "Find text before replacing".into();
            cx.notify();
            return;
        };
        let Some(doc) = self.document() else { return };
        if doc.is_read_only() {
            self.status = "File is read-only".into();
            cx.notify();
            return;
        }
        let (rope, revision) = doc.search_snapshot();
        let tab_id = self.tabs[self.active].id;
        let replacement = self.replacement.clone();
        let sender = self.replace_sender.clone();
        let cancel = self.replace_job.restart();
        let job_cancel = cancel.clone();
        match re_tasks::spawn(
            format!("rusteditor-replace-{tab_id}"),
            Some(cancel),
            move || {
                search::replace_all_cancellable(
                    &rope,
                    &query,
                    &replacement,
                    Some(job_cancel.flag()),
                )
            },
            move |result| {
                let _ = sender.send(ReplaceMessage {
                    tab_id,
                    revision,
                    result,
                });
            },
        ) {
            Ok(()) => self.status = "Replacing in background...".into(),
            Err(error) => self.status = format!("Cannot start Replace All: {error}"),
        }
        cx.notify();
    }

    fn bookmark_match(&mut self, all: bool, cx: &mut Context<Self>) {
        let Some(results) = &self.search_results else {
            return;
        };
        let lines: Vec<_> = if all {
            results
                .matches
                .iter()
                .map(|found| found.line_index)
                .collect()
        } else {
            results
                .matches
                .get(self.search_index)
                .map(|found| found.line_index)
                .into_iter()
                .collect()
        };
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.bookmarks.extend(lines);
            self.status = format!("{} bookmarked lines", tab.bookmarks.len());
            cx.notify();
        }
    }

    fn bookmarked_lines(&self) -> Vec<usize> {
        self.tabs
            .get(self.active)
            .map_or_else(Vec::new, |tab| tab.bookmarks.iter().copied().collect())
    }

    fn copy_bookmarks(&mut self, cx: &mut Context<Self>) {
        let lines = self.bookmarked_lines();
        if lines.is_empty() {
            self.status = "No bookmarked lines".into();
        } else if self.huge().is_some() {
            match self.huge_lines_text(&lines) {
                Ok(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.status = format!("Copied {} bookmarked lines", lines.len());
                }
                Err(error) => self.status = format!("Copy failed: {error:#}"),
            }
        } else if let Some(doc) = self.document() {
            cx.write_to_clipboard(ClipboardItem::new_string(doc.lines_text(&lines)));
            self.status = format!("Copied {} bookmarked lines", lines.len());
        }
        cx.notify();
    }

    fn delete_bookmarks(&mut self, cut: bool, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        let lines = self.bookmarked_lines();
        if lines.is_empty() {
            return;
        }
        if self.document().is_some_and(Document::is_read_only) {
            self.status = "File is read-only".into();
            cx.notify();
            return;
        }
        if cut {
            self.copy_bookmarks(cx)
        }
        if let Some(doc) = self.document_mut() {
            let deleted = doc.delete_lines(&lines);
            self.status = format!("Deleted {deleted} bookmarked lines");
        }
        self.tabs[self.active].bookmarks.clear();
        self.with_editor(|doc, editor| editor.clamp(doc));
        cx.notify();
    }

    fn adjust_bookmarks(&mut self, line: usize, delta: isize) {
        adjust_bookmarks(&mut self.tabs[self.active].bookmarks, line, delta);
    }

    fn escape_action(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.command.is_some() {
            return self.cancel_command(cx);
        }
        if self
            .tabs
            .get_mut(self.active)
            .is_some_and(|tab| tab.editor.collapse_to_primary())
        {
            cx.notify();
        }
    }

    fn undo_action(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_redo(false, cx);
    }

    fn redo_action(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_redo(true, cx);
    }

    /// Runs a registered command by id (keymap, palette, toolbar, plugins).
    pub fn run_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_menu.is_some() && !id.starts_with("app.menu.") && self.menu_key(id, window, cx)
        {
            return;
        }
        if id.starts_with("plugin.") {
            return self.run_plugin_command(id, cx);
        }
        // Any other command cancels a pending approval question.
        if self.pending_approval.as_deref() != Some(id) {
            self.pending_approval = None;
        }
        match command_handler(id) {
            Some(handler) => handler(self, window, cx),
            None => {
                self.status = format!("Unknown command '{id}'");
                cx.notify();
            }
        }
    }

    /// Asks once before running something that needs the user's trust.
    /// Returns `true` if approved (now or before). The first call explains
    /// and returns `false`; running the same command again approves.
    fn confirm(
        &mut self,
        command_id: &str,
        key: &str,
        question: String,
        cx: &mut Context<Self>,
    ) -> bool {
        // Without loaded plugin state (tests), approvals live in memory.
        if cx
            .default_global::<PluginState>()
            .approvals
            .is_approved(key)
        {
            return true;
        }
        if self.pending_approval.as_deref() == Some(command_id) {
            self.pending_approval = None;
            if let Err(error) = cx.default_global::<PluginState>().approvals.approve(key) {
                self.status = format!("Approved for this session (cannot save: {error})");
            }
            return true;
        }
        self.pending_approval = Some(command_id.to_owned());
        self.status = question;
        cx.notify();
        false
    }

    fn ask_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = format!(
            "{} sends this file's text (including unsaved edits) to GitHub Copilot over the network. \
             Run it again to allow; Forget Approvals revokes.",
            ProviderKind::ALL[0].display_name()
        );
        if self.confirm("tools.ask_ai", COPILOT_EGRESS_KEY, question, cx) {
            self.start_command(CommandMode::Ask, window, cx);
        }
    }

    fn run_plugin_command(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.command.is_some() {
            return;
        }
        let Some((plugin, command)) = cx
            .try_global::<PluginState>()
            .and_then(|state| state.registry.find(id))
        else {
            self.status = format!("Unknown plugin command '{id}'");
            cx.notify();
            return;
        };
        let manifest = &plugin.manifest;
        let question = if manifest.kind.is_sandboxed() {
            format!(
                "Run plugin \"{}\" {}? It is sandboxed: it only sees the text you select{} \
                 and cannot access files or the network. Run it again to allow.",
                manifest.name,
                manifest.version,
                if plugin.has(re_plugin::Capability::Document) {
                    " or the whole file"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "Plugin \"{}\" {} runs a program with FULL ACCESS to your files{}. \
                 Only allow plugins you trust. Run it again to allow.",
                manifest.name,
                manifest.version,
                if plugin.has(re_plugin::Capability::Network) {
                    " and sends your text over the network"
                } else {
                    ""
                }
            )
        };
        if !self.confirm(id, &plugin.approval_key(), question, cx) {
            return;
        }
        let Some((doc, editor)) = self.document().zip(self.editor()) else {
            return;
        };
        let inputs = match command.input {
            re_plugin::Input::Selection if editor.all_empty() => {
                self.status = format!("{}: select text first", command.title);
                cx.notify();
                return;
            }
            re_plugin::Input::Selection => editor.selected_texts(doc),
            re_plugin::Input::Document => vec![doc.text()],
        };
        let request = re_plugin::Request {
            inputs,
            path: doc.path().map(|p| p.to_string_lossy().into_owned()),
        };
        if request.size() > re_plugin::MAX_MESSAGE_BYTES {
            self.status = format!("{}: input is too large for a plugin", command.title);
            cx.notify();
            return;
        }
        let tab_id = self.tabs[self.active].id;
        let revision = doc.revision();
        let title = command.title.clone();
        let input = command.input;
        let sender = self.plugin_sender.clone();
        let started = re_tasks::spawn(
            format!("rusteditor-plugin-{}", manifest.id),
            None,
            move || re_plugin::run(&plugin, &command, &request),
            move |result| {
                let _ = sender.send(PluginMessage {
                    tab_id,
                    revision,
                    title,
                    input,
                    result,
                });
            },
        );
        self.status = match started {
            Ok(()) => format!("Running {}...", id),
            Err(error) => format!("Cannot start plugin: {error}"),
        };
        cx.notify();
    }

    /// Applies a plugin result as an ordinary undoable edit, only if the
    /// document is unchanged since the request.
    fn apply_plugin_result(&mut self, message: PluginMessage) {
        let title = message.title;
        let outcome = match message.result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.status = format!("{title} failed: {error:#}");
                return;
            }
        };
        let active = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.id == message.tab_id);
        let current = self
            .document()
            .is_some_and(|d| d.revision() == message.revision);
        if !active || !current {
            self.status = format!("{title}: not applied, the document changed meanwhile");
            return;
        }
        match outcome {
            re_plugin::Outcome::Show { text, status } => {
                self.json_viewer = Some(ropey::Rope::from_str(&text));
                self.status = status.unwrap_or_else(|| format!("{title} (read-only view)"));
            }
            re_plugin::Outcome::Replace { outputs, status } => {
                if self.document().is_some_and(Document::is_read_only) {
                    self.status = "File is read-only".into();
                    return;
                }
                let revision = message.revision;
                let applied = match message.input {
                    re_plugin::Input::Selection => self
                        .with_editor(|doc, editor| editor.insert_each(doc, outputs, true))
                        .flatten()
                        .map(|outcome| self.after_edit(outcome, revision))
                        .is_some(),
                    re_plugin::Input::Document => {
                        let text = outputs.into_iter().next().unwrap_or_default();
                        self.with_editor(|doc, editor| {
                            doc.replace_all(&text);
                            editor.set_cursor(doc, 0);
                        });
                        self.tabs[self.active].bookmarks.clear();
                        self.search_results = None;
                        true
                    }
                };
                self.status = if applied {
                    status.unwrap_or_else(|| format!("{title} applied"))
                } else {
                    format!("{title}: nothing changed")
                };
            }
        }
    }

    /// Jumps to a 1-based line number.
    fn go_to_line(&mut self, input: &str, cx: &mut Context<Self>) {
        let Ok(number) = input.trim().replace([',', '_', ' '], "").parse::<usize>() else {
            self.status = format!("'{input}' is not a line number");
            cx.notify();
            return;
        };
        let line = number.max(1) - 1;
        if self.huge().is_some() {
            self.huge_rows = 0..0;
            self.huge_jump(line);
        } else if let Some(start) = self.document().map(|doc| {
            let rope = doc.rope();
            rope.line_to_char(line.min(rope.len_lines() - 1))
        }) {
            self.select_global_position(start);
        }
        self.status = format!("Line {}", line + 1);
        cx.notify();
    }

    /// Loads a huge-file tab completely into memory so it can be edited
    /// (asks first: it needs several times the file size in RAM).
    fn open_fully(&mut self, cx: &mut Context<Self>) {
        let Some((path, size)) = self
            .huge()
            .map(|view| (view.file.path().to_path_buf(), view.file.len()))
        else {
            self.status = "Open Fully applies to huge files opened read-only".into();
            cx.notify();
            return;
        };
        if self.pending_approval.as_deref() != Some("file.open_fully") {
            self.pending_approval = Some("file.open_fully".into());
            self.status = format!(
                "Loading this {} file needs roughly {} of memory and may take a while. \
                 Run Open Fully again to continue.",
                format_bytes(size),
                format_bytes(size.saturating_mul(3))
            );
            cx.notify();
            return;
        }
        self.pending_approval = None;
        self.tabs[self.active].bookmarks.clear();
        self.search_results = None;
        self.start_loader(self.active, path, true);
        self.status = "Loading the whole file...".into();
        cx.notify();
    }

    fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        let dir = cx.try_global::<PluginState>().and_then(|s| s.dir.clone());
        let mut errors = load_plugins(cx, dir, None);
        errors.extend(bind_keymap(cx, None));
        let count = cx.global::<PluginState>().registry.plugins.len();
        self.status = match errors.as_slice() {
            [] => format!("{count} plugin(s) loaded"),
            [first, rest @ ..] => format!(
                "{count} plugin(s) loaded; {first}{}",
                if rest.is_empty() {
                    String::new()
                } else {
                    format!(" (+{} more)", rest.len())
                }
            ),
        };
        cx.notify();
    }

    fn forget_approvals(&mut self, cx: &mut Context<Self>) {
        let result = cx.default_global::<PluginState>().approvals.clear();
        self.pending_approval = None;
        self.status = match result {
            Ok(()) => "Approvals forgotten; plugins and Copilot will ask again".into(),
            Err(error) => format!("Cannot save approvals: {error}"),
        };
        cx.notify();
    }

    fn run_command_action(
        &mut self,
        action: &RunCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_command(&action.0, window, cx);
    }

    fn insert_line(&mut self, cx: &mut Context<Self>) {
        self.edit(cx, |doc, editor| editor.insert_line_below(doc));
    }

    fn delete_current_line(&mut self, cx: &mut Context<Self>) {
        self.edit(cx, |doc, editor| editor.delete_line(doc));
    }

    fn toggle_search_option(&mut self, regex: bool, cx: &mut Context<Self>) {
        if regex {
            self.regex = !self.regex;
        } else {
            self.case_sensitive = !self.case_sensitive;
        }
        self.search_results = None;
        self.search_generation = self.search_generation.wrapping_add(1);
        self.status = "Search options changed; run Find again".into();
        cx.notify();
    }

    fn clear_bookmarks(&mut self, cx: &mut Context<Self>) {
        self.tabs[self.active].bookmarks.clear();
        cx.notify();
    }

    fn toggle_scope(&mut self, cx: &mut Context<Self>) {
        self.selection_scope = !self.selection_scope;
        cx.notify();
    }

    fn close_viewer(&mut self, cx: &mut Context<Self>) {
        self.json_viewer = None;
        cx.notify();
    }

    fn open_keymap(&mut self, cx: &mut Context<Self>) {
        let Some(path) = re_platform::paths::keymap_path() else {
            self.status = "Cannot find the configuration directory".into();
            cx.notify();
            return;
        };
        if !path.exists() {
            let created = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, crate::config::USER_KEYMAP_TEMPLATE));
            if let Err(error) = created {
                self.status = format!("Cannot create {}: {error}", path.display());
                cx.notify();
                return;
            }
        }
        self.open_path(path);
        self.status = "Edit, save, then run Reload Keymap".into();
        cx.notify();
    }

    fn reload_keymap(&mut self, cx: &mut Context<Self>) {
        // If the keymap is open in a tab, read our buffer: the tab holds an
        // exclusive lock, so reading the file from disk fails on Windows.
        let path = re_platform::paths::keymap_path();
        let open_text = self.tabs.iter().find_map(|tab| match &tab.state {
            TabState::Ready(doc) if path.is_some() && doc.path() == path.as_deref() => {
                Some(doc.text())
            }
            _ => None,
        });
        let errors = bind_keymap(cx, open_text);
        self.status = keymap_status(&errors, "Keymap reloaded");
        cx.notify();
    }

    fn undo_redo(&mut self, redo: bool, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        // The command bar edits its own text, not the document.
        if self.command.is_some() {
            return;
        }
        let Some(document) = self.document_mut() else {
            return;
        };
        if document.is_read_only() {
            self.status = "File is read-only".into();
            cx.notify();
            return;
        }
        let applied = if redo {
            document.redo()
        } else {
            document.undo()
        };
        let Some(applied) = applied else {
            self.status = if redo {
                "Nothing to redo"
            } else {
                "Nothing to undo"
            }
            .into();
            cx.notify();
            return;
        };
        self.search_results = None;
        match applied.lines {
            Some((first_line, line_delta)) => self.adjust_bookmarks(first_line, line_delta),
            None => self.tabs[self.active].bookmarks.clear(),
        }
        self.select_global_position(applied.cursor);
        self.status = if redo { "Redo" } else { "Undo" }.into();
        cx.notify();
    }

    fn target_text(&self) -> anyhow::Result<String> {
        let (doc, editor) = self
            .document()
            .zip(self.editor())
            .ok_or_else(|| anyhow::anyhow!("no document is ready"))?;
        if self.selection_scope {
            if editor.is_empty() {
                anyhow::bail!("select text, or switch scope to Entire file");
            }
            Ok(editor.selected_text(doc))
        } else {
            Ok(doc.text())
        }
    }

    fn transform(&mut self, op: TransformOp, cx: &mut Context<Self>) {
        if self.refuse_huge_edit(cx) {
            return;
        }
        let apply = |text: &str| match op {
            TransformOp::Case(case) => Ok(transforms::convert_case(text, case)),
            TransformOp::Base64 => transforms::decode_base64(text),
            TransformOp::Json => transforms::pretty_json(text),
        };
        // Selection scope: transform each selection separately.
        let result = if self.selection_scope {
            match self.document().zip(self.editor()) {
                Some((_, editor)) if editor.all_empty() => Err(anyhow::anyhow!(
                    "select text, or switch scope to Entire file"
                )),
                Some((doc, editor)) => editor
                    .selected_texts(doc)
                    .iter()
                    .map(|text| apply(text))
                    .collect::<anyhow::Result<Vec<_>>>(),
                None => Err(anyhow::anyhow!("no document is ready")),
            }
        } else {
            self.target_text()
                .and_then(|text| apply(&text))
                .map(|t| vec![t])
        };
        let replacements = match result {
            Ok(texts) => texts,
            Err(error) => {
                self.status = format!("{error:#}");
                cx.notify();
                return;
            }
        };
        if self.document().is_some_and(Document::is_read_only) {
            self.status = "File is read-only".into();
        } else if self.selection_scope {
            let revision = self.document().map_or(0, Document::revision);
            // Keep the transformed text selected.
            let outcome =
                self.with_editor(|doc, editor| editor.insert_each(doc, replacements, true));
            match outcome.flatten() {
                Some(outcome) => {
                    self.after_edit(outcome, revision);
                    self.status = "Text transformed".into();
                }
                None => self.status = "Cannot transform: document changed".into(),
            }
        } else {
            let replacement = replacements.into_iter().next().unwrap_or_default();
            self.with_editor(|doc, editor| {
                doc.replace_all(&replacement);
                editor.set_cursor(doc, 0);
            });
            self.tabs[self.active].bookmarks.clear();
            self.search_results = None;
            self.ensure_cursor_visible();
            self.status = "File transformed".into();
        }
        cx.notify();
    }

    fn view_json(&mut self, cx: &mut Context<Self>) {
        match self
            .target_text()
            .and_then(|text| transforms::pretty_json(&text))
        {
            Ok(text) => {
                self.json_viewer = Some(ropey::Rope::from_str(&text));
                self.status = "JSON viewer (read-only)".into();
            }
            Err(error) => self.status = format!("JSON viewer: {error:#}"),
        }
        cx.notify();
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        match (&self.bounds, &self.layout) {
            (Some(bounds), Some(layout)) => layout.closest_index_for_x(position.x - bounds.left()),
            _ => 0,
        }
    }
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.selecting = true;
        let index = self.index_at(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx)
        } else {
            self.move_to(index, cx)
        }
        window.focus(&self.focus);
    }
    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.index_at(event.position), cx)
        }
    }

    fn palette_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let registry = command_registry(cx);
        let platform = re_commands::Platform::current();
        let rows: Vec<_> = re_commands::palette::filter(&registry, &self.content)
            .into_iter()
            .take(PALETTE_ROWS)
            .enumerate()
            .map(|(index, command)| {
                let id = command.id.to_string();
                let keys = cx
                    .try_global::<KeymapState>()
                    .and_then(|k| k.resolved.labels(&id, platform).into_iter().next())
                    .unwrap_or_default();
                div()
                    .id(gpui::SharedString::from(format!("palette-{id}")))
                    .flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    // Enter runs the first row.
                    .when(index == 0, |row| row.bg(rgb(0xd3e5ff)))
                    .hover(|row| row.bg(rgb(0xe0ecff)))
                    .child(
                        div()
                            .flex_1()
                            .child(format!("{}: {}", command.category, command.title)),
                    )
                    .child(div().text_color(rgb(0x606060)).child(keys))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.cancel_command(cx);
                        this.run_command(&id, window, cx);
                    }))
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .bg(rgb(0xf7faff))
            .border_b_1()
            .border_color(rgb(0xc8d8f0))
            .children(rows)
            .when(empty, |list| {
                list.child(div().px_2().py_1().child("No matching commands"))
            })
    }

    // ----- Menu bar -----

    /// Opens menu `index` (or closes it if it is already open).
    fn toggle_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open_menu = if self.open_menu == Some(index) {
            None
        } else {
            Some(index)
        };
        self.menu_highlight = None;
        cx.notify();
    }

    fn open_menu_named(&mut self, title: &str, cx: &mut Context<Self>) {
        if let Some(index) = re_commands::menus::MENUS
            .iter()
            .position(|menu| menu.title == title)
        {
            self.open_menu = Some(index);
            // Keyboard opening highlights the first item.
            self.menu_highlight = Some(0);
            cx.notify();
        }
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.open_menu.take().is_some() {
            self.menu_highlight = None;
            cx.notify();
        }
    }

    /// The runnable items of a menu: (command id, label).
    fn menu_entries(&self, index: usize, cx: &App) -> Vec<Option<(String, String)>> {
        use re_commands::menus::{MENUS, MenuItem};
        let registry = command_registry(cx);
        let mut entries = Vec::new();
        for item in MENUS[index].items {
            match item {
                MenuItem::Separator => entries.push(None),
                MenuItem::Command { id, label } => {
                    let title = registry
                        .get(id)
                        .map_or_else(|| (*id).to_owned(), |c| c.title.to_string());
                    entries.push(Some(((*id).to_owned(), label.map_or(title, str::to_owned))));
                }
                MenuItem::PluginCommands => {
                    let before = entries.len();
                    if let Some(state) = cx.try_global::<PluginState>() {
                        for plugin in &state.registry.plugins {
                            for command in &plugin.manifest.commands {
                                entries.push(Some((
                                    plugin.command_id(command),
                                    format!("{}: {}", plugin.manifest.name, command.title),
                                )));
                            }
                        }
                    }
                    if entries.len() == before {
                        entries.push(Some((String::new(), "No plugins installed".to_owned())));
                    }
                }
            }
        }
        // No leading, trailing or doubled separators.
        let mut cleaned: Vec<Option<(String, String)>> = Vec::new();
        for entry in entries {
            if entry.is_some() || cleaned.last().is_some_and(Option::is_some) {
                cleaned.push(entry);
            }
        }
        while cleaned.last().is_some_and(Option::is_none) {
            cleaned.pop();
        }
        cleaned
    }

    /// Checkmark state for toggle commands; `None` for plain commands.
    fn menu_checked(&self, id: &str) -> Option<bool> {
        let doc = self.document();
        Some(match id {
            "search.toggle_case" => self.case_sensitive,
            "search.toggle_regex" => self.regex,
            "view.toggle_word_wrap" => self.word_wrap,
            "transform.toggle_scope" => self.selection_scope,
            "file.encoding_utf8" => doc.is_some_and(|d| d.encoding() == Encoding::Utf8),
            "file.encoding_utf8_bom" => doc.is_some_and(|d| d.encoding() == Encoding::Utf8Bom),
            "file.encoding_utf16le" => doc.is_some_and(|d| d.encoding() == Encoding::Utf16Le),
            "file.encoding_utf16be" => doc.is_some_and(|d| d.encoding() == Encoding::Utf16Be),
            "file.encoding_windows1252" => {
                doc.is_some_and(|d| d.encoding() == Encoding::Windows1252)
            }
            "file.line_endings_lf" => doc.is_some_and(|d| d.line_ending() == LineEnding::Lf),
            "file.line_endings_crlf" => doc.is_some_and(|d| d.line_ending() == LineEnding::CrLf),
            _ => return None,
        })
    }

    fn run_menu_item(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        if !id.is_empty() {
            window.focus(&self.focus);
            self.run_command(id, window, cx);
        }
    }

    /// Keyboard navigation while a menu is open. Returns `true` if handled.
    fn menu_key(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(open) = self.open_menu else {
            return false;
        };
        let entries = self.menu_entries(open, cx);
        let runnable: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.as_ref().is_some_and(|(id, _)| !id.is_empty()))
            .map(|(i, _)| i)
            .collect();
        let count = re_commands::menus::MENUS.len();
        let step = |current: Option<usize>, forward: bool| -> Option<usize> {
            let position = current.and_then(|c| runnable.iter().position(|&r| r == c));
            let next = match (position, forward) {
                (None, true) => 0,
                (None, false) => runnable.len().checked_sub(1)?,
                (Some(p), true) => (p + 1) % runnable.len(),
                (Some(p), false) => (p + runnable.len() - 1) % runnable.len(),
            };
            runnable.get(next).copied()
        };
        match id {
            "editor.down" => self.menu_highlight = step(self.menu_highlight, true),
            "editor.up" => self.menu_highlight = step(self.menu_highlight, false),
            "editor.right" => {
                self.open_menu = Some((open + 1) % count);
                self.menu_highlight = Some(0);
            }
            "editor.left" => {
                self.open_menu = Some((open + count - 1) % count);
                self.menu_highlight = Some(0);
            }
            "editor.cancel" => self.close_menu(cx),
            "editor.newline" => {
                let command = self
                    .menu_highlight
                    .and_then(|i| entries.get(i).cloned().flatten())
                    .map(|(id, _)| id);
                match command {
                    Some(command) => self.run_menu_item(&command, window, cx),
                    None => self.close_menu(cx),
                }
                return true;
            }
            _ => {
                // Any other command closes the menu and runs normally.
                self.close_menu(cx);
                return false;
            }
        }
        cx.notify();
        true
    }

    fn render_menu_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use re_commands::menus::MENUS;
        let platform = re_commands::Platform::current();
        let show_mnemonics = platform != re_commands::Platform::MacOs;
        let open = self.open_menu;
        div()
            .flex()
            .items_center()
            .h(px(MENU_BAR_HEIGHT))
            .px_1()
            .bg(rgb(0xf5f5f5))
            .border_b_1()
            .border_color(rgb(0xdddddd))
            .children(MENUS.iter().enumerate().map(|(index, menu)| {
                let is_open = open == Some(index);
                let title = menu.title;
                let mnemonic = menu
                    .title
                    .char_indices()
                    .find(|(_, c)| c.to_ascii_lowercase() == menu.mnemonic)
                    .map(|(i, _)| i);
                let label = match (show_mnemonics, mnemonic) {
                    (true, Some(i)) => {
                        let ch = title[i..].chars().next().map_or(0, char::len_utf8);
                        div()
                            .flex()
                            .child(title[..i].to_owned())
                            .child(div().underline().child(title[i..i + ch].to_owned()))
                            .child(title[i + ch..].to_owned())
                    }
                    _ => div().child(title),
                };
                let titles = self.menu_titles.clone();
                let bounds_probe = canvas(
                    move |bounds, _, _| {
                        let mut titles = titles.borrow_mut();
                        if titles.len() <= index {
                            titles.resize(index + 1, None);
                        }
                        titles[index] = Some(bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full();
                div()
                    .id(("menu", index))
                    .relative()
                    .h(px(MENU_TITLE_HEIGHT))
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded_sm()
                    .cursor_default()
                    .when(is_open, |d| d.bg(rgb(0xcfe0f7)))
                    .when(!is_open, |d| d.hover(|s| s.bg(rgb(0xe3e3e3))))
                    .child(bounds_probe)
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_menu(index, cx);
                        }),
                    )
                    // Moving across titles while a menu is open switches menus.
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.open_menu.is_some() && this.open_menu != Some(index) {
                            this.open_menu = Some(index);
                            this.menu_highlight = None;
                            cx.notify();
                        }
                    }))
            }))
            .when_some(open, |bar, index| {
                bar.child(self.render_menu_dropdown(index, cx))
            })
    }

    fn render_menu_dropdown(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let platform = re_commands::Platform::current();
        let keymap = cx.try_global::<KeymapState>();
        let highlight = self.menu_highlight;
        let rows: Vec<gpui::AnyElement> = self
            .menu_entries(index, cx)
            .into_iter()
            .enumerate()
            .map(|(row, entry)| match entry {
                None => div().my_1().h(px(1.)).bg(rgb(0xe0e0e0)).into_any_element(),
                Some((id, label)) => {
                    let enabled = !id.is_empty();
                    let keys = keymap
                        .and_then(|k| k.resolved.labels(&id, platform).into_iter().next())
                        .unwrap_or_default();
                    let checked = self.menu_checked(&id);
                    let highlighted = highlight == Some(row);
                    div()
                        .id(("menu-item", row))
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .when(highlighted && enabled, |d| d.bg(rgb(0xcfe0f7)))
                        .when(enabled && !highlighted, |d| {
                            d.hover(|s| s.bg(rgb(0xe8f0fb)))
                        })
                        .when(!enabled, |d| d.text_color(rgb(0x999999)))
                        .child(div().w(px(14.)).child(if checked == Some(true) {
                            "✓"
                        } else {
                            ""
                        }))
                        .child(div().flex_1().child(label))
                        .child(div().text_color(rgb(0x707070)).child(keys))
                        .when(enabled, |d| {
                            d.on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if this.menu_highlight != Some(row) {
                                    this.menu_highlight = Some(row);
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.run_menu_item(&id, window, cx);
                                },
                            ))
                        })
                        .into_any_element()
                }
            })
            .collect();
        // Directly below the title's painted bounds, in window coordinates,
        // so the dropdown never covers the title that opened it.
        let origin = self
            .menu_titles
            .borrow()
            .get(index)
            .copied()
            .flatten()
            .map_or(point(px(4.), px(MENU_BAR_HEIGHT)), |b| {
                point(b.left(), b.bottom() + px(2.))
            });
        deferred(
            anchored().position(origin).snap_to_window().child(
                div()
                    .id(("menu-dropdown", index))
                    .occlude()
                    .min_w(px(280.))
                    .p_1()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xc8c8c8))
                    .rounded_md()
                    .shadow_md()
                    .flex()
                    .flex_col()
                    .children(rows)
                    // Clicks on a menu title are handled by the title
                    // (toggle/switch); closing here would reopen it.
                    .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                        let on_title = this
                            .menu_titles
                            .borrow()
                            .iter()
                            .flatten()
                            .any(|b| b.contains(&event.position));
                        if !on_title {
                            this.close_menu(cx);
                        }
                    })),
            ),
        )
        .with_priority(1)
    }

    /// A compact button in the quick-access toolbar under the menu bar.
    fn quick_button(
        &self,
        label: &'static str,
        command: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let checked = self.menu_checked(command) == Some(true);
        div()
            .id(gpui::SharedString::from(format!("quick-{command}")))
            .px_2()
            .py_0p5()
            .rounded_sm()
            .cursor_pointer()
            .border_1()
            .border_color(if checked {
                rgb(0x7aa7e0)
            } else {
                rgb(0xf0f0f0)
            })
            .when(checked, |d| d.bg(rgb(0xdde9f8)))
            .hover(|s| s.bg(rgb(0xe3e3e3)))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus);
                this.run_command(command, window, cx);
            }))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let separator = || div().w(px(1.)).h(px(16.)).mx_1().bg(rgb(0xd0d0d0));
        div()
            .flex()
            .items_center()
            .gap_0p5()
            .px_1()
            .py_0p5()
            .bg(rgb(0xfafafa))
            .border_b_1()
            .border_color(rgb(0xe2e2e2))
            .child(self.quick_button("New", "file.new", cx))
            .child(self.quick_button("Open", "file.open", cx))
            .child(self.quick_button("Save", "file.save", cx))
            .child(separator())
            .child(self.quick_button("Undo", "edit.undo", cx))
            .child(self.quick_button("Redo", "edit.redo", cx))
            .child(separator())
            .child(self.quick_button("Find", "search.find", cx))
            .child(self.quick_button("Replace", "search.replace", cx))
            .child(separator())
            .child(self.quick_button("Aa", "search.toggle_case", cx))
            .child(self.quick_button(".*", "search.toggle_regex", cx))
            .child(self.quick_button("Wrap", "view.toggle_word_wrap", cx))
            .child(separator())
            .child(self.quick_button("Commands", "app.command_palette", cx))
    }

    fn toolbar_button(
        label: &str,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(gpui::SharedString::from(label.to_owned()))
            .px_2()
            .py_1()
            .border_1()
            .border_color(rgb(0xc8c8c8))
            .bg(rgb(0xf9f9f9))
            .cursor_pointer()
            .on_click(on_click)
            .child(label.to_owned())
    }

    fn input_widget(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("input-scroll", self.active))
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .track_focus(&self.focus)
            .key_context("Editor")
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .cursor(CursorStyle::IBeam)
            .child(LineElement {
                workspace: cx.entity(),
            })
    }

    // ----- Document mouse handling (hit-testing uses the last paint) -----

    fn doc_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pos) = self.paint_cache.borrow().char_at(event.position) else {
            return; // gutter or nothing painted
        };
        if self.command.is_some() {
            self.cancel_command(cx);
        }
        window.focus(&self.focus);
        let clicks = event.click_count;
        let extend = event.modifiers.shift;
        let add = event.modifiers.alt;
        self.with_editor(|doc, editor| {
            if add {
                editor.add_cursor(doc, pos);
            } else if clicks >= 2 {
                editor.select_word_at(doc, pos);
            } else {
                editor.move_head_to(doc, pos, extend);
            }
        });
        self.selecting = clicks < 2 && !add;
        cx.notify();
    }

    fn doc_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || self.command.is_some() {
            return;
        }
        let Some(pos) = self.paint_cache.borrow().char_at(event.position) else {
            return;
        };
        self.with_editor(|doc, editor| editor.move_head_to(doc, pos, true));
        cx.notify();
    }

    fn doc_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        let horizontal = if event.modifiers.shift && delta.x == px(0.) {
            delta.y
        } else {
            delta.x
        };
        let char_width = self.paint_cache.borrow().char_width.unwrap_or(px(8.));
        if horizontal == px(0.) || char_width <= px(0.) {
            return;
        }
        let columns = (horizontal / char_width).round() as isize;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.h_scroll = tab.h_scroll.saturating_add_signed(-columns);
            cx.notify();
        }
    }

    /// Renders list row `index`: a display row when wrapping, else a line.
    /// One line of a huge file (read on demand through the chunk cache).
    fn render_huge_row(&mut self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let tab = &self.tabs[self.active];
        let bookmarked = tab.bookmarks.contains(&index);
        let Some(view) = self.huge() else {
            return div().into_any_element();
        };
        let current = view.current_line == index;
        let text = match view.file.line_text(&view.index, index, HUGE_DISPLAY_BYTES) {
            Ok(Some((text, true))) => format!("{text} ..."),
            Ok(Some((text, false))) => text,
            Ok(None) => String::new(),
            Err(error) => format!("<cannot read: {error}>"),
        };
        div()
            .id(index)
            .flex()
            .h(px(ROW_HEIGHT))
            .w_full()
            .items_center()
            .bg(if current {
                rgb(0xe8f2ff)
            } else if index.is_multiple_of(2) {
                rgb(0xffffff)
            } else {
                rgb(0xfafafa)
            })
            .child(
                div()
                    .w(px(GUTTER_WIDTH))
                    .flex_none()
                    .px_2()
                    .text_color(rgb(0x777777))
                    .bg(rgb(0xf2f2f2))
                    .child(format!(
                        "{} {}",
                        if bookmarked { "B" } else { " " },
                        index + 1
                    ))
                    .id(("bookmark", index))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tab) = this.tabs.get_mut(this.active) {
                            if !tab.bookmarks.insert(index) {
                                tab.bookmarks.remove(&index);
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(
                div()
                    .id(("huge-line", index))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(text)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus);
                        if let Some(view) = this.huge_mut() {
                            view.current_line = index;
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_row(&mut self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.huge().is_some() {
            return self.render_huge_row(index, cx);
        }
        let wrap = self
            .wrap_map()
            .map(|map| (map.row_to_line(index), map.columns()));
        let (line, segment, last_segment, wrap_columns) = match wrap {
            Some(((line, seg), columns)) => {
                let segments = self.line_segments(line);
                let last = seg + 1 >= segments.len();
                (line, segments.get(seg).cloned(), last, Some(columns))
            }
            None => (index, None, true, None),
        };
        let first_segment = segment.as_ref().is_none_or(|seg| seg.start == 0);
        let bookmarked = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.bookmarks.contains(&line));
        let cursor_line = self.document().zip(self.editor()).map(|(doc, editor)| {
            doc.rope()
                .char_to_line(editor.head().min(doc.rope().len_chars()))
        });
        div()
            .id(index)
            .flex()
            .h(px(ROW_HEIGHT))
            .w_full()
            .items_center()
            .bg(if cursor_line == Some(line) {
                rgb(0xe8f2ff)
            } else if line.is_multiple_of(2) {
                rgb(0xffffff)
            } else {
                rgb(0xfafafa)
            })
            .child(
                div()
                    .w(px(GUTTER_WIDTH))
                    .flex_none()
                    .px_2()
                    .text_color(rgb(0x777777))
                    .bg(rgb(0xf2f2f2))
                    .child(if first_segment {
                        format!("{} {}", if bookmarked { "B" } else { " " }, line + 1)
                    } else {
                        String::new()
                    })
                    .id(("bookmark", index))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tab) = this.tabs.get_mut(this.active) {
                            if !tab.bookmarks.insert(line) {
                                tab.bookmarks.remove(&line);
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(RowText {
                workspace: cx.entity(),
                line,
                display_row: index,
                segment,
                last_segment,
                wrap_columns,
            })
            .into_any_element()
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for Workspace {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if self.command.is_none() {
            let (text, covered) = self
                .document()
                .zip(self.editor())
                .map(|(doc, editor)| editor.text_for_utf16(doc, range))?;
            *actual = Some(covered);
            return Some(text);
        }
        let bytes = self.utf16_to_bytes(range);
        *actual = Some(self.to_utf16(bytes.clone()));
        Some(self.content[bytes].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if self.command.is_none() {
            let (doc, editor) = self.document().zip(self.editor())?;
            return Some(UTF16Selection {
                range: editor.to_utf16(doc, editor.range()),
                reversed: editor.reversed(),
            });
        }
        Some(UTF16Selection {
            range: self.to_utf16(self.selected.clone()),
            reversed: self.reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        if self.command.is_none() {
            let (doc, editor) = self.document().zip(self.editor())?;
            return editor.marked().map(|range| editor.to_utf16(doc, range));
        }
        self.marked.clone().map(|range| self.to_utf16(range))
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if self.command.is_none() {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.editor.unmark();
            }
            return;
        }
        self.marked = None
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command.is_none() {
            if self.refuse_huge_edit(cx) {
                return;
            }
            let plain_typing = range.is_none();
            let Some(target) = self.document().zip(self.editor()).map(|(doc, editor)| {
                range
                    .map(|range| editor.from_utf16(doc, range))
                    .or_else(|| editor.marked())
                    .unwrap_or_else(|| editor.range())
            }) else {
                return;
            };
            if text.len() > 1_048_576 && !self.document().is_some_and(Document::is_read_only) {
                let whole = target.start == 0
                    && self
                        .document()
                        .is_some_and(|doc| target.end == doc.rope().len_chars());
                self.queue_paste(target.start, target.end, text.to_owned(), whole);
                cx.notify();
                return;
            }
            if plain_typing && self.editor().is_some_and(|e| e.marked().is_none()) {
                // Plain typing: insert at every cursor.
                self.edit(cx, |doc, editor| editor.insert(doc, text));
            } else {
                self.edit(cx, |doc, editor| editor.replace_range(doc, target, text));
            }
            return;
        }
        let bytes = range
            .map(|range| self.utf16_to_bytes(range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selected.clone());
        self.content.replace_range(bytes.clone(), text);
        let cursor = bytes.start + text.len();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command.is_none() {
            self.edit(cx, |doc, editor| {
                editor.replace_and_mark(doc, range, text, selection)
            });
            return;
        }
        self.replace_text_in_range(range, text, window, cx);
        if !text.is_empty() && self.cursor() >= text.len() && self.cursor() <= self.content.len() {
            let end = self.cursor();
            self.marked = Some(end - text.len()..end);
            if let Some(selection) = selection {
                let marked_start = end - text.len();
                let start = self.content[marked_start..]
                    .char_indices()
                    .take(selection.start)
                    .last()
                    .map_or(marked_start, |(i, ch)| marked_start + i + ch.len_utf8());
                self.selected = start..start;
            }
        }
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.command.is_none() {
            let (doc, editor) = self.document().zip(self.editor())?;
            let chars = editor.from_utf16(doc, range);
            let rope = doc.rope();
            let line = rope.char_to_line(chars.start.min(rope.len_chars()));
            let cache = self.paint_cache.borrow();
            let row = cache.row_for_pos(line, chars.start)?;
            let x = |pos: usize| {
                let offset = pos.clamp(row.start_char, row.end_char) - row.start_char;
                row.layout.x_for_index(byte_offset(&row.text, offset))
            };
            let end = chars.end.clamp(chars.start, row.end_char);
            return Some(Bounds::from_corners(
                point(row.bounds.left() + x(chars.start), row.bounds.top()),
                point(row.bounds.left() + x(end), row.bounds.bottom()),
            ));
        }
        let range = self.utf16_to_bytes(range);
        let layout = self.layout.as_ref()?;
        Some(Bounds::from_corners(
            point(
                bounds.left() + layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if self.command.is_none() {
            let pos = self.paint_cache.borrow().char_at(position)?;
            let (doc, editor) = self.document().zip(self.editor())?;
            return Some(editor.to_utf16(doc, pos..pos).start);
        }
        Some(self.to_utf16(0..self.index_at(position)).end)
    }
}

impl Workspace {
    fn utf16_to_bytes(&self, range: Range<usize>) -> Range<usize> {
        from_utf16(&self.content, range)
    }
    fn to_utf16(&self, range: Range<usize>) -> Range<usize> {
        to_utf16(&self.content, range)
    }
}

fn from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    fn byte_at(text: &str, offset: usize) -> usize {
        let mut units = 0;
        for (i, ch) in text.char_indices() {
            if units >= offset {
                return i;
            }
            units += ch.len_utf16();
        }
        text.len()
    }
    byte_at(text, range.start)..byte_at(text, range.end)
}

fn to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

struct LineElement {
    workspace: Entity<Workspace>,
}
struct LinePaint {
    line: Option<ShapedLine>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for LineElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for LineElement {
    type RequestLayoutState = ();
    type PrepaintState = LinePaint;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        let content_chars = self.workspace.read(cx).content.chars().count();
        style.size.width = px(content_chars as f32 * 9.0 + 32.0).into();
        style.min_size.width = gpui::relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> LinePaint {
        let state = self.workspace.read(cx);
        let content: gpui::SharedString = state.content.clone().into();
        let style = window.text_style();
        let run = TextRun {
            len: content.len(),
            font: style.font(),
            color: rgb(0x202020).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(content, font_size, &[run], None);
        let selection = if state.selected.is_empty() {
            None
        } else {
            Some(fill(
                Bounds::from_corners(
                    point(
                        bounds.left() + line.x_for_index(state.selected.start),
                        bounds.top(),
                    ),
                    point(
                        bounds.left() + line.x_for_index(state.selected.end),
                        bounds.bottom(),
                    ),
                ),
                rgba(0x5599ff66),
            ))
        };
        let cursor = Some(fill(
            Bounds::new(
                point(
                    bounds.left() + line.x_for_index(state.cursor()),
                    bounds.top(),
                ),
                size(px(2.), bounds.bottom() - bounds.top()),
            ),
            rgb(0x202020),
        ));
        LinePaint {
            line: Some(line),
            selection,
            cursor,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        paint: &mut LinePaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.workspace.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.workspace.clone()),
            cx,
        );
        if let Some(selection) = paint.selection.take() {
            window.paint_quad(selection)
        }
        let line = paint.line.take().expect("prepaint supplies shaped line");
        line.paint(bounds.origin, window.line_height(), window, cx)
            .expect("shape line paints");
        if focus.is_focused(window)
            && let Some(cursor) = paint.cursor.take()
        {
            window.paint_quad(cursor)
        }
        self.workspace.update(cx, |state, _| {
            state.layout = Some(line);
            state.bounds = Some(bounds);
        });
    }
}

/// Byte index of the `chars`-th character in `text` (clamped).
fn byte_offset(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

/// Shifts line-anchored bookmarks after lines were inserted or removed after
/// `line` (used when only the net line delta is known, e.g. undo).
fn adjust_bookmarks(bookmarks: &mut BTreeSet<usize>, line: usize, delta: isize) {
    if delta == 0 {
        return;
    }
    *bookmarks = bookmarks
        .iter()
        .filter_map(|&index| {
            if index <= line {
                Some(index)
            } else {
                index.checked_add_signed(delta)
            }
        })
        .collect();
}

/// Paints the visible slice of one document line: selection, text, IME
/// underline and cursor. Registers its geometry in the paint cache.
struct RowText {
    workspace: Entity<Workspace>,
    line: usize,
    display_row: usize,
    /// Char range within the line when wrapping; `None` = horizontal scroll.
    segment: Option<Range<usize>>,
    last_segment: bool,
    /// The wrap map's width, to detect a width change.
    wrap_columns: Option<usize>,
}

#[derive(Default)]
struct RowPaint {
    line: Option<ShapedLine>,
    quads: Vec<PaintQuad>,
    cursors: Vec<PaintQuad>,
}

impl IntoElement for RowText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for RowText {
    type RequestLayoutState = ();
    type PrepaintState = RowPaint;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style {
            flex_grow: 1.,
            ..Style::default()
        };
        style.min_size.width = px(0.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> RowPaint {
        let state = self.workspace.read(cx);
        let Some((doc, editor)) = state.document().zip(state.editor()) else {
            return RowPaint::default();
        };
        let h_scroll = state.tabs[state.active].h_scroll;
        let rope = doc.rope();
        if self.line >= rope.len_lines() {
            return RowPaint::default();
        }
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut cache = state.paint_cache.borrow_mut();
        if cache.char_width.is_none() {
            let font_id = window.text_system().resolve_font(&style.font());
            cache.char_width = window
                .text_system()
                .ch_advance(font_id, font_size)
                .ok()
                .filter(|width| *width > px(0.));
        }
        cache.text_width = Some(bounds.size.width);
        let char_width = cache.char_width.unwrap_or(px(8.));
        let columns = ((bounds.size.width / char_width) as usize).max(16);
        // The wrap width changed (resize, first paint): draw again so the
        // display map is rebuilt for the real width.
        if let Some(map_columns) = self.wrap_columns
            && wrap_columns_for(bounds.size.width, char_width) != map_columns
        {
            window.request_animation_frame();
        }

        // Shape only one wrapped segment, or the visible window of the line
        // (horizontal virtualization): long lines never shape more.
        let line_start = rope.line_to_char(self.line);
        let line_len = doc.line_len_chars(self.line);
        let (slice_start, slice_end) = match &self.segment {
            Some(segment) => (segment.start.min(line_len), segment.end.min(line_len)),
            None => {
                let start = h_scroll.min(line_len);
                (
                    start,
                    (start + (columns + 32).min(ROW_WINDOW_CHARS)).min(line_len),
                )
            }
        };
        let text: String = rope
            .slice(line_start + slice_start..line_start + slice_end)
            .to_string();
        let shared: gpui::SharedString = text.clone().into();
        let run = TextRun {
            len: shared.len(),
            font: style.font(),
            color: rgb(0x202020).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout = window
            .text_system()
            .shape_line(shared, font_size, &[run], None);

        let doc_slice = line_start + slice_start..line_start + slice_end;
        let line_end = line_start + line_len;
        // The end of a non-final wrapped segment belongs to the next row.
        let owns = |pos: usize| {
            pos >= doc_slice.start
                && (pos < doc_slice.end || (self.last_segment && pos <= doc_slice.end))
        };
        let x_at = |pos: usize| {
            let offset = pos.clamp(doc_slice.start, doc_slice.end) - doc_slice.start;
            bounds.left() + layout.x_for_index(byte_offset(&text, offset))
        };
        let mut quads = Vec::new();
        let mut cursors = Vec::new();
        let show_cursors = state.command.is_none();
        for (index, selection) in editor.selections().iter().enumerate() {
            let range = selection.range();
            let reaches_row = if self.last_segment {
                range.start <= line_end
            } else {
                range.start < doc_slice.end
            };
            if !range.is_empty() && reaches_row && range.end > doc_slice.start {
                let start = x_at(range.start);
                let mut end = x_at(range.end);
                // Show that the line break is selected.
                if self.last_segment && range.end > line_end {
                    end += char_width / 2.;
                }
                if end > start {
                    quads.push(fill(
                        Bounds::from_corners(
                            point(start, bounds.top()),
                            point(end, bounds.bottom()),
                        ),
                        rgba(0x5599ff66),
                    ));
                }
            }
            if show_cursors && owns(selection.head) && selection.head <= line_end {
                let primary = index == editor.primary_index();
                cursors.push(fill(
                    Bounds::new(
                        point(x_at(selection.head), bounds.top()),
                        size(px(if primary { 2. } else { 1.5 }), bounds.size.height),
                    ),
                    rgb(if primary { 0x202020 } else { 0x505050 }),
                ));
            }
        }
        if let Some(marked) = editor.marked()
            && marked.start < doc_slice.end
            && marked.end > doc_slice.start
        {
            quads.push(fill(
                Bounds::from_corners(
                    point(x_at(marked.start), bounds.bottom() - px(2.)),
                    point(x_at(marked.end), bounds.bottom()),
                ),
                rgb(0x202020),
            ));
        }
        cache.rows.retain(|row| row.display_row != self.display_row);
        cache.rows.push(PaintedRow {
            display_row: self.display_row,
            line: self.line,
            start_char: doc_slice.start,
            end_char: doc_slice.end,
            last_segment: self.last_segment,
            text,
            layout: layout.clone(),
            bounds,
        });
        RowPaint {
            line: Some(layout),
            quads,
            cursors,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        paint: &mut RowPaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        for quad in paint.quads.drain(..) {
            window.paint_quad(quad);
        }
        if let Some(line) = paint.line.take() {
            let _ = line.paint(bounds.origin, window.line_height(), window, cx);
        }
        let focus = self.workspace.read(cx).focus.clone();
        if focus.is_focused(window) {
            for cursor in paint.cursors.drain(..) {
                window.paint_quad(cursor);
            }
        }
    }
}

/// Registers the document's IME/text input handler each frame. It takes no
/// space; the handler itself works on the whole document.
struct ImeAnchor {
    workspace: Entity<Workspace>,
}

impl IntoElement for ImeAnchor {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ImeAnchor {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style {
            position: gpui::Position::Absolute,
            ..Style::default()
        };
        style.size.width = px(0.).into();
        style.size.height = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = self.workspace.read(cx);
        if state.command.is_none() {
            let focus = state.focus.clone();
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.workspace.clone()),
                cx,
            );
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Visible rows re-register their geometry as they prepaint.
        self.paint_cache.borrow_mut().rows.clear();
        self.sync_wrap();
        let list_rows = self
            .wrap_map()
            .map(DisplayMap::total_rows)
            .or_else(|| self.huge().map(|view| view.index.lines()));
        let active = self.active;
        let labels = self
            .tabs
            .iter()
            .map(|tab| {
                let name = tab.path.as_deref().and_then(Path::file_name).map_or_else(
                    || "Untitled".to_owned(),
                    |name| name.to_string_lossy().into_owned(),
                );
                match &tab.state {
                    TabState::Loading => format!("{name} (loading...)"),
                    TabState::Huge(view) if !view.index.is_complete() => format!(
                        "{name} (indexing {}%)",
                        view.index.scanned_bytes() * 100 / view.file.len().max(1)
                    ),
                    TabState::Huge(_) => format!("{name} (read-only)"),
                    TabState::Ready(doc) if doc.is_dirty() => format!("{name} *"),
                    TabState::Failed(_) => format!("{name} (failed)"),
                    _ => name,
                }
            })
            .collect::<Vec<_>>();
        let ready = self.document();
        let count = ready.map_or(0, Document::line_count);
        let status = match self.tabs.get(active).map(|tab| &tab.state) {
            Some(TabState::Failed(error)) => error.clone(),
            Some(TabState::Huge(view)) if view.error.is_some() && self.status.is_empty() => {
                view.error.clone().unwrap_or_default()
            }
            _ => self.status.clone(),
        };
        let huge_info = self.huge().map(|view| {
            let lines = if view.index.is_complete() {
                format!("{} lines", view.index.lines())
            } else {
                format!(
                    "{}+ lines, indexing {}%",
                    view.index.lines(),
                    view.index.scanned_bytes() * 100 / view.file.len().max(1)
                )
            };
            format!(
                "{} | Ln {} | {} | {} | read-only (huge file)",
                status,
                view.current_line + 1,
                lines,
                format_bytes(view.file.len())
            )
        });
        let position = ready.zip(self.editor()).map(|(doc, editor)| {
            let rope = doc.rope();
            let head = editor.head().min(rope.len_chars());
            let line = rope.char_to_line(head);
            let selected = editor.range().len();
            (
                line + 1,
                head - rope.line_to_char(line) + 1,
                selected,
                editor.cursor_count(),
            )
        });
        let viewing_json = self.json_viewer.is_some();
        let json_lines = self.json_viewer.as_ref().map_or(0, ropey::Rope::len_lines);
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x202020))
            .font_family("Consolas")
            .text_size(px(14.))
            .key_context("RustEditor")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::run_command_action))
            .child(self.render_menu_bar(cx))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .p_1()
                    .bg(rgb(0xe8e8e8))
                    .children(labels.into_iter().enumerate().map(|(index, label)| {
                        div()
                            .id(("tab", index))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .bg(if index == active {
                                rgb(0xffffff)
                            } else {
                                rgb(0xdcdcdc)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.select_tab(index, cx)))
                            .child(label)
                    }))
                    .child(Self::toolbar_button(
                        "+",
                        cx.listener(|this, _, _, cx| {
                            this.add_blank();
                            cx.notify();
                        }),
                    )),
            )
            .when(self.command.is_some(), |root| {
                root.child(
                    div()
                        .flex()
                        .h(px(ROW_HEIGHT + 12.))
                        .items_center()
                        .gap_2()
                        .p_1()
                        .bg(rgb(0xe8f2ff))
                        .child(match self.command {
                            Some(CommandMode::Palette) => "Command:",
                            Some(CommandMode::Find) => "Find:",
                            Some(CommandMode::GoToLine) => "Go to line:",
                            Some(CommandMode::Ask) => "Ask Copilot:",
                            Some(CommandMode::Calculator) => "Calculator:",
                            Some(CommandMode::Replace) => "Replace match with:",
                            Some(CommandMode::ReplaceAll) => "Replace all with:",
                            None => "",
                        })
                        .child(self.input_widget(cx))
                        .child(Self::toolbar_button(
                            if matches!(self.command, Some(CommandMode::Calculator)) {
                                "="
                            } else if matches!(self.command, Some(CommandMode::Ask)) {
                                "Ask"
                            } else if matches!(
                                self.command,
                                Some(CommandMode::Replace | CommandMode::ReplaceAll)
                            ) {
                                "Replace"
                            } else if matches!(self.command, Some(CommandMode::Palette)) {
                                "Run"
                            } else {
                                "Search"
                            },
                            cx.listener(|this, _, window, cx| this.finish_command(window, cx)),
                        ))
                        .child(Self::toolbar_button(
                            "Cancel",
                            cx.listener(|this, _, _, cx| this.cancel_command(cx)),
                        )),
                )
            })
            .when(matches!(self.command, Some(CommandMode::Palette)), |root| {
                root.child(self.palette_results(cx))
            })
            .when(
                matches!(self.command, Some(CommandMode::Calculator)),
                |root| {
                    root.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .p_1()
                            .bg(rgb(0xe8f2ff))
                            .children(
                                [
                                    "7",
                                    "8",
                                    "9",
                                    "/",
                                    "4",
                                    "5",
                                    "6",
                                    "*",
                                    "1",
                                    "2",
                                    "3",
                                    "-",
                                    "0",
                                    ".",
                                    "(",
                                    ")",
                                    "+",
                                    "C",
                                    "Backspace",
                                    "=",
                                ]
                                .into_iter()
                                .map(|key| {
                                    Self::toolbar_button(
                                        key,
                                        cx.listener(move |this, _, window, cx| {
                                            this.calculator_key(key, window, cx)
                                        }),
                                    )
                                }),
                            ),
                    )
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when(viewing_json, |pane| {
                        pane.child(Self::toolbar_button(
                            "Close viewer",
                            cx.listener(|this, _, _, cx| {
                                this.json_viewer = None;
                                cx.notify();
                            }),
                        ))
                        .child(
                            uniform_list(
                                "json-lines",
                                json_lines,
                                cx.processor(|this, range: Range<usize>, _, _| {
                                    range
                                        .map(|index| {
                                            let line = this
                                                .json_viewer
                                                .as_ref()
                                                .expect("viewer is open")
                                                .line(index);
                                            let preview = line
                                                .slice(..line.len_chars().min(PREVIEW_CHARS))
                                                .to_string();
                                            div()
                                                .h(px(ROW_HEIGHT))
                                                .whitespace_nowrap()
                                                .child(preview)
                                                .into_any_element()
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .h_full(),
                        )
                    })
                    .when(!viewing_json, |pane| {
                        let editor = div()
                            .id("editor-pane")
                            .relative()
                            .size_full()
                            .key_context("Editor")
                            .cursor(CursorStyle::IBeam)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::doc_mouse_down))
                            .on_mouse_move(cx.listener(Self::doc_mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
                            .on_scroll_wheel(cx.listener(Self::doc_scroll_wheel))
                            .child(
                                uniform_list(
                                    "editor-lines",
                                    list_rows.unwrap_or(count),
                                    cx.processor(|this, range: Range<usize>, _window, cx| {
                                        this.huge_rows = range.clone();
                                        range
                                            .map(|index| this.render_row(index, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .track_scroll(self.list_scroll.clone())
                                .h_full(),
                            )
                            .child(ImeAnchor {
                                workspace: cx.entity(),
                            });
                        // While the command bar is open it owns the focus.
                        pane.child(if self.command.is_none() {
                            editor.track_focus(&self.focus).into_any_element()
                        } else {
                            editor.into_any_element()
                        })
                    }),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .bg(rgb(0xeeeeee))
                    .child(huge_info.unwrap_or_else(|| {
                        format!(
                            "{} | {}{} lines | {}",
                            status,
                            position.map_or_else(
                                String::new,
                                |(line, column, selected, cursors)| {
                                    let mut text = format!("Ln {line}, Col {column}");
                                    if selected > 0 {
                                        text.push_str(&format!(" ({selected} selected)"));
                                    }
                                    if cursors > 1 {
                                        text.push_str(&format!(" | {cursors} cursors"));
                                    }
                                    text + " | "
                                }
                            ),
                            count,
                            ready.map_or_else(String::new, |doc| {
                                let mut info = format!(
                                    "{} | {}{}",
                                    doc.encoding().label(),
                                    doc.line_ending().label(),
                                    if doc.has_mixed_line_endings() {
                                        " (mixed)"
                                    } else {
                                        ""
                                    }
                                );
                                if doc.is_read_only() {
                                    info.push_str(" | Read-only");
                                }
                                info
                            })
                        )
                    })),
            )
    }
}

/// `1.5 GB`-style size.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Files opened through the OS ("Open With", dock/Finder on macOS) before
/// the window exists are queued, then sent to the workspace.
#[derive(Default)]
enum OpenedFiles {
    #[default]
    Empty,
    Queued(Vec<PathBuf>),
    Ready(WakingSender<Vec<PathBuf>>),
}

impl OpenedFiles {
    fn deliver(&mut self, paths: Vec<PathBuf>) {
        match self {
            Self::Empty => *self = Self::Queued(paths),
            Self::Queued(queued) => queued.extend(paths),
            Self::Ready(sender) => {
                let _ = sender.send(paths);
            }
        }
    }

    fn connect(&mut self, sender: WakingSender<Vec<PathBuf>>) {
        if let Self::Queued(paths) = std::mem::take(self) {
            let _ = sender.send(paths);
        }
        *self = Self::Ready(sender);
    }
}

pub fn run(paths: Vec<PathBuf>, server: Option<InstanceServer>) {
    let opened = Rc::new(RefCell::new(OpenedFiles::default()));
    let application = Application::new();
    let on_open = opened.clone();
    application.on_open_urls(move |urls| {
        let paths: Vec<PathBuf> = urls
            .iter()
            .filter_map(|url| re_platform::paths::file_url_to_path(url))
            .collect();
        if !paths.is_empty() {
            on_open.borrow_mut().deliver(paths);
        }
    });
    application.run(move |cx: &mut App| {
        let plugin_errors = load_plugins(cx, None, None);
        let mut keymap_errors = bind_keymap(cx, None);
        keymap_errors.extend(plugin_errors.into_iter().map(|e| format!("plugin {e}")));
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("RustEditor".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let workspace = cx.new(|cx| {
                    let mut workspace = Workspace::new(paths, server, cx);
                    if !keymap_errors.is_empty() {
                        workspace.status = keymap_status(&keymap_errors, "");
                    }
                    // Returning to the window checks the file for changes
                    // made by other programs (no polling).
                    cx.observe_window_activation(window, |workspace, window, cx| {
                        if window.is_window_active() {
                            workspace.check_disk(cx);
                        }
                    })
                    .detach();
                    workspace
                });
                window.focus(&workspace.read(cx).focus);
                opened
                    .borrow_mut()
                    .connect(workspace.read(cx).incoming_sender.clone());
                workspace
            },
        ) {
            eprintln!("Cannot open RustEditor window: {error:#}");
        }
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A keystroke with the platform's primary modifier (`mod` in keymaps).
    fn primary(key: &str) -> String {
        re_commands::keystroke::normalize(&format!("mod-{key}"), re_commands::Platform::current())
            .unwrap()
    }

    fn open_test_window(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Workspace> {
        cx.update(|cx| {
            // The real default keymap (empty user keymap for isolation).
            assert!(bind_keymap(cx, Some(String::new())).is_empty());
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1000.), px(700.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                |window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut workspace = Workspace::new(Vec::new(), None, cx);
                        // Deterministic LF documents on every platform.
                        workspace.tabs[0].state =
                            TabState::Ready(Document::with_line_ending(LineEnding::Lf));
                        workspace
                    });
                    window.focus(&workspace.read(cx).focus);
                    workspace
                },
            )
            .unwrap()
        })
    }

    fn text(window: &gpui::WindowHandle<Workspace>, cx: &mut gpui::TestAppContext) -> String {
        window
            .update(cx, |editor, _, _| editor.document().unwrap().text())
            .unwrap()
    }

    /// Selection as (anchor, head) char offsets.
    fn selection(
        window: &gpui::WindowHandle<Workspace>,
        cx: &mut gpui::TestAppContext,
    ) -> (usize, usize) {
        window
            .update(cx, |editor, _, _| {
                let e = editor.editor().unwrap();
                (e.anchor(), e.head())
            })
            .unwrap()
    }

    fn set_text(window: &gpui::WindowHandle<Workspace>, cx: &mut gpui::TestAppContext, text: &str) {
        window
            .update(cx, |editor, _, cx| {
                editor.with_editor(|doc, e| {
                    doc.replace_all(text);
                    e.set_cursor(doc, 0);
                });
                cx.notify();
            })
            .unwrap();
    }

    #[test]
    fn every_builtin_command_has_a_handler() {
        for command in re_commands::Registry::builtin().iter() {
            assert!(
                command_handler(&command.id).is_some(),
                "no handler for {}",
                command.id
            );
        }
        assert!(command_handler("no.such").is_none());
    }

    #[test]
    fn default_keymap_resolves_cleanly_on_every_platform() {
        for platform in [
            re_commands::Platform::Windows,
            re_commands::Platform::MacOs,
            re_commands::Platform::Linux,
        ] {
            let resolved = re_commands::keymap::resolve(
                DEFAULT_KEYMAP,
                None,
                &re_commands::Registry::builtin(),
                platform,
            );
            assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
            assert!(resolved.bindings.len() >= 40, "{}", resolved.bindings.len());
        }
        // macOS: Option moves by word, Cmd by line; Cmd+H stays "Hide".
        let mac = re_commands::keymap::resolve(
            DEFAULT_KEYMAP,
            None,
            &re_commands::Registry::builtin(),
            re_commands::Platform::MacOs,
        );
        let command = |keys: &str| {
            mac.bindings
                .iter()
                .find(|(_, k, _)| k == keys)
                .map(|(_, _, c)| c.as_str())
        };
        assert_eq!(command("alt-left"), Some("editor.word_left"));
        assert_eq!(command("cmd-left"), Some("editor.home"));
        assert_eq!(command("cmd-h"), None);
        assert_eq!(command("alt-cmd-f"), Some("search.replace"));
        assert_eq!(command("shift-cmd-z"), Some("edit.redo"));
        let windows = re_commands::keymap::resolve(
            DEFAULT_KEYMAP,
            None,
            &re_commands::Registry::builtin(),
            re_commands::Platform::Windows,
        );
        assert!(
            windows
                .bindings
                .iter()
                .any(|(_, k, c)| k == "ctrl-left" && c == "editor.word_left")
        );
        assert!(
            windows
                .bindings
                .iter()
                .any(|(_, k, c)| k == "ctrl-h" && c == "search.replace")
        );
    }

    #[test]
    fn utf16_offsets_respect_non_bmp_text() {
        assert_eq!(from_utf16("a😀é", 1..3), 1..5);
        assert_eq!(to_utf16("a😀é", 1..5), 1..3);
    }

    #[gpui::test]
    fn typing_and_arrow_keys_edit_across_lines(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        cx.simulate_input(*window, "ab😀");
        cx.simulate_keystrokes(*window, "enter");
        cx.simulate_input(*window, "cd");
        cx.simulate_keystrokes(*window, "enter");
        cx.simulate_input(*window, "e");
        assert_eq!(text(&window, cx), "ab😀\ncd\ne");
        // Up keeps the column (clamped), Home/End work per line.
        cx.simulate_keystrokes(*window, "up up");
        assert_eq!(selection(&window, cx), (1, 1));
        cx.simulate_keystrokes(*window, "end");
        assert_eq!(selection(&window, cx), (3, 3));
        cx.simulate_keystrokes(*window, "down");
        assert_eq!(selection(&window, cx), (6, 6));
        cx.simulate_input(*window, "X");
        assert_eq!(text(&window, cx), "ab😀\ncdX\ne");
        // Right at a line end moves onto the next line.
        cx.simulate_keystrokes(*window, "right");
        cx.simulate_input(*window, ">");
        assert_eq!(text(&window, cx), "ab😀\ncdX\n>e");
        cx.simulate_keystrokes(*window, &primary("home"));
        assert_eq!(selection(&window, cx), (0, 0));
        cx.simulate_keystrokes(*window, &primary("end"));
        assert_eq!(selection(&window, cx), (10, 10));
    }

    #[gpui::test]
    fn shift_selection_spans_lines_and_delete_joins_them(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "one\r\ntwo\nthree");
        cx.simulate_keystrokes(*window, "right shift-down shift-down");
        assert_eq!(selection(&window, cx), (1, 10));
        cx.simulate_keystrokes(*window, &primary("c"));
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("ne\r\ntwo\nt"));
        cx.simulate_keystrokes(*window, "backspace");
        assert_eq!(text(&window, cx), "ohree");
        // Undo restores the multi-line text in one step.
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "one\r\ntwo\nthree");
        // Backspace at a line start joins lines (CRLF removed as a unit).
        window
            .update(cx, |editor, _, _| editor.select_global_position(5))
            .unwrap();
        cx.simulate_keystrokes(*window, "backspace");
        assert_eq!(text(&window, cx), "onetwo\nthree");
        cx.simulate_keystrokes(*window, "end delete");
        assert_eq!(text(&window, cx), "onetwothree");
        // Word motion.
        set_text(&window, cx, "let foo = bar;");
        cx.simulate_keystrokes(
            *window,
            &format!("{} {}", primary("right"), primary("right")),
        );
        assert_eq!(selection(&window, cx), (7, 7));
        cx.simulate_keystrokes(*window, &primary("shift-left"));
        assert_eq!(selection(&window, cx), (7, 4));
    }

    #[gpui::test]
    fn select_all_then_type_replaces_everything_and_undo_restores(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        let big = "some text\n".repeat(50_000);
        set_text(&window, cx, &big);
        window
            .update(cx, |editor, _, _| editor.tabs[0].bookmarks.insert(10))
            .unwrap();
        cx.simulate_keystrokes(*window, &primary("a"));
        cx.simulate_input(*window, "x");
        assert_eq!(text(&window, cx), "x");
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx).len(), big.len());
        // The whole-range delete was recorded as a snapshot, not copied.
        window
            .update(cx, |editor, _, _| {
                assert!(editor.document().unwrap().can_redo());
            })
            .unwrap();
    }

    #[gpui::test]
    fn ime_composition_works_on_later_lines_with_utf16(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "😀\nab");
        window
            .update(cx, |editor, window, cx| {
                editor.select_global_position(3); // "a|b" on line 2
                // UTF-16: "😀" is 2 units, "\n" 1, "a" 1 => cursor at 4.
                let sel = editor.selected_text_range(false, window, cx).unwrap();
                assert_eq!(sel.range, 4..4);
                editor.replace_and_mark_text_in_range(None, "に", Some(1..1), window, cx);
                assert_eq!(editor.marked_text_range(window, cx), Some(4..5));
                editor.replace_and_mark_text_in_range(None, "日本", Some(2..2), window, cx);
                editor.replace_text_in_range(None, "日本", window, cx);
                assert_eq!(editor.marked_text_range(window, cx), None);
                let mut actual = None;
                let got = editor.text_for_range(0..7, &mut actual, window, cx);
                assert_eq!(got.as_deref(), Some("😀\na日本b"));
                assert_eq!(actual, Some(0..7));
            })
            .unwrap();
        assert_eq!(text(&window, cx), "😀\na日本b");
    }

    #[gpui::test]
    fn painted_rows_support_click_and_drag_selection(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "first line\nsecond line\nthird");
        cx.run_until_parked();
        let rows = window
            .update(cx, |editor, _, _| {
                editor
                    .paint_cache
                    .borrow()
                    .rows
                    .iter()
                    .map(|row| (row.line, row.bounds))
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(rows.len(), 3, "rows painted: {rows:?}");
        let row_bounds = |line: usize| rows.iter().find(|(l, _)| *l == line).unwrap().1;
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        // Click far right on line 2 => end of "second line".
        let second = row_bounds(1);
        let far = point(second.right() - px(1.), second.center().y);
        vcx.simulate_click(far, gpui::Modifiers::none());
        assert_eq!(selection(&window, cx), (22, 22));
        // Click in the gutter does not move the cursor.
        let gutter = point(px(5.), row_bounds(0).center().y);
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        vcx.simulate_click(gutter, gpui::Modifiers::none());
        assert_eq!(selection(&window, cx), (22, 22));
        // Drag from the start of line 1 to line 3 selects across lines.
        let start = point(row_bounds(0).left() + px(0.5), row_bounds(0).center().y);
        let end = point(row_bounds(2).right() - px(1.), row_bounds(2).center().y);
        vcx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::none());
        vcx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::none());
        vcx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::none());
        assert_eq!(selection(&window, cx), (0, 28));
    }

    #[gpui::test]
    fn word_wrap_splits_long_lines_into_display_rows(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        let words = "lorem ipsum dolor sit amet ".repeat(40); // ~1,080 chars
        set_text(&window, cx, &format!("short\n{words}\nlast"));
        cx.run_until_parked();
        cx.simulate_keystrokes(*window, "alt-z");
        cx.run_until_parked();
        cx.run_until_parked();
        let (rows, columns, painted) = window
            .update(cx, |editor, _, _| {
                let map = editor.wrap_map().expect("wrap map built");
                let cache = editor.paint_cache.borrow();
                let painted: Vec<_> = cache
                    .rows
                    .iter()
                    .map(|row| (row.display_row, row.line, row.last_segment))
                    .collect();
                (map.total_rows(), map.columns(), painted)
            })
            .unwrap();
        assert!(rows > 3, "rows {rows}");
        assert!(columns >= 8);
        // Line 1 spans several painted rows; only its last is `last_segment`.
        let line1: Vec<_> = painted.iter().filter(|(_, l, _)| *l == 1).collect();
        assert!(line1.len() > 1, "{painted:?}");
        assert_eq!(line1.iter().filter(|(_, _, last)| *last).count(), 1);
        // Down from line 0 goes to the first display row of line 1, then to
        // its second row (not to line 2).
        cx.simulate_keystrokes(*window, "down down");
        let head = selection(&window, cx).1;
        assert!(head > 6 && head < 6 + words.len(), "head {head}");
        window
            .update(cx, |editor, _, _| {
                let doc = editor.document().unwrap();
                let map = editor.wrap_map().unwrap();
                let row = WrapLayout { map }.row(doc.rope(), head);
                assert_eq!(row, 2);
            })
            .unwrap();
        // Typing keeps the map current incrementally (same revision).
        cx.simulate_input(*window, "INSERTED ");
        window
            .update(cx, |editor, _, _| {
                let tab = &editor.tabs[0];
                let doc = editor.document().unwrap();
                let wrap = tab.wrap.as_ref().unwrap();
                assert_eq!(wrap.revision, doc.revision());
                let rebuilt = DisplayMap::build(doc.rope(), wrap.map.columns(), None).unwrap();
                assert_eq!(wrap.map.total_rows(), rebuilt.total_rows());
            })
            .unwrap();
        // Clicking the second painted row of line 1 lands inside line 1.
        cx.run_until_parked();
        let second = window
            .update(cx, |editor, _, _| {
                let cache = editor.paint_cache.borrow();
                let mut rows: Vec<_> = cache.rows.iter().filter(|r| r.line == 1).collect();
                rows.sort_by_key(|r| r.display_row);
                (rows[1].bounds, rows[1].start_char)
            })
            .unwrap();
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        vcx.simulate_click(
            point(second.0.left() + px(0.5), second.0.center().y),
            gpui::Modifiers::none(),
        );
        assert_eq!(selection(&window, cx), (second.1, second.1));
        // Toggling off returns to one row per line.
        cx.simulate_keystrokes(*window, "alt-z");
        window
            .update(cx, |editor, _, _| assert!(editor.wrap_map().is_none()))
            .unwrap();
    }

    #[gpui::test]
    fn large_documents_wrap_on_a_worker(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        let big = "word ".repeat(SYNC_WRAP_CHARS / 5 + 1000);
        set_text(&window, cx, &big);
        cx.simulate_keystrokes(*window, "alt-z");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            let done = window
                .update(cx, |editor, _, _| editor.wrap_map().is_some())
                .unwrap();
            if done {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "wrap never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
        window
            .update(cx, |editor, _, _| {
                assert!(editor.wrap_map().unwrap().total_rows() > 1_000);
                assert!(editor.tabs[0].wrap_job.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn multiple_cursors_type_move_copy_paste_and_collapse(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "one\ntwo\nthree");
        cx.simulate_keystrokes(*window, "end");
        cx.simulate_keystrokes(*window, &primary("alt-down"));
        cx.simulate_keystrokes(*window, &primary("alt-down"));
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.editor().unwrap().cursor_count(), 3)
            })
            .unwrap();
        cx.simulate_input(*window, "!");
        assert_eq!(text(&window, cx), "one!\ntwo!\nthr!ee");
        // One undo step for the multi-cursor keystroke. (Undo/redo restore a
        // single cursor.)
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "one\ntwo\nthree");
        cx.simulate_keystrokes(*window, &primary("y"));
        assert_eq!(text(&window, cx), "one!\ntwo!\nthr!ee");
        cx.simulate_keystrokes(*window, &primary("home"));
        cx.simulate_keystrokes(*window, "end");
        cx.simulate_keystrokes(*window, &primary("alt-down"));
        cx.simulate_keystrokes(*window, &primary("alt-down"));
        // Select each line start..cursor and copy: joined by newlines.
        cx.simulate_keystrokes(*window, "shift-home");
        cx.simulate_keystrokes(*window, &primary("c"));
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("one!\ntwo!\nthr!"));
        // Pasting 3 lines into 3 cursors distributes them.
        cx.write_to_clipboard(ClipboardItem::new_string("A\nB\nC".into()));
        cx.simulate_keystrokes(*window, &primary("v"));
        assert_eq!(text(&window, cx), "A\nB\nCee");
        // Escape collapses to the primary cursor.
        cx.simulate_keystrokes(*window, "escape");
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.editor().unwrap().cursor_count(), 1)
            })
            .unwrap();
    }

    #[gpui::test]
    fn alt_click_adds_a_cursor(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "first\nsecond");
        cx.run_until_parked();
        let second = window
            .update(cx, |editor, _, _| {
                editor
                    .paint_cache
                    .borrow()
                    .row_for_pos(1, 6)
                    .unwrap()
                    .bounds
            })
            .unwrap();
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        vcx.simulate_click(
            point(second.left() + px(0.5), second.center().y),
            gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        cx.simulate_input(*window, ">");
        assert_eq!(text(&window, cx), ">first\n>second");
    }

    #[gpui::test]
    fn long_lines_are_edited_anywhere_and_scroll_horizontally(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        let long = "x".repeat(100_000) + "😀end";
        set_text(&window, cx, &long);
        cx.run_until_parked();
        cx.simulate_keystrokes(*window, "end");
        cx.simulate_input(*window, "!");
        assert!(text(&window, cx).ends_with("😀end!"));
        cx.run_until_parked();
        window
            .update(cx, |editor, _, _| {
                let tab = &editor.tabs[0];
                assert!(tab.h_scroll > 99_000, "h_scroll {}", tab.h_scroll);
                // The painted slice is bounded, not the whole line.
                let cache = editor.paint_cache.borrow();
                let row = cache.row_for_pos(0, 0).or(cache.rows.first()).unwrap();
                assert!(row.text.chars().count() <= ROW_WINDOW_CHARS);
                assert!(row.text.ends_with("😀end!"));
            })
            .unwrap();
        cx.simulate_keystrokes(*window, "home");
        cx.simulate_keystrokes(*window, "delete");
        assert_eq!(text(&window, cx).chars().count(), 100_004);
        window
            .update(cx, |editor, _, _| assert_eq!(editor.tabs[0].h_scroll, 0))
            .unwrap();
    }

    #[gpui::test]
    fn command_palette_runs_the_best_match_from_the_keyboard(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "Hello World");
        cx.simulate_keystrokes(*window, &primary("a"));
        cx.simulate_keystrokes(*window, &primary("shift-p"));
        window
            .update(cx, |editor, _, _| {
                assert!(matches!(editor.command, Some(CommandMode::Palette)));
            })
            .unwrap();
        cx.simulate_input(*window, "upper");
        cx.simulate_keystrokes(*window, "enter");
        window
            .update(cx, |editor, _, _| {
                assert!(editor.command.is_none());
                assert_eq!(
                    editor.document().unwrap().text(),
                    "HELLO WORLD",
                    "status: {}",
                    editor.status
                );
            })
            .unwrap();
        // The palette edits go through history like any other command.
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "Hello World");
        // No match reports an error and leaves the document alone.
        cx.simulate_keystrokes(*window, "f1");
        cx.simulate_input(*window, "qqqqzz");
        cx.simulate_keystrokes(*window, "enter");
        window
            .update(cx, |editor, _, _| {
                assert!(editor.status.contains("No command matches"));
            })
            .unwrap();
        assert_eq!(text(&window, cx), "Hello World");
        // Typing goes back to the document after the command bar closes.
        cx.simulate_input(*window, "!");
        assert!(text(&window, cx).contains('!'), "{}", text(&window, cx));
    }

    #[gpui::test]
    fn user_keymap_overrides_and_unbinds_default_keys(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        let user =
            "[global]\n\"mod-z\" = \"\"\n\"f5\" = \"edit.undo\"\n\"bad-key\" = \"edit.undo\"\n";
        let errors = cx.update(|cx| bind_keymap(cx, Some(user.to_owned())));
        assert_eq!(errors.len(), 1, "{errors:?}");
        cx.simulate_input(*window, "abc");
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "abc");
        cx.simulate_keystrokes(*window, "f5");
        assert_eq!(text(&window, cx), "");
        let labels = cx.update(|cx| {
            cx.global::<KeymapState>()
                .resolved
                .labels("edit.undo", re_commands::Platform::current())
        });
        assert_eq!(labels, vec!["F5"]);
    }

    #[gpui::test]
    fn undo_and_redo_restore_text_cursor_and_bookmarks(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        window
            .update(cx, |editor, _, _| {
                editor.tabs[0].bookmarks.insert(0);
            })
            .unwrap();
        cx.simulate_input(*window, "aé");
        cx.simulate_input(*window, "😀");
        cx.simulate_keystrokes(*window, "enter");
        cx.simulate_input(*window, "z");
        assert_eq!(text(&window, cx), "aé😀\nz");

        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "aé😀\n");
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "aé😀");
        assert_eq!(selection(&window, cx), (3, 3));
        // The typed run "aé😀" is a single undo step.
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "");
        window
            .update(cx, |editor, _, _| {
                assert!(!editor.document().unwrap().is_dirty());
                assert_eq!(editor.status, "Undo");
            })
            .unwrap();
        cx.simulate_keystrokes(*window, &primary("z"));
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.status, "Nothing to undo")
            })
            .unwrap();

        cx.simulate_keystrokes(*window, &format!("{0} {0}", primary("y")));
        assert_eq!(text(&window, cx), "aé😀\n");
        // Typing continues where redo left the cursor.
        cx.simulate_input(*window, "!");
        assert_eq!(text(&window, cx), "aé😀\n!");
        window
            .update(cx, |editor, _, _| {
                assert!(!editor.document().unwrap().can_redo());
                assert!(editor.tabs[0].bookmarks.contains(&0));
            })
            .unwrap();
    }

    #[gpui::test]
    fn undo_of_whole_document_replacement_clears_bookmarks(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        window
            .update(cx, |editor, window, cx| {
                editor.document_mut().unwrap().replace_all("one\ntwo");
                editor.tabs[0].bookmarks.insert(1);
                editor.select_global_position(5);
                editor.undo_action(&Undo, window, cx);
                assert_eq!(editor.document().unwrap().text(), "");
                assert!(editor.tabs[0].bookmarks.is_empty());
                assert_eq!(editor.editor().unwrap().head(), 0);
                editor.redo_action(&Redo, window, cx);
                assert_eq!(editor.document().unwrap().text(), "one\ntwo");
            })
            .unwrap();
    }

    #[gpui::test]
    fn edits_shift_and_merge_bookmarks(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "a\nb\nc\nd");
        window
            .update(cx, |editor, _, _| {
                editor.tabs[0].bookmarks.extend([1, 3]);
                editor.select_global_position(1); // end of "a"
            })
            .unwrap();
        cx.simulate_keystrokes(*window, "enter");
        window
            .update(cx, |editor, _, _| {
                let marks: Vec<_> = editor.tabs[0].bookmarks.iter().copied().collect();
                assert_eq!(marks, vec![2, 4]);
            })
            .unwrap();
        // Select from line 1 into line 3 and delete: line 3's bookmark merges.
        window
            .update(cx, |editor, _, _| {
                editor.with_editor(|doc, e| e.select(doc, 2..6));
            })
            .unwrap();
        cx.simulate_keystrokes(*window, "delete");
        assert_eq!(text(&window, cx), "a\n\nd");
        window
            .update(cx, |editor, _, _| {
                let marks: Vec<_> = editor.tabs[0].bookmarks.iter().copied().collect();
                assert_eq!(marks, vec![1, 2]);
            })
            .unwrap();
    }

    #[gpui::test]
    fn transforms_apply_to_multi_line_selections(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "keep\nfoo bar\nbaz qux\nkeep");
        window
            .update(cx, |editor, window, cx| {
                editor.with_editor(|doc, e| e.select(doc, 5..20));
                editor.run_command("transform.upper", window, cx);
                assert_eq!(editor.editor().unwrap().range(), 5..20);
            })
            .unwrap();
        assert_eq!(text(&window, cx), "keep\nFOO BAR\nBAZ QUX\nkeep");
    }

    fn temp_path(name: &str) -> PathBuf {
        // Tests run in parallel; the clock alone is not unique enough.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rusteditor-gpui-{name}-{}-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Runs the executor until `done` holds (worker threads are real).
    fn wait_until(
        window: &gpui::WindowHandle<Workspace>,
        cx: &mut gpui::TestAppContext,
        what: &str,
        done: impl Fn(&Workspace) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if window.update(cx, |editor, _, _| done(editor)).unwrap() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let status = window.update(cx, |e, _, _| e.status.clone()).unwrap();
                panic!("timed out: {what} (status: {status})");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[gpui::test]
    fn open_files_are_not_locked_and_external_changes_are_handled(cx: &mut gpui::TestAppContext) {
        let path = temp_path("external");
        std::fs::write(&path, "one\r\ntwo").unwrap();
        let window = open_test_window(cx);
        window
            .update(cx, |editor, _, _| {
                editor.tabs[0].state = TabState::Ready(Document::open(&path).unwrap());
                editor.tabs[0].path = Some(path.clone());
            })
            .unwrap();
        // Another program can read and rewrite the open file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\r\ntwo");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "rewritten\r\nby tool").unwrap();
        // A clean tab reloads automatically when checked (window activation
        // or tab switch call this).
        window
            .update(cx, |editor, _, cx| editor.check_disk(cx))
            .unwrap();
        wait_until(&window, cx, "auto reload", |e| {
            e.document().unwrap().text() == "rewritten\r\nby tool"
        });
        window
            .update(cx, |editor, _, _| {
                assert!(!editor.document().unwrap().is_dirty());
                assert!(editor.status.starts_with("Reloaded"), "{}", editor.status);
            })
            .unwrap();
        // The reload is undoable.
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "one\r\ntwo");
        cx.simulate_keystrokes(*window, &primary("y"));

        // With unsaved edits, an external change only warns (once)...
        cx.simulate_keystrokes(*window, &primary("end"));
        cx.simulate_keystrokes(*window, "enter");
        assert_eq!(text(&window, cx), "rewritten\r\nby tool\r\n"); // CRLF file
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "third version").unwrap();
        window
            .update(cx, |editor, _, cx| {
                editor.check_disk(cx);
                assert!(
                    editor.status.contains("changed on disk"),
                    "{}",
                    editor.status
                );
                editor.status.clear();
                editor.check_disk(cx);
                assert!(editor.status.is_empty(), "warned twice");
                assert_eq!(
                    editor.document().unwrap().text(),
                    "rewritten\r\nby tool\r\n"
                );
            })
            .unwrap();
        // ...and Save asks before overwriting it; saving again overwrites.
        cx.simulate_keystrokes(*window, &primary("s"));
        window
            .update(cx, |editor, _, _| {
                assert!(editor.status.contains("Save again"), "{}", editor.status);
            })
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "third version");
        cx.simulate_keystrokes(*window, &primary("s"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "rewritten\r\nby tool\r\n"
        );
        window
            .update(cx, |editor, _, _| {
                assert!(editor.status.starts_with("Saved"), "{}", editor.status);
                assert!(!editor.document().unwrap().is_dirty());
            })
            .unwrap();
        // Deleted files are reported.
        std::fs::remove_file(&path).unwrap();
        window
            .update(cx, |editor, _, cx| {
                editor.check_disk(cx);
                assert!(editor.status.contains("deleted"), "{}", editor.status);
            })
            .unwrap();
    }

    #[gpui::test]
    fn encoding_and_line_ending_commands(cx: &mut gpui::TestAppContext) {
        let path = temp_path("encoding");
        std::fs::write(&path, b"caf\xe9\nna\xefve\n").unwrap();
        let window = open_test_window(cx);
        window
            .update(cx, |editor, _, _| {
                editor.tabs[0].state = TabState::Ready(Document::open(&path).unwrap());
                let doc = editor.document().unwrap();
                assert_eq!(doc.encoding(), Encoding::Windows1252);
                assert_eq!(doc.text(), "caf\u{e9}\nna\u{ef}ve\n");
            })
            .unwrap();
        window
            .update(cx, |editor, window, cx| {
                editor.run_command("file.encoding_utf8", window, cx);
                editor.run_command("file.line_endings_crlf", window, cx);
                let doc = editor.document().unwrap();
                assert_eq!(doc.text(), "caf\u{e9}\r\nna\u{ef}ve\r\n");
                assert_eq!(doc.line_ending(), LineEnding::CrLf);
                editor.save_document(false, cx);
            })
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "caf\u{e9}\r\nna\u{ef}ve\r\n".as_bytes()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn a_new_search_cancels_the_previous_one(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, &"needle hay\n".repeat(20_000));
        window
            .update(cx, |editor, _, cx| {
                editor.run_find("needle".into(), cx);
                let first = editor.search_job.restart(); // what a 2nd search does
                editor.run_find("hay".into(), cx);
                assert!(first.is_cancelled());
            })
            .unwrap();
        wait_until(&window, cx, "search", |e| e.search_results.is_some());
        window
            .update(cx, |editor, _, _| {
                let query = editor.search_query.as_ref().unwrap();
                assert_eq!(query.text, "hay");
                assert_eq!(
                    editor.search_results.as_ref().unwrap().matches.len(),
                    20_000
                );
            })
            .unwrap();
    }

    /// A plugin folder with a WAT module that answers every request with
    /// `response`. Returns the plugins root.
    fn wat_plugin(name: &str, manifest: &str, response: &str) -> PathBuf {
        let root = temp_path(&format!("plugins-{name}"));
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), manifest).unwrap();
        let escaped: String = response.bytes().map(|b| format!("\\{b:02x}")).collect();
        let wasm = wat::parse_str(format!(
            r#"(module (memory (export "memory") 1)
                (data (i32.const 1024) "{escaped}")
                (func (export "re_alloc") (param i32) (result i32) i32.const 8192)
                (func (export "re_transform") (param i32 i32) (result i64)
                    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len}))))"#,
            len = response.len()
        ))
        .unwrap();
        std::fs::write(dir.join("m.wasm"), wasm).unwrap();
        root
    }

    const DEMO_MANIFEST: &str = r#"{"api":0,"id":"demo","name":"Demo","version":"1.0.0",
        "kind":"wasm","module":"m.wasm",
        "commands":[{"id":"shout","title":"Shout Selection"},
                    {"id":"info","title":"Demo Info","output":"show"}]}"#;

    #[gpui::test]
    fn plugin_commands_need_approval_then_edit_undoably(cx: &mut gpui::TestAppContext) {
        let root = wat_plugin(
            "demo",
            DEMO_MANIFEST,
            r#"{"outputs":["SHOUT"],"status":"shouted"}"#,
        );
        let approvals = root.join("approvals.txt");
        let window = open_test_window(cx);
        let errors = cx.update(|cx| {
            let mut errors = load_plugins(cx, Some(root.clone()), Some(approvals.clone()));
            errors.extend(bind_keymap(cx, Some(String::new())));
            errors
        });
        assert!(errors.is_empty(), "{errors:?}");
        set_text(&window, cx, "say hello world");
        window
            .update(cx, |editor, _, _| {
                editor.with_editor(|doc, e| e.select(doc, 4..9));
            })
            .unwrap();
        // The plugin's command is in the palette under its plugin name.
        cx.simulate_keystrokes(*window, &primary("shift-p"));
        cx.simulate_input(*window, "shout");
        window
            .update(cx, |editor, _, cx| {
                let registry = command_registry(cx);
                let top = re_commands::palette::filter(&registry, &editor.content)[0].clone();
                assert_eq!(top.id, "plugin.demo.shout");
                assert_eq!(top.category, "Plugin: Demo");
            })
            .unwrap();
        // First run asks (sandboxed wording) and does nothing.
        cx.simulate_keystrokes(*window, "enter");
        window
            .update(cx, |editor, _, _| {
                assert!(editor.status.contains("sandboxed"), "{}", editor.status);
                assert_eq!(editor.document().unwrap().text(), "say hello world");
            })
            .unwrap();
        // Running it again approves, runs on a worker, and edits.
        window
            .update(cx, |editor, window, cx| {
                editor.run_command("plugin.demo.shout", window, cx)
            })
            .unwrap();
        wait_until(&window, cx, "plugin result", |e| {
            e.document().unwrap().text() == "say SHOUT world"
        });
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.status, "shouted");
                assert_eq!(editor.editor().unwrap().range(), 4..9); // result selected
            })
            .unwrap();
        assert!(
            std::fs::read_to_string(&approvals)
                .unwrap()
                .contains("plugin:demo@1.0.0:")
        );
        // One undo step.
        cx.simulate_keystrokes(*window, &primary("z"));
        assert_eq!(text(&window, cx), "say hello world");
        // Approval persists: a fresh load runs without asking.
        cx.update(|cx| load_plugins(cx, Some(root.clone()), Some(approvals.clone())));
        window
            .update(cx, |editor, window, cx| {
                editor.with_editor(|doc, e| e.select(doc, 0..3));
                editor.run_command("plugin.demo.shout", window, cx);
                assert!(editor.status.starts_with("Running"), "{}", editor.status);
            })
            .unwrap();
        wait_until(&window, cx, "second run", |e| {
            e.document().unwrap().text() == "SHOUT hello world"
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn plugin_results_are_dropped_if_the_document_changed(cx: &mut gpui::TestAppContext) {
        let root = wat_plugin("demo", DEMO_MANIFEST, r#"{"outputs":["SHOUT"]}"#);
        let window = open_test_window(cx);
        cx.update(|cx| load_plugins(cx, Some(root.clone()), Some(root.join("a.txt"))));
        set_text(&window, cx, "abc");
        window
            .update(cx, |editor, window, cx| {
                editor.with_editor(|doc, e| e.select(doc, 0..1));
                editor.run_command("plugin.demo.shout", window, cx); // asks
                editor.run_command("plugin.demo.shout", window, cx); // runs
                // Edit before the worker's result is applied.
                editor.edit(cx, |doc, e| {
                    e.set_cursor(doc, 3);
                    e.insert(doc, "!")
                });
            })
            .unwrap();
        wait_until(&window, cx, "stale result", |e| {
            e.status.contains("not applied")
        });
        assert_eq!(text(&window, cx), "abc!");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn plugins_can_show_text_and_bad_plugins_are_reported(cx: &mut gpui::TestAppContext) {
        let root = wat_plugin("demo", DEMO_MANIFEST, r#"{"show":"3 words"}"#);
        std::fs::create_dir_all(root.join("broken")).unwrap();
        std::fs::write(root.join("broken/plugin.json"), "{ not json").unwrap();
        let window = open_test_window(cx);
        let errors = cx.update(|cx| load_plugins(cx, Some(root.clone()), Some(root.join("a.txt"))));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("broken"));
        set_text(&window, cx, "one two three");
        window
            .update(cx, |editor, window, cx| {
                editor.run_command("plugin.demo.info", window, cx);
                editor.run_command("plugin.demo.info", window, cx);
                // Selection input with nothing selected is refused.
                assert!(
                    editor.status.contains("select text first"),
                    "{}",
                    editor.status
                );
                editor.with_editor(|doc, e| e.select_all(doc));
                editor.run_command("plugin.demo.info", window, cx);
            })
            .unwrap();
        wait_until(&window, cx, "show", |e| e.json_viewer.is_some());
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.json_viewer.as_ref().unwrap().to_string(), "3 words");
                assert_eq!(editor.document().unwrap().text(), "one two three");
            })
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn copilot_asks_before_sending_text_off_the_machine(cx: &mut gpui::TestAppContext) {
        let root = temp_path("copilot-consent");
        let window = open_test_window(cx);
        cx.update(|cx| load_plugins(cx, Some(root.join("none")), Some(root.join("a.txt"))));
        window
            .update(cx, |editor, window, cx| {
                editor.run_command("tools.ask_ai", window, cx);
                assert!(
                    editor.command.is_none(),
                    "must not open the question field yet"
                );
                assert!(
                    editor.status.contains("over the network"),
                    "{}",
                    editor.status
                );
                // Any other command cancels the pending question.
                editor.run_command("edit.select_all", window, cx);
                editor.run_command("tools.ask_ai", window, cx);
                assert!(editor.command.is_none());
                editor.run_command("tools.ask_ai", window, cx);
                assert!(matches!(editor.command, Some(CommandMode::Ask)));
                editor.cancel_command(cx);
                // Remembered, and revocable.
                editor.run_command("tools.ask_ai", window, cx);
                assert!(matches!(editor.command, Some(CommandMode::Ask)));
                editor.cancel_command(cx);
                editor.run_command("plugins.forget_approvals", window, cx);
                editor.run_command("tools.ask_ai", window, cx);
                assert!(editor.command.is_none());
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A text file of `lines` numbered lines ("line N ...").
    fn numbered_file(name: &str, lines: usize) -> PathBuf {
        use std::io::Write;
        let path = temp_path(name);
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for n in 0..lines {
            writeln!(
                out,
                "line {n} {}",
                if n % 1000 == 7 { "NEEDLE" } else { "hay" }
            )
            .unwrap();
        }
        out.flush().unwrap();
        path
    }

    fn open_huge(
        window: &gpui::WindowHandle<Workspace>,
        cx: &mut gpui::TestAppContext,
        path: &Path,
    ) {
        window
            .update(cx, |editor, _, _| {
                editor.huge_threshold = 1; // everything counts as huge
                editor.open_path(path.to_path_buf());
            })
            .unwrap();
        wait_until(window, cx, "huge index", |e| {
            e.huge().is_some_and(|view| view.index.is_complete())
        });
    }

    #[gpui::test]
    fn huge_files_open_read_only_and_render_lines_on_demand(cx: &mut gpui::TestAppContext) {
        let path = numbered_file("huge-view", 50_000);
        let window = open_test_window(cx);
        open_huge(&window, cx, &path);
        cx.run_until_parked();
        window
            .update(cx, |editor, _, cx| {
                assert!(editor.document().is_none(), "no in-memory document");
                let view = editor.huge().unwrap();
                assert_eq!(view.index.lines(), 50_001); // trailing newline
                assert!(view.index.memory_bytes() < 4096);
                let (text, _) = view.file.line_text(&view.index, 123, 100).unwrap().unwrap();
                assert_eq!(text, "line 123 hay");
                // The list renders only visible rows; each reads its line.
                assert!(!editor.huge_rows.is_empty() && editor.huge_rows.len() < 200);
                drop(editor.render_huge_row(123, cx));
            })
            .unwrap();
        // Editing is refused with an explanation.
        cx.simulate_input(*window, "x");
        window
            .update(cx, |editor, _, _| {
                assert!(editor.status.contains("read-only"), "{}", editor.status);
            })
            .unwrap();
        // Keyboard navigation moves the highlighted line.
        cx.simulate_keystrokes(*window, "down down down");
        cx.simulate_keystrokes(*window, &primary("end"));
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.huge().unwrap().current_line, 50_000);
            })
            .unwrap();
        cx.simulate_keystrokes(*window, &primary("home"));
        cx.simulate_keystrokes(*window, "down");
        // Copy copies the highlighted line.
        cx.simulate_keystrokes(*window, &primary("c"));
        assert_eq!(
            cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
            Some("line 1 hay\n")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn huge_files_support_find_bookmarks_and_go_to_line(cx: &mut gpui::TestAppContext) {
        let path = numbered_file("huge-find", 20_000);
        let window = open_test_window(cx);
        open_huge(&window, cx, &path);
        window
            .update(cx, |editor, _, cx| editor.run_find("needle".into(), cx))
            .unwrap();
        wait_until(&window, cx, "huge search", |e| e.search_results.is_some());
        window
            .update(cx, |editor, window, cx| {
                let results = editor.search_results.as_ref().unwrap();
                assert_eq!(results.matches.len(), 20);
                assert_eq!(results.matches[0].line_index, 7);
                assert_eq!(results.matches[0].start_char, 7);
                assert_eq!(editor.huge().unwrap().current_line, 7);
                editor.next_match(true, cx);
                assert_eq!(editor.huge().unwrap().current_line, 1007);
                editor.run_command("bookmark.all_matches", window, cx);
                editor.run_command("bookmark.copy", window, cx);
            })
            .unwrap();
        let copied = cx.read_from_clipboard().and_then(|i| i.text()).unwrap();
        assert_eq!(copied.lines().count(), 20);
        assert!(copied.starts_with("line 7 NEEDLE\nline 1007 NEEDLE\n"));
        // Go to Line (1-based) with thousands separators.
        cx.simulate_keystrokes(*window, &primary("g"));
        cx.simulate_input(*window, "12,345");
        cx.simulate_keystrokes(*window, "enter");
        window
            .update(cx, |editor, window, cx| {
                assert_eq!(editor.huge().unwrap().current_line, 12_344);
                // Deleting bookmarked lines is an edit: refused.
                editor.run_command("bookmark.delete", window, cx);
                assert!(editor.status.contains("read-only"));
            })
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn open_fully_loads_a_huge_file_for_editing_after_confirmation(cx: &mut gpui::TestAppContext) {
        let path = numbered_file("huge-full", 100);
        let window = open_test_window(cx);
        open_huge(&window, cx, &path);
        window
            .update(cx, |editor, window, cx| {
                editor.run_command("file.open_fully", window, cx);
                assert!(editor.status.contains("memory"), "{}", editor.status);
                assert!(editor.huge().is_some());
                editor.run_command("file.open_fully", window, cx);
            })
            .unwrap();
        wait_until(&window, cx, "full load", |e| e.document().is_some());
        cx.simulate_input(*window, "edited ");
        assert!(text(&window, cx).starts_with("edited line 0 hay"));
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn go_to_line_works_in_normal_documents(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "a\nbb\nccc\n");
        cx.simulate_keystrokes(*window, &primary("g"));
        cx.simulate_input(*window, "3");
        cx.simulate_keystrokes(*window, "enter");
        assert_eq!(selection(&window, cx), (5, 5));
        cx.simulate_keystrokes(*window, &primary("g"));
        cx.simulate_input(*window, "999");
        cx.simulate_keystrokes(*window, "enter");
        assert_eq!(selection(&window, cx), (9, 9)); // clamped to the last line
        cx.simulate_keystrokes(*window, &primary("g"));
        cx.simulate_input(*window, "abc");
        cx.simulate_keystrokes(*window, "enter");
        window
            .update(cx, |editor, _, _| {
                assert!(editor.status.contains("not a line number"))
            })
            .unwrap();
    }

    fn menu_title_center(
        window: &gpui::WindowHandle<Workspace>,
        cx: &mut gpui::TestAppContext,
        title: &str,
    ) -> Point<Pixels> {
        cx.run_until_parked();
        let index = re_commands::menus::MENUS
            .iter()
            .position(|m| m.title == title)
            .unwrap();
        window
            .update(cx, |editor, _, _| {
                editor.menu_titles.borrow()[index]
                    .unwrap_or_else(|| panic!("{title} not painted"))
                    .center()
            })
            .unwrap()
    }

    fn open_menu_title(
        window: &gpui::WindowHandle<Workspace>,
        cx: &mut gpui::TestAppContext,
    ) -> Option<&'static str> {
        window
            .update(cx, |editor, _, _| {
                editor.open_menu.map(|i| re_commands::menus::MENUS[i].title)
            })
            .unwrap()
    }

    #[gpui::test]
    fn menu_bar_opens_switches_on_hover_and_closes(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "hello");
        let file = menu_title_center(&window, cx, "File");
        let search = menu_title_center(&window, cx, "Search");
        assert!(search.x > file.x, "titles are laid out left to right");
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        // Click opens (without running anything under the pointer).
        vcx.simulate_click(file, gpui::Modifiers::none());
        assert_eq!(open_menu_title(&window, cx), Some("File"));
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.tabs.len(), 1, "no command ran")
            })
            .unwrap();
        // The dropdown is drawn below the title, not over it.
        let (title, entries) = window
            .update(cx, |editor, _, cx| {
                (
                    editor.menu_titles.borrow()[0].unwrap(),
                    editor.menu_entries(0, cx),
                )
            })
            .unwrap();
        assert!(entries.iter().flatten().any(|(id, _)| id == "file.save"));
        assert!(title.bottom() <= px(MENU_BAR_HEIGHT + 40.));
        // Moving to another title while open switches menus.
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        vcx.simulate_mouse_move(search, None, gpui::Modifiers::none());
        assert_eq!(open_menu_title(&window, cx), Some("Search"));
        // Clicking the open title closes it; clicking elsewhere closes too.
        vcx.simulate_click(search, gpui::Modifiers::none());
        assert_eq!(open_menu_title(&window, cx), None);
        vcx.simulate_click(file, gpui::Modifiers::none());
        assert_eq!(open_menu_title(&window, cx), Some("File"));
        vcx.simulate_click(point(px(600.), px(500.)), gpui::Modifiers::none());
        assert_eq!(open_menu_title(&window, cx), None);
        assert_eq!(text(&window, cx), "hello");
    }

    #[gpui::test]
    fn menu_items_run_their_commands_from_mouse_and_keyboard(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        set_text(&window, cx, "Hello World");
        // Keyboard: Alt+T opens Transform (Windows/Linux keymap), arrows move,
        // Enter runs. Items are selection-scoped transforms.
        cx.simulate_keystrokes(*window, &primary("a"));
        if cfg!(target_os = "macos") {
            // No Alt+letter menu shortcuts on macOS.
            window
                .update(cx, |editor, _, cx| editor.open_menu_named("Transform", cx))
                .unwrap();
        } else {
            cx.simulate_keystrokes(*window, "alt-t");
        }
        assert_eq!(open_menu_title(&window, cx), Some("Transform"));
        let upper_row = window
            .update(cx, |editor, _, cx| {
                let index = editor.open_menu.unwrap();
                editor
                    .menu_entries(index, cx)
                    .iter()
                    .position(|e| e.as_ref().is_some_and(|(id, _)| id == "transform.upper"))
                    .unwrap()
            })
            .unwrap();
        // First item is highlighted; walk down to UPPERCASE.
        loop {
            let highlight = window.update(cx, |e, _, _| e.menu_highlight).unwrap();
            if highlight == Some(upper_row) {
                break;
            }
            cx.simulate_keystrokes(*window, "down");
        }
        cx.simulate_keystrokes(*window, "enter");
        assert_eq!(open_menu_title(&window, cx), None);
        assert_eq!(text(&window, cx), "HELLO WORLD");
        // Escape closes without running; typing still edits afterwards.
        window
            .update(cx, |editor, _, cx| editor.open_menu_named("Edit", cx))
            .unwrap();
        cx.simulate_keystrokes(*window, "escape");
        assert_eq!(open_menu_title(&window, cx), None);
        // Mouse: click Edit, then click "Undo" in the dropdown.
        let edit = menu_title_center(&window, cx, "Edit");
        let mut vcx = gpui::VisualTestContext::from_window(*window, cx);
        vcx.simulate_click(edit, gpui::Modifiers::none());
        let (row_height, undo_row, bottom) = window
            .update(cx, |editor, _, cx| {
                let entries = editor.menu_entries(1, cx);
                let row = entries
                    .iter()
                    .position(|e| e.as_ref().is_some_and(|(id, _)| id == "edit.undo"))
                    .unwrap();
                (
                    px(ROW_HEIGHT),
                    row,
                    editor.menu_titles.borrow()[1].unwrap().bottom(),
                )
            })
            .unwrap();
        assert_eq!(undo_row, 0);
        // First row: below the title, inside the dropdown's padding.
        let undo = point(edit.x + px(40.), bottom + px(2.) + px(4.) + row_height / 2.);
        vcx.simulate_click(undo, gpui::Modifiers::none());
        assert_eq!(open_menu_title(&window, cx), None);
        assert_eq!(text(&window, cx), "Hello World", "Undo ran from the menu");
    }

    #[test]
    fn toggle_menu_items_report_their_state() {
        // Pure check of the checkmark mapping, no window needed.
        for id in [
            "search.toggle_case",
            "search.toggle_regex",
            "view.toggle_word_wrap",
            "transform.toggle_scope",
            "file.line_endings_lf",
        ] {
            assert!(
                re_commands::menus::MENUS.iter().any(|m| m.items.iter().any(
                    |i| matches!(i, re_commands::menus::MenuItem::Command { id: x, .. } if *x == id)
                )),
                "{id} missing from menus"
            );
        }
    }

    #[gpui::test]
    fn toggles_show_checkmarks_that_follow_state(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        window
            .update(cx, |editor, window, cx| {
                assert_eq!(editor.menu_checked("view.toggle_word_wrap"), Some(false));
                assert_eq!(editor.menu_checked("file.line_endings_lf"), Some(true));
                assert_eq!(editor.menu_checked("file.save"), None);
                editor.run_command("view.toggle_word_wrap", window, cx);
                editor.run_command("search.toggle_case", window, cx);
                assert_eq!(editor.menu_checked("view.toggle_word_wrap"), Some(true));
                assert_eq!(editor.menu_checked("search.toggle_case"), Some(true));
            })
            .unwrap();
    }

    #[gpui::test]
    fn gpui_edit_save_round_trip(cx: &mut gpui::TestAppContext) {
        let path = std::env::temp_dir().join(format!(
            "rusteditor-gpui-save-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "before").unwrap();
        let window = open_test_window(cx);
        window
            .update(cx, |editor, window, cx| {
                editor.tabs[0].state = TabState::Ready(Document::open(&path).unwrap());
                editor.select_all(&SelectAll, window, cx);
                editor.replace_text_in_range(None, "after😀", window, cx);
                editor.save_document(false, cx);
                assert_eq!(editor.status, "Saved");
                assert!(!editor.document().unwrap().is_dirty());
            })
            .unwrap();
        window
            .update(cx, |editor, _, _| {
                editor.tabs[0].state = TabState::Ready(Document::default());
            })
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after😀");
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn worker_results_apply_via_wake_without_manual_poll(cx: &mut gpui::TestAppContext) {
        let path = std::env::temp_dir().join(format!(
            "rusteditor-gpui-wake-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "loaded by worker").unwrap();
        let window = open_test_window(cx);
        window
            .update(cx, |editor, _, _| editor.open_path(path.clone()))
            .unwrap();
        // The loader runs on a real thread; its send must wake the UI task,
        // which then calls `poll`. No timer is involved.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            let loaded = window
                .update(cx, |editor, _, _| {
                    editor.document().map(Document::text).as_deref() == Some("loaded by worker")
                })
                .unwrap();
            if loaded {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker result never applied"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |editor, _, _| {
                for tab in &mut editor.tabs {
                    tab.state = TabState::Ready(Document::default());
                }
            })
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn whole_document_large_paste_runs_on_worker(cx: &mut gpui::TestAppContext) {
        let window = open_test_window(cx);
        window
            .update(cx, |editor, window, cx| {
                editor.document_mut().unwrap().replace_all("before");
                editor.tabs[0].bookmarks.insert(0);
                editor.select_all(&SelectAll, window, cx);
                cx.write_to_clipboard(ClipboardItem::new_string("x".repeat(1_048_577)));
                editor.paste(&Paste, window, cx);
                assert_eq!(editor.document().unwrap().text(), "before");
                let message = editor
                    .paste_receiver
                    .recv_timeout(Duration::from_secs(5))
                    .expect("worker completed");
                assert!(message.whole_document);
                editor.paste_sender.send(message).unwrap();
                editor.poll(cx);
                assert_eq!(editor.document().unwrap().text().len(), 1_048_577);
                assert!(editor.tabs[0].bookmarks.is_empty());
                assert!(editor.editor().unwrap().is_empty());
            })
            .unwrap();
    }
}
