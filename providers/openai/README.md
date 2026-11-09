# RustEditor provider helper

`rusteditor-openai` is a separate executable; it adds no HTTP stack to the
editor. It speaks `re-agent` JSON-lines protocol on stdio. Backends:

- `--backend openai` (default): OpenAI-compatible streaming chat completions.
  Also supports local Ollama, Azure OpenAI, etc. via `--base-url`,
  `--auth-header`, `--api-key-env`, `--max-tokens-field`, `--no-usage`.
- `--backend anthropic`: native Anthropic Messages API (`/v1/messages`),
  `x-api-key`, `anthropic-version: 2023-06-01`, streaming text, tool-use
  arguments, stop reason and usage. Default environment: `ANTHROPIC_API_KEY`.
- `--backend copilot`: bridges to the official GitHub Copilot SDK Node
  provider in `copilot-sdk/dist/main.js`. Uses SDK-native streaming/tools
  and the SDK-matched runtime. `--copilot-sdk PATH` overrides the entrypoint;
  `--check-auth` queries SDK authentication without a document/model request.
  See [the SDK provider guide](../copilot/README.md).

## Setup in the editor

Build both executables:

```sh
cargo build --release -p rusteditor -p rusteditor-openai
```

Install them side by side (the install scripts copy the helper). Use
**Agent > Providers...**, choose a provider, enter an API key or sign in with
GitHub, optionally edit its model, and Save. No file editing is needed.

The UI saves non-secret `provider.json` profiles automatically. For API-key
profiles it passes `--credential openai` or `--credential anthropic`: the
helper reads the key directly from the OS credential store, falling back to
the corresponding environment variable only when no key is stored (or the
store is unavailable but an environment key exists). Keys are not arguments
or part of model requests/conversations. Linux needs `secret-tool` and an
unlocked Secret Service; no plaintext fallback is used. Copilot's own CLI
controls its token storage, including any fallback it provides.

## Copilot SDK

The old CLI prompt/JSON parser has been removed. Native SDK callbacks only
queue tool proposals; RustEditor still validates and approves execution.
The SDK runs in empty mode with ambient tools/config discovery disabled and
permission requests rejected. The install scripts build/copy the separate
Node package (Node.js 20.19+ or 22.12+ required). It shares the CLI's GitHub
sign-in and the settings UI can check SDK authentication independently.

Tests use injected SDK clients and a real unauthenticated SDK/runtime stdio
handshake, never real model requests or user credentials. An authenticated
service smoke test is still needed. Stop kills the helper; Windows' job guard
kills owned Node/runtime descendants. macOS hard-kill descendant cleanup is
not yet equivalent.
