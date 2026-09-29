# RustEditor — Design Critique and Proposal

Objective: **a modular text editor with rich plugin support, very fast UI
rendering, a small binary, and easy porting to Windows, macOS, and Linux.**

This document reviews the current code (commit `2a2b7c3` plus the uncommitted
GPUI work) against that objective. It then proposes a target architecture and
a step-by-step migration plan.

---

## 1. What the current design gets right

Keep these:

- **A rope for storage** (`ropey`): O(log n) edits and cheap snapshot clones.
- **Virtualized rendering** (`uniform_list`): only visible rows are laid out.
- **Snapshot + revision workers**: slow work runs off the UI thread, and
  results that no longer match the document are dropped.
- **Streamed save**: rope chunks go through a `BufWriter`, so the document is
  never materialized to save it.
- **Pure logic already in its own modules**: `search`, `transforms`,
  `calculator`, `paste`. These are the seeds of a core crate.
- **Headless GPUI tests** that go through the real input path.

## 2. Critique

### 2.1 Modularity: one monolithic UI module
- `gpui_workspace.rs` is about 2,600 lines. It holds tabs, the editing model,
  selection, the IME bridge, five worker channels, search state, command
  modes, toolbars, rendering, keybindings, and tests. `Workspace` has more than 40 fields.
- Features are wired in by hand. `CommandMode { Find, Ask, Calculator,
  Replace, ReplaceAll }` and `TransformOp` are closed enums. Adding a feature
  means editing the central struct, `poll()`, `render()`, and `run()`.
- There is **no plugin boundary at all**. AI, JSON, Base64, and the calculator
  are compiled-in special cases.
- It is a single crate. Core logic can't be tested, reused, or built without
  pulling in GPUI.

### 2.2 The editing model is shaped by a UI workaround
- "Only the active line/page is editable" (`content`, `page_start`,
  `page_end`) was an implementation choice. GPUI's `EntityInputHandler`
  works on arbitrary UTF-16 ranges (Zed exposes whole multi-line buffers
  through it), so the IME did not force this. The model leaks into the
  document layer. As a result:
  - there is no multi-line selection, and no multi-cursor editing;
  - editing across the 4,096-character page boundary is awkward, and IME
    composition can't span pages;
  - three offset systems (UTF-16, UTF-8 bytes, chars) are converted by hand
    at many call sites. This is a known source of bugs.
- **There is no undo or redo.** This is the biggest functional gap for an
  editor.
- `document_selected` and `page_selected` are special-case booleans instead
  of a general selection set.

### 2.3 Document and I/O
- The whole file is read into the rope when it opens. Memory use is about
  1× the file size plus rope overhead, and nothing is visible until the load
  finishes. "Very large files" really means "files that fit comfortably in RAM".
- Only UTF-8 is supported. There is no encoding detection (UTF-16 BOM, Latin-1)
  and no line-ending detection or preservation policy.
- **Save is not atomic.** `set_len(0)` followed by a rewrite of the same file
  means a crash or power loss mid-save destroys the file.
- **The exclusive session lock is a portability and usability problem.**
  - On Windows, `fs2` locks are mandatory: git, grep, compilers, and Copilot
    can't read the file while it is open.
  - On Unix, the locks are advisory, so the "protection" doesn't exist there.
    Behavior differs by OS.
- A single `revision` counter makes background results all-or-nothing. One
  keystroke during a Replace All throws the whole result away.

### 2.4 Concurrency
- Five hand-written `mpsc` channel pairs are drained by a **200 ms polling
  timer**. This adds up to 200 ms of latency to every background result and
  wakes the CPU five times a second while idle (bad for battery life).
- Workers are raw `thread::spawn` calls. There is no cancellation: an
  outdated search keeps running to completion, and there is no priority or
  back-pressure.

### 2.5 Rendering performance
- A fixed `ROW_HEIGHT` of 26 px rules out word wrap, mixed fonts, and inline
  decorations.
- Lines are shaped every time they are rendered. There is no shaped-line cache
  keyed by (line, revision, style).
- There is no decoration or highlight pipeline. Syntax highlighting, search-hit
  painting, and diagnostics have nowhere to go.
- The 1,024-character preview limit handles long lines by truncating them. It
  isn't horizontal virtualization.

### 2.6 Binary size
- The release binary is **about 13.8 MB**, and `Cargo.toml` has **no
  `[profile.release]` tuning** (no LTO, `codegen-units = 1`, `strip`, or
  `panic = "abort"`).
