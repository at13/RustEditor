# RustEditor

RustEditor is a Windows text viewer and editor written in Rust. The active UI
implementation uses GPUI; `src/app.rs` and `src/workspace.rs` contain the
previous egui interface for reference, but are not compiled. The archived
egui design and feature documentation is in `docs/egui-archive.md`.

## Run

```powershell
cargo run --release
```

The window has a standard **menu bar** (File, Edit, Search, View,
Transform, Bookmarks, Tools, Agent, Plugins). Click a menu to open it, move across
the bar to switch menus, and click an item or press Enter; items show their
keyboard shortcut and a checkmark for on/off settings (Match Case, Word
Wrap, encoding, line endings). On Windows and Linux, Alt plus the underlined
letter opens a menu (Alt+F for File) and the arrow keys move through it;
Escape or a click elsewhere closes it. Below it, a small toolbar has New,
Open, Save, Undo, Redo, Find, Replace, Match Case (`Aa`), Regex (`.*`), Word
Wrap and the Command Palette. Every menu item is also in the palette.

Open paths from the command line or use **Open...** (`Ctrl+O`). **New** (`Ctrl+N`)
and the tab-strip `+` create untitled tabs; **Save** (`Ctrl+S`) and **Save As...**
(`Ctrl+Shift+S`) write the active file. The whole document is editable:
click to place the cursor, drag or Shift+click to select across lines, and
double-click to select a word. Arrow keys, Home/End, PageUp/PageDown,
`Ctrl+Left/Right` (words) and `Ctrl+Home/End` (document) move the cursor; add
Shift to select. Backspace/Delete at a line boundary joins lines (a CRLF is
removed as one unit). `Ctrl+A` selects the whole document; `Ctrl+C`,
`Ctrl+X` and `Ctrl+V` work on the selection. Long lines scroll horizontally
to follow the cursor (Shift+wheel also scrolls sideways). The status bar
shows the cursor line, column, selection size and cursor count.

**Word wrap** (`Alt+Z`, View > Word Wrap, or toolbar **Wrap**) wraps long lines at the window
width, breaking after spaces; Up/Down and PageUp/PageDown then move by
screen rows. Documents up to 1M characters wrap instantly; larger ones wrap
on a background thread (about 30M characters per second) and show unwrapped
until it finishes. Lines longer than 64K characters wrap at fixed widths.

**Multiple cursors:** `Alt+click` adds a cursor; `Ctrl+Alt+Up/Down` adds one
on the row above/below. Typing, Backspace/Delete, motions, Shift-selection,
copy/cut/paste and transforms apply to every cursor; one keystroke is one
undo step. Copy joins the selections with line breaks, and pasting as many
lines as there are cursors puts one line at each cursor. `Escape` returns to
a single cursor.
**Undo** (`Ctrl+Z`) and **Redo** (`Ctrl+Y` or `Ctrl+Shift+Z`) cover every
edit, including pastes, Replace All, transforms and line deletion. A run of
consecutive typing is one undo step. Undoing back to the saved text clears
the modified marker. History is kept in memory for the session (up to
10,000 steps; ranged edits are capped at 64 MiB of text).

**Find...** (`Ctrl+F`) searches in the background; **Case** and **Regex**
toggle matching options. `F3` and `Shift+F3` cycle through results.
**Replace...** (`Ctrl+H`) changes the current match; **Replace All...** runs
on a worker and applies only if the document has not changed. Click the
gutter to bookmark a line or use the bookmark-match controls. Bookmarked
lines can be copied, cut, deleted, or cleared.

Use the **Scope** toggle to run JSON formatting/viewing, Base64 decoding,
or case transformations on a selection or the complete document. **Copilot**
opens a question field and displays the response in a read-only pane; the
existing restricted Copilot CLI backend snapshots unsaved changes before
asking. The first time, it asks before sending the file's text (including
unsaved edits) off the machine; run it again to allow. **Calculator** opens a keypad and expression field with arithmetic,
parentheses, and precedence. Errors appear in the status bar.

