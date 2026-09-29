# RustEditor (historical egui implementation)

RustEditor is a small native text viewer/editor written in Rust. Its first milestone
intentionally focuses on opening, viewing, editing, and saving UTF-8 text files,
with a light visual treatment inspired by Notepad++.

The application window and workspace are created before any requested file is
read. Each file opens in its own tab and is loaded by a dedicated worker thread.
Multiple command-line paths are accepted and dispatched to separate loaders.
RustEditor is single-instance per Windows user: later launches forward their file
paths to the existing window, which opens them as tabs and moves to the
foreground.

## Run

```powershell
cargo run --release
```

Use **File > Open** (`Ctrl+O`) to open a file, click a line to edit it, and use
**File > Save** (`Ctrl+S`) to write changes. `Enter`, **Insert line**, and
**Delete line** provide basic line editing. Pressing `Enter` splits the active
line at the text cursor; any highlighted text is replaced by the line break.
`Ctrl+A` selects all text in the active line or current long-line page. Press it
again to select the entire document. If no line editor has keyboard focus, one
`Ctrl+A` selects the entire document. `Ctrl+C` then copies the complete file.
`Delete` or `Backspace` removes the current line/page selection, or clears the
document when the entire document is selected.

Create an untitled file with **File > New** (`Ctrl+N`) or click the `+` button
immediately to the right of the rightmost tab.

For wide lines, drag the editor content left or right with the mouse to pan
horizontally. The bottom scrollbar and horizontal touchpad/wheel gestures remain
available. The line-number/bookmark gutter remains pinned at the left edge while
text pans underneath it. Dragging inside the active text field continues to
select text.

The **Tools** menu adds:

- Background search and replace (`Ctrl+F` / `Ctrl+H`) with case-sensitive or
  case-insensitive matching, optional regular expressions, and line bookmarks.
- A parsed, pretty-printed JSON viewer and an in-place JSON formatter.
- Standard, unpadded, and URL-safe Base64 decoding into UTF-8 text.
- `lowercase`, `UPPERCASE`, `Proper Case`, `camelCase`, `PascalCase`,
  `snake_case`, and `kebab-case` conversion.
- A provider-neutral **Copilot** button for asking questions about the selected
  text, active line, or entire file without blocking the editor.
- A **Calculator** overlay for basic arithmetic with decimals, unary signs,
  parentheses, and operator precedence.

Choose **Selection** or **Entire file** in the toolbar before running a tool.
Selection scope uses the highlighted text in the active editable line. Invalid
JSON, Base64, selection ranges, and non-UTF-8 decoded bytes are reported in the
status bar without changing the document.

Search runs on a rope snapshot in a per-tab worker thread. **Find All** returns
up to 100,000 matches and supports `F3` / `Shift+F3` navigation. Replace-current
and Replace All verify the document revision before applying results, so edits
made while a search is running cannot be overwritten by stale worker output.
Regex replacement supports capture references such as `$1`. Regex matching is
line-oriented, which avoids materializing the complete file as one `String`.
Use **Bookmark current match** or **Bookmark all matching lines** in the search
panel. Bookmarked rows show a `B` marker in the gutter; clicking that marker
also toggles a bookmark. The search panel can copy all bookmarked lines to the
clipboard, cut them, delete them, or clear the bookmarks. Copy preserves each
line's existing line ending. Cut and delete are disabled for read-only files.

The **Copilot** button opens an AI Assistant window. Enter a question, choose
**Selection / active line** or **Entire file**, and click **Ask**. RustEditor
currently invokes GitHub Copilot CLI, so `copilot` must be installed, available
on `PATH`, and authenticated. RustEditor sends a temporary snapshot path, requested
line or scope, question, and any exact highlighted text. Copilot then uses its
file viewer to inspect only the parts of the snapshot it needs. If explicitly
asked to create a new or transformed file, Copilot can create it inside the
isolated request directory and return its absolute path.

## Install on Windows

Build and install RustEditor for the current Windows user:

```powershell
cargo build --release
.\scripts\install.ps1
```

The installer copies the executable to
`%LOCALAPPDATA%\Programs\RustEditor\rusteditor.exe`, adds that directory to the
persistent user `PATH`, and adds **Open with RustEditor** to the context menu for
all files. The context-menu command passes the selected file path to RustEditor,
which opens it at startup. Open a new terminal before invoking `rusteditor` by
name. On Windows 11, the entry may appear under **Show more options**.

To remove the installed executable, PATH entry, and context-menu registration:

```powershell
.\scripts\uninstall.ps1
```

## Architecture

```text
src/
  main.rs        Native window creation and application startup
  instance.rs    Per-user process lock and loopback path forwarding
  workspace.rs   Tabs, loader threads, load-result messages, and base UI
  app.rs         Per-document editor UI, commands, and editing state
  ai.rs          Provider-neutral AI requests and Copilot CLI backend
  document.rs    Rope storage, streamed file I/O, and lock ownership
  transforms.rs  JSON, Base64, and case-conversion operations
```

The base workspace owns lightweight tab states: `Loading`, `Ready`, or `Failed`.
Opening a path immediately creates and selects a `Loading` tab, then starts a
named operating-system thread for that file. The worker owns all open, lock,
UTF-8 validation, and rope construction work. It sends either a complete
`Document` or an error through an `mpsc` channel and requests an egui repaint.
The UI thread only polls completed messages and never waits for a loader.

Each ready tab owns a separate `RustEditorApp` editor state. A huge, slow, invalid,
or inaccessible file therefore cannot block startup, prevent switching to an
already loaded tab, or prevent opening another file. A panic inside a loader is
caught at the thread boundary and converted into a failed tab instead of
unwinding the UI thread.

