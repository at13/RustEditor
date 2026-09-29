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
#[cfg(feature = "wasm")]
pub mod wasm;

use std::path::Path;

pub use manifest::{Capability, Command, Input, Kind, Manifest, Output, Plugin, Registry};
pub use protocol::{Outcome, Request};

/// Plugin API version this host implements.
pub const API_VERSION: u64 = 0;

/// Largest request or response accepted (bytes).
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Runs `command` of `plugin` with `request` (blocking; call on a worker).
pub fn run(plugin: &Plugin, command: &Command, request: &Request) -> anyhow::Result<Outcome> {
    let json = request.to_json(plugin, command);
    let response = match &plugin.manifest.kind {
        #[cfg(feature = "wasm")]
        Kind::Wasm { module } => wasm::run(&plugin.dir.join(module), json.as_bytes())?,
        #[cfg(not(feature = "wasm"))]
        Kind::Wasm { .. } => anyhow::bail!("this build has no WebAssembly plugin support"),
        Kind::Process { argv, timeout } => {
            process::run(&plugin.dir, argv, *timeout, json.as_bytes())?
        }
    };
    protocol::interpret(&response, command, request.inputs.len())
        .map_err(|error| error.context(format!("plugin {}", plugin.manifest.name)))
}

/// Resolves a manifest-relative path, refusing anything outside `dir`.
pub(crate) fn inside(dir: &Path, relative: &str) -> anyhow::Result<std::path::PathBuf> {
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
