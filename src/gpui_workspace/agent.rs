//! The agent panel: per-file agents, the Extension Builder and custom
//! agents, wired to tabs, workers and approvals.
//!
//! The loop itself is `re_agent::Session` (pure state machine). This module
//! performs its effects: starts provider turns, prepares and runs tools on
//! workers (`re_tasks::spawn`), applies `edit_buffer` edits to documents as
//! one undo step (after the user accepts the diff), and shows the panel.
//! Every worker result carries the live session's generation, so results
//! that arrive after Stop are dropped.

use std::sync::Mutex;

use re_agent::{
    Action, Approval, DocSnapshot, Effect, Message, Prepared, Remember, Session, Subject, ToolCall,
    ToolContext, ToolOutput, ToolRegistry,
    diff::DiffKind,
    protocol::Event,
    provider::{ProcessProvider, Provider, ProviderConfig, ProviderRun},
    store::{ConversationStore, Summary},
    tools::{ExtEnv, HostEffect},
};

use super::*;

/// Width of the docked panel.
pub(super) const PANEL_WIDTH: f32 = 440.;
/// Most transcript messages rendered (older ones are summarized).
const SHOWN_MESSAGES: usize = 200;
/// Longest diff shown in an approval card.
const DIFF_LINES: usize = 300;

const FILE_AGENT_JSON: &str = include_str!("../../assets/agents/file-agent.json");
const FILE_AGENT_PROMPT: &str = include_str!("../../assets/agents/file-agent.md");
const BUILDER_JSON: &str = include_str!("../../assets/agents/extension-builder.json");
const BUILDER_PROMPT: &str = include_str!("../../assets/agents/extension-builder.md");
const PLUGINS_DOC: &str = include_str!("../../docs/plugins.md");

pub(super) const FILE_AGENT: &str = "file-agent";
pub(super) const BUILDER: &str = "extension-builder";

/// An agent the user can talk to.
#[derive(Clone, Debug)]
pub(super) struct AgentDef {
    pub id: String,
    pub name: String,
    /// `true`: one shared list of conversations; else per file.
    pub global: bool,
    pub prompt: String,
    pub tools: Vec<String>,
    pub provider: Option<String>,
    /// Approval key of the plugin that defines it (custom agents).
    pub approval_key: Option<String>,
}

fn builtin(json: &str, prompt: &str) -> AgentDef {
    let value: serde_json::Value = serde_json::from_str(json).expect("built-in agent json");
    AgentDef {
        id: value["id"].as_str().unwrap_or_default().to_owned(),
        name: value["name"].as_str().unwrap_or_default().to_owned(),
        global: value["scope"] == "global",
        prompt: re_agent::prompt::strip_comments(prompt),
        tools: value["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect(),
        provider: None,
        approval_key: None,
    }
}

/// Built-in agents plus agents defined by plugins (`"kind": "agent"`).
fn agent_defs(registry: Option<&re_plugin::Registry>) -> Vec<AgentDef> {
    let mut builder = builtin(BUILDER_JSON, BUILDER_PROMPT);
    builder.prompt.push_str("\n\n");
    builder.prompt.push_str(PLUGINS_DOC);
    let mut defs = vec![builtin(FILE_AGENT_JSON, FILE_AGENT_PROMPT), builder];
    for plugin in registry.map(|r| r.plugins.as_slice()).unwrap_or_default() {
        let re_plugin::Kind::Agent(def) = &plugin.manifest.kind else {
            continue;
        };
        let prompt = re_plugin::inside(&plugin.dir, &def.system_prompt)
            .and_then(|path| Ok(std::fs::read_to_string(path)?))
            .map(|text| text.chars().take(64 * 1024).collect::<String>())
            .unwrap_or_else(|error| format!("(prompt unavailable: {error:#})"));
        defs.push(AgentDef {
            id: format!("plugin:{}", plugin.manifest.id),
            name: plugin.manifest.name.clone(),
            global: def.scope == re_plugin::AgentScope::Global,
            prompt: re_agent::prompt::strip_comments(&prompt),
            tools: def.tools.clone(),
            provider: def.provider.clone(),
            approval_key: Some(plugin.approval_key()),
        });
    }
    defs
}

pub(super) struct ProviderEntry {
    pub config: ProviderConfig,
    pub provider: Arc<dyn Provider>,
}

/// Profiles saved by the settings UI (legacy custom profiles still work).
fn load_providers(dir: Option<&Path>) -> (Vec<ProviderEntry>, Vec<String>) {
    let (configs, errors) = dir
        .map(re_agent::provider::load_configs)
        .unwrap_or_default();
    let entries: Vec<ProviderEntry> = configs
        .into_iter()
        .map(|config| ProviderEntry {
            provider: Arc::new(ProcessProvider {
                config: config.clone(),
            }),
            config,
        })
        .collect();
    (entries, errors)
}

/// Where agent data lives; tests point these at temp folders.
#[derive(Clone, Default)]
pub(super) struct AgentPaths {
    /// Installed extensions (from `PluginState`, refreshed).
    pub plugins: Option<PathBuf>,
    pub conversations: Option<PathBuf>,
    pub staging: Option<PathBuf>,
    pub providers: Option<PathBuf>,
}

impl AgentPaths {
    fn defaults() -> Self {
        let agents = re_platform::paths::data_dir().map(|d| d.join("agents"));
        Self {
            plugins: None,
            conversations: agents.as_ref().map(|d| d.join("conversations")),
            staging: agents.map(|d| d.join("staging")),
            providers: re_platform::paths::config_dir().map(|d| d.join("providers")),
        }
    }
}

/// One open conversation.
pub(super) struct Live {
    pub key: u64,
    pub def: AgentDef,
    pub session: Session,
    /// The tab it belongs to (`None` for global agents).
    pub tab_id: Option<u64>,
    pub scope: Option<PathBuf>,
    pub provider: Option<String>,
    run: Option<ProviderRun>,
    generation: u64,
    /// The prepared action of the current tool call.
    action: Option<(String, Action)>,
    cancel: Option<CancelToken>,
    /// What it is doing ("Waiting for the model...").
    pub activity: String,
}

pub(super) enum AgentMessage {
    ProviderAuth {
        result: String,
    },
    Settings {
        id: String,
        result: Result<String, String>,
    },
    Model {
        key: u64,
        generation: u64,
        event: Event,
    },
    Prepared {
        key: u64,
        generation: u64,
        call_id: String,
        result: Result<Prepared, String>,
    },
    ToolDone {
        key: u64,
        generation: u64,
        call_id: String,
        output: ToolOutput,
    },
}

/// Text waiting for consent to send it to a provider.
pub(super) struct Consent {
    pub key: u64,
    pub provider: String,
    pub text: String,
}

pub(super) struct AgentState {
    pub open: bool,
    pub lives: Vec<Live>,
    /// Live shown in the panel.
    pub current: Option<u64>,
    next_key: u64,
    pub paths: AgentPaths,
    store: Option<ConversationStore>,
    pub defs: Vec<AgentDef>,
    pub tools: ToolRegistry,
    pub providers: Vec<ProviderEntry>,
    pub provider_errors: Vec<String>,
    pub providers_loaded: bool,
    pub default_provider: Option<String>,
    pub consent: Option<Consent>,
    pub show_list: bool,
    pub list: Vec<Summary>,
    pub(super) sender: WakingSender<AgentMessage>,
    pub receiver: Receiver<AgentMessage>,
    pub scroll: gpui::ScrollHandle,
    /// Second "Delete Agent Data" confirms.
    pub confirm_delete: bool,
    pub settings: super::provider_settings::Settings,
}

impl AgentState {
    pub fn new(sender: WakingSender<AgentMessage>, receiver: Receiver<AgentMessage>) -> Self {
        Self {
            open: false,
            lives: Vec::new(),
            current: None,
            next_key: 1,
            paths: AgentPaths::defaults(),
            store: None,
            defs: agent_defs(None),
            tools: ToolRegistry::default(),
            providers: Vec::new(),
            provider_errors: Vec::new(),
            providers_loaded: false,
            default_provider: None,
            consent: None,
            show_list: false,
            list: Vec::new(),
            sender,
            receiver,
            scroll: gpui::ScrollHandle::new(),
            confirm_delete: false,
            settings: Default::default(),
        }
    }

    fn store(&mut self) -> Result<&ConversationStore, String> {
        if self.store.is_none() {
            let root = self
                .paths
                .conversations
                .clone()
                .ok_or("no per-user data folder for conversations")?;
            if let Some(parent) = root.parent() {
                re_platform::paths::ensure_private_dir(parent).map_err(|e| e.to_string())?;
            }
            re_platform::paths::ensure_private_dir(&root).map_err(|e| e.to_string())?;
            self.store = Some(ConversationStore::new(root));
        }
        Ok(self.store.as_ref().expect("store"))
    }

    pub fn live(&self, key: u64) -> Option<&Live> {
        self.lives.iter().find(|l| l.key == key)
    }

    fn live_mut(&mut self, key: u64) -> Option<&mut Live> {
        self.lives.iter_mut().find(|l| l.key == key)
    }

    pub fn def(&self, id: &str) -> Option<&AgentDef> {
        self.defs.iter().find(|d| d.id == id)
    }

    fn provider(&self, id: Option<&str>) -> Option<&ProviderEntry> {
        let wanted = id.or(self.default_provider.as_deref());
        wanted
            .and_then(|id| self.providers.iter().find(|p| p.config.id == id))
            .or_else(|| self.providers.first())
    }
}

/// The conversation subject for a tab.
fn tab_subject(tab: &Tab) -> Subject {
    match &tab.path {
        Some(path) => Subject::File { path: path.clone() },
        None => Subject::Untitled { tab: tab.id },
    }
}

fn language(path: Option<&Path>) -> String {
    let ext = path
        .and_then(Path::extension)
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => "Rust",
        "py" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "json" => "JSON",
        "md" => "Markdown",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "c" | "h" => "C",
        "cpp" | "cc" | "hpp" => "C++",
        "cs" => "C#",
        "go" => "Go",
        "java" => "Java",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "xml" => "XML",
        "sh" => "shell script",
        "ps1" => "PowerShell",
        "sql" => "SQL",
        "csv" => "CSV",
        "log" => "log",
        "" => "plain text",
        other => return other.to_owned(),
    }
    .to_owned()
}