### Single-window file forwarding

The first RustEditor process holds an advisory lock on a per-user file in the
Windows temporary directory and writes a randomly assigned loopback TCP port
into that file. It listens only on `127.0.0.1`. A later launch cannot acquire
the process lock, so it reads the port, sends a bounded length-prefixed list of
file paths, waits for acknowledgement, and exits without creating a window.
The primary process receives those paths through an internal channel, starts
the normal independent file loaders, and requests window focus.

Search and Replace All use the same isolation model: each operation runs on a
cloned rope snapshot in a worker thread. Results include the source document
revision and are rejected if that tab changed before the result arrived.

AI requests also run on a per-tab worker thread. The UI depends on a provider
interface rather than Copilot-specific process details, so additional model
backends can be added without changing editor selection or response UI. Every
request streams the current rope to a uniquely named temporary file and sets
`-C` to its isolated directory. This avoids conflicts with RustEditor's exclusive
lock on the original file and guarantees Copilot sees all in-memory edits,
including dirty and untitled documents. The snapshot is removed after the
request finishes.

The backend sends only metadata, highlighted text when present, and the
question over standard input. Copilot receives `view` plus `apply_patch`.
`apply_patch` may create new output files inside the isolated request workspace;
an explicit permission denial protects the source snapshot from modification.
Shell execution, URL access, and built-in MCP servers remain disabled. After
completion RustEditor removes the source snapshot. If Copilot created output
files, the request directory remains so returned paths continue to work;
otherwise the empty directory is removed. Provider errors, missing CLI
installation, snapshot failures, and authentication failures are shown inside
the AI Assistant window. Captured responses are capped before being displayed.

The editor is also separated from the document storage model. `RustEditorApp`
keeps only the active line page's editable `String`; `Document` owns the
complete text, file handle, dirty state, and lock.

### Loading and displaying large files

- Each file is decoded on its own worker thread directly from a
  `BufReader<File>` into a `ropey::Rope`. RustEditor does not first read the
  complete file into one contiguous `String`.
- A rope stores text as a balanced tree of smaller chunks. Inserting or deleting
  text therefore does not require moving all bytes after the edit, as it would
  with one large `String`.
- The editor uses egui's virtualized `ScrollArea::show_rows`. Only the rows in
  the viewport are converted from rope slices to display strings and laid out.
  A million-line file does not create a million UI widgets.
- Inactive rows render at most a 1,024-character preview. Lines longer than
  16,384 characters are opened as editable 4,096-character pages with previous
  and next controls. RustEditor never sends a multi-megabyte line to egui for
  shaping or layout.
- Only the selected line or long-line page becomes a `TextEdit`; inactive
  visible lines are cheap labels. This avoids duplicating the whole document in
  egui's edit buffer.
- Clipboard paste events are intercepted before the single-line widget can
  flatten and lay out the payload. Small pastes are spliced directly into the
  rope; pastes of 1 MiB or more run on a worker-owned rope snapshot. The worker
  builds the pasted text as its own rope and joins it with the document prefix
  and suffix using logarithmic tree append operations. Rope metadata supplies
  the inserted character count and line-count change without rescanning the
  clipboard text. Newlines are preserved, and egui receives only the bounded
  page around the resulting cursor.
- Saving iterates over rope chunks and writes through `BufWriter`, so saving
  does not allocate a second full-size copy of the document.
- Whole-document transformation tools intentionally materialize an input and
  output `String` because JSON parsing and identifier-style case conversion
  need complete context. AI requests instead give Copilot a file path; dirty
  rope contents are streamed to a temporary snapshot without creating a second
  full-document `String`.

This design keeps UI and edit costs tied mostly to visible content and the
edited page. Opening still validates and indexes the full UTF-8 file because
random line access needs a complete rope. Memory usage is therefore proportional
to file size, but without a second full-document display buffer. Long-line page
edits are written directly into the corresponding rope character range, so the
rest of the line is neither copied nor reshaped.

### File locking

RustEditor first opens a file for reading and writing and requests an exclusive
advisory lock with `fs2`. The same handle and lock are retained for the lifetime
of the open document.

- If the exclusive lock succeeds, the file is editable.
- If write access or locking fails, RustEditor reopens the file read-only and
  clearly shows `READ ONLY` in the status bar.
- Save writes through the already locked handle, flushes the buffered writer,
  and calls `sync_all`.
- Opening another file, using Save As, or closing the application releases the
  old lock.

Advisory locks require cooperating applications. They prevent another RustEditor
instance (and other lock-aware software) from editing simultaneously, but they
cannot stop a program that ignores advisory locks. Saving in place preserves
the lock throughout the write. The tradeoff is that a process or machine crash
during saving can leave a partially written file; a future version can add a
journal/backup strategy while preserving explicit lock ownership.

### Current scope and decisions

- **UTF-8 only:** invalid UTF-8 is rejected with an error instead of silently
  replacing bytes. This avoids corrupting unknown encodings when saving.
- **Line-oriented editing:** virtualized rows keep the implementation small and
  predictable for large files. Tool selection currently applies within the
  active line; whole-file scope handles multi-line transformations. Multi-line
  mouse selection, undo/redo, search, syntax highlighting, tabs, and encoding
  conversion are future work.
- **Immediate-mode UI:** `eframe`/`egui` gives a compact cross-platform native
  application without a browser runtime.
- **Independent background loading:** each open request receives a dedicated
  loader thread and tab state. The current version reports indeterminate
  progress because `ropey::Rope::from_reader` does not expose byte progress.
  Cancellation and byte-level progress are future enhancements.

## Build

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```