- GPUI is heavy (it is Zed's UI framework). `rfd`, `regex`, and `serde_json`
  are always compiled in, with no feature flags to leave them out.

### 2.7 Cross-platform portability
- GPUI is **pinned to `=0.2.2`**. It is pre-1.0, its API changes often, it is
  tied to Zed's release cycle, and it has limited documentation. The entire UI
  is written directly against it, with no abstraction layer.
- Keybindings are hard-coded as `ctrl-…`. macOS expects `cmd-…`.
- Single instance uses a lock file and endpoint file in the shared
  `temp_dir()` plus an **unauthenticated loopback TCP port**. On multi-user
  Unix machines this collides across users and lets any local process inject
  paths. Named pipes or Unix domain sockets in a per-user runtime directory
  are the right tool.
- The installer, PATH setup, and context menu are PowerShell-only. There is
  no packaging for macOS or Linux, and no CI matrix.
- There is no config or settings layer (no config directory, no user keymap
  files).

---

## 3. Target architecture

### 3.1 Principles
1. **Core knows nothing about the UI.** The buffer, selections, undo, search,
   and commands are plain Rust. Every piece is unit-testable and usable
   without any UI.
2. **Everything user-facing is a command.** Keybindings, toolbar buttons,
   menus, the command palette, and plugins all call commands through one
   registry.
3. **Features are extensions**, including the built-in ones. The core stays
   small; the size of the binary is chosen with Cargo features.
4. **The frontend is thin and replaceable.** It renders a view model and turns
   input into commands. This limits the damage from GPUI churn and lets us
   swap renderers if size or portability requires it.
5. **The platform layer is the only place with `#[cfg(target_os)]`.**

### 3.2 Cargo workspace layout

```
crates/
  re-core/        buffer, selections, transactions, undo, encoding, line endings
  re-io/          load/save (atomic), pread+cache large-file backend, file watching
  re-search/      plain/regex search + replace, cancellable, streaming results
  re-view/        display map: wrap, tabs, folds, horizontal virtualization,
                  decorations; viewport queries (UI-agnostic)
  re-commands/    command registry, keymap (TOML), context predicates
  re-tasks/       job system: cancellation tokens, priorities, UI wake-ups
  re-plugin-api/  stable, versioned plugin API types (no heavy deps)
  re-plugin-host/ out-of-process plugin host (JSON-RPC over stdio)
  re-platform/    single instance IPC, config/runtime dirs, clipboard,
                  dialogs, shell integration — all cfg-gated
  re-ext-*/       built-in extensions: transforms, calculator, json, ai, …
  re-ui-gpui/     GPUI frontend (default)
  re-ui-tui/      optional terminal frontend (tests, SSH, CI smoke tests)
app/rusteditor/   binary: wires crates together, chooses features
```

Dependency direction: `app → ui-* → view/commands/tasks → core`. `core` has
no dependencies beyond `ropey` (and possibly `smallvec`).

### 3.3 Core editing model (replaces "active line/page")
- `Buffer { text: Rope, revision, line_ending, encoding }`.
- `Selections`: a sorted, non-overlapping set of `Selection { anchor, head }`
  in **char offsets**. Multi-cursor and multi-line selection come for free.
- `Transaction`: a list of `(Range<char>, replacement)` changes applied
  atomically. It produces a `ChangeSet` that can be inverted and composed.
- **Undo tree**: a history of `ChangeSet`s grouped by time or command. Huge
  operations (a whole-file transform, Replace All) store a rope snapshot
  instead of an inverted diff. Rope clones are O(1) and share structure, so
  this is cheap.
- **Position mapping**: `ChangeSet::map_pos` lets background results (search
  hits, diagnostics, plugin decorations) be *rebased* onto the current
  revision instead of discarded.
- UTF-16 appears **only** at the IME boundary in the frontend. There is one
  conversion helper there, covered by property tests.

**IME:** the GPUI input handler exposes the **whole document** in UTF-16,
using ropey's O(log n) `char_to_utf16_cu` / `utf16_cu_to_char`. There is one
coordinate system and no page edges, and composition can span lines.
`bounds_for_range` (where the candidate popup appears) is answered by the
display map, so it works with wrap and virtual rows. Linux IME
(IBus/Fcitx over Wayland `text-input-v3` / X11 XIM) is the least mature
path in GPUI and must be tested early with a real CJK IME.

### 3.4 Large-file strategy
- **Normal files (under a configurable limit, e.g. 256 MB):** keep
  **ropey 1.x** (stable, used by Helix), loaded on a worker. Stream partial
  loads into the view so the first screen appears right away. Only consider
  ropey 2.0 if benchmarks show a need. It is still in-memory, so it doesn't
  solve the huge-file problem.
- **Huge files:** a **piece table over positioned reads (`pread` /
  `seek_read`) with an LRU chunk cache**, plus an append buffer for edits.
  Opening is instant and memory is bounded (cache plus edits). The line
  index is built in the background with `memchr` (several GB/s); show an
  approximate line count until it finishes.
  - **Why not mmap:** truncation by another process causes SIGBUS or an
    access violation (a crash). On Windows, a mapped file can't be truncated
    and is hard to replace, which recreates the lock-out problem and blocks
    atomic save. Mapping is unreliable on network drives. Other encodings
    need decoding anyway.
  - Ship this mode **read-only first** (viewing huge logs covers most use),
    then add editing. Saves stream the pieces to a temp file and rename it
    into place. The source isn't mapped, so this works on every OS.
  - External changes are detected with the file watcher plus a size/mtime
    check, and the user is prompted.
- Both backends sit behind one `TextSource` trait so view, search, and save
  code don't care which is in use.

### 3.5 Save and file safety (portable)
- **Atomic save**: write to a temp file in the same directory, `fsync`, then
  rename over the original (`ReplaceFileW` on Windows keeps ACLs and
  attributes). Preserve permissions and ownership on Unix. Fall back to an
  in-place write only when rename is impossible (for example, when the
  target is a symlink or a mount).
- **Replace the exclusive session lock with external-change detection**
  (`notify` plus an mtime/size/hash check before saving), and offer
  "reload / keep mine / diff". This behaves the same on every OS and lets
  other tools read the file.
- Optional crash-recovery swap files in the per-user data directory.

### 3.6 Concurrency: `re-tasks`
Phase 1 already replaced the 200 ms polling timer with event-driven wakes
(`src/wake.rs`); `re-tasks` builds on that.
- One job API on top of GPUI's `background_executor` (or `rayon` for CPU-heavy
  work):
  `spawn(priority, CancellationToken, FnOnce(Snapshot) -> Result<T>)`.
- Results come back through executor futures that **wake the UI right away**.
  This removes the 200 ms polling timer.
- A new search cancels the previous one. Search streams hits in batches so the
  first results appear immediately.
- Every result carries `(buffer_id, revision)` and is rebased through the
  position mapping, or dropped if that isn't possible.

### 3.7 Rendering pipeline (ultra-fast)
- The **display map** (`re-view`) turns buffer lines into display rows
  (wrap, tabs, folds). It updates incrementally from `ChangeSet`s and never
  walks the whole document.