impl Workspace {
    // ----- Setup -----

    /// Reloads agent definitions, tools and (once) providers.
    pub(super) fn refresh_agents(&mut self, cx: &mut Context<Self>) {
        let registry = cx.try_global::<PluginState>().map(|s| &s.registry);
        self.agents.defs = agent_defs(registry);
        self.agents.paths.plugins = Self::plugins_dir(cx);
        let mut tools = re_agent_tools::builtin_tools();
        if let Some(registry) = registry {
            for tool in re_agent_tools::plugin::plugin_tools(registry) {
                tools.register(tool);
            }
        }
        self.agents.tools = tools;
        if !self.agents.providers_loaded {
            let (providers, errors) = load_providers(self.agents.paths.providers.as_deref());
            self.agents.providers = providers;
            self.agents.provider_errors = errors;
            if let Some(id) = self
                .agents
                .paths
                .providers
                .as_deref()
                .and_then(re_agent::setup::default_id)
                && self.agents.providers.iter().any(|p| p.config.id == id)
            {
                self.agents.default_provider = Some(id);
            }
            self.agents.providers_loaded = true;
        }
    }

    fn plugins_dir(cx: &App) -> Option<PathBuf> {
        cx.try_global::<PluginState>()
            .and_then(|s| s.dir.clone())
            .or_else(|| re_platform::paths::config_dir().map(|c| c.join("plugins")))
    }

    /// Specs the model sees: the agent's tools (and, for the File Agent,
    /// every contributed tool).
    fn agent_tool_specs(&self, def: &AgentDef) -> Vec<re_agent::ToolSpec> {
        let mut names = def.tools.clone();
        if def.id == FILE_AGENT {
            names.extend(
                self.agents
                    .tools
                    .names()
                    .into_iter()
                    .filter(|n| n.contains("__")),
            );
        }
        self.agents.tools.specs(&names)
    }

    /// The system prompt for `def` about the active tab.
    fn agent_prompt(&self, def: &AgentDef) -> String {
        let contributed: String = self
            .agent_tool_specs(def)
            .iter()
            .filter(|s| s.name.contains("__"))
            .map(|s| format!("| `{}` | {} |\n", s.name, s.description))
            .collect();
        let mut vars = vec![("extra_tools", contributed.trim_end().to_owned())];
        if let Some((doc, editor)) = self.document().zip(self.editor()) {
            let rope = doc.rope();
            let head = editor.head().min(rope.len_chars());
            let range = editor.range();
            let selection = if range.is_empty() {
                String::new()
            } else {
                let first = rope.char_to_line(range.start) + 1;
                let last = rope.char_to_line(range.end.min(rope.len_chars())) + 1;
                format!(
                    "; lines {first}-{last} are selected ({} characters)",
                    range.len()
                )
            };
            vars.extend([
                ("file_name", doc.display_name()),
                ("file_size", format_bytes(rope.len_bytes() as u64)),
                ("language", language(doc.path())),
                ("line_count", doc.line_count().to_string()),
                ("line_ending", doc.line_ending().label().to_owned()),
                ("encoding", doc.encoding().label().to_owned()),
                (
                    "read_only_note",
                    if doc.is_read_only() {
                        ", read-only: do not propose edits".to_owned()
                    } else {
                        String::new()
                    },
                ),
                ("cursor_line", (rope.char_to_line(head) + 1).to_string()),
                ("selection_note", selection),
            ]);
        } else {
            vars.push(("file_name", "(no file open)".to_owned()));
        }
        re_agent::prompt::fill(&def.prompt, &vars)
    }

