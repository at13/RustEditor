//! Tools contributed by plugins (`"tools"` in plugin.json).

use std::sync::Arc;

use re_agent::{
    Action, Approval, Prepared, Remember, Risk, Tool, ToolContext, ToolOutput, ToolSpec,
};
use re_plugin::{Plugin, ToolDecl};
use serde_json::Value;

use crate::cap;

/// Name the model sees: `<plugin-id>__<tool>`.
pub fn tool_name(plugin: &Plugin, tool: &ToolDecl) -> String {
    format!("{}__{}", plugin.manifest.id, tool.name)
}

pub struct PluginTool {
    plugin: Arc<Plugin>,
    decl: ToolDecl,
}

/// Every tool contributed by the loaded plugins.
pub fn plugin_tools(registry: &re_plugin::Registry) -> Vec<Arc<dyn Tool>> {
    registry
        .plugins
        .iter()
        .flat_map(|plugin| {
            plugin.manifest.tools.iter().map(|decl| {
                Arc::new(PluginTool {
                    plugin: plugin.clone(),
                    decl: decl.clone(),
                }) as Arc<dyn Tool>
            })
        })
        .collect()
}

impl PluginTool {
    fn risk(&self) -> Risk {
        match self.decl.risk.as_str() {
            "write" => Risk::Write,
            "process" => Risk::Process,
            "network" => Risk::Network,
            _ if self.plugin.manifest.kind.is_sandboxed() => Risk::Read,
            // A program reading data still runs with full access.
            _ => Risk::Process,
        }
    }
}

impl Tool for PluginTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: tool_name(&self.plugin, &self.decl),
            description: format!(
                "{} (from extension {})",
                self.decl.description, self.plugin.manifest.name
            ),
            input_schema: self.decl.input_schema.clone(),
            risk: self.risk(),
        }
    }

    fn prepare(&self, _: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let plugin = self.plugin.clone();
        let name = self.decl.name.clone();
        let shown = args_text(args);
        let args = args.clone();
        let action = Action::Worker(Box::new(move |_| {
            match re_plugin::call_tool(&plugin, &name, &args) {
                Ok(value) => {
                    let text = match value {
                        Value::String(s) => s,
                        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
                    };
                    ToolOutput::ok(cap(text, "the tool returned a lot"))
                }
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }));
        let manifest = &self.plugin.manifest;
        let risk = self.risk();
        let (title, remember) = if risk == Risk::Read {
            // Same approval as running the plugin's commands; asked once.
            (
                format!(
                    "Let agents use \"{}\" {} (sandboxed: it only computes on the arguments)",
                    manifest.name, manifest.version
                ),
                Remember::Persistent(self.plugin.approval_key()),
            )
        } else {
            (
                format!(
                    "Run tool `{}` of \"{}\": it runs a program with your full access ({})",
                    self.decl.name,
                    manifest.name,
                    risk.label()
                ),
                Remember::Once,
            )
        };
        Ok(Prepared::Ask(
            Approval {
                title,
                detail: format!("Arguments: {}", args_preview(&shown)),
                diff: Vec::new(),
                risk,
                remember,
            },
            action,
        ))
    }
}

fn args_text(args: &Value) -> String {
    serde_json::to_string(args).unwrap_or_default()
}

fn args_preview(text: &str) -> String {
    text.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    #[test]
    fn contributed_tools_are_named_and_approved_by_trust() {
        let root = temp_dir("plugtools");
        let dir = root.join("wc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"api":0,"id":"wc","name":"Word Count","version":"1.0.0","kind":"process",
                "command":["prog"],"commands":[{"id":"c","title":"C"}],
                "tools":[{"name":"count","description":"Counts words","input_schema":{"type":"object"}},
                         {"name":"post","description":"Posts","input_schema":{"type":"object"},"risk":"network"}]}"#,
        )
        .unwrap();
        let registry = re_plugin::Registry::load(&root);
        assert!(registry.errors.is_empty(), "{:?}", registry.errors);
        let tools = plugin_tools(&registry);
        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names, ["wc__count", "wc__post"]);
        // A process plugin's "read" tool still runs a program: Process risk.
        assert_eq!(tools[0].spec().risk, Risk::Process);
        let Prepared::Ask(approval, _) = tools[1]
            .prepare(&ToolContext::default(), &serde_json::json!({}))
            .unwrap()
        else {
            panic!("ask");
        };
        assert_eq!(approval.risk, Risk::Network);
        assert_eq!(approval.remember, Remember::Once);
        std::fs::remove_dir_all(root).unwrap();
    }
}