- **Horizontal virtualization**: very long lines are shaped only for the
  visible column window plus a margin. This replaces the 1,024-character
  preview and the 4,096-character edit-page workaround.
- **Shaped-line LRU cache** keyed by `(line_id, revision_of_line,
  style_hash, wrap_width)`. Scrolling reuses shaped lines, and an edit
  reshapes only the affected rows.
- **Decoration layers**: syntax, search hits, selections, bookmarks, and
  plugin decorations are merged into styled runs per visible row.
- **Highlighting**: a `Highlighter` trait. The default is a small incremental
  line-state lexer (cheap, a few KB per language). Tree-sitter is available
  behind a Cargo feature for users who want it (it costs roughly 0.3–2 MB per
  grammar).
- **Performance budgets enforced in CI**: keystroke-to-paint under 8 ms on a
  1 GB file, 60 fps scrolling, zero allocations per frame for unchanged
  rows. A `re-bench` crate uses criterion to measure the core and view layers.

### 3.8 Plugin system (three tiers)

| Tier | Mechanism | Trust | Use for | Cost |
|------|-----------|-------|---------|------|
| **A. Built-in** | Rust crates implementing `Extension`, selected by Cargo features | Trusted (our code) | transforms, calculator, JSON, highlighters | Only what you enable |
| **B. WASM (default for third-party)** | WIT component model; the plugin can call **only** host functions we expose | **Sandboxed, enforced** | transforms, highlighters, commands, decorations, panels | `wasmi` (small) by default; `wasmtime` behind a feature for speed |
| **C. External process** | JSON-RPC over stdio (LSP-style) | **Full trust: "runs as you"**, shown at install | LSP servers, AI providers, formatters, linters | ~0 in the binary; crash-isolated |

**Trust model:**
- We don't promise capabilities we can't enforce. A separate process runs
  with the user's full permissions (OS sandboxing differs per platform and
  is incomplete: AppContainer, deprecated `sandbox-exec`,
  landlock/seccomp). So tier C is labelled full trust, not "sandboxed".
- Capabilities in `plugin.toml` are **enforced for WASM plugins**, because a
  capability is a host function that is either linked or not.
- **Data leaving the machine is a separate consent.** Any plugin that sends
  buffer text over the network (for example AI providers) asks the user per
  workspace.

Plugin API v0 (the same concepts in every tier):
- `commands`: register a command id, title, and default keybinding, plus
  context predicates.
- `transforms`: `(selection text | streamed chunks) -> text`. The host
  applies the result as a transaction, so it is undoable automatically.
- `decorations`: ranges plus styles for a revision (rebased by the host).
- `panels`: declarative view models (lists, trees, text), rendered by the
  host. Plugins never touch GPUI.
- `events`: open, save, change (debounced), selection change.
- **Capabilities** declared in `plugin.toml` (`read-buffer`, `write-buffer`,
  `fs`, `net`, `process`). The user approves them; they are enforced for WASM
  and informational for external processes (see the trust model).

Language support should reuse **LSP** directly (as tier B) rather than invent
a new protocol.

The existing AI provider becomes a tier-C plugin with network-egress consent.
It already works by handing a temp snapshot to a child process, so it fits
this model naturally.

### 3.9 Commands and keymaps
- `CommandRegistry` maps `CommandId` to `handler(&mut EditorCtx, args)`. The
  toolbar, menus, the palette (`Ctrl/Cmd+Shift+P`), and plugins all go
  through it.
- The keymap is TOML using a platform-neutral **`mod`** modifier (Ctrl on
  Windows/Linux, Cmd on macOS). A default keymap per OS is overlaid with the
  user's keymap.
- Settings are TOML in the platform config directory (`directories` crate)
  and hot-reload.

### 3.10 Platform layer (`re-platform`)

| Concern | Windows | macOS | Linux |
|---------|---------|-------|-------|
| Single instance | named pipe `\\.\pipe\rusteditor-<SID>` | Unix socket in `$TMPDIR` (per-user) | Unix socket in `$XDG_RUNTIME_DIR` |
| Open-file forwarding | pipe message | + Apple Events `open` (`application:openFiles`) | socket message / D-Bus optional |
| Config dir | `%APPDATA%\RustEditor` | `~/Library/Application Support/RustEditor` | `$XDG_CONFIG_HOME/rusteditor` |
| Shell integration | registry context menu | Services / `Info.plist` doc types | `.desktop` file + MIME types |
| Packaging | MSI / winget | `.dmg`, notarized | AppImage, `.deb`, Flatpak |

- Use the `interprocess` crate for pipes and sockets. No TCP, no shared temp
  files.
- Use `cargo-packager` or `cargo-dist` for the installers. The PowerShell
  scripts become the Windows-only fallback.
- Paths stay `OsString`/`PathBuf` from end to end (no lossy UTF-8 conversion).

### 3.11 Frontend choice
**GPUI concerns:** pre-1.0 with API churn (we pin `=0.2.2`; Zed itself uses
git), thin docs, a large binary share, a hard GPU requirement (RDP, VMs and
old Linux GPUs may fail), little or no screen-reader support, and a roadmap
set by Zed.

| Option | Size | Portability | Large-file fit | Notes |
|---|---|---|---|---|
| **GPUI (current)** | Large | Good, needs a GPU | Excellent | Churn, weak accessibility |
| **Custom: winit + softbuffer/wgpu + cosmic-text/swash + AccessKit** | Smallest | Excellent; CPU path works anywhere | Full control | Most work: widgets, IME, accessibility |
| Floem (Lapce) | Medium | Good | Built for editors | Pre-1.0, small community |
| Iced | Medium to large | Good, CPU fallback | Weak for huge text | Stabler API |
| Slint | Small | Excellent, CPU renderer, AccessKit | Mediocre for custom text surfaces | Licensing terms |
| egui | Small to medium | Excellent | Already retired here | Immediate mode is a poor fit |

