# GitHub Copilot SDK provider

This package uses the official **`@github/copilot-sdk` 1.0.17** and its
matched runtime (1.0.93). It replaces the prompt-based CLI/JSON adapter.
Natural assistant text is streamed from native SDK events; tool calls come
from registered SDK tools, never from parsing assistant prose.

## Build/install

Requires Node.js **20.19+ or 22.12+** and npm:

```sh
cd providers/copilot
npm ci
npm test
npm run build
```

The editor install scripts build and copy `dist/`, `node_modules/` and
`package.json` into `copilot-sdk/` alongside `rusteditor` and
`rusteditor-openai`. The Rust helper's `--backend copilot` forwards the editor
protocol over stdio to `copilot-sdk/dist/main.js`. `--copilot-sdk PATH` is an
explicit entrypoint override for development/custom installs.

These Node dependencies and the SDK's optional platform runtime are separate
from the editor executable. They add deployment size, not editor linkage.
The SDK-matched runtime avoids relying on an arbitrary CLI version for inference.

## Authentication

Use **Agent > Providers > GitHub Copilot > Sign in with GitHub** to complete
the installed Copilot CLI's OAuth flow. The SDK uses the same logged-in
identity (`useLoggedInUser: true`, standard `COPILOT_HOME` / `~/.copilot`).
**Check Copilot connection** calls the SDK's authentication/status API without
sending document text. No token is returned to RustEditor or written into its
provider profile. Token storage is managed by GitHub's runtime/CLI.

A Copilot subscription/model entitlement is required. `auto` means “let the
SDK/runtime choose”; it is not sent as a literal model id. Other model names
are passed directly to the SDK.

## Tool ownership and safety

- `mode: "empty"`, stdio transport (no TCP listener).
- Only declared custom tools are exposed. Short `rusteditor_tool_N` aliases
  avoid clashes with built-in tools and model tool-name length limits; the
  host sees the original tool name and native call id.
- Built-ins and MCP tools are excluded; discovery, skills, file hooks,
  host git operations, shared session store, telemetry and remote sessions
  are disabled.
- SDK permission requests are **rejected**. There is no `approveAll`.
- Our custom callbacks only emit proposals. `skipPermission` applies solely
  to that inert queue operation, not the actual editor action.
- `isTerminal: true` ends the SDK turn after queueing, without running a
  second model call on a fictitious result. RustEditor then validates,
  prepares, requests approval, and executes the actual tool. Its next turn
  supplies the real acceptance/rejection/result in the host transcript.
- Every model turn has a fresh SDK session; previous conversation comes from
  RustEditor's structured transcript rather than unrelated CLI history.
  Only the provider's own session is disconnected/deleted during cleanup.
- The legacy **Ask Copilot** command also uses this SDK provider, with no
  tools and a bounded read-only inline snapshot. Use the File Agent for edits.

## Tests and limitations

`npm test` type-checks against the installed SDK and uses injected clients to
verify native streaming (including non-JSON prose), tool schemas/aliases,
terminal callbacks, permission denial, history/results, unauthenticated
failures, abort and cleanup without reading user credentials or making model
requests.

`RUSTEDITOR_SDK_RUNTIME_SMOKE=1 npm test` additionally starts the **real**
SDK-matched runtime with logged-in-user authentication disabled, tokens
removed and an isolated data directory, and checks its stdio protocol
handshake/version. CI runs this on Windows/macOS/Linux.

An authenticated model request still needs a manual smoke test after sign-in.
The stateless editor protocol supplies history as context data, not a native
SDK session resume. SDK output-token limits use runtime defaults; RustEditor's
step/output limits still apply. macOS descendant cleanup on hard process kill
remains weaker than Windows' job-object lifetime guard.
