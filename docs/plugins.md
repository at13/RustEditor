# Writing RustEditor plugins

A plugin is a folder in your plugins directory with a `plugin.json` and
the code it runs:

| OS | Plugins directory |
|----|-------------------|
| Windows | `%APPDATA%\RustEditor\plugins\<plugin>\` |
| macOS | `~/Library/Application Support/RustEditor/plugins/<plugin>/` |
| Linux | `~/.config/rusteditor/plugins/<plugin>/` |

After adding or changing a plugin, run **Reload Plugins** from the Command
Palette. Its commands then appear in the palette as `Plugin: <name>: ...`
and can be bound to keys in your keymap with the id
`plugin.<plugin-id>.<command-id>`:

```toml
[editor]
"mod-alt-s" = "plugin.sort-lines.sort"
```

Problems (bad JSON, missing module, unknown capability) are shown in the
status bar when plugins load; other plugins still load.

## Two kinds of plugin, two levels of trust

| | `wasm` | `process` |
|-|--------|-----------|
| Runs | WebAssembly in RustEditor's interpreter | A program on your computer |
| Can access | Only the text the host sends it | Everything you can (files, network, programs) |
| Limits | CPU budget, 256 MiB memory, 32 MiB output | Time (default 30 s, max 600 s), 32 MiB output |
| Use for | Text transforms, analysis | Formatters, linters, AI services, anything needing the system |

Before a plugin runs for the first time, RustEditor explains what it can
do and asks you to run the command again to allow it. Approval is
remembered for that exact plugin version, entry point, and capability set;
an update asks again. **Forget Plugin and Copilot Approvals** revokes all
approvals. A process plugin's approval is only as good as the program: the
editor cannot sandbox it, and says so when asking.

Results are always applied by the editor as normal edits (one undo step),
and only if the document has not changed since the command started.

## plugin.json

```json
{
  "api": 0,
  "id": "sort-lines",
  "name": "Sort Lines",
  "version": "1.0.0",
  "kind": "wasm",
  "module": "sort_lines.wasm",
  "capabilities": [],
  "commands": [
    { "id": "sort", "title": "Sort Lines", "input": "selection", "output": "replace" },
    { "id": "stats", "title": "Selection Statistics", "input": "selection", "output": "show" }
  ]
}
```

- `api`: must be `0`.
- `id`, command `id`: 1-40 characters of `a-z 0-9 - _`.
- `kind`: `"wasm"` with `"module"` (a path inside the plugin folder), or
  `"process"` with `"command": ["program", "arg", ...]` and optional
  `"timeout_seconds"`. `program` is looked up in the plugin folder first,
  then on `PATH`. It runs with the plugin folder as working directory.
- `capabilities` (optional):
  - `"document"`: commands may use `"input": "document"` (the whole file,
    including unsaved edits) and receive the file's `path`.
  - `"network"`: declares that the plugin sends text off the machine
    (process plugins only; the approval question says so).
- Command `input`: `"selection"` (default; the text of every selection) or
  `"document"`. Command `output`: `"replace"` (default; replaces each
  input) or `"show"` (text in a read-only viewer).

## Messages

Both kinds get one JSON request (UTF-8) and return one JSON response.

Request:

```json
{"api": 0, "plugin": "sort-lines", "command": "sort",
 "inputs": ["b\na", "second selection"], "path": "only with the document capability"}
```

Response, one of:

```json
{"outputs": ["a\nb", "..."], "status": "optional status-bar message"}
{"show": "text for the viewer"}
{"error": "what went wrong"}
```

For `"replace"` commands, `outputs` must have exactly one string per input.

## WebAssembly ABI

The module must export:

| Export | Signature | Purpose |
|--------|-----------|---------|
| `memory` | memory | Linear memory the host reads and writes |
| `re_alloc` | `(len: i32) -> i32` | Returns a buffer of `len` bytes for the request |
| `re_transform` | `(ptr: i32, len: i32) -> i64` | Handles the request; returns `response_ptr << 32 \| response_len` |

The only import allowed is `env.log(ptr: i32, len: i32)` (UTF-8 debug
text). A module importing anything else (for example WASI) is rejected.
Each command runs in a fresh instance, so plugins keep no state between
runs and need not free memory.

`plugins/examples/sort-lines` is a complete example in Rust using
`serde_json`:

```sh
rustup target add wasm32-unknown-unknown
cd plugins/examples/sort-lines
cargo build --release --target wasm32-unknown-unknown
# copy plugin.json and target/wasm32-unknown-unknown/release/sort_lines.wasm
# into <plugins directory>/sort-lines/
```

Any language that compiles to `wasm32` without WASI works (C, Zig,
AssemblyScript, TinyGo with `-target wasm`).

## Process plugins

The program reads the request from stdin and writes the response to stdout.
A non-zero exit shows stderr in the status bar. On Windows no console window
is shown. Example `plugin.json`:

```json
{
  "api": 0, "id": "fmt", "name": "Formatter", "version": "1.0.0",
  "kind": "process", "command": ["python", "format.py"], "timeout_seconds": 10,
  "capabilities": ["document"],
  "commands": [
    { "id": "file", "title": "Format File", "input": "document", "output": "replace" }
  ]
}
```

## Not yet supported

Plugins cannot yet add panels, decorations (highlighting), or react to
events (open/save/change); there is no long-running JSON-RPC session or
LSP client; and there is no plugin installer or registry. Copilot is still
built in (it now asks before sending text), not a process plugin.
