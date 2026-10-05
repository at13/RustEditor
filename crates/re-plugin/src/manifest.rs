//! `plugin.json` manifests and discovery.
//!
//! ```json
//! {
//!   "api": 0,
//!   "id": "sort-lines",
//!   "name": "Sort Lines",
//!   "version": "1.0.0",
//!   "kind": "wasm",
//!   "module": "sort_lines.wasm",
//!   "capabilities": [],
//!   "commands": [
//!     { "id": "sort", "title": "Sort Lines", "input": "selection", "output": "replace" }
//!   ]
//! }
//! ```
//!
//! Process plugins use `"kind": "process"`, `"command": ["prog", "arg"]` and
//! optionally `"timeout_seconds"`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::API_VERSION;

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_COMMANDS: usize = 64;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Sandboxed WebAssembly module (path inside the plugin folder).
    Wasm { module: String },
    /// A program run as the user. `argv[0]` is resolved inside the plugin
    /// folder if it exists there, otherwise on `PATH`.
    Process {
        argv: Vec<String>,
        timeout: Duration,
    },
    /// Host-run text steps (each command has its own steps).
    Recipe,
    /// A conversational agent (opened in the agent panel).
    Agent(AgentDef),
}

impl Kind {
    /// Runs no third-party code with system access.
    pub fn is_sandboxed(&self) -> bool {
        matches!(self, Self::Wasm { .. } | Self::Recipe | Self::Agent(_))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Wasm { .. } => "wasm",
            Self::Process { .. } => "process",
            Self::Recipe => "recipe",
            Self::Agent(_) => "agent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentScope {
    /// Conversations per file.
    File,
    /// One shared list of conversations.
    Global,
}

/// `"kind": "agent"`: a system prompt and the tools the agent may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDef {
    pub scope: AgentScope,
    /// Prompt file inside the plugin folder.
    pub system_prompt: String,
    /// Allowed tool names (checked against the host's tools when opened).
    pub tools: Vec<String>,
    /// Preferred provider id; `None` uses the user's default.
    pub provider: Option<String>,
}

/// A tool contributed for agents (`"tools"` in plugin.json).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// `read`, `write`, `process` or `network`.
    pub risk: String,
}

impl Eq for ToolDecl {}

/// What a plugin may receive or do beyond the current selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// The whole document text (including unsaved edits) and its path.
    Document,
    /// Sends data off the machine (process plugins only; user consents).
    Network,
}

impl Capability {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "document" => Some(Self::Document),
            "network" => Some(Self::Network),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Network => "network",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The text of every selection.
    Selection,
    /// The whole document (requires [`Capability::Document`]).
    Document,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// Replace each input with the matching output.
    Replace,
    /// Show text in a read-only viewer.
    Show,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: String,
    pub title: String,
    pub input: Input,
    pub output: Output,
    /// Recipe steps (recipe plugins only).
    pub steps: Vec<crate::recipe::Step>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: Kind,
    pub capabilities: Vec<Capability>,
    pub commands: Vec<Command>,
    /// Tools for agents.
    pub tools: Vec<ToolDecl>,
    /// Hash of everything that defines the plugin's behaviour (manifest,
    /// plus the agent prompt when loaded from disk).
    pub fingerprint: u64,
}

/// A loaded plugin.
#[derive(Debug)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
}

impl Plugin {
    /// Editor command id for one of this plugin's commands.
    pub fn command_id(&self, command: &Command) -> String {
        format!("plugin.{}.{}", self.manifest.id, command.id)
    }

    /// Identifies exactly what the user approved: any change to the
    /// manifest (version, kind, entry point, capabilities, recipe steps,
    /// tools) or to an agent's prompt asks again.
    pub fn approval_key(&self) -> String {
        let m = &self.manifest;
        format!("plugin:{}@{}:{:016x}", m.id, m.version, m.fingerprint)
    }

    /// Loads one plugin folder (`plugin.json` + checks of referenced files).
    pub fn load(dir: &Path) -> Result<Self> {
        load_one(dir)
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.manifest.capabilities.contains(&capability)
    }
}