**Plugins** add commands to the palette. Put a plugin folder in
`<config>/plugins/` and run **Reload Plugins**; see
[docs/plugins.md](docs/plugins.md) and the example in
`plugins/examples/sort-lines`. WebAssembly plugins are sandboxed (only the
text they are given; CPU and memory limits). Process plugins run a program
with your full access and say so. Every plugin asks before its first run;
**Forget Plugin and Copilot Approvals** revokes. Results are ordinary
undoable edits, applied only if the document did not change meanwhile.

**Agents** (`Ctrl+Shift+A`, Agent > Agent Panel; `Cmd+Shift+A` on macOS)
open a panel on the right. The **File Agent** answers questions about the
active file and proposes changes with its tools (read the buffer, edit it,
read/list/search nearby files, write or create other files). Every change is
shown as a diff that you **Accept** or **Reject**; an accepted edit is one
undo step, and nothing is written before you accept. Reading outside the
file's folder (its git repository, or its folder) asks once per folder.
Each file can have many conversations (**New**, **History**); they are
saved per file in the per-user data folder (`%LOCALAPPDATA%\RustEditor\agents`
on Windows) and **Delete Agent Data** removes them all. **Stop** ends a
running turn.
The **Extension Builder** (Agent > Extension Builder) creates and changes
extensions by conversation: it writes them in a staging folder, validates
and test-runs them in the real plugin sandbox, and installs only after you
accept a summary; each update keeps the previous version for rollback, and
the new command still asks before its first run. Extensions can be
recipes (lists of built-in text steps), WebAssembly, programs, or **custom
agents** (a prompt plus a tool allowlist), and plugins can contribute
**tools** that agents may call; see [docs/plugins.md](docs/plugins.md).
Models are reached through **provider programs**, never by the editor
itself: `rusteditor-openai` (built from `providers/openai`) talks to any
OpenAI-compatible API, including local Ollama and Azure OpenAI; it is used
automatically when it sits next to `rusteditor.exe` and `OPENAI_API_KEY` is
set, and **Agent > Providers...** shows how to configure others. The first
message to each provider asks before sending the conversation (and any file
text the agent reads) off the machine. API keys stay in environment
variables. Limits: providers are not yet tested against every service;
custom agents cannot add UI; the transcript renders plain text (no
Markdown).

**Command Palette** (`Ctrl+Shift+P` or `F1`; `Cmd+Shift+P` on macOS) lists
every command with its shortcut. Type part of a name (for example
`fmt json` or `bookmark clear`) and press Enter to run the top match, or
click a row.

**Keyboard shortcuts** come from a keymap. The defaults are in
`assets/default-keymap.toml`. To change them, run **Open Keymap File** from
the palette. This creates your user keymap (`%APPDATA%\RustEditor\keymap.toml`
on Windows, `~/Library/Application Support/RustEditor/keymap.toml` on macOS,
`~/.config/rusteditor/keymap.toml` on Linux). Save it, then run **Reload
Keymap**. `mod` means Ctrl on Windows/Linux and Cmd on macOS; an empty
command (`""`) removes a default binding. Mistakes are reported in the
status bar and the rest of the keymap still loads:

```toml
[global]
"mod-shift-f" = "transform.json_format"
"mod-h" = ""

[editor]
"mod-d" = "edit.delete_line"

[editor.macos]          # only on macOS; also .windows and .linux
"ctrl-a" = "editor.home"
```

The macOS defaults follow Mac conventions: Option+Left/Right move by word,
Cmd+Left/Right to the line start/end, Cmd+Up/Down to the document start/end,
Replace is Cmd+Option+F (Cmd+H stays "Hide"), and Redo is Cmd+Shift+Z.

