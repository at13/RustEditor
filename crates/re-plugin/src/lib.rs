//! Plugins (UI-independent).
//!
//! A plugin is a folder with a `plugin.json` manifest that contributes
//! commands. Two kinds, with honestly different trust:
//!
//! - **`wasm`** (sandboxed): a WebAssembly module run by an interpreter. It
//!   can only compute: its only import is `env.log`; no files, network,
//!   clock or processes. CPU (fuel) and memory are capped.
//! - **`process`** (full trust): a program started for each command. It runs
//!   as the user and can do anything the user can; the editor only limits
//!   time and output size. Users approve it knowing that.
//!
//! Both receive the same JSON request and return the same JSON response
//! ([`protocol`]). The host always applies results as ordinary undoable
//! edits after checking the document did not change meanwhile.
//! See `docs/plugins.md`.

pub mod approvals;
pub mod manifest;
pub mod process;
pub mod protocol;
pub mod recipe;
#[cfg(feature = "wasm")]
pub mod wasm;

use std::path::Path;

pub use manifest::{
    AgentDef, AgentScope, Capability, Command, Input, Kind, Manifest, Output, Plugin, Registry,
    ToolDecl,
};
pub use protocol::{Outcome, Request};

/// Plugin API version this host implements.
pub const API_VERSION: u64 = 0;

/// Largest request or response accepted (bytes).
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Runs `command` of `plugin` with `request` (blocking; call on a worker).
pub fn run(plugin: &Plugin, command: &Command, request: &Request) -> anyhow::Result<Outcome> {
    if plugin.manifest.kind == Kind::Recipe {
        let outputs = request
            .inputs
            .iter()
            .map(|input| recipe::apply(&command.steps, input))
            .collect::<anyhow::Result<Vec<_>>>()?;
        return Ok(match command.output {
            Output::Replace => Outcome::Replace {
                outputs,
                status: None,
            },
            Output::Show => Outcome::Show {
                text: outputs.join(
                    "
",
                ),
                status: None,
            },
        });
    }
    let json = request.to_json(plugin, command);
    let response = exchange(plugin, json.as_bytes())?;
    protocol::interpret(&response, command, request.inputs.len())
        .map_err(|error| error.context(format!("plugin {}", plugin.manifest.name)))
}

/// Calls a tool the plugin contributes (blocking). Request
/// `{"api":0,"plugin":id,"tool":name,"arguments":{...}}`; response
/// `{"result": any}` or `{"error": "..."}`.
pub fn call_tool(
    plugin: &Plugin,
    tool: &str,
    arguments: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    if !plugin.manifest.tools.iter().any(|t| t.name == tool) {
        anyhow::bail!("plugin {} has no tool `{tool}`", plugin.manifest.id);
    }
    let request = serde_json::json!({
        "api": API_VERSION,
        "plugin": plugin.manifest.id,
        "tool": tool,
        "arguments": arguments,
    });
    let response = exchange(plugin, request.to_string().as_bytes())?;
    let value: serde_json::Value = serde_json::from_slice(&response)
        .map_err(|e| anyhow::anyhow!("tool response is not valid JSON: {e}"))?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("{}", error.as_str().unwrap_or("failed"));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("tool response has no `result`"))
}

/// Sends one request to a code plugin and returns its raw response.
fn exchange(plugin: &Plugin, request: &[u8]) -> anyhow::Result<Vec<u8>> {
    match &plugin.manifest.kind {
        #[cfg(feature = "wasm")]
        Kind::Wasm { module } => wasm::run(&plugin.dir.join(module), request),
        #[cfg(not(feature = "wasm"))]
        Kind::Wasm { .. } => anyhow::bail!("this build has no WebAssembly plugin support"),
        Kind::Process { argv, timeout } => process::run(&plugin.dir, argv, *timeout, request),
        Kind::Recipe => anyhow::bail!("recipes run in the editor, not as a program"),
        Kind::Agent(_) => {
            anyhow::bail!("agents open in the agent panel; they are not run as commands")
        }
    }
}

/// Resolves a manifest-relative path, refusing anything outside `dir`.
pub fn inside(dir: &Path, relative: &str) -> anyhow::Result<std::path::PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute()
        || candidate.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("`{relative}` must be a path inside the plugin folder");
    }
    Ok(dir.join(candidate))
}