    /// The live conversation the panel shows.
    pub(super) fn shown_live(&self) -> Option<&Live> {
        let tab_id = self.tabs.get(self.active).map(|t| t.id);
        let current = self.agents.current.and_then(|k| self.agents.live(k));
        match current {
            Some(live) if live.tab_id.is_none() || live.tab_id == tab_id => Some(live),
            _ => self
                .agents
                .lives
                .iter()
                .rev()
                .find(|l| l.tab_id.is_some() && l.tab_id == tab_id),
        }
    }

    fn shown_key(&self) -> Option<u64> {
        self.shown_live().map(|l| l.key)
    }

    /// Starts a new conversation with agent `id` (about the active tab for
    /// file agents). Returns its key.
    #[inline(never)]
    fn new_live(&mut self, id: &str, conversation: Option<re_agent::Conversation>) -> Option<u64> {
        let def = self.agents.def(id)?.clone();
        let tab = self.tabs.get(self.active);
        let (tab_id, subject, scope) = if def.global {
            (None, Subject::Global, None)
        } else {
            let tab = tab?;
            let scope = tab
                .path
                .as_deref()
                .and_then(re_agent_tools::scope::default_scope);
            (Some(tab.id), tab_subject(tab), scope)
        };
        let conversation =
            conversation.unwrap_or_else(|| re_agent::Conversation::new(&def.id, subject));
        let session = Session::new(
            conversation,
            self.agent_prompt(&def),
            self.agent_tool_specs(&def),
        );
        let key = self.agents.next_key;
        self.agents.next_key += 1;
        self.agents.lives.push(Live {
            key,
            provider: def.provider.clone(),
            def,
            session,
            tab_id,
            scope,
            run: None,
            generation: 0,
            action: None,
            cancel: None,
            activity: String::new(),
        });
        self.agents.current = Some(key);
        Some(key)
    }

