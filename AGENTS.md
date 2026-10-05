# AGENTS.md — RustEditor

Guidance for coding agents working in this repository. Read `README.md` for
user-facing behavior; this file covers architecture, invariants, and workflow.

## Project summary

RustEditor is a Windows, Notepad++-style text editor for very large files.
Rust 2024 edition, GPUI (`=0.2.2`, `windows-manifest` feature) for the UI,
and `ropey::Rope` for document storage.

`src/app.rs` and `src/workspace.rs` are the **retired egui UI**. They are not
compiled (not declared in `main.rs`); keep them only as reference. Do not
edit them or re-add `eframe`. Historical docs: `docs/egui-archive.md`.

## Module map

The repository is a Cargo workspace. The root package is the GPUI app
(`rusteditor`); UI-independent logic lives in `crates/` (see
`docs/design-proposal.md` for the target layout).

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Startup: makes paths absolute, single-instance acquisition, launches GPUI |
| `src/gpui_workspace.rs` | Entire GPUI UI: tabs, virtual rows, native text input, actions/keybindings, worker channels, toolbars, headless tests |
| `crates/re-core/src/document.rs` | Rope, revision, undo, save (`save_checked`, `SaveConflict`), reload (`reload_if_revision`), `disk_status`, encoding/line-ending settings |
| `crates/re-core/src/huge.rs` | Huge files: `HugeFile` (positioned reads, chunk cache, line-start cache), `build_index` (sampled `LineIndex`, progress, cancel), `Lines` (streaming, capped) |
| `crates/re-core/src/text_io.rs` | Encoding detection, streaming decode/encode (UTF-8/BOM, UTF-16, Windows-1252), `atomic_write`, `convert_line_endings`, `DiskState` |
| `crates/re-tasks/` | `spawn` (named worker, panics become errors), `CancelToken`, `Latest`, `wake` (`WakingSender`) |
| `crates/re-core/src/editor.rs` | `EditorState`: multi-cursor selections, motions via `VerticalLayout` (CRLF-aware), multi-range edits returning `LineEdit`s, `shift_lines`, IME/UTF-16 mapping |
| `crates/re-view/src/lib.rs` | Soft wrap (`cell_width`, `segments`), `DisplayMap` (line <-> display row, incremental `apply`), `WrapLayout` |
| `crates/re-core/src/change.rs` | `ChangeSet`: atomic multi-range edits, inverse, position mapping |
| `crates/re-core/src/history.rs` | Undo/redo groups, typing merge, memory bounds |
| `crates/re-core/src/selection.rs` | `Selection` (with vertical goal column) and `Selections` (sorted, merged, primary) |
| `crates/re-core/src/paste.rs` | Rope splice (`paste::apply`) for large pastes on worker threads |
| `crates/re-search/src/lib.rs` | Line-oriented search and replace engine (plain/regex, case) |
| `crates/re-ext-transforms/src/lib.rs` | JSON view/format, Base64 decode, case conversions |
| `crates/re-ext-calculator/src/lib.rs` | Arithmetic expression parser for the calculator overlay |
| `src/ai.rs` | Copilot CLI provider; sends a temp snapshot of unsaved text |
| `src/config.rs` | User keymap template (paths come from `re-platform`) |
| `crates/re-plugin/` | `plugin.json` (`Manifest`, `Registry`; kinds `wasm`, `process`, `recipe`, `agent`; contributed `tools`), `protocol` (request/response JSON), `recipe` (declarative steps, run host-side), `wasm` (wasmi sandbox: only `env.log`, fuel + memory caps), `process` (stdin/stdout, timeout, output cap), `call_tool`, `approvals` |
| `assets/agents/` | Built-in agents (`file-agent`, `extension-builder`): `*.json` (scope, tool allowlist) + system prompts, embedded with `include_str!`. The builder's prompt gets `docs/plugins.md` appended, so keep that doc accurate. Keep tool names in prompts, `*.json`, `re-agent-tools` and the design in sync |
| `crates/re-agent/` | Agent core, UI-free: `session` (loop state machine returning `Effect`s), `conversation` + `store` (private per-user JSON, atomic), `tools` (`Tool`, `ToolRegistry`, `Risk`, `Approval`/`Remember`, `Action`), `schema` (argument validation), `protocol` + `provider` (JSON-lines provider programs, `ScriptedProvider` for tests), `diff`, `prompt` |
| `crates/re-agent-tools/` | Built-in tools (`buffer`, `files`, `search`, `process`), scope checks (`scope`), Extension Builder tools (`ext`: staging, validate, test, install with `.history` rollback), plugin-contributed tools (`plugin`, named `<id>__<tool>`) |
| `src/gpui_workspace/agent.rs` | Agent panel: `AgentState`, live sessions, provider runs, tool prepare/run on workers, approval/consent cards, applying `edit_buffer` edits |
| `providers/openai/` | `rusteditor-openai`: OpenAI-compatible provider program (ureq + rustls); the editor itself never links an HTTP client of its own |
| `docs/plugins.md`, `plugins/examples/` | Plugin author guide and the example WASM plugin (built separately for `wasm32-unknown-unknown`) |
| `crates/re-platform/` | `paths` (config/runtime dirs, `ensure_private_dir`, `file_url_to_path`), `instance` (lock + named pipe / Unix socket forwarding). The only crate with OS-specific code besides GPUI |
| `packaging/`, `scripts/install.sh` | Linux desktop entry and per-user install; macOS bundle notes (`[package.metadata.bundle]` in `Cargo.toml`) |
| `assets/default-keymap.toml` | Default keymap, embedded with `include_str!` |
| `crates/re-commands/src/menus.rs` | Menu bar as data (`MENUS`): every item is a command id; tests require every visible command to be in exactly one menu |
| `crates/re-commands/` | Command registry (`builtin.rs`), keystroke normalization (`mod`), keymap parse/resolve, palette fuzzy match |
| `scripts/install.ps1`, `scripts/uninstall.ps1` | Per-user install, PATH, context menu |