**Migration note:** The GPUI UI is a new implementation, not a drop-in
egui replacement. Undo/redo restore a single cursor, and IME composition
uses only the primary cursor. Whole file actions and copying a document-wide selection materialize
the complete text in memory. Close any previously running RustEditor window before launching
a newly installed build. Builds from before the platform layer used a
different single-instance mechanism, so an old window and a new one would
not see each other and could both open the same file.

## Architecture and large-file decisions

```text
src/                  GPUI application (root package `rusteditor`)
  main.rs             Startup: absolute paths, single instance, launches GPUI
  gpui_workspace.rs   GPUI tabs, virtual rows, input, tools, and workers
  gpui_workspace/agent.rs  Agent panel: sessions, tool workers, approvals
  ai.rs               Copilot provider and temporary snapshot handling
  config.rs           User keymap template
assets/
  default-keymap.toml Default key bindings (embedded in the binary)
  agents/             Built-in agents (File Agent, Extension Builder)
crates/               UI-independent crates (never depend on GPUI)
  re-core/            Rope storage, revision, save/reload (document.rs);
                      encodings, streaming decode/encode and crash-safe
                      writes (text_io.rs); change sets, undo history and
                      multi-cursor selections; rope splice for large pastes
  re-view/            Soft wrap and the display map (line <-> display row)
  re-tasks/           Worker threads, cancellation tokens, UI wake-ups
  re-platform/        Per-user dirs, single instance (named pipe / Unix socket)
  re-plugin/          Plugin manifests, WASM sandbox (wasmi), process host,
                      recipes, agent/tool declarations, approvals
  re-agent/           Agent loop (state machine), conversations + store,
                      tool registry, risks, provider protocol and host
  re-agent-tools/     Built-in agent tools and the Extension Builder's tools
providers/openai/     rusteditor-openai: OpenAI-compatible provider program
plugins/examples/     Example plugin (Rust -> wasm32)
packaging/            Linux .desktop entry, macOS bundle notes
  re-commands/        Command registry, platform-neutral keymaps, palette matching
  re-search/          Line-oriented search and replacement engine
  re-ext-transforms/  JSON, Base64, and case conversion
  re-ext-calculator/  Arithmetic parser and result formatting
```

Each path immediately creates a loading tab. A separate worker decodes the
file chunk by chunk into a `ropey::Rope` (UTF-8, UTF-8 with BOM, UTF-16
LE/BE with BOM, or Windows-1252 when the bytes are not valid UTF-8; files
with NUL bytes are refused as binary); completed tabs are delivered over a
channel. Every worker send also wakes the UI task directly, so results
appear immediately and an idle editor does not poll. Invalid files and worker failures show an error tab. The GPUI
`uniform_list` creates only visible rows, and each row shapes at most 1,024
characters: the visible window of the line starting at the horizontal scroll
column, so a multi-megabyte line never reaches the text shaper. The editor
state (`re-core::editor::EditorState`) is a selection in Unicode char offsets
over the whole rope; edits go through the document's undo history. UTF-16
appears only at the native text-input (IME) boundary, converted with ropey
in O(log n). Selecting and deleting a huge range is recorded as an O(1) rope
snapshot rather than copying the removed text into undo history.

Pastes above 1 MiB run on a worker-owned rope snapshot; prefix, insertion,
and suffix are joined with rope operations. A worker result is applied only
if the document revision still matches. Search runs on the same snapshot
model. A new search or Replace All cancels the previous one. Saving streams
rope chunks through the encoder instead of flattening the file. Whole-file JSON and case transforms necessarily
materialize their input/output; avoid them on files that exceed available
memory.

**Huge files (256 MB and larger) open instantly, read-only,** without being
loaded into memory. The file is read in 64 KB pieces as you scroll (at most
64 MB cached), and a background scan builds a line index that keeps one
position per 1,024 lines (about 113 KB for 15 million lines); the tab shows
"indexing N%" until it finishes, and you can scroll meanwhile. Measured on
this machine (release build, SSD): a 2 GB, 25-million-line log opened with
the window at ~100 MB of memory; indexing ran at about 2 GB/s; jumping to a
random line takes about 0.1 ms. In this mode:

