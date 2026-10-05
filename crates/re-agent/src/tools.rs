//! Tools: what an agent can do, and how the host runs them safely.
//!
//! A tool is called in two steps. [`Tool::prepare`] checks the arguments
//! against the current snapshot (cheap, no side effects) and returns either
//! an action to run now or an [`Approval`] the user must accept first. The
//! host then runs the [`Action`]: on a worker thread, or, for edits to the
//! open document, on the UI thread through the editor's undo history.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use re_tasks::CancelToken;
use ropey::Rope;
use serde_json::Value;

use crate::diff::DiffLine;

/// How much a tool call can affect; decides whether the user is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Reads inside the conversation's scope: never asks.
    Read,
    /// Reads outside the scope: asks once per conversation and folder.
    ReadOutsideScope,
    /// Changes files or the document: asks every time (diff shown).
    Write,
    /// Runs a program: asks every time.
    Process,
    /// Sends data over the network: asks every time.
    Network,
    /// Installs or changes editor extensions: asks every time.
    Install,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ReadOutsideScope => "read outside this folder",
            Self::Write => "write",
            Self::Process => "run a program",
            Self::Network => "use the network",
            Self::Install => "install an extension",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments (subset, see [`crate::schema`]).
    pub input_schema: Value,
    /// The least risk any call has (the actual call may be higher).
    pub risk: Risk,
}

/// The open document, as of one revision.
#[derive(Clone, Debug)]
pub struct DocSnapshot {
    pub path: Option<PathBuf>,
    pub name: String,
    pub rope: Rope,
    pub revision: u64,
    pub read_only: bool,
    /// `\n` or `\r\n` (used when the model writes plain `\n`).
    pub line_ending: &'static str,
}

/// Paths for the Extension Builder's tools.
#[derive(Clone, Debug)]
pub struct ExtEnv {
    /// Installed extensions (`<config>/plugins`).
    pub plugins_dir: PathBuf,
    /// Work in progress (`<data>/agents/staging`).
    pub staging_dir: PathBuf,
    /// Tool names agents may list (built-ins and contributed tools).
    pub known_tools: Vec<String>,
}

/// Everything a tool may use. Built by the host for each call.
#[derive(Clone, Debug, Default)]
pub struct ToolContext {
    /// Folder the agent works in (the file's repository root or folder).
    pub scope: Option<PathBuf>,
    /// The conversation's open document, if it is open.
    pub doc: Option<DocSnapshot>,
    pub ext: Option<ExtEnv>,
    pub cancel: CancelToken,
}

/// Something the host does after a tool finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEffect {
    /// Extensions changed: reload plugins and agents.
    ReloadPlugins,
    /// A file on disk changed: open tabs of it should check the disk.
    FileChanged(PathBuf),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    pub is_error: bool,
    pub effects: Vec<HostEffect>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            effects: Vec::new(),
        }
    }
}

/// One replacement in the open document, in char offsets of `revision`,
/// with the exact text it replaces (used to re-find it if the user edited
/// the document meanwhile).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferEdit {
    pub range: std::ops::Range<usize>,
    pub old_text: String,
    pub new_text: String,
}

pub type WorkerFn = Box<dyn FnOnce(&CancelToken) -> ToolOutput + Send>;

pub enum Action {
    /// Runs on a worker thread.
    Worker(WorkerFn),
    /// Applied by the host to the open document as one undo step.
    EditBuffer {
        revision: u64,
        edits: Vec<BufferEdit>,
    },
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Worker(_) => f.write_str("Action::Worker"),
            Self::EditBuffer { revision, edits } => f
                .debug_struct("Action::EditBuffer")
                .field("revision", revision)
                .field("edits", &edits.len())
                .finish(),
        }
    }
}

/// How long an approval lasts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remember {
    /// Only this call.
    Once,
    /// The rest of this conversation (e.g. reading one outside folder).
    Conversation(String),
    /// Saved (e.g. using an approved plugin's tool).
    Persistent(String),
}

/// Shown to the user; nothing the model writes can accept it.
#[derive(Clone, Debug, PartialEq)]
pub struct Approval {
    pub title: String,
    pub detail: String,
    pub diff: Vec<DiffLine>,
    pub risk: Risk,
    pub remember: Remember,
}

#[derive(Debug)]
pub enum Prepared {
    Run(Action),
    Ask(Approval, Action),
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Checks the call; no side effects. An `Err` is returned to the model
    /// as a tool error so it can correct itself.
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String>;
}

/// The tools an agent may call, by name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Specs of the allowed tools that exist, in allowlist order.
    pub fn specs(&self, allow: &[String]) -> Vec<ToolSpec> {
        allow
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.spec())
            .collect()
    }

    /// Validates `args` against the tool's schema, then prepares it.
    pub fn prepare(&self, ctx: &ToolContext, name: &str, args: &Value) -> Result<Prepared, String> {
        let tool = self
            .get(name)
            .ok_or_else(|| format!("unknown tool `{name}`"))?;
        crate::schema::validate(&tool.spec().input_schema, args)?;
        tool.prepare(ctx, args)
    }
}

/// Runs a worker action, turning a panic into a tool error.
pub fn run_worker(action: WorkerFn, cancel: &CancelToken) -> ToolOutput {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(cancel)))
        .unwrap_or_else(|_| ToolOutput::error("the tool crashed"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A tool for tests: echoes, optionally asking first.
    pub(crate) struct Echo {
        pub name: &'static str,
        pub ask: Option<Remember>,
    }

    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.into(),
                description: "echo".into(),
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
                risk: if self.ask.is_some() {
                    Risk::Write
                } else {
                    Risk::Read
                },
            }
        }

        fn prepare(&self, _: &ToolContext, args: &Value) -> Result<Prepared, String> {
            let text = args["text"].as_str().unwrap_or_default().to_owned();
            let action = Action::Worker(Box::new(move |_| ToolOutput::ok(text)));
            Ok(match &self.ask {
                None => Prepared::Run(action),
                Some(remember) => Prepared::Ask(
                    Approval {
                        title: "Echo?".into(),
                        detail: String::new(),
                        diff: Vec::new(),
                        risk: Risk::Write,
                        remember: remember.clone(),
                    },
                    action,
                ),
            })
        }
    }

    #[test]
    fn registry_validates_arguments_before_preparing() {
        let mut registry = ToolRegistry::default();
        registry.register(Arc::new(Echo {
            name: "echo",
            ask: None,
        }));
        let ctx = ToolContext::default();
        assert!(
            registry
                .prepare(&ctx, "nope", &json!({}))
                .unwrap_err()
                .contains("unknown")
        );
        assert!(
            registry
                .prepare(&ctx, "echo", &json!({}))
                .unwrap_err()
                .contains("missing")
        );
        let Prepared::Run(Action::Worker(run)) = registry
            .prepare(&ctx, "echo", &json!({"text": "hi"}))
            .unwrap()
        else {
            panic!("run");
        };
        assert_eq!(run_worker(run, &ctx.cancel).content, "hi");
        let crash: WorkerFn = Box::new(|_| panic!("boom"));
        assert!(run_worker(crash, &ctx.cancel).is_error);
        assert_eq!(registry.specs(&["echo".into(), "missing".into()]).len(), 1);
    }
}