## Architecture and invariants

Preserve these when changing code:

1. **Never materialize the whole document on the UI thread** for normal
   operations. Rendering uses GPUI `uniform_list` (visible rows only);
   each row shapes at most `ROW_WINDOW_CHARS` (1,024) characters starting at
   the tab's `h_scroll` column (horizontal virtualization).
   Files of `HUGE_FILE_BYTES` (256 MB) or more open as `TabState::Huge`
   (`re_core::huge`): no rope, positioned reads through a 64 MiB chunk
   cache, sampled line index built on a worker. `document()` is `None`
   there, so editing paths no-op; every mutating entry point must call
   `refuse_huge_edit` first so the user gets a message. Never read a whole
   huge file on the UI thread; per-line reads are capped
   (`HUGE_DISPLAY_BYTES`, `HUGE_LINE_BYTES`, `HUGE_COPY_BYTES`).
2. **The whole document is editable.** Each `Tab` owns an
   `re_core::editor::EditorState` (anchor/head in char offsets). Document
   editing logic belongs in `EditorState` (UI-free, unit-tested); the UI
   calls it via `Workspace::edit`/`motion`/`with_editor`, which also handle
   read-only, bookmarks (`after_edit`) and scrolling
   (`ensure_cursor_visible`). The command bar is a separate single-line input
   (`content`/`selected`/`marked`); `self.command.is_some()` decides which
   one handlers and the IME act on.
3. **Offsets:** the rope and `EditorState` use Unicode char offsets. UTF-16
   appears only in `EntityInputHandler` (`EditorState::to_utf16` /
   `from_utf16`, via ropey). The command bar uses UTF-8 byte ranges
   (`to_utf16`, `from_utf16`, `utf16_to_bytes`). Test with non-BMP text (for
   example `😀`) and CRLF (the cursor never sits inside `\r\n`).
   Hit-testing and IME bounds use the `PaintCache` filled by `RowText`
   during prepaint; it only knows visible rows.
   With word wrap on, list items are display rows (`DisplayMap`), not lines.
   After an edit, pass the pre-edit revision to `after_edit` so the wrap map
   is updated incrementally from the `EditOutcome`'s `LineEdit`s; anything
   else (undo, Replace All, paste on a worker, load) makes `sync_wrap`
   rebuild it (on a worker above `SYNC_WRAP_CHARS`). Wrapping must use
   `re_view::cell_width` so row counts match what is painted.