**Decision process (don't switch yet):**
1. Keep GPUI only inside `re-ui-gpui`, consuming `re-view` view models and
   emitting `re-commands` calls. The choice can then be undone.
2. Fix pass/fail criteria first: binary size, keystroke-to-screen time on a
   1 GB file, rendering over RDP or in a VM, screen reader support, and IME
   on all three OSes.
3. Build a 1–2 week spike of the custom winit stack against the shared view
   model. Measure it against those criteria, then decide. A CPU-rendering
   fallback is valuable for portability even if GPUI stays primary.

Plan accessibility (AccessKit or platform APIs) regardless of the framework.
- The `re-ui-tui` frontend (`ratatui`) is cheap to maintain because the view
  model is shared. It gives CI a fast, deterministic end-to-end harness and
  users a terminal mode.

### 3.12 Binary size plan

```toml
[profile.release]
lto = "fat"
codegen-units = 1
strip = true
# opt-level = "s": try later and measure; keep 3 if render perf drops.
# panic = "abort": not used; the loader relies on catch_unwind.
```

- Put optional dependencies behind Cargo features: `regex` (a literal search
  fallback is still available), `json`, `ai`, `tree-sitter`, `wasm`, `dialogs`.
- Run `cargo bloat --crates` in CI with a size budget that fails the build on
  regressions.
- Targets (to be confirmed by measurement): **< 8 MB** default build with
  GPUI; **< 4 MB** if the lean frontend is adopted.

---

## 4. Migration plan (tests green at every step)

| Phase | Work | Outcome |
|-------|------|---------|
| **1. Quick wins** | Release profile tuning; record size baseline; GitHub Actions matrix (win/mac/linux) running fmt/test/clippy; replace 200 ms poll with executor wake-ups | Smaller binary, lower latency, portability regressions caught early |
| **2. Workspace split** | Move `document`, `search`, `paste`, `transforms`, `calculator` into `re-core` / `re-search` / `re-ext-*` crates unchanged | UI-free core; no behavior change |
| **3. Core editing model** | Add `Selections`, `Transaction`, `ChangeSet`, undo/redo in `re-core` with property tests (incl. non-BMP text) | Undo/redo; foundation for multi-line editing |
| **4. Commands + keymap** | `re-commands` registry; port every `actions!` handler; TOML keymap with `mod` modifier; command palette | Features no longer hard-wired; macOS keybindings correct |
| **5. View rewrite** | `re-view` display map + shaped-line cache + horizontal virtualization; whole-document UTF-16 IME; retire active line/page model | Multi-line selection, wrap, faster rendering |
| **6. Tasks + safe I/O** | `re-tasks` with cancellation; atomic save; file watching replacing the exclusive lock; encoding/line-ending detection | Crash-safe saves, identical behavior on all OSes |
| **7. Platform layer** | `interprocess`-based single instance; config dirs; `cargo-packager` installers | Ships on macOS/Linux |
| **8. Plugins** | Plugin API v0 (WIT); WASM host (wasmi); external-process host; port AI provider to tier C with egress consent; LSP client | Rich, safe plugin ecosystem without growing the binary |
| **9. Huge files** | Piece table over `pread` + LRU chunk cache as a `TextSource`; background `memchr` line index; read-only first, then editable | Instant open of multi-GB files |

Phases 1–2 are low-risk and can start immediately. Phase 5 is the largest and
depends on Phases 3 and 4. The GPUI spike (section 3.11) can run in
parallel once Phase 2 lands.

## 5. Resolved questions and remaining risks
- **IME (resolved):** expose the whole document in UTF-16 (section 3.3).
  Risk: Linux IME maturity; test with a real CJK IME in Phase 5.
- **GPUI (process agreed):** keep it isolated, decide using fixed criteria
  and a winit spike (section 3.11).
- **Plugin trust (resolved):** WASM is sandboxed and the default; external
  processes are labelled full trust; network egress needs consent
  (section 3.8). Risk: wasmi speed for heavy plugins; wasmtime behind a
  feature.
- **Huge files (resolved):** piece table over `pread` + cache, not mmap;
  read-only first (section 3.4). Risk: line-index memory for
  billion-line files; sample the index if needed. Re-check ropey 2.0 status
  before Phase 9.

## 6. Progress
- **Phase 1 (quick wins): done, except verifying CI on macOS/Linux.**
  - Release profile: `lto = "fat"`, `codegen-units = 1`, `strip = true`.
    Windows binary **13,827,072 → 8,721,920 bytes (−37%)**. `panic = "abort"`
    is not used, because the file loader relies on `catch_unwind` so a panic
    fails one tab instead of the whole process.
  - The 200 ms polling timer is replaced by `src/wake.rs` (`WakingSender`):
    each worker send wakes the UI task. Results apply immediately and idle
    CPU wake-ups are gone. Regression test:
    `worker_results_apply_via_wake_without_manual_poll` (checked to fail
    when wakes are disabled).
  - `.github/workflows/ci.yml`: fmt/clippy/test on Windows, macOS and Linux,
    plus a Windows release size budget (9.2 MB). macOS/Linux jobs are
    `continue-on-error` until they are confirmed to build (GPUI is built
    with only the Windows manifest feature; Linux package list is a best
    guess).
- **Phase 2 (workspace split): done.**
  - Cargo workspace with shared `[workspace.dependencies]`. Moved unchanged:
    `document.rs` + `paste.rs` → `crates/re-core`, `search.rs` →
    `crates/re-search`, `transforms.rs` → `crates/re-ext-transforms`,
    `calculator.rs` → `crates/re-ext-calculator`.
  - The app no longer depends directly on `base64`, `heck`, `regex`, or
    `serde_json`; they come in through the crates.
  - CI runs `--workspace` and fails if any crate under `crates/` depends on
    `gpui` or `rfd`. (Fixed after Phase 6: the workflow file contained
    literal backspace characters instead of `\b`, so the check never
    matched and the YAML was invalid. The script is now extracted from the
    workflow and tested both ways locally.)
  - Still in the app crate: `ai.rs` (future tier-C plugin, Phase 8),
    `instance.rs` (future `re-platform`, Phase 7), `wake.rs` (future
    `re-tasks`, Phase 6). Moving them now would only rename things.
- **Phase 3 (core editing model): done.**
  - `re-core::change`: `ChangeSet` of sorted, non-overlapping char-offset
    replacements; `apply` returns the exact inverse; `map_pos` with
    `Assoc::Before/After` for rebasing positions.
  - `re-core::history`: linear undo/redo of groups. Typed insertions that
    continue the previous one within 1 s merge; saving seals the group.
    Whole-document edits (Replace All, large paste, transforms) store rope
    snapshots (O(1), structure-shared). Bounded to 10,000 steps / 64 MiB of
    change text.
  - `Document`: every mutation goes through history. Dirty is now
    `state != saved_state`, so undoing to the saved text clears `*`.
    Undo/redo bump `revision`, so stale worker results are still dropped.
  - `re-core::selection`: multi-cursor `Selections` (sorted, merged, mapped
    through changes). Not used by the UI until Phase 5.
  - UI: `Ctrl+Z`, `Ctrl+Y` / `Ctrl+Shift+Z`, and toolbar buttons. The cursor
    moves to the edit; bookmarks shift (or clear after snapshot undo).
  - Tests: property tests (2,000 random multi-range edits with emoji, CRLF
    and CJK; 200 random undo/redo sequences), document tests, and GPUI tests
    through `simulate_input` / `simulate_keystrokes`.
  - Known limits: the history is linear (no undo tree), and it is lost when
    the tab closes. Backspace/Delete runs are not merged (each is a step).
    Bookmarks on deleted lines use the existing shift rule, not exact
    per-line mapping.
- **Phase 4 (commands + keymap): done.**
  - New UI-independent crate `re-commands` (no dependencies):
    - `Registry` of `CommandInfo { id, title, category, hidden }` with 54
      built-in commands; `register()` accepts runtime commands (plugins).
    - `keystroke`: `mod` resolves to Ctrl (Windows/Linux) or Cmd (macOS);
      accepts `-`/`+`, any modifier order/case, chord sequences; labels as
      `Ctrl+Shift+S` or `⇧⌘S`.
    - `keymap`: a TOML-subset parser (quoted keys/values, `[global]` and
      `[editor]`, comments). No `toml` crate, to keep the binary small.
      User keymap layers over defaults; `""` unbinds; bad entries are
      reported with line numbers and skipped.
    - `palette`: fuzzy subsequence scoring (word starts, contiguous runs,
      shorter titles win ties), matching title or "category: title".
  - App: one `RunCommand(id)` GPUI action. All keys come from the keymap;
    `run_command(id)` is the single dispatch point for keys, palette and
    toolbar buttons (which previously duplicated logic). A test fails if a
    built-in command has no handler, and the default keymap is checked on
    both platforms.
  - Command palette (`mod-shift-p`, `F1`), Open Keymap File (creates a
    commented template in the per-user config dir), Reload Keymap (reads the
    open tab's buffer if the keymap is open, since the tab's lock blocks
    reading it from disk on Windows).
  - Known limits: palette has no arrow-key selection yet (Enter runs the top
    match, or click); toolbar labels are not yet generated from the
    registry; no file watching for the keymap (use Reload Keymap);
    `mod-h` conflicts with macOS "Hide" and should get a macOS-specific
    default when macOS is verified.
- **Phase 5a (whole-document editing): done.**
  - `re-core::editor::EditorState` (UI-free): one selection over the whole
    rope in char offsets; char/word/line/page/document motions with a goal
    column; the cursor never lands inside CRLF; edits through `Document`
    (undoable); IME composition (`replace_and_mark`) and UTF-16 mapping via
    ropey in O(log n), with IME text queries capped to a window around the
    cursor.
  - `Document`: ranged edits of at least 256K chars are stored as O(1) rope
    snapshots (`SNAPSHOT_EDIT_CHARS`), so select-all + delete on a huge file
    never copies it into undo history. `Applied` now reports
    `lines: Option<(first_line, delta)>` for bookmark updates.
  - UI: the active line/page model (`line`, `page_start`, `page_end`,
    `PAGE_CHARS`, page buttons, `document_selected`, `page_selected`,
    `saved_editor`, join/page-boundary special cases) is gone. Rows are custom
    `RowText` elements that paint selection, text, IME underline and cursor,
    shaping only a <= 1,024-char window from the tab's horizontal scroll
    column. One `EntityInputHandler` serves the whole document; `ImeAnchor`
    registers it each frame. `PaintCache` holds last-frame row geometry for
    click/drag/double-click hit-testing and IME `bounds_for_range`.
  - New keys: Up/Down, PageUp/PageDown, Ctrl+Left/Right (word),
    Ctrl+Home/End, and Shift variants. Status bar shows Ln/Col/selection.
  - Tests: 13 `EditorState` unit tests; GPUI tests for multi-line typing and
    navigation, cross-line selection/copy/delete/undo, select-all on a
    500 KB document, IME on a later line with non-BMP text, real
    painted-row click/drag selection, 100K-char line editing with bounded
    shaping and horizontal scroll, bookmark shifting/merging, multi-line
    transforms. Screenshot-checked on Windows.
  - Known limits: single selection (the multi-cursor `Selections` model is
    not wired to the UI yet); no word wrap; tabs and wide (CJK) characters
    use the monospace cell estimate for horizontal scroll; drag-selection
    does not auto-scroll past the viewport edge; IME candidate bounds are
    only known for painted rows; each IME composition update is its own undo
    step; macOS word/line keys (`alt-left`, `cmd-left`) are not mapped yet.
- **Phase 5b (word wrap, layout reuse, multi-cursor): done.**
  - New UI-free crate `re-view`: soft wrap on a monospace cell model
    (`cell_width`: 2 cells for CJK/emoji, 4 for tab) shared by wrapping and
    painting so row counts are exact; breaks after whitespace, splits long
    words; lines > 64K chars hard-wrap in O(1). `DisplayMap` stores rows per
    line with 1,024-line block sums (row lookup O(lines/1024 + 1024)) and
    updates incrementally from `LineEdit`s (O(1,024) when the line count is
    unchanged). Builds are cancellable; ~30M chars/s in release (9.9M chars
    in 0.32 s).
  - UI: list items are display rows when wrapping; `RowText` shapes one
    segment. Small documents (<= 1M chars) wrap on the UI thread, larger
    ones on a worker (revision-checked, cancelled when superseded). Width
    changes trigger a re-wrap via `request_animation_frame`. Up/Down/Page
    move by display rows through `re_core::editor::VerticalLayout`
    (`WrapLayout` vs `LogicalLines`).
  - Layout reuse: GPUI's `LineLayoutCache` already keeps shaped lines from
    the previous frame keyed by (text, font, size), so unchanged rows are
    not reshaped and edited rows naturally miss. A second shaped-line cache
    would duplicate it; the cost that did need caching is wrapping, so wrap
    segments are cached per (revision, width, tab).
  - Multi-cursor: `EditorState` holds `Selections` (goal column per
    selection). Edits build one multi-range `ChangeSet` (one undo step;
    typing merges across all cursors), return sorted `LineEdit`s for
    bookmarks (`shift_lines`) and the wrap map. Alt+click, Ctrl+Alt+Up/Down,
    Escape to collapse; copy joins selections, paste distributes lines;
    transforms apply per selection.
  - Tests: 5 new core tests (incl. a randomized check that `LineEdit`s keep
    unedited lines aligned), 8 `re-view` tests (random segments vs row
    counts, incremental map == rebuild across blocks, cancellation, wrapped
    vertical motion), 4 GPUI tests (wrap rows/motion/click/incremental map,
    worker wrap of a > 1M-char document, multi-cursor typing/undo/copy/
    paste/escape, Alt+click).
  - Known limits: undo/redo restore a single cursor; IME composition uses the
    primary cursor only; no "select next occurrence" yet; Home/End are
    logical-line (not display-row) with wrap on; during a background re-wrap
    (e.g. undo on a > 1M-char document) the view shows unwrapped text; tabs
    count as 4 cells regardless of position; wrapped continuation rows are
    not indented. Not screenshot-verified: automated capture could not
    reliably bring the window to the foreground, so wrap rendering is
    verified only by headless tests on real painted rows.
- **Phase 6 (tasks + safe I/O): done.**
  - New crate `re-tasks`: `spawn` (named worker; a panic becomes an error
    so one tab fails, not the process), `CancelToken`, `Latest` (starting a
    job cancels the previous one of its kind), and `wake` (moved from the
    app). Every worker (load, reload, search, Replace All, wrap, paste,
    Copilot) now goes through it. Search and Replace All check the token
    every 1,024 lines (`re-search::*_cancellable`).
  - `re-core::text_io` (no new dependencies): streaming decode into the
    rope for UTF-8, UTF-8 BOM, UTF-16 LE/BE (BOM), and Windows-1252 when the
    bytes are not valid UTF-8 (re-read from disk, so large files are never
    buffered twice); NUL bytes => refused as binary. Line-ending counts are
    gathered while decoding. Streaming encode on save; characters the
    encoding cannot represent fail the save before the file is touched.
  - Crash-safe save: `atomic_write` writes a temp file in the same directory,
    fsyncs, keeps permissions, follows symlinks, renames over the original
    (and fsyncs the directory on Unix). If the rename is refused (Windows,
    file held open by another program) it writes in place and says so. A
    failure while producing the content never touches the original.
  - The exclusive session lock (and the `fs2` dependency in `re-core`) is
    gone; other programs can read/write open files. `disk_status()` compares
    mtime/size; checked on window activation and tab switch (no polling).
    Clean tabs reload automatically (undoable snapshot); dirty tabs warn
    once; Save refuses with `SaveConflict` and a second Save overwrites.
    Read-only now means the file's read-only attribute/permission.
  - Line endings: the dominant ending is used for Enter (native for new
    files); "(mixed)" shown in the status bar; Convert Line Endings to
    LF/CRLF is one undo step and streams the rope. Save with Encoding
    commands; the status bar shows encoding and line ending.
  - Tests: 9 `text_io` tests (round trips, chunk-boundary multibyte and
    surrogates, 1252 fallback incl. after a BOM beyond the first chunk,
    binary refusal, unrepresentable chars, atomic write keeps permissions
    and leaves no temp files, failed write leaves the original), 5 document
    tests (no lock + conflict + force, reload undo/staleness, encoding
    round trip/change, line-ending conversion, read-only attribute), 4
    `re-tasks` tests, 1 cancellation test in `re-search`, 3 GPUI tests
    (external change: auto reload, undo, dirty warning once, save conflict
    then overwrite, deletion; encoding/line-ending commands; a new search
    cancels the old one).
  - Size: 9,126,912 bytes (+97 KB). `cargo bloat` shows ~1.4 MB of image
    and SVG decoders (`image`, `zune_jpeg`, `exr`, `tiff`, `image_webp`,
    `usvg`) pulled in by GPUI with default features; they are not optional
    in gpui 0.2.2. Removing them needs a GPUI fork/patch or the lean
    frontend (section 3.11). Decision: the CI budget was raised from 9.2 MB
    to 10 MB (~870 KB headroom) for now; revisit with the GPUI decision.
  - Known limits: no file watcher (changes are noticed on activation/tab
    switch, not instantly); ACLs/ownership beyond permission bits and
    creation time are not preserved by the rename on Windows; changing the
    encoding is not undoable; no encoding auto-detection beyond BOM/UTF-8
    validity (e.g. Shift-JIS opens as Windows-1252).
- **Phase 7 (platform layer): done, but macOS/Linux not yet run.**
  - New crate `re-platform` (no dependencies): `paths` (config and runtime
    dirs per OS; `ensure_private_dir` creates 0700 and rejects dirs owned by
    another user or reached through a symlink; `file_url_to_path`) and
    `instance`. `src/instance.rs` is gone; `src/config.rs` keeps only the
    keymap template.
  - Single instance: `File::try_lock` (std, replaces `fs2`, which is no
    longer a dependency) in the per-user runtime dir. Windows uses a named
    pipe with a 128-bit random name (from the OS-seeded hasher; no RNG crate)
    written to the per-user dir, `FILE_FLAG_FIRST_PIPE_INSTANCE`,
    `PIPE_REJECT_REMOTE_CLIENTS`, and a spare instance kept open so the name
    is never free while serving. The Win32 surface is ~5 hand-declared
    functions (no `windows-sys`/`interprocess`). Unix uses a socket in the
    0700 runtime dir. The unauthenticated loopback TCP port and the shared
    `%TEMP%` lock/endpoint files are gone. Forwarding has a 3 s timeout; a
    client sending garbage does not stop the server.
  - Fixed: forwarded relative paths were resolved in the primary's working
    directory; `main` now makes paths absolute (`std::path::absolute`).
  - Fixed: GPUI built with `default-features = false` had no text system on
    macOS (blank text) and no window backend on Linux (panic at start).
    `Cargo.toml` now enables `font-kit` (macOS) and `font-kit, wayland, x11`
    (Linux) in target sections; Windows features are unchanged (checked with
    `cargo tree -e features` for all three targets).
  - macOS "Open With"/Finder: `on_open_urls` is registered before the app
    starts; files arriving before the window exists are queued.
  - Keymaps: `Platform` is now Windows/MacOs/Linux; sections can be limited
    to one OS (`[editor.macos]`), applied after plain sections. macOS
    defaults: Option+Left/Right word, Cmd+Left/Right line, Cmd+Up/Down
    document, Replace on Cmd+Option+F (Cmd+H is Hide), no Cmd+Y redo.
  - Packaging: `packaging/linux/rusteditor.desktop`, `scripts/install.sh` /
    `uninstall.sh` (per-user, `~/.local`), `[package.metadata.bundle]` for
    `cargo bundle` with text document types, `.gitattributes` keeps `*.sh`
    and `*.desktop` LF. CI validates the desktop file and script syntax.
  - Tests: 8 `re-platform` tests (message round trip/limits, token
    uniqueness, real primary + two forwards over the pipe with a relative
    path made absolute, garbage client, forged endpoint refused, dirs,
    file URLs), keymap OS-section test, macOS default-keymap assertions.
    Manual smoke test of the release build: second launch forwarded and
    exited in 110 ms, one process, random pipe name, no listening TCP port.
  - Verified for Linux: all core crates (incl. the Unix socket code) pass
    `cargo clippy --target x86_64-unknown-linux-gnu -- -D warnings` (this
    caught an unused-constant error that would have failed Linux CI). The
    app crate cannot be checked here: GPUI's HTTP client pulls in `ring`,
    which needs a Linux C toolchain.
  - Size: 9,173,504 bytes (+47 KB), budget 10 MB.
  - Known limits: nothing has run on macOS or Linux yet; CI keeps them
    `continue-on-error` until a first green run. No app icon, code signing,
    notarization, `.dmg`, `.deb`, AppImage or Flatpak. The Windows pipe uses
    the default DACL (others may have read access to a pipe whose name they
    cannot learn). Old builds (TCP scheme) and new builds do not see each
    other; close the old window before running the new build.
- **Phase 8 (plugins v0): done.**
  - New UI-free crate `re-plugin`: `plugin.json` manifests (strict
    validation: ids, api version, capabilities `document`/`network`, input
    `selection`/`document`, output `replace`/`show`, module paths confined to
    the plugin folder), a `Registry` that loads a folder and reports bad
    plugins without failing the rest, one JSON request/response protocol for
    both kinds, and remembered `approvals`.
  - WASM (sandboxed, feature `wasm`, default on): wasmi 0.51 interpreter;
    exports `memory`, `re_alloc`, `re_transform`; the only import is
    `env.log` (anything else, e.g. WASI, is rejected before running); 2e9
    fuel (about 1-2 s), 256 MiB memory, 32 MiB request/response; traps map
    to readable messages. Each run uses a fresh instance.
  - Process (full trust): one program run per command, JSON on stdin/stdout,
    stdin/stdout/stderr on threads (no pipe deadlock), timeout (default 30 s,
    max 600 s) kills the process, output capped, stderr shown on failure, no
    console window on Windows.
  - App: plugin commands join the palette and keymap (`plugin.<id>.<cmd>`,
    category `Plugin: <name>`). `Workspace::confirm` asks before the first
    run (sandboxed vs. full-access wording) and again after an update;
    approvals persist in `<config>/approvals.txt`. Requests are built from a
    snapshot and run on a worker; results are applied through `EditorState`
    (one undo step, results selected) only if the revision still matches.
    **Reload Plugins**, **Forget Plugin and Copilot Approvals**.
  - Copilot: now asks once before sending document text (including unsaved
    edits) to GitHub Copilot (network-egress consent), revocable. It is
    still built in rather than a process plugin (see limits).
  - Example: `plugins/examples/sort-lines` (Rust + serde_json -> 81 KB
    wasm): sort, sort descending, unique, statistics (show). An ignored
    integration test runs the built module through the host; it passes.
  - Tests: 17 `re-plugin` unit tests (manifest parsing/rejections, approval
    keys, registry incl. path escape, protocol, WASM echo round trip with
    non-BMP text, fuel exhaustion, memory growth refused, WASI import
    rejected, out-of-bounds response, process stdout with a 200 KB request,
    exit status + stderr, missing program, timeout kill), 1 ignored example
    test, 4 GPUI tests (palette entry, approval then undoable edit and
    persistence, stale result dropped, show output + bad plugin reported +
    empty selection refused, Copilot consent/cancel/revoke).
  - Size: 10,564,608 bytes (+1.39 MB; `cargo bloat`: wasmi 404 KB,
    wasmparser 280 KB, wasmi_ir 62 KB of code). CI budget raised to 11 MB.
    `cargo build --release --no-default-features` builds without WASM
    plugins.
  - Known limits: no panels, decorations or events; no long-running
    JSON-RPC session or LSP; no installer/registry; process plugins are not
    sandboxed (by design, and stated to the user); Copilot is not yet moved
    into a process plugin; plugin commands are not in the toolbar; a WASM
    module is recompiled on every run (fine for small plugins).
- **Phase 9 (huge files, read-only): done.**
  - `re-core::huge` (UI-free): `HugeFile` reads at positions
    (`seek_read`/`read_at`, no shared cursor, no lock) through a cache of
    1,024 x 64 KiB chunks (oldest evicted first); `build_index` scans once
    with `memchr` into a `LineIndex` sampling every 1,024th line start
    (8 bytes per 1,024 lines), reporting progress every 64 MiB and stopping
    on cancel; `line_text` finds a line from the nearest sample (or the
    previous line, via a 4,096-entry line-start cache), strips `\r`, decodes
    lossily and caps length without splitting a character; `Lines` streams
    every line (capped) for search. UTF-16 is refused.
  - `re-search::find_in_lines` searches any line stream (same results as
    `find_all`, tested against it).
  - UI: files >= 256 MB open as `TabState::Huge`. The loader sends the view
    immediately, then index progress (tab shows "indexing N%"). Rows read
    their line on demand (first 2 KB). Keyboard/mouse move a highlighted
    line; Find streams the whole file on a cancellable worker; bookmarks and
    copy (current line, bookmarked lines; 1 MB/line, 16 MB total) work. All
    edit paths refuse with a message (`refuse_huge_edit`). **Open Fully for
    Editing** loads into a rope after a memory warning. External changes are
    reported; Reload re-reads. New for all files: **Go to Line** (`Ctrl+G`,
    `Cmd+L` on macOS; macOS `Cmd+G` is now Find Next).
  - Measured (release, SSD, Windows): 1 GiB / 14.9M lines: open 15 ms,
    index 0.5 s (2 GB/s), index memory 113 KB, random line 0.11 ms, one
    screen 0.1 ms. The first design (1 MiB chunks) made random jumps 10x
    slower (1.2 ms); 64 KiB chunks fixed it. End to end, the release build
    opened a 2 GiB, 25M-line log and stayed at ~100 MB private memory
    (a rope would need several GB); screenshot checked.
  - Tests: 6 `re-core::huge` tests (every line equals a rope's for four
    chunk/sample geometries incl. 16-byte chunks, with and without a final
    newline, random then sequential access; cut long lines keep whole
    chars; progress/cancel; streaming lines incl. caps; UTF-16 refused) plus
    an ignored 1 GiB benchmark; 1 `re-search` stream-vs-rope test; 4 GPUI
    tests (open read-only, line count, index memory, visible-rows-only,
    edit refused, navigation, copy; find/next/bookmark all/copy
    bookmarks/go to line/delete refused; open fully after confirmation then
    edit; go to line in normal documents). A deliberately wrong assertion
    was checked to fail.
  - Size: 10,652,672 bytes (+88 KB), budget 11 MB.
  - Known limits: read-only (no piece-table editing yet); no word wrap,
    horizontal scroll or selection within a line in huge mode (lines show
    their first 2 KB); a cache miss reads from disk on the UI thread (fast
    on local disks, could stutter on slow network drives); search skips text
    beyond 1 MB of a single line; UTF-16 huge files are refused; a file
    changed by another program is detected but not followed (no "tail -f").
- **UI: menu bar with dropdowns (after Phase 9).** The rows of toolbar
  buttons were replaced by a standard menu bar (File, Edit, Search, View,
  Transform, Bookmarks, Tools, Plugins) defined as data in
  `re-commands::menus` (every item a command id; tests require each visible
  command in exactly one menu, valid ids, unique mnemonics) plus a compact
  quick toolbar. Dropdowns render with GPUI `deferred` + `anchored` at the
  painted bounds of their title (recorded with a `canvas` probe), show
  keymap shortcuts and checkmarks, switch on hover, close on outside click
  or Escape, and support Alt+letter (Windows/Linux keymap sections) and
  arrow/Enter navigation. Plugin commands fill the Plugins menu. GPUI tests
  drive real mouse events at painted coordinates (open, hover-switch,
  toggle-close, outside-close, click an item) and keyboard navigation; a
  mutation check confirmed items really run. Screenshot-checked on Windows
  in an isolated instance (separate runtime dir, input posted only to that
  window). Known limits: no submenus (encodings/line endings are flat items
  with checkmarks); no native macOS menu bar yet (`cx.set_menus`), so macOS
  also shows the in-window bar; disabled states are not shown (commands
  that do not apply explain why when run).
- **All nine phases of the plan are done.** Remaining work from the
  proposal: editable huge files (piece table + append buffer), plugin panels/
  decorations/events and an LSP client, running CI on macOS/Linux for the
  first time, and the GPUI-vs-lean-frontend decision (section 3.11).