    /// Shows the latest conversation with agent `id` for this tab (or the
    /// global one), creating one if needed.
    pub(super) fn show_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        let tab_id = self.tabs.get(self.active).map(|t| t.id);
        let Some(global) = self.agents.def(id).map(|d| d.global) else {
            self.status = format!("No agent '{id}'");
            cx.notify();
            return;
        };
        let existing = self
            .agents
            .lives
            .iter()
            .rev()
            .find(|l| l.def.id == id && (global || l.tab_id == tab_id))
            .map(|l| l.key);
        let key = existing.or_else(|| self.new_live(id, None));
        self.agents.current = key;
        self.agents.open = true;
        self.agents.show_list = false;
        self.refresh_list();
        cx.notify();
    }

    fn refresh_list(&mut self) {
        let subject = match self.shown_live() {
            Some(live) => live.session.conversation.subject.clone(),
            None => match self.tabs.get(self.active) {
                Some(tab) => tab_subject(tab),
                None => return,
            },
        };
        self.agents.list = match self.agents.store() {
            Ok(store) => store.list(&subject),
            Err(_) => Vec::new(),
        };
    }

    pub(super) fn open_saved(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(live) = self
            .agents
            .lives
            .iter()
            .find(|l| l.session.conversation.id == id)
        {
            self.agents.current = Some(live.key);
            self.agents.show_list = false;
            cx.notify();
            return;
        }
        let subject = match self.shown_live() {
            Some(live) => live.session.conversation.subject.clone(),
            None => self
                .tabs
                .get(self.active)
                .map(tab_subject)
                .unwrap_or(Subject::Global),
        };
        let loaded = self
            .agents
            .store()
            .and_then(|s| s.load(&subject, id).map_err(|e| e.to_string()));
        match loaded {
            Ok(mut conversation) => {
                let agent = if self.agents.def(&conversation.agent).is_some() {
                    conversation.agent.clone()
                } else {
                    conversation.push(Message::Notice {
                        text: format!(
                            "Agent '{}' is not installed; continuing with the File Agent.",
                            conversation.agent
                        ),
                    });
                    FILE_AGENT.to_owned()
                };
                self.new_live(&agent, Some(conversation));
                self.agents.show_list = false;
            }
            Err(error) => self.status = format!("Cannot open the conversation: {error}"),
        }
        cx.notify();
    }

    pub(super) fn delete_saved(&mut self, id: &str, cx: &mut Context<Self>) {
        let subject =
            self.agents
                .list
                .iter()
                .any(|s| s.id == id)
                .then(|| match self.shown_live() {
                    Some(live) => live.session.conversation.subject.clone(),
                    None => self
                        .tabs
                        .get(self.active)
                        .map(tab_subject)
                        .unwrap_or(Subject::Global),
                });
        if let Some(subject) = subject
            && let Ok(store) = self.agents.store()
            && let Err(error) = store.delete(&subject, id)
        {
            self.status = format!("Cannot delete the conversation: {error}");
        }
        let keys: Vec<u64> = self
            .agents
            .lives
            .iter()
            .filter(|l| l.session.conversation.id == id)
            .map(|l| l.key)
            .collect();
        for key in keys {
            self.stop_live(key);
            self.agents.lives.retain(|l| l.key != key);
        }
        self.refresh_list();
        cx.notify();
    }

    // ----- Commands -----

    pub(super) fn agent_toggle_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agents.open {
            self.agents.open = false;
            if matches!(
                self.command,
                Some(CommandMode::Agent | CommandMode::ProviderSetting)
            ) {
                self.cancel_command(cx);
            }
            if !self.agents.settings.busy {
                self.agents.settings = Default::default();
            }
            cx.notify();
            return;
        }
        self.refresh_agents(cx);
        if self.shown_live().is_none() {
            let id = if self.document().is_some() {
                FILE_AGENT
            } else {
                BUILDER
            };
            self.new_live(id, None);
        }
        self.agents.open = true;
        self.refresh_list();
        self.agent_focus_input(window, cx);
    }

    pub(super) fn agent_focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agents.settings.open {
            if self.agents.settings.busy {
                return;
            }
            self.provider_settings_close(cx);
        }
        if self.command.is_some() && !matches!(self.command, Some(CommandMode::Agent)) {
            self.cancel_command(cx);
        }
        if self.command.is_none() {
            self.start_command(CommandMode::Agent, window, cx);
        }
        cx.notify();
    }

    pub(super) fn agent_new_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_agents(cx);
        let id = self
            .shown_live()
            .map(|l| l.def.id.clone())
            .unwrap_or_else(|| FILE_AGENT.to_owned());
        if self.new_live(&id, None).is_none() {
            self.status = "Open a file to talk to the File Agent".into();
        }
        self.agents.open = true;
        self.agents.show_list = false;
        self.agent_focus_input(window, cx);
    }

    pub(super) fn agent_extension_builder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_agents(cx);
        self.show_agent(BUILDER, cx);
        self.agent_focus_input(window, cx);
    }

    pub(super) fn agent_stop(&mut self, cx: &mut Context<Self>) {
        match self.shown_key() {
            Some(key) => {
                let effects = self.stop_live(key);
                self.handle_effects(key, effects, cx);
                self.status = "Agent stopped".into();
            }
            None => self.status = "No agent is running".into(),
        }
        cx.notify();
    }

    pub(super) fn agent_answer(&mut self, accepted: bool, cx: &mut Context<Self>) {
        let pending = self.shown_live().and_then(|live| {
            live.session
                .pending_approval()
                .map(|(call, approval)| (live.key, call.id.clone(), approval.remember.clone()))
        });
        let Some((key, call_id, remember)) = pending else {
            self.status = "Nothing to approve".into();
            cx.notify();
            return;
        };
        if accepted
            && let Remember::Persistent(approval_key) = &remember
            && let Err(error) = cx
                .default_global::<PluginState>()
                .approvals
                .approve(approval_key)
        {
            self.status = format!("Approved for this session (cannot save: {error})");
        }
        let effects = self
            .agents
            .live_mut(key)
            .map(|l| l.session.on_approval(&call_id, accepted))
            .unwrap_or_default();
        self.handle_effects(key, effects, cx);
    }

    pub(super) fn agent_providers(&mut self, cx: &mut Context<Self>) {
        self.agents.providers_loaded = false;
        self.refresh_agents(cx);
        if self.command.is_some() {
            self.cancel_command(cx);
        }
        self.agents.open = true;
        self.agents.settings.open = true;
        let kind = self.agents.settings.kind;
        self.provider_select(kind, cx);
    }

    pub(super) fn agent_cycle_provider(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .agents
            .providers
            .iter()
            .map(|p| p.config.id.clone())
            .collect();
        if ids.is_empty() {
            self.status = "No providers configured (Agent > Providers...)".into();
            cx.notify();
            return;
        }
        let current = self
            .agents
            .provider(None)
            .map(|p| p.config.id.clone())
            .unwrap_or_default();
        let next = ids
            .iter()
            .position(|id| *id == current)
            .map_or(0, |i| (i + 1) % ids.len());
        self.agents.default_provider = Some(ids[next].clone());
        if let Some(key) = self.shown_key()
            && let Some(live) = self.agents.live_mut(key)
        {
            live.provider = Some(ids[next].clone());
        }
        self.status = format!("Agent provider: {}", ids[next]);
        cx.notify();
    }

    pub(super) fn agent_delete_data(&mut self, cx: &mut Context<Self>) {
        if !self.agents.confirm_delete {
            self.agents.confirm_delete = true;
            self.status =
                "Delete all saved agent conversations? Run Delete Agent Data again to confirm."
                    .into();
            cx.notify();
            return;
        }
        self.agents.confirm_delete = false;
        let keys: Vec<u64> = self.agents.lives.iter().map(|l| l.key).collect();
        for key in keys {
            self.stop_live(key);
        }
        self.agents.lives.clear();
        self.agents.current = None;
        self.agents.consent = None;
        self.status = match self
            .agents
            .store()
            .and_then(|s| s.delete_all().map_err(|e| e.to_string()))
        {
            Ok(()) => "Agent conversations deleted".into(),
            Err(error) => format!("Cannot delete agent data: {error}"),
        };
        self.agents.list.clear();
        cx.notify();
    }

    /// Opens a plugin-defined agent (after the plugin approval).
    pub(super) fn open_plugin_agent(
        &mut self,
        plugin: &re_plugin::Plugin,
        command_id: &str,
        cx: &mut Context<Self>,
    ) {
        let re_plugin::Kind::Agent(def) = &plugin.manifest.kind else {
            return;
        };
        let question = format!(
            "Agent \"{}\" {} is an extension. It can use these tools: {}. Every write is still shown \
             for approval. Run it again to allow.",
            plugin.manifest.name,
            plugin.manifest.version,
            if def.tools.is_empty() {
                "none".to_owned()
            } else {
                def.tools.join(", ")
            }
        );
        if !self.confirm(command_id, &plugin.approval_key(), question, cx) {
            return;
        }
        self.refresh_agents(cx);
        self.show_agent(&format!("plugin:{}", plugin.manifest.id), cx);
    }

    // ----- Sending and the loop -----

    /// Sends the input as a user message to the shown conversation.
    pub(super) fn agent_send(&mut self, text: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
        if !self.agents.providers_loaded {
            self.refresh_agents(cx);
        }
        let key = match self.shown_key() {
            Some(key) => key,
            None => match self
                .new_live(FILE_AGENT, None)
                .or_else(|| self.new_live(BUILDER, None))
            {
                Some(key) => key,
                None => return,
            },
        };
        let live = self.agents.live(key).expect("live");
        if !live.session.is_idle() {
            self.status = "The agent is busy; Stop it or wait".into();
            cx.notify();
            return;
        }
        let Some(entry) = self.agents.provider(live.provider.as_deref()) else {
            self.status = "No model provider is configured: see Agent > Providers...".into();
            cx.notify();
            return;
        };
        // Custom agents run only after their plugin approval (asked when
        // opened from the Plugins menu or palette).
        if let Some(approval_key) = &live.def.approval_key
            && !cx
                .default_global::<PluginState>()
                .approvals
                .is_approved(approval_key)
        {
            self.status = format!(
                "Open \"{}\" from the Plugins menu first to approve it",
                live.def.name
            );
            cx.notify();
            return;
        }
        let egress = entry.config.egress_key();
        if !cx
            .default_global::<PluginState>()
            .approvals
            .is_approved(&egress)
        {
            self.agents.consent = Some(Consent {
                key,
                provider: entry.config.id.clone(),
                text,
            });
            cx.notify();
            return;
        }
        let effects = self
            .agents
            .live_mut(key)
            .map(|l| l.session.send(&text))
            .unwrap_or_default();
        self.handle_effects(key, effects, cx);
    }

    pub(super) fn agent_consent(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(consent) = self.agents.consent.take() else {
            return;
        };
        if !allow {
            self.status = "Not sent".into();
            cx.notify();
            return;
        }
        let Some(egress) = self
            .agents
            .providers
            .iter()
            .find(|p| p.config.id == consent.provider)
            .map(|p| p.config.egress_key())
        else {
            return;
        };
        if let Err(error) = cx
            .default_global::<PluginState>()
            .approvals
            .approve(&egress)
        {
            self.status = format!("Allowed for this session (cannot save: {error})");
        }
        if let Some(live) = self.agents.live_mut(consent.key) {
            live.provider = Some(consent.provider);
        }
        let effects = self
            .agents
            .live_mut(consent.key)
            .map(|l| l.session.send(&consent.text))
            .unwrap_or_default();
        self.handle_effects(consent.key, effects, cx);
    }

    #[inline(never)]
    fn handle_effects(&mut self, key: u64, effects: Vec<Effect>, cx: &mut Context<Self>) {
        let mut queue: std::collections::VecDeque<Effect> = effects.into();
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::Save => self.save_live(key),
                Effect::StartModel(request) => {
                    // Settings can change between tool turns. Never inherit
                    // consent for a different model/program/destination.
                    let egress = self
                        .agents
                        .live(key)
                        .and_then(|l| self.agents.provider(l.provider.as_deref()))
                        .map(|p| p.config.egress_key());
                    if egress.as_deref().is_some_and(|k| {
                        !cx.default_global::<PluginState>().approvals.is_approved(k)
                    }) {
                        let more = self.agents.live_mut(key).map(|l| l.session.on_event(Event::Error {
                            message: "Provider settings changed. Send another message to approve the new provider before continuing.".into(),
                        })).unwrap_or_default();
                        queue.extend(more);
                    } else {
                        self.start_model(key, request);
                    }
                }
                Effect::Prepare(call) => queue.extend(self.prepare_call(key, call)),
                Effect::Run { call_id } => queue.extend(self.run_action(key, &call_id, cx)),
                Effect::Ask { call_id, approval } => {
                    if let Remember::Persistent(approval_key) = &approval.remember
                        && cx
                            .default_global::<PluginState>()
                            .approvals
                            .is_approved(approval_key)
                    {
                        let more = self
                            .agents
                            .live_mut(key)
                            .map(|l| l.session.on_approval(&call_id, true))
                            .unwrap_or_default();
                        queue.extend(more);
                        continue;
                    }
                    if let Some(live) = self.agents.live_mut(key) {
                        live.activity = "Waiting for your approval".into();
                    }
                    self.agents.open = true;
                    self.status = format!("Agent asks: {}", approval.title);
                }
                Effect::Idle => {
                    if let Some(live) = self.agents.live_mut(key) {
                        live.activity.clear();
                        live.run = None;
                        live.cancel = None;
                        live.action = None;
                    }
                    self.refresh_list();
                }
            }
        }
        self.agents.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn save_live(&mut self, key: u64) {
        let subject = {
            let Some(live) = self.agents.live(key) else {
                return;
            };
            // An untitled tab saved as a file: keep its conversations.
            match (&live.session.conversation.subject, live.tab_id) {
                (Subject::Untitled { .. }, Some(tab_id)) => self
                    .tabs
                    .iter()
                    .find(|t| t.id == tab_id)
                    .and_then(|t| t.path.clone())
                    .map(|path| Subject::File { path }),
                _ => None,
            }
        };
        if let Some(subject) = subject
            && let Some(live) = self.agents.live_mut(key)
        {
            live.session.conversation.subject = subject;
        }
        let Some(conversation) = self
            .agents
            .live(key)
            .map(|l| l.session.conversation.clone())
        else {
            return;
        };
        let result = self
            .agents
            .store()
            .and_then(|s| s.save(&conversation).map_err(|e| e.to_string()));
        if let Err(error) = result {
            self.status = format!("Cannot save the conversation: {error}");
        }
    }

    #[inline(never)]
    fn start_model(&mut self, key: u64, request: re_agent::protocol::ModelRequest) {
        let sender = self.agents.sender.clone();
        let Some(live) = self.agents.lives.iter().find(|l| l.key == key) else {
            return;
        };
        let provider = self
            .agents
            .provider(live.provider.as_deref())
            .map(|p| (p.provider.clone(), p.config.name.clone()));
        let live = self.agents.live_mut(key).expect("live");
        live.generation += 1;
        let generation = live.generation;
        let Some((provider, name)) = provider else {
            let _ = sender.send(AgentMessage::Model {
                key,
                generation,
                event: Event::Error {
                    message: "no model provider is configured".into(),
                },
            });
            return;
        };
        live.activity = format!("Waiting for {name}...");
        // The sink runs on the provider's thread; the sender is not Sync.
        let sender = Mutex::new(sender);
        live.run = Some(provider.start(
            request,
            Box::new(move |event| {
                let _ = sender.lock().expect("sender").send(AgentMessage::Model {
                    key,
                    generation,
                    event,
                });
            }),
        ));
    }

    /// A snapshot of the conversation's document, if its tab is ready.
    fn doc_snapshot(&self, tab_id: Option<u64>) -> Option<DocSnapshot> {
        let tab = self.tabs.iter().find(|t| Some(t.id) == tab_id)?;
        let TabState::Ready(doc) = &tab.state else {
            return None;
        };
        Some(DocSnapshot {
            path: doc.path().map(Path::to_path_buf),
            name: doc.display_name(),
            rope: doc.rope().clone(),
            revision: doc.revision(),
            read_only: doc.is_read_only(),
            line_ending: doc.line_ending().as_str(),
        })
    }

    #[inline(never)]
    fn prepare_call(&mut self, key: u64, call: ToolCall) -> Vec<Effect> {
        let Some(live) = self.agents.live(key) else {
            return Vec::new();
        };
        if !live.session.tools().iter().any(|t| t.name == call.name) {
            let error = format!("`{}` is not one of your tools", call.name);
            return self
                .agents
                .live_mut(key)
                .map(|l| l.session.on_prepared(&call.id, Err(error)))
                .unwrap_or_default();
        }
        let ext = (live.def.id == BUILDER).then(|| ExtEnv {
            plugins_dir: self
                .agents
                .paths
                .plugins
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("rusteditor-plugins")),
            staging_dir: self
                .agents
                .paths
                .staging
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("rusteditor-staging")),
            known_tools: self
                .agents
                .tools
                .names()
                .into_iter()
                .filter(|n| !n.starts_with("ext_"))
                .collect(),
        });
        let ctx = ToolContext {
            scope: live.scope.clone(),
            doc: self.doc_snapshot(live.tab_id),
            ext,
            cancel: CancelToken::new(),
        };
        let generation = live.generation;
        let registry = self.agents.tools.clone();
        let sender = self.agents.sender.clone();
        let call_id = call.id.clone();
        let name = call.name.clone();
        if let Some(live) = self.agents.live_mut(key) {
            live.activity = format!("Preparing {name}...");
        }
        let started = re_tasks::spawn(
            format!("rusteditor-agent-prepare-{name}"),
            None,
            move || Ok(registry.prepare(&ctx, &call.name, &call.arguments)),
            move |result: anyhow::Result<Result<Prepared, String>>| {
                let result = result.unwrap_or_else(|e| Err(format!("{e:#}")));
                let _ = sender.send(AgentMessage::Prepared {
                    key,
                    generation,
                    call_id,
                    result,
                });
            },
        );
        match started {
            Ok(()) => Vec::new(),
            Err(error) => self
                .agents
                .live_mut(key)
                .map(|l| l.session.on_prepared(&call.id, Err(error.to_string())))
                .unwrap_or_default(),
        }
    }

    #[inline(never)]
    fn run_action(&mut self, key: u64, call_id: &str, cx: &mut Context<Self>) -> Vec<Effect> {
        let Some(live) = self.agents.live_mut(key) else {
            return Vec::new();
        };
        let action = match live.action.take() {
            Some((id, action)) if id == call_id => action,
            _ => {
                return live.session.on_tool_result(
                    call_id,
                    ToolOutput::error("internal error: the prepared action is missing"),
                    true,
                );
            }
        };
        match action {
            Action::Worker(work) => {
                let cancel = CancelToken::new();
                live.cancel = Some(cancel.clone());
                let name = live
                    .session
                    .pending_call_name(call_id)
                    .unwrap_or_else(|| "tool".into());
                live.activity = format!("Running {name}...");
                let generation = live.generation;
                let sender = self.agents.sender.clone();
                let id = call_id.to_owned();
                let started = re_tasks::spawn(
                    format!("rusteditor-agent-{name}"),
                    None,
                    move || Ok(re_agent::tools::run_worker(work, &cancel)),
                    move |result: anyhow::Result<ToolOutput>| {
                        let output = result.unwrap_or_else(|e| ToolOutput::error(format!("{e:#}")));
                        let _ = sender.send(AgentMessage::ToolDone {
                            key,
                            generation,
                            call_id: id,
                            output,
                        });
                    },
                );
                match started {
                    Ok(()) => Vec::new(),
                    Err(error) => self
                        .agents
                        .live_mut(key)
                        .map(|l| {
                            l.session.on_tool_result(
                                call_id,
                                ToolOutput::error(error.to_string()),
                                true,
                            )
                        })
                        .unwrap_or_default(),
                }
            }
            Action::EditBuffer { revision, edits } => {
                let tab_id = live.tab_id;
                let output = self.apply_agent_edit(tab_id, revision, &edits, cx);
                self.agents
                    .live_mut(key)
                    .map(|l| l.session.on_tool_result(call_id, output, false))
                    .unwrap_or_default()
            }
        }
    }

    /// Applies accepted `edit_buffer` edits as one undo step. If the user
    /// edited meanwhile, the edits are found again by their old text.
    #[inline(never)]
    fn apply_agent_edit(
        &mut self,
        tab_id: Option<u64>,
        revision: u64,
        edits: &[re_agent::tools::BufferEdit],
        cx: &mut Context<Self>,
    ) -> ToolOutput {
        let Some(index) = self.tabs.iter().position(|t| Some(t.id) == tab_id) else {
            return ToolOutput::error("the file is no longer open");
        };
        let active = index == self.active;
        let tab = &mut self.tabs[index];
        let TabState::Ready(doc) = &mut tab.state else {
            return ToolOutput::error("the file cannot be edited now (loading or too large)");
        };
        if doc.is_read_only() {
            return ToolOutput::error("the file is read-only");
        }
        let ranges = if doc.revision() == revision {
            edits
                .iter()
                .map(|e| (e.range.clone(), e.new_text.clone()))
                .collect()
        } else {
            match re_agent_tools::buffer::rebase(&doc.text(), edits) {
                Ok(ranges) => ranges,
                Err(error) => return ToolOutput::error(error),
            }
        };
        let before = doc.revision();
        let count = ranges.len();
        let Some(outcome) = tab.editor.replace_ranges(doc, ranges) else {
            return ToolOutput::error("the edit changed nothing");
        };
        let lines = doc.line_count();
        if active {
            self.after_edit(outcome, before);
        } else {
            let marks = std::mem::take(&mut tab.bookmarks);
            tab.bookmarks = shift_lines(marks, &outcome.lines).into_iter().collect();
        }
        self.status = "Agent edit applied (Undo reverts it)".into();
        cx.notify();
        ToolOutput::ok(format!(
            "Applied {count} edit(s). The file now has {lines} lines. The change is unsaved; the user can undo it."
        ))
    }

    /// Stops a live conversation; returns the session's effects.
    fn stop_live(&mut self, key: u64) -> Vec<Effect> {
        let Some(live) = self.agents.live_mut(key) else {
            return Vec::new();
        };
        live.generation += 1;
        if let Some(run) = live.run.take() {
            run.cancel();
        }
        if let Some(cancel) = live.cancel.take() {
            cancel.cancel();
        }
        live.action = None;
        live.activity.clear();
        live.session.cancel()
    }

    /// Drains worker messages (called from `poll`). Returns whether
    /// anything changed.
    #[inline(never)]
    pub(super) fn poll_agents(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(message) = self.agents.receiver.try_recv() {
            changed = true;
            match message {
                AgentMessage::ProviderAuth { result } => {
                    self.agents.settings.busy = false;
                    self.agents.settings.notice = result;
                }
                AgentMessage::Settings { id, result } => {
                    self.agents.settings.busy = false;
                    let success = result.is_ok();
                    self.agents.settings.notice = result.unwrap_or_else(|e| e);
                    self.agents.providers_loaded = false;
                    self.refresh_agents(cx);
                    if success && self.agents.providers.iter().any(|p| p.config.id == id) {
                        self.agents.default_provider = Some(id.clone());
                        if let Some(key) = self.shown_key()
                            && let Some(live) = self.agents.live_mut(key)
                            && live.session.is_idle()
                        {
                            live.provider = Some(id);
                        }
                    }
                }
                AgentMessage::Model {
                    key,
                    generation,
                    event,
                } => {
                    let effects = match self.agents.live_mut(key) {
                        Some(live) if live.generation == generation => {
                            if let Event::Error { message } = &event {
                                self.status = format!("Agent: {message}");
                            }
                            live.session.on_event(event)
                        }
                        _ => continue,
                    };
                    if !effects.is_empty() {
                        self.handle_effects(key, effects, cx);
                    } else {
                        self.agents.scroll.scroll_to_bottom();
                    }
                }
                AgentMessage::Prepared {
                    key,
                    generation,
                    call_id,
                    result,
                } => {
                    let effects = match self.agents.live_mut(key) {
                        Some(live) if live.generation == generation => {
                            let answer = match result {
                                Ok(prepared) => {
                                    let (approval, action) = re_agent::session::split(prepared);
                                    live.action = Some((call_id.clone(), action));
                                    Ok(approval)
                                }
                                Err(error) => Err(error),
                            };
                            live.session.on_prepared(&call_id, answer)
                        }
                        _ => continue,
                    };
                    self.handle_effects(key, effects, cx);
                }
                AgentMessage::ToolDone {
                    key,
                    generation,
                    call_id,
                    output,
                } => {
                    let host_effects = output.effects.clone();
                    let effects = match self.agents.live_mut(key) {
                        Some(live) if live.generation == generation => {
                            live.cancel = None;
                            live.session.on_tool_result(&call_id, output, false)
                        }
                        _ => continue,
                    };
                    for effect in host_effects {
                        self.host_effect(effect, cx);
                    }
                    self.handle_effects(key, effects, cx);
                }
            }
        }
        changed
    }

    pub(super) fn host_effect(&mut self, effect: HostEffect, cx: &mut Context<Self>) {
        match effect {
            HostEffect::ReloadPlugins => {
                let dir = cx.try_global::<PluginState>().and_then(|s| s.dir.clone());
                if let Some(dir) = dir {
                    let registry = re_plugin::Registry::load(&dir);
                    cx.default_global::<PluginState>().registry = registry;
                }
                self.refresh_agents(cx);
                let count = cx
                    .try_global::<PluginState>()
                    .map_or(0, |s| s.registry.plugins.len());
                self.status = format!("Extensions reloaded ({count} installed)");
            }
            HostEffect::FileChanged(path) => {
                let active = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.path.as_deref())
                    .is_some_and(|p| {
                        std::fs::canonicalize(p).ok().as_deref() == Some(path.as_path())
                    });
                if active {
                    self.check_disk(cx);
                }
            }
        }
    }

    // ----- Rendering -----

    #[inline(never)]
    pub(super) fn render_agent_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.agents.settings.open {
            return self.render_provider_settings(cx);
        }
        let live = self.shown_live();
        let typing = matches!(self.command, Some(CommandMode::Agent));
        let busy = live.is_some_and(|l| !l.session.is_idle());
        let mut header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .p_1()
            .bg(rgb(0xe8e8f4))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(live.map_or_else(|| "Agent".to_owned(), |l| l.def.name.clone())),
            )
            .child(Self::toolbar_button(
                "New",
                cx.listener(|this, _, window, cx| this.agent_new_conversation(window, cx)),
            ))
            .child(Self::toolbar_button(
                "History",
                cx.listener(|this, _, _, cx| {
                    this.agents.show_list = !this.agents.show_list;
                    this.refresh_list();
                    cx.notify();
                }),
            ));
        if busy {
            header = header.child(Self::toolbar_button(
                "Stop",
                cx.listener(|this, _, _, cx| this.agent_stop(cx)),
            ));
        }
        header = header.child(Self::toolbar_button(
            "x",
            cx.listener(|this, _, window, cx| this.agent_toggle_panel(window, cx)),
        ));
        let tab_has_doc = self.document().is_some();
        let picker = div().flex().flex_wrap().gap_1().p_1().children(
            self.agents
                .defs
                .iter()
                .filter(|d| d.global || tab_has_doc)
                .map(|d| {
                    let id = d.id.clone();
                    let selected = live.is_some_and(|l| l.def.id == d.id);
                    div()
                        .id(gpui::SharedString::from(format!("agent-pick-{}", d.id)))
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(rgb(0xc8c8c8))
                        .bg(if selected {
                            rgb(0xd0e0ff)
                        } else {
                            rgb(0xf9f9f9)
                        })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.show_agent(&id, cx)))
                        .child(d.name.clone())
                }),
        );
        let mut body = div()
            .id("agent-transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.agents.scroll)
            .p_1()
            .flex()
            .flex_col()
            .gap_1();
        if self.agents.show_list {
            body = body
                .child("Conversations:")
                .children(self.agents.list.iter().map(|s| {
                    let open_id = s.id.clone();
                    let delete_id = s.id.clone();
                    let agent = self
                        .agents
                        .def(&s.agent)
                        .map_or(s.agent.as_str(), |d| d.name.as_str());
                    div()
                        .flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .id(gpui::SharedString::from(format!("conv-{}", s.id)))
                                .flex_1()
                                .min_w_0()
                                .px_1()
                                .cursor_pointer()
                                .bg(rgb(0xf4f4f4))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.open_saved(&open_id, cx)
                                    }),
                                )
                                .child(format!("{} ({agent}, {} messages)", s.title, s.messages)),
                        )
                        .child(
                            div()
                                .id(gpui::SharedString::from(format!("conv-del-{}", s.id)))
                                .px_1()
                                .cursor_pointer()
                                .text_color(rgb(0xa00000))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_saved(&delete_id, cx)
                                }))
                                .child("Delete"),
                        )
                }));
            if self.agents.list.is_empty() {
                body = body.child(div().text_color(rgb(0x808080)).child("(none saved yet)"));
            }
        } else if let Some(live) = live {
            let messages = &live.session.conversation.messages;
            let skip = messages.len().saturating_sub(SHOWN_MESSAGES);
            if skip > 0 {
                body = body.child(
                    div()
                        .text_color(rgb(0x808080))
                        .child(format!("({skip} earlier messages not shown)")),
                );
            }
            for message in &messages[skip..] {
                body = body.child(render_message(message));
            }
            if let Some(text) = live.session.streaming_text() {
                body = body.child(text_block(if text.is_empty() { "..." } else { text }));
            }
            if let Some((call, approval)) = live.session.pending_approval() {
                body = body.child(self.approval_card(call, approval, cx));
            }
            if self
                .agents
                .consent
                .as_ref()
                .is_some_and(|c| c.key == live.key)
            {
                body = body.child(self.consent_card(cx));
            }
            if messages.is_empty() && live.session.is_idle() {
                body = body.child(div().text_color(rgb(0x808080)).child(if live.def.global {
                    format!("Ask the {} for something.", live.def.name)
                } else {
                    format!(
                        "Ask about {} or ask for changes. You approve every change.",
                        self.document()
                            .map_or_else(|| "this file".to_owned(), Document::display_name)
                    )
                }));
            }
        } else {
            body = body.child("Open a file, or pick an agent above.");
        }
        let provider = live
            .and_then(|l| self.agents.provider(l.provider.as_deref()))
            .or_else(|| self.agents.provider(None))
            .map(|p| format!("{} · {}", p.config.name, p.config.model));
        let footer = div()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .text_color(rgb(0x606060))
            .child(
                div().flex_1().min_w_0().child(
                    live.map(|l| l.activity.clone())
                        .filter(|a| !a.is_empty())
                        .unwrap_or_else(|| {
                            let tokens = live.map_or((0, 0), |l| l.session.usage);
                            match (provider.clone(), tokens) {
                                (None, _) => "No provider: Agent > Providers...".to_owned(),
                                (Some(p), (0, 0)) => p,
                                (Some(p), (i, o)) => format!("{p} · {i} in / {o} out tokens"),
                            }
                        }),
                ),
            )
            .child(Self::toolbar_button(
                "Next Provider",
                cx.listener(|this, _, _, cx| this.agent_cycle_provider(cx)),
            ));
        let input = div()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .bg(rgb(0xe8f2ff))
            .child(if typing {
                self.input_widget(cx).into_any_element()
            } else {
                div()
                    .id("agent-input-placeholder")
                    .flex_1()
                    .px_1()
                    .bg(rgb(0xffffff))
                    .text_color(rgb(0x909090))
                    .cursor(CursorStyle::IBeam)
                    .on_click(cx.listener(|this, _, window, cx| this.agent_focus_input(window, cx)))
                    .child("Type a message...")
                    .into_any_element()
            })
            .child(Self::toolbar_button(
                "Send",
                cx.listener(|this, _, window, cx| {
                    if matches!(this.command, Some(CommandMode::Agent)) {
                        this.finish_command(window, cx);
                    } else {
                        this.agent_focus_input(window, cx);
                    }
                }),
            ));
        div()
            .flex()
            .flex_col()
            .w(px(PANEL_WIDTH))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(rgb(0xc8c8c8))
            .bg(rgb(0xfcfcfc))
            .font_family("Segoe UI")
            .text_size(px(13.))
            .child(header)
            .child(picker)
            .child(body)
            .child(footer)
            .child(input)
            .into_any_element()
    }

    #[inline(never)]
    fn approval_card(
        &self,
        call: &ToolCall,
        approval: &Approval,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .border_1()
            .border_color(rgb(0xd0a000))
            .bg(rgb(0xfff8e0))
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(approval.title.clone()),
            )
            .child(div().text_color(rgb(0x606060)).child(format!(
                "{} · {}",
                call.name,
                approval.risk.label()
            )));
        if !approval.detail.is_empty() {
            card = card.child(text_block(&approval.detail));
        }
        if !approval.diff.is_empty() {
            let mut diff = div()
                .flex()
                .flex_col()
                .font_family("Consolas")
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xe0e0e0));
            for line in approval.diff.iter().take(DIFF_LINES) {
                let (sign, bg) = match line.kind {
                    DiffKind::Added => ("+", rgb(0xe6ffe6)),
                    DiffKind::Removed => ("-", rgb(0xffe6e6)),
                    DiffKind::Context => (" ", rgb(0xffffff)),
                    DiffKind::Skip => ("", rgb(0xf0f0f0)),
                };
                let number = line
                    .line
                    .map_or_else(|| "    ".to_owned(), |n| format!("{n:>4}"));
                let text: String = line.text.chars().take(200).collect();
                diff = diff.child(
                    div()
                        .bg(bg)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(format!("{number} {sign} {text}")),
                );
            }
            if approval.diff.len() > DIFF_LINES {
                diff = diff.child(format!(
                    "... {} more lines",
                    approval.diff.len() - DIFF_LINES
                ));
            }
            card = card.child(diff);
        }
        let (accept, reject) = match approval.risk {
            re_agent::Risk::ReadOutsideScope => ("Allow", "Deny"),
            re_agent::Risk::Process => ("Run", "Don't run"),
            re_agent::Risk::Install => ("Install", "Cancel"),
            _ => ("Accept", "Reject"),
        };
        card.child(
            div()
                .flex()
                .gap_1()
                .child(Self::toolbar_button(
                    accept,
                    cx.listener(|this, _, _, cx| this.agent_answer(true, cx)),
                ))
                .child(Self::toolbar_button(
                    reject,
                    cx.listener(|this, _, _, cx| this.agent_answer(false, cx)),
                )),
        )
        .into_any_element()
    }

    #[inline(never)]
    fn consent_card(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(consent) = &self.agents.consent else {
            return div().into_any_element();
        };
        let (name, destination) = self
            .agents
            .providers
            .iter()
            .find(|p| p.config.id == consent.provider)
            .map(|p| (p.config.name.clone(), p.config.destination.clone()))
            .unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .border_1()
            .border_color(rgb(0xd0a000))
            .bg(rgb(0xfff8e0))
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(format!("Send to {name}?")),
            )
            .child(text_block(&format!(
                "This conversation, including text the agent reads from your files, is sent to \
                 {destination}. Allowed once per provider; Plugins > Forget Approvals revokes."
            )))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(Self::toolbar_button(
                        "Allow",
                        cx.listener(|this, _, _, cx| this.agent_consent(true, cx)),
                    ))
                    .child(Self::toolbar_button(
                        "Don't send",
                        cx.listener(|this, _, _, cx| this.agent_consent(false, cx)),
                    )),
            )
            .into_any_element()
    }
}