4. **Background work uses snapshots + revision checks.** Loading, reload,
   search, Replace All, wrapping, large pastes (>1 MiB), and Copilot run via
   `re_tasks::spawn` (never raw `thread::spawn`) with a rope snapshot and
   send results over `WakingSender` channels
   (`waker.message_channel()`); each send wakes the UI task, which drains
   them in `Workspace::poll`. Do not reintroduce a polling timer. Apply a result only if `document.revision()` still
   matches (`replace_rope_if_revision`, `replace_global_range_if_revision`).
   Searches also carry a generation number so stale results are dropped.
   Long jobs take a `CancelToken` (`Latest::restart` cancels the previous
   job of the same kind) and check it periodically.
5. **No file locks; detect external changes.** Documents never hold the file
   open. `disk_status()` compares mtime/size with the last read/write;
   `check_disk` runs on window activation and tab switch (no polling). A
   clean tab reloads (undoable); a dirty one warns once; `save_checked`
   refuses with `SaveConflict` unless forced (second Save). Read-only means
   the file's read-only attribute/permission. Every mutating path must still
   check `is_read_only()` first. Writes go through `text_io::atomic_write`
   (temp file + fsync + rename, in-place fallback only if the rename is
   refused); never truncate the original before the new content is safely
   on disk. Copilot still gets a temp snapshot, because it must see unsaved
   edits.
6. **Bookmarks** are per-tab line numbers. Adjust them when lines are
   inserted/removed (`adjust_bookmarks`); clear them on whole-document
   replacement.
7. **Saving streams** rope chunks through `text_io::encode`; do not call
   `rope.to_string()` to save. Decoding also streams (`text_io::decode`).
   Enter inserts `doc.line_ending()`; tests that assert on `\n` should use
   `Document::with_line_ending(LineEnding::Lf)`.
8. **Undo:** every text mutation must go through a `Document` method
   (`apply_changes`, `edit_global_range_if_revision`, `replace_all`,
   `replace_rope_if_revision`, …) so it is recorded in history. Never mutate
   the rope directly. Pass `EditKind::Typing` only for small typed
   insertions. After `undo`/`redo`, use the returned `Applied` to move the
   cursor and adjust or clear bookmarks. Dirty state is "current state ≠
   saved state", so undoing back to the saved text clears `*`.
9. **Single instance:** a second launch forwards absolute paths to the
   running window over a named pipe (random name stored in the per-user
   runtime dir) or a Unix socket in a 0700 dir. Never open a TCP port or put
   the endpoint in a shared directory. Keep OS-specific code (`#[cfg]`,
   `unsafe` FFI) in `re-platform`. Keymap bindings that differ by OS go in
   `[global.macos]`/`[editor.macos]` (etc.) sections, not in code.
10. **Portability:** GPUI is built without default features; the macOS and
   Linux backends are enabled in `Cargo.toml` target sections. Do not remove
   them (macOS would render no text; Linux would panic at startup). Shell
   scripts and `.desktop` files must stay LF (`.gitattributes`). Check core
   crates for Linux with
   `cargo clippy --target x86_64-unknown-linux-gnu -p re-core ... -- -D warnings`
   (the app crate needs a Linux C toolchain, so only CI can build it).
11. **Plugins:** plugin code never touches editor state directly. Build a
   `re_plugin::Request` from a snapshot, run it with `re_tasks::spawn`, and
   apply the `Outcome` in `poll` only if the tab and revision still match,
   through `EditorState`/`Document` (undoable). Anything that runs third-party
   code or sends document text off the machine must go through
   `Workspace::confirm` with a key that changes when what is trusted
   changes (`Plugin::approval_key`, `COPILOT_EGRESS_KEY`). Never describe a
   process plugin as sandboxed. WASM plugins may import only `env.log`;
   keep fuel/memory/output limits.