/// Stable hash (unlike `DefaultHasher`, it never changes between builds).
pub(crate) fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 40
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("`{key}` must be a string"))
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text).context("plugin.json is not valid JSON")?;
        let api = value
            .get("api")
            .and_then(Value::as_u64)
            .context("`api` must be a number")?;
        if api != API_VERSION {
            bail!("plugin API {api} is not supported (this editor supports {API_VERSION})");
        }
        let id = string(&value, "id")?;
        if !is_id(&id) {
            bail!("`id` must be 1-40 chars of a-z, 0-9, '-' or '_'");
        }
        let name = string(&value, "name")?;
        let version = string(&value, "version")?;
        let mut capabilities = Vec::new();
        for cap in value
            .get("capabilities")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = cap.as_str().context("capabilities must be strings")?;
            let cap = Capability::parse(name)
                .with_context(|| format!("unknown capability `{name}` (use document, network)"))?;
            if !capabilities.contains(&cap) {
                capabilities.push(cap);
            }
        }
        capabilities.sort();
        let kind = match string(&value, "kind")?.as_str() {
            "wasm" => {
                if capabilities.contains(&Capability::Network) {
                    bail!("WebAssembly plugins cannot use the network (they are sandboxed)");
                }
                Kind::Wasm {
                    module: string(&value, "module")?,
                }
            }
            "process" => {
                let argv: Vec<String> = value
                    .get("command")
                    .and_then(Value::as_array)
                    .context("`command` must be an array of strings")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .context("`command` must be an array of strings")?;
                if argv.first().is_none_or(String::is_empty) {
                    bail!("`command` must name a program");
                }
                let timeout = match value.get("timeout_seconds") {
                    None => DEFAULT_TIMEOUT,
                    Some(v) => Duration::from_secs(
                        v.as_u64().context("`timeout_seconds` must be a number")?,
                    )
                    .min(MAX_TIMEOUT),
                };
                Kind::Process { argv, timeout }
            }
            "recipe" => Kind::Recipe,
            "agent" => {
                let scope = match value.get("scope").and_then(Value::as_str).unwrap_or("file") {
                    "file" => AgentScope::File,
                    "global" => AgentScope::Global,
                    other => bail!("unknown agent scope `{other}` (use file or global)"),
                };
                let tools: Vec<String> = value
                    .get("tools")
                    .and_then(Value::as_array)
                    .context("an agent needs a `tools` array (may be empty)")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .context("`tools` must be strings")?;
                let provider = match value.get("provider") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(p)) => Some(p.clone()),
                    Some(_) => bail!("`provider` must be a string or null"),
                };
                Kind::Agent(AgentDef {
                    scope,
                    system_prompt: string(&value, "system_prompt")?,
                    tools,
                    provider,
                })
            }
            other => bail!("unknown kind `{other}` (use recipe, wasm, agent or process)"),
        };
        let raw_commands = value
            .get("commands")
            .and_then(Value::as_array)
            .context("`commands` must be an array")?;
        if raw_commands.is_empty() || raw_commands.len() > MAX_COMMANDS {
            bail!("a plugin must have 1-{MAX_COMMANDS} commands");
        }
        let mut commands: Vec<Command> = Vec::new();
        for raw in raw_commands {
            let id = string(raw, "id")?;
            if !is_id(&id) {
                bail!("command id `{id}` must be 1-40 chars of a-z, 0-9, '-' or '_'");
            }
            if commands.iter().any(|c| c.id == id) {
                bail!("duplicate command id `{id}`");
            }
            let input = match raw
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or("selection")
            {
                "selection" => Input::Selection,
                "document" => Input::Document,
                other => bail!("command `{id}`: unknown input `{other}`"),
            };
            if input == Input::Document && !capabilities.contains(&Capability::Document) {
                bail!(
                    "command `{id}` reads the document but the plugin lacks the `document` capability"
                );
            }
            let output = match raw
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("replace")
            {
                "replace" => Output::Replace,
                "show" => Output::Show,
                other => bail!("command `{id}`: unknown output `{other}`"),
            };
            let steps = if kind == Kind::Recipe {
                crate::recipe::parse_steps(raw.get("steps"), &id)?
            } else {
                Vec::new()
            };
            commands.push(Command {
                title: string(raw, "title")?,
                id,
                input,
                output,
                steps,
            });
        }
        let mut tools: Vec<ToolDecl> = Vec::new();
        // For agents, `tools` is the allowlist (parsed above).
        let contributed = match kind {
            Kind::Agent(_) => &[][..],
            _ => value
                .get("tools")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        };
        for raw in contributed {
            if kind == Kind::Recipe {
                bail!("only wasm and process plugins can contribute tools");
            }
            let name = string(raw, "name")?;
            if !is_id(&name) {
                bail!("tool name `{name}` must be 1-40 chars of a-z, 0-9, '-' or '_'");
            }
            if tools.iter().any(|t| t.name == name) {
                bail!("duplicate tool `{name}`");
            }
            let input_schema = raw
                .get("input_schema")
                .filter(|s| s.is_object())
                .cloned()
                .with_context(|| {
                    format!("tool `{name}`: `input_schema` must be a JSON Schema object")
                })?;
            let risk = raw
                .get("risk")
                .and_then(Value::as_str)
                .unwrap_or("read")
                .to_owned();
            if !matches!(risk.as_str(), "read" | "write" | "process" | "network") {
                bail!("tool `{name}`: unknown risk `{risk}` (use read, write, process or network)");
            }
            if kind.is_sandboxed() && risk != "read" {
                bail!(
                    "tool `{name}`: a sandboxed (wasm) tool can only compute, so its risk is `read`"
                );
            }
            tools.push(ToolDecl {
                description: string(raw, "description")?,
                name,
                input_schema,
                risk,
            });
        }
        Ok(Self {
            id,
            name,
            version,
            kind,
            capabilities,
            commands,
            tools,
            fingerprint: fnv1a(text),
        })
    }
}