- Arrow keys, PageUp/PageDown and `Ctrl+Home/End` move the highlighted line;
  click a line to highlight it; **Go to Line** (`Ctrl+G`, `Cmd+L` on macOS).
- **Find** streams through the whole file on a worker (cancelled by a new
  search); Next/Previous jump to matches; bookmark matches and **Copy
  bookmarked lines**. `Ctrl+C` copies the highlighted line.
- Editing is refused with a message. **Open Fully for Editing** (palette)
  loads the file into memory after warning how much it needs.
- Lines show their first 2 KB; search and copy use up to 1 MB per line.
- UTF-16 files this large are not supported yet. Changes by other programs
  are reported; **Reload from Disk** re-reads the file.

**Files are not locked.** Other programs (git, compilers, grep, Copilot) can
read and write a file while it is open. Instead, RustEditor remembers the
file's modification time and size and checks them when you return to the
window or switch tabs:

- If the tab has no unsaved edits, it reloads automatically (undoable with
  `Ctrl+Z`).
- If it has unsaved edits, the status bar warns once. **Reload from Disk**
  (palette) discards your edits; **Save** refuses and asks you to save again
  to overwrite the other program's change.
- A deleted file is reported; Save recreates it.

**Saving is crash-safe:** the text is written to a temporary file next to
the original, flushed to disk, and renamed over it, so a crash or power loss
leaves either the old or the new file, never a truncated one. Permissions
are kept and symlinks are followed. If another program holds the file open
so it cannot be replaced (possible on Windows), RustEditor writes in place
and says so in the status bar. Files marked read-only open read-only.

The status bar shows the encoding and line ending (`UTF-8 | CRLF`, with
"(mixed)" if both LF and CRLF were found). Enter inserts the file's line
ending; new files use the platform's (CRLF on Windows, LF elsewhere). The
palette has **Save with Encoding: ...** (UTF-8, UTF-8 with BOM, UTF-16
LE/BE, Windows-1252) and **Convert Line Endings to LF/CRLF** (one undo step).
Saving text that the chosen encoding cannot represent fails with a message
and leaves the file untouched.

**One window per user.** Launching RustEditor again (from a terminal, the
context menu, or "Open With") hands the files to the running window and
exits. Relative paths are resolved in the launching terminal's directory
first. The first process takes a lock in a per-user runtime directory
(`%LOCALAPPDATA%\RustEditor`, `$XDG_RUNTIME_DIR/rusteditor`, or
`$TMPDIR/rusteditor` on macOS) and listens on a **named pipe with a random
name** (Windows, remote clients rejected) or a **Unix socket** in that
directory (created 0700 and refused if another user owns it). No network
port is opened, and messages are size-limited. On macOS, files opened from
Finder arrive as Apple Events and are handled the same way.

## Installation

To build and install the GPUI application on Windows, first close any running
RustEditor window so Windows can replace the executable:

```powershell
cargo build --release
.\scripts\install.ps1
```

The installer copies the binary to `%LOCALAPPDATA%\Programs\RustEditor`,
registers the user PATH entry, and adds an **Open with RustEditor** context
menu entry. `.\scripts\uninstall.ps1` removes those registrations and the
installed executable.

**Linux** (per user; needs the GPUI build dependencies listed in
`.github/workflows/ci.yml`):

```sh
cargo build --release
./scripts/install.sh      # ~/.local/bin/rusteditor + app-menu entry
./scripts/uninstall.sh
```

**macOS:** `cargo install cargo-bundle && cargo bundle --release` builds
`RustEditor.app`; see `packaging/macos/README.md`. Signing and notarization
are not set up.

The Linux and macOS builds have not been run yet (they are compiled in CI,
which has not run); treat them as untested.