12. **Agents:** the model never approves anything. Tools must not mutate
   in `prepare` (no side effects; return `Prepared::Ask` with a diff for
   every write, `run_command`, install, or read outside the scope); the
   host runs the `Action` only after the session says `Run`. Buffer edits
   go through `EditorState::replace_ranges` (one undo step) and are rebased
   by old text if the document changed (`re_agent_tools::buffer::rebase`).
   Tool workers and provider events carry the live session's generation;
   drop stale ones. Sending a conversation to a provider needs the
   per-provider egress approval (`ProviderConfig::egress_key`). Custom
   agents run only after their plugin approval. `ext_*` tools are only
   given to the built-in Extension Builder. Tests use `ScriptedProvider`
   and isolated folders (`agent_setup`); never let a test write to the real
   config/approvals (pass explicit paths to `load_plugins`).

Whole-file JSON/case transforms and copying a document-wide selection do
materialize text; that is an accepted, documented cost.

The target architecture and migration plan are in `docs/design-proposal.md`.
The release profile keeps unwinding on purpose (`re_tasks::spawn` turns a
worker panic into an error so one tab fails instead of the whole process);
do not set `panic = "abort"`.

## Adding features

- Add a command: register it in `crates/re-commands/src/builtin.rs` (stable
  dotted id, title, category), add its handler in `command_handler()` in
  `gpui_workspace.rs` (a test fails if one is missing), give it a default key
  in `assets/default-keymap.toml` if it deserves one, and put it in a menu
  in `crates/re-commands/src/menus.rs` (a test fails if a visible command is
  in no menu). On/off commands get a checkmark in `Workspace::menu_checked`.
  The quick toolbar (`render_toolbar`) is for a few frequent commands only. Do not add `KeyBinding::new`
  calls; all keys come from the keymap and dispatch `RunCommand(id)`.
  Keymaps use `mod` (Ctrl on Windows/Linux, Cmd on macOS), never `ctrl`
  for primary shortcuts. In tests, bind with
  `bind_keymap(cx, Some(String::new()))` and press keys with `primary("z")`.
- Put pure logic (parsing, transforming, searching) in a crate under
  `crates/` with unit tests; keep `gpui_workspace.rs` focused on UI and state.
  Crates under `crates/` must never depend on `gpui` or `rfd` (CI checks this).
- Any operation that may take more than a few milliseconds on a large file
  belongs on a worker using the snapshot + revision pattern above.
- Report errors in the status bar (`self.status`); do not panic on user input
  or I/O failures.
- Update `README.md` when user-visible behavior or architecture changes.

## Build, test, lint

Run from the repository root:

```powershell
cargo fmt --all
cargo test --workspace --quiet
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

All three must pass before finishing. GPUI UI behavior is tested with
`#[gpui::test]` headless tests in `gpui_workspace.rs` (requires the
`test-support` dev-dependency feature). Use `cx.simulate_input` and
`cx.simulate_keystrokes` to exercise the real input path; add a regression
test for every editing bug fixed.

Windows UI automation cannot type into GPUI's custom text surface, so do not
rely on it for editing verification; screenshots are fine for layout checks.

## Release and install

`target\release\rusteditor.exe` cannot be replaced while RustEditor is
running. Build to a separate file instead of killing the user's process
(it may contain unsaved edits):

```powershell
cargo rustc --release --bin rusteditor -- -o target\release\rusteditor-candidate.exe
.\scripts\install.ps1 -Source .\target\release\rusteditor-candidate.exe
```

Verify the installed file's SHA-256 matches the built file. A release
build takes about 8 minutes; run it in the background with a log instead
of a blocking command with a short timeout.

## Agent conduct

- The worktree contains uncommitted user work; never `git reset`,
  `git checkout --`, or clean untracked files.
- Never stop a running RustEditor process without confirming it has no
  unsaved changes.
- Keep changes surgical; do not reformat or refactor unrelated code.
- Remove temporary diagnostics (trace files, debug prints) before finishing.
- State limitations honestly; for example, undo/redo restore only a single
  cursor, and IME composition uses only the primary cursor.