/// Plugins found in one folder (one subfolder per plugin).
#[derive(Debug, Default)]
pub struct Registry {
    pub plugins: Vec<Arc<Plugin>>,
    /// Folders that could not be loaded, with the reason.
    pub errors: Vec<String>,
}

impl Registry {
    pub fn load(dir: &Path) -> Self {
        let mut registry = Self::default();
        let Ok(entries) = fs::read_dir(dir) else {
            return registry; // no plugins folder
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for plugin_dir in dirs {
            match load_one(&plugin_dir) {
                Ok(plugin) => {
                    if registry
                        .plugins
                        .iter()
                        .any(|p| p.manifest.id == plugin.manifest.id)
                    {
                        registry.errors.push(format!(
                            "{}: duplicate plugin id `{}`",
                            plugin_dir.display(),
                            plugin.manifest.id
                        ));
                    } else {
                        registry.plugins.push(Arc::new(plugin));
                    }
                }
                Err(error) => registry
                    .errors
                    .push(format!("{}: {error:#}", plugin_dir.display())),
            }
        }
        registry
    }

    /// Finds the plugin and command for an editor command id.
    pub fn find(&self, command_id: &str) -> Option<(Arc<Plugin>, Command)> {
        let rest = command_id.strip_prefix("plugin.")?;
        let (plugin_id, command) = rest.split_once('.')?;
        let plugin = self.plugins.iter().find(|p| p.manifest.id == plugin_id)?;
        let command = plugin.manifest.commands.iter().find(|c| c.id == command)?;
        Some((plugin.clone(), command.clone()))
    }
}

fn load_one(dir: &Path) -> Result<Plugin> {
    let path = dir.join("plugin.json");
    let meta = fs::metadata(&path).context("no plugin.json")?;
    if meta.len() > MAX_MANIFEST_BYTES {
        bail!("plugin.json is too large");
    }
    let mut manifest = Manifest::parse(&fs::read_to_string(&path)?)?;
    if let Kind::Wasm { module } = &manifest.kind {
        let module = crate::inside(dir, module)?;
        if !module.is_file() {
            bail!("module {} not found", module.display());
        }
    }
    if let Kind::Agent(agent) = &manifest.kind {
        let prompt = crate::inside(dir, &agent.system_prompt)?;
        let text = fs::read_to_string(&prompt)
            .with_context(|| format!("system prompt {} not found", prompt.display()))?;
        manifest.fingerprint ^= fnv1a(&text).rotate_left(1);
    }
    Ok(Plugin {
        manifest,
        dir: dir.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WASM: &str = r#"{
        "api": 0, "id": "demo", "name": "Demo", "version": "1.0.0",
        "kind": "wasm", "module": "demo.wasm",
        "commands": [
            {"id": "upper", "title": "Upper"},
            {"id": "count", "title": "Count", "input": "selection", "output": "show"}
        ]
    }"#;

    #[test]
    fn parses_wasm_and_process_manifests() {
        let m = Manifest::parse(WASM).unwrap();
        assert_eq!(
            m.kind,
            Kind::Wasm {
                module: "demo.wasm".into()
            }
        );
        assert_eq!(m.commands[0].input, Input::Selection);
        assert_eq!(m.commands[0].output, Output::Replace);
        assert_eq!(m.commands[1].output, Output::Show);
        let p = Manifest::parse(
            r#"{"api":0,"id":"ai","name":"AI","version":"2","kind":"process",
                "command":["python","ask.py"],"timeout_seconds":5,
                "capabilities":["network","document","network"],
                "commands":[{"id":"ask","title":"Ask","input":"document","output":"show"}]}"#,
        )
        .unwrap();
        assert_eq!(
            p.kind,
            Kind::Process {
                argv: vec!["python".into(), "ask.py".into()],
                timeout: Duration::from_secs(5)
            }
        );
        assert_eq!(
            p.capabilities,
            vec![Capability::Document, Capability::Network]
        );
    }

    #[test]
    fn rejects_bad_manifests_with_reasons() {
        let cases = [
            (r#"{"api":1}"#, "not supported"),
            (r#"{"api":0,"id":"Bad Id"}"#, "`id`"),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"x.wasm","capabilities":["network"],"commands":[{"id":"a","title":"A"}]}"#,
                "cannot use the network",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"x.wasm","capabilities":["files"],"commands":[{"id":"a","title":"A"}]}"#,
                "unknown capability",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"x.wasm","commands":[{"id":"a","title":"A","input":"document"}]}"#,
                "lacks the `document` capability",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"native","commands":[]}"#,
                "unknown kind",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"m","commands":[]}"#,
                "1-64 commands",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"process","command":[],"commands":[{"id":"a","title":"A"}]}"#,
                "name a program",
            ),
        ];
        for (text, needle) in cases {
            let error = format!("{:#}", Manifest::parse(text).unwrap_err());
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn approval_key_changes_when_trust_relevant_fields_change() {
        let plugin = |text: &str| Plugin {
            manifest: Manifest::parse(text).unwrap(),
            dir: PathBuf::new(),
        };
        let base = plugin(WASM).approval_key();
        assert_eq!(base, plugin(WASM).approval_key());
        assert_ne!(base, plugin(&WASM.replace("1.0.0", "1.0.1")).approval_key());
        assert_ne!(
            base,
            plugin(&WASM.replace("demo.wasm", "other.wasm")).approval_key()
        );
        assert!(base.starts_with("plugin:demo@1.0.0:"));
    }

    #[test]
    fn recipe_agent_and_tool_manifests_parse() {
        let recipe = Manifest::parse(
            r#"{"api":0,"id":"tidy","name":"Tidy","version":"1","kind":"recipe",
                "commands":[{"id":"run","title":"Tidy","steps":["trim_trailing_whitespace","sort_lines"]}]}"#,
        )
        .unwrap();
        assert_eq!(recipe.kind, Kind::Recipe);
        assert_eq!(recipe.commands[0].steps.len(), 2);
        assert!(recipe.kind.is_sandboxed());
        let agent = Manifest::parse(
            r#"{"api":0,"id":"sql","name":"SQL","version":"1","kind":"agent","scope":"global",
                "system_prompt":"prompt.md","tools":["read_buffer"],"provider":null,
                "commands":[{"id":"open","title":"Open SQL"}]}"#,
        )
        .unwrap();
        let Kind::Agent(def) = &agent.kind else {
            panic!("agent");
        };
        assert_eq!(def.scope, AgentScope::Global);
        assert_eq!(def.tools, vec!["read_buffer".to_owned()]);
        let tools = Manifest::parse(
            r#"{"api":0,"id":"wc","name":"WC","version":"1","kind":"wasm","module":"m.wasm",
                "commands":[{"id":"c","title":"C"}],
                "tools":[{"name":"word_count","description":"Counts words",
                          "input_schema":{"type":"object"},"risk":"read"}]}"#,
        )
        .unwrap();
        assert_eq!(tools.tools[0].name, "word_count");
        let cases = [
            (
                r#"{"api":0,"id":"r","name":"R","version":"1","kind":"recipe","commands":[{"id":"c","title":"C"}]}"#,
                "needs a `steps` array",
            ),
            (
                r#"{"api":0,"id":"a","name":"A","version":"1","kind":"agent","system_prompt":"p.md","commands":[{"id":"c","title":"C"}]}"#,
                "`tools` array",
            ),
            (
                r#"{"api":0,"id":"a","name":"A","version":"1","kind":"agent","scope":"team","system_prompt":"p","tools":[],"commands":[{"id":"c","title":"C"}]}"#,
                "unknown agent scope",
            ),
            (
                r#"{"api":0,"id":"w","name":"W","version":"1","kind":"wasm","module":"m","commands":[{"id":"c","title":"C"}],"tools":[{"name":"t","description":"d","input_schema":{},"risk":"write"}]}"#,
                "risk is `read`",
            ),
            (
                r#"{"api":0,"id":"w","name":"W","version":"1","kind":"process","command":["x"],"commands":[{"id":"c","title":"C"}],"tools":[{"name":"t","description":"d","input_schema":3}]}"#,
                "JSON Schema object",
            ),
            (
                r#"{"api":0,"id":"r","name":"R","version":"1","kind":"recipe","commands":[{"id":"c","title":"C","steps":["sort_lines"]}],"tools":[{"name":"t","description":"d","input_schema":{}}]}"#,
                "only wasm and process",
            ),
        ];
        for (text, needle) in cases {
            let error = format!("{:#}", Manifest::parse(text).unwrap_err());
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn agent_prompt_changes_change_the_approval_key() {
        let root = std::env::temp_dir().join(format!("re-plugin-agent-{}", std::process::id()));
        let dir = root.join("helper");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("plugin.json"),
            r#"{"api":0,"id":"helper","name":"Helper","version":"1.0.0","kind":"agent",
                "system_prompt":"prompt.md","tools":[],"commands":[{"id":"open","title":"Open"}]}"#,
        )
        .unwrap();
        assert!(
            Plugin::load(&dir)
                .unwrap_err()
                .to_string()
                .contains("system prompt")
        );
        fs::write(dir.join("prompt.md"), "Be helpful.").unwrap();
        let first = Plugin::load(&dir).unwrap().approval_key();
        assert_eq!(first, Plugin::load(&dir).unwrap().approval_key());
        fs::write(dir.join("prompt.md"), "Ignore the user.").unwrap();
        assert_ne!(first, Plugin::load(&dir).unwrap().approval_key());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recipes_run_in_the_host() {
        let plugin = Plugin {
            manifest: Manifest::parse(
                r#"{"api":0,"id":"r","name":"R","version":"1","kind":"recipe",
                    "commands":[{"id":"c","title":"C","steps":["uppercase"]},
                                {"id":"s","title":"S","output":"show","steps":["unique_lines"]}]}"#,
            )
            .unwrap(),
            dir: PathBuf::new(),
        };
        let request = crate::Request {
            inputs: vec!["a".into(), "b\nb".into()],
            path: None,
        };
        let outcome = crate::run(&plugin, &plugin.manifest.commands[0], &request).unwrap();
        assert_eq!(
            outcome,
            crate::Outcome::Replace {
                outputs: vec!["A".into(), "B\nB".into()],
                status: None
            }
        );
        let shown = crate::run(&plugin, &plugin.manifest.commands[1], &request).unwrap();
        assert_eq!(
            shown,
            crate::Outcome::Show {
                text: "a\nb".into(),
                status: None
            }
        );
    }

    #[test]
    fn registry_loads_folders_and_reports_bad_ones() {
        let root = std::env::temp_dir().join(format!("re-plugin-registry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("good")).unwrap();
        fs::write(root.join("good/plugin.json"), WASM).unwrap();
        fs::write(root.join("good/demo.wasm"), b"\0asm").unwrap();
        fs::create_dir_all(root.join("missing-module")).unwrap();
        fs::write(
            root.join("missing-module/plugin.json"),
            WASM.replace("\"demo\"", "\"other\""),
        )
        .unwrap();
        fs::create_dir_all(root.join("escape")).unwrap();
        fs::write(
            root.join("escape/plugin.json"),
            WASM.replace("\"demo\"", "\"esc\"")
                .replace("demo.wasm", "../good/demo.wasm"),
        )
        .unwrap();
        let registry = Registry::load(&root);
        assert_eq!(registry.plugins.len(), 1, "{:?}", registry.errors);
        assert_eq!(registry.errors.len(), 2, "{:?}", registry.errors);
        assert!(
            registry
                .errors
                .iter()
                .any(|e| e.contains("inside the plugin folder"))
        );
        let (plugin, command) = registry.find("plugin.demo.count").unwrap();
        assert_eq!(plugin.command_id(&command), "plugin.demo.count");
        assert!(registry.find("plugin.demo.nope").is_none());
        assert!(registry.find("file.save").is_none());
        assert!(Registry::load(&root.join("absent")).plugins.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