/// Text with its line breaks.
#[inline(never)]
fn text_block(text: &str) -> gpui::Div {
    div().flex().flex_col().children(text.lines().map(|line| {
        div().child(if line.is_empty() {
            " ".to_owned()
        } else {
            line.to_owned()
        })
    }))
}

#[inline(never)]
fn render_message(message: &Message) -> gpui::AnyElement {
    match message {
        Message::User { text } => div()
            .p_1()
            .bg(rgb(0xe3ecff))
            .child(text_block(text))
            .into_any_element(),
        Message::Assistant { text, tool_calls } => div()
            .flex()
            .flex_col()
            .when(!text.is_empty(), |d| d.child(text_block(text)))
            .children(tool_calls.iter().map(|call| {
                let args = call.arguments.to_string();
                let args: String = args.chars().take(120).collect();
                div()
                    .text_color(rgb(0x707070))
                    .child(format!("-> {} {args}", call.name))
            }))
            .into_any_element(),
        Message::Tool {
            name,
            content,
            is_error,
            ..
        } => {
            let first = content.lines().next().unwrap_or_default();
            let first: String = first.chars().take(120).collect();
            let more = content.lines().count().saturating_sub(1);
            div()
                .text_color(if *is_error {
                    rgb(0xa00000)
                } else {
                    rgb(0x307030)
                })
                .child(format!(
                    "{} {name}: {first}{}",
                    if *is_error { "x" } else { "ok" },
                    if more > 0 {
                        format!(" (+{more} lines)")
                    } else {
                        String::new()
                    }
                ))
                .into_any_element()
        }
        Message::Notice { text } => div()
            .text_color(rgb(0x808080))
            .italic()
            .child(text.clone())
            .into_any_element(),
    }
}
