//! The Extension Builder's tools. Work happens in a staging folder per
//! extension; nothing affects the editor until `ext_install`, which always
//! asks the user. Installed extensions still need the normal plugin
//! approval before they run.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use re_agent::{
    Action, Approval, Prepared, Remember, Risk, Tool, ToolContext, ToolOutput, ToolSpec,
    diff::line_diff,
    tools::{ExtEnv, HostEffect},
};
use re_plugin::{Kind, Plugin};
use serde_json::{Value, json};

use crate::cap;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_FILES: usize = 200;
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);
const HISTORY: &str = ".history";

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ExtList),
        Arc::new(ExtRead),
        Arc::new(ExtWrite),
        Arc::new(ExtValidate),
        Arc::new(ExtToolchain),
        Arc::new(ExtBuild),
        Arc::new(ExtTest),
        Arc::new(ExtInstall),
        Arc::new(ExtRollback),
    ]
}

fn env(ctx: &ToolContext) -> Result<&ExtEnv, String> {
    ctx.ext
        .as_ref()
        .ok_or_else(|| "only the Extension Builder can use this tool".into())
}

fn id_arg(args: &Value) -> Result<String, String> {
    let id = args.get("id").and_then(Value::as_str).unwrap_or_default();
    let valid = !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if valid {
        Ok(id.to_owned())
    } else {
        Err("`id` must be 1-40 chars of a-z, 0-9, '-' or '_'".into())
    }
}

fn id_schema(extra: Value) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {"id": {"type": "string", "description": "Extension id (folder name)."}},
        "required": ["id"],
        "additionalProperties": false
    });
    if let Value::Object(extra) = extra {
        for (k, v) in extra {
            if k == "required" {
                for r in v.as_array().into_iter().flatten() {
                    schema["required"]
                        .as_array_mut()
                        .expect("array")
                        .push(r.clone());
                }
            } else {
                schema["properties"][k] = v;
            }
        }
    }
    schema
}

fn spec(name: &str, description: &str, schema: Value, risk: Risk) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        risk,
    }
}

fn run_now(f: impl FnOnce() -> Result<String, String> + Send + 'static) -> Prepared {
    Prepared::Run(Action::Worker(Box::new(move |_| match f() {
        Ok(text) => ToolOutput::ok(cap(text, "")),
        Err(e) => ToolOutput::error(e),
    })))
}

/// Copies a folder, skipping build output and history.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == HISTORY {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Removes everything in `dir` except history.
fn clear_except_history(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == HISTORY {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn files_in(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if name == "target" || name == HISTORY {
                continue;
            }
            let path = entry.path();
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(path),
                Ok(m) => out.push((
                    path.strip_prefix(dir).unwrap_or(&path).to_path_buf(),
                    m.len(),
                )),
                Err(_) => {}
            }
        }
    }
    out.sort();
    out
}

/// What an extension can access, in words, for summaries.
pub fn access_summary(plugin: &Plugin) -> String {
    match &plugin.manifest.kind {
        Kind::Recipe => "Recipe: only the text it is given (runs in the editor).".into(),
        Kind::Wasm { .. } => format!(
            "WebAssembly (sandboxed): only the text it is given{}; no files, network or programs.",
            if plugin.has(re_plugin::Capability::Document) {
                " (may be the whole file)"
            } else {
                ""
            }
        ),
        Kind::Agent(def) => format!(
            "Agent with tools: {}. Every write is still shown for approval.",
            if def.tools.is_empty() {
                "none".to_owned()
            } else {
                def.tools.join(", ")
            }
        ),
        Kind::Process { argv, .. } => format!(
            "Program `{}`: FULL ACCESS to your files{} (not sandboxed).",
            argv.join(" "),
            if plugin.has(re_plugin::Capability::Network) {
                " and the network"
            } else {
                ""
            }
        ),
    }
}

/// Loads and checks a staged extension.
fn validate(env: &ExtEnv, id: &str) -> Result<(Plugin, Vec<String>), String> {
    let dir = env.staging_dir.join(id);
    if !dir.is_dir() {
        return Err(format!(
            "nothing staged for `{id}`; write files with ext_write first"
        ));
    }
    let plugin = Plugin::load(&dir).map_err(|e| format!("{e:#}"))?;
    if plugin.manifest.id != id {
        return Err(format!(
            "plugin.json has id `{}` but the folder is `{id}`",
            plugin.manifest.id
        ));
    }
    let mut warnings = Vec::new();
    if let Kind::Agent(def) = &plugin.manifest.kind {
        let unknown: Vec<&String> = def
            .tools
            .iter()
            .filter(|t| !env.known_tools.contains(t))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown tools {unknown:?}; available: {}",
                env.known_tools.join(", ")
            ));
        }
    }
    let installed = env.plugins_dir.join(id);
    if let Ok(current) = Plugin::load(&installed)
        && current.manifest.version == plugin.manifest.version
        && current.manifest.fingerprint != plugin.manifest.fingerprint
    {
        warnings.push(format!(
            "version {} is already installed with different content; increase `version`",
            current.manifest.version
        ));
    }
    Ok((plugin, warnings))
}

struct ExtList;
impl Tool for ExtList {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_list",
            "List installed extensions (id, name, version, kind, commands, tools), staged work, and saved versions.",
            json!({"type": "object", "additionalProperties": false}),
            Risk::Read,
        )
    }
    fn prepare(&self, ctx: &ToolContext, _: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        Ok(run_now(move || {
            let mut out = String::from("Installed:\n");
            let registry = re_plugin::Registry::load(&env.plugins_dir);
            for p in &registry.plugins {
                let m = &p.manifest;
                let history: Vec<String> = std::fs::read_dir(p.dir.join(HISTORY))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                out.push_str(&format!(
                    "- {} \"{}\" {} ({}) commands: {} tools: {}{}\n",
                    m.id,
                    m.name,
                    m.version,
                    m.kind.name(),
                    m.commands
                        .iter()
                        .map(|c| c.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    m.tools
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if history.is_empty() {
                        String::new()
                    } else {
                        format!(" saved versions: {}", history.join(", "))
                    }
                ));
            }
            for e in &registry.errors {
                out.push_str(&format!("- (broken) {e}\n"));
            }
            out.push_str("Staged (not installed):\n");
            for entry in std::fs::read_dir(&env.staging_dir)
                .into_iter()
                .flatten()
                .flatten()
            {
                out.push_str(&format!("- {}\n", entry.file_name().to_string_lossy()));
            }
            out.push_str(&format!(
                "Tools agents may use: {}\n",
                env.known_tools.join(", ")
            ));
            Ok(out)
        }))
    }
}

struct ExtRead;
impl Tool for ExtRead {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_read",
            "Read a file of an extension (the staged copy if there is one, else the installed one). \
             Without `path`, lists its files.",
            id_schema(json!({"path": {"type": "string"}})),
            Risk::Read,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(run_now(move || {
            let staged = env.staging_dir.join(&id);
            let dir = if staged.is_dir() {
                staged
            } else {
                env.plugins_dir.join(&id)
            };
            if !dir.is_dir() {
                return Err(format!("no extension `{id}`"));
            }
            if path.is_empty() {
                return Ok(files_in(&dir)
                    .into_iter()
                    .map(|(p, size)| format!("{} ({size} bytes)\n", p.display()))
                    .collect());
            }
            let file = re_plugin::inside(&dir, &path).map_err(|e| e.to_string())?;
            let bytes = std::fs::read(&file).map_err(|e| format!("cannot read {path}: {e}"))?;
            if bytes.len() > MAX_FILE_BYTES {
                return Err("the file is larger than 1 MB".into());
            }
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }))
    }
}

struct ExtWrite;
impl Tool for ExtWrite {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_write",
            "Create or replace a file in the staging folder of an extension (starting from the installed \
             copy if it exists). Does not affect the editor until ext_install.",
            id_schema(json!({
                "path": {"type": "string", "minLength": 1},
                "content": {"type": "string"},
                "required": ["path", "content"]
            })),
            Risk::Read,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        let path = args["path"].as_str().unwrap_or_default().to_owned();
        let content = args["content"].as_str().unwrap_or_default().to_owned();
        if content.len() > MAX_FILE_BYTES {
            return Err("files are limited to 1 MB".into());
        }
        Ok(run_now(move || {
            let staged = env.staging_dir.join(&id);
            if !staged.is_dir() {
                let installed = env.plugins_dir.join(&id);
                if installed.is_dir() {
                    copy_tree(&installed, &staged).map_err(|e| e.to_string())?;
                } else {
                    std::fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
                }
            }
            if files_in(&staged).len() >= MAX_FILES {
                return Err(format!("an extension may have at most {MAX_FILES} files"));
            }
            let file = re_plugin::inside(&staged, &path).map_err(|e| e.to_string())?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&file, &content).map_err(|e| e.to_string())?;
            Ok(format!("Staged {id}/{path} ({} bytes)", content.len()))
        }))
    }
}

struct ExtValidate;
impl Tool for ExtValidate {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_validate",
            "Check the staged extension exactly as the editor will load it (manifest, referenced files, \
             agent tools). Always run before testing or installing.",
            id_schema(json!({})),
            Risk::Read,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        Ok(run_now(move || {
            let (plugin, warnings) = validate(&env, &id)?;
            let m = &plugin.manifest;
            let mut out = format!(
                "OK: {} \"{}\" {} ({})\nCommands: {}\n{}\n",
                m.id,
                m.name,
                m.version,
                m.kind.name(),
                m.commands
                    .iter()
                    .map(|c| format!("{} \"{}\"", c.id, c.title))
                    .collect::<Vec<_>>()
                    .join(", "),
                access_summary(&plugin)
            );
            for w in warnings {
                out.push_str(&format!("Warning: {w}\n"));
            }
            Ok(out)
        }))
    }
}

/// `--version` of a program, or `None`.
fn version_of(program: &str, args: &[&str]) -> Option<String> {
    let argv: Vec<String> = std::iter::once(program)
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    let out = crate::process::run(
        &argv,
        Path::new("."),
        Duration::from_secs(20),
        &Default::default(),
    );
    (!out.is_error).then(|| {
        out.content
            .lines()
            .skip_while(|l| !l.starts_with("--- stdout"))
            .nth(1)
            .unwrap_or_default()
            .trim()
            .to_owned()
    })
}

struct ExtToolchain;
impl Tool for ExtToolchain {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_toolchain",
            "Report which build tools exist here: Rust (cargo) with the wasm32-unknown-unknown target, Python, Node.",
            json!({"type": "object", "additionalProperties": false}),
            Risk::Read,
        )
    }
    fn prepare(&self, ctx: &ToolContext, _: &Value) -> Result<Prepared, String> {
        env(ctx)?;
        Ok(run_now(|| {
            let cargo = version_of("cargo", &["--version"]);
            let wasm = version_of("rustup", &["target", "list", "--installed"]);
            let wasm_ok = wasm.is_some()
                && crate::process::run(
                    &[
                        "rustup".into(),
                        "target".into(),
                        "list".into(),
                        "--installed".into(),
                    ],
                    Path::new("."),
                    Duration::from_secs(20),
                    &Default::default(),
                )
                .content
                .contains("wasm32-unknown-unknown");
            let python = version_of("python3", &["--version"])
                .or_else(|| version_of("python", &["--version"]));
            let node = version_of("node", &["--version"]);
            let line = |name: &str, v: Option<String>| {
                format!("{name}: {}\n", v.unwrap_or_else(|| "not found".into()))
            };
            Ok(format!(
                "{}wasm32-unknown-unknown target: {}\n{}{}Recipes need no build tools.",
                line("cargo", cargo),
                if wasm_ok {
                    "installed"
                } else {
                    "not installed (rustup target add wasm32-unknown-unknown)"
                },
                line("python", python),
                line("node", node)
            ))
        }))
    }
}

struct ExtBuild;
impl Tool for ExtBuild {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_build",
            "Build a staged WebAssembly extension written in Rust (cargo build --release --target \
             wasm32-unknown-unknown) and copy the .wasm to the manifest's `module`. Returns compiler output. \
             The user approves each build.",
            id_schema(json!({})),
            Risk::Process,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        let dir = env.staging_dir.join(&id);
        if !dir.join("Cargo.toml").is_file() {
            return Err(format!("{id} has no Cargo.toml in staging"));
        }
        let module = std::fs::read_to_string(dir.join("plugin.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["module"].as_str().map(str::to_owned))
            .ok_or("plugin.json must exist and name the `module` to build")?;
        let target = re_plugin::inside(&dir, &module).map_err(|e| e.to_string())?;
        Ok(Prepared::Ask(
            Approval {
                title: format!("Build extension `{id}`"),
                detail:
                    "Runs the Rust compiler on the staged code. Build scripts and macros in it run \
                         with your full access, so only approve code you expect."
                        .into(),
                diff: Vec::new(),
                risk: Risk::Process,
                remember: Remember::Once,
            },
            Action::Worker(Box::new(move |cancel| {
                let argv: Vec<String> = [
                    "cargo",
                    "build",
                    "--release",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--target-dir",
                    "target",
                ]
                .map(str::to_owned)
                .to_vec();
                let mut out = crate::process::run(&argv, &dir, BUILD_TIMEOUT, cancel);
                if out.is_error {
                    return out;
                }
                let release = dir.join("target/wasm32-unknown-unknown/release");
                let built = std::fs::read_dir(&release)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| p.extension().is_some_and(|e| e == "wasm"));
                match built {
                    Some(built) => match std::fs::copy(&built, &target) {
                        Ok(bytes) => {
                            out.content =
                                format!("Built {} ({bytes} bytes).\n{}", module, out.content)
                        }
                        Err(e) => return ToolOutput::error(format!("cannot copy the module: {e}")),
                    },
                    None => {
                        return ToolOutput::error(format!(
                            "no .wasm produced; is crate-type = [\"cdylib\"]?\n{}",
                            out.content
                        ));
                    }
                }
                out
            })),
        ))
    }
}

struct ExtTest;
impl Tool for ExtTest {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_test",
            "Run one command of the staged extension on sample inputs through the real plugin host (same \
             sandbox and limits). Returns the outputs or the error.",
            id_schema(json!({
                "command": {"type": "string"},
                "inputs": {"type": "array", "items": {"type": "string"}},
                "required": ["command", "inputs"]
            })),
            Risk::Read,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        let (plugin, _) = validate(&env, &id)?;
        let command_id = args["command"].as_str().unwrap_or_default().to_owned();
        let command = plugin
            .manifest
            .commands
            .iter()
            .find(|c| c.id == command_id)
            .cloned()
            .ok_or_else(|| format!("no command `{command_id}`"))?;
        if matches!(plugin.manifest.kind, Kind::Agent(_)) {
            return Err("agents are tried by opening them after ext_install".into());
        }
        let inputs: Vec<String> = args["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let sandboxed = plugin.manifest.kind.is_sandboxed();
        let name = plugin.manifest.name.clone();
        let action = Action::Worker(Box::new(move |_| {
            let request = re_plugin::Request { inputs, path: None };
            match re_plugin::run(&plugin, &command, &request) {
                Ok(re_plugin::Outcome::Replace { outputs, status }) => ToolOutput::ok(cap(
                    format!(
                        "outputs: {}{}",
                        serde_json::to_string_pretty(&outputs).unwrap_or_default(),
                        status.map(|s| format!("\nstatus: {s}")).unwrap_or_default()
                    ),
                    "",
                )),
                Ok(re_plugin::Outcome::Show { text, .. }) => {
                    ToolOutput::ok(cap(format!("shows:\n{text}"), ""))
                }
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }));
        Ok(if sandboxed {
            Prepared::Run(action)
        } else {
            Prepared::Ask(
                Approval {
                    title: format!("Test-run \"{name}\" (a program with your full access)"),
                    detail: "The staged program runs on the sample inputs.".into(),
                    diff: Vec::new(),
                    risk: Risk::Process,
                    remember: Remember::Once,
                },
                action,
            )
        })
    }
}

struct ExtInstall;
impl Tool for ExtInstall {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_install",
            "Install the staged extension (the user sees a summary and approves). The previous version is \
             kept for ext_rollback. The editor then asks the user before the extension first runs.",
            id_schema(
                json!({"summary": {"type": "string", "description": "What it does, for the user."}}),
            ),
            Risk::Install,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        let (plugin, warnings) = validate(&env, &id)?;
        if !warnings.is_empty() {
            return Err(warnings.join("; "));
        }
        let staged = env.staging_dir.join(&id);
        let installed = env.plugins_dir.join(&id);
        let current = Plugin::load(&installed).ok();
        let old_manifest =
            std::fs::read_to_string(installed.join("plugin.json")).unwrap_or_default();
        let new_manifest = std::fs::read_to_string(staged.join("plugin.json")).unwrap_or_default();
        let files: Vec<String> = files_in(&staged)
            .into_iter()
            .map(|(p, size)| format!("{} ({size} bytes)", p.display()))
            .collect();
        let m = &plugin.manifest;
        let title = match &current {
            Some(c) => format!(
                "Update extension \"{}\" {} -> {}",
                m.name, c.manifest.version, m.version
            ),
            None => format!("Install extension \"{}\" {}", m.name, m.version),
        };
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let detail = format!(
            "{summary}\n{}\nFiles: {}\nYou will be asked again before it first runs.",
            access_summary(&plugin),
            files.join(", ")
        );
        let previous = current.map(|c| c.manifest.version);
        Ok(Prepared::Ask(
            Approval {
                title,
                detail: detail.trim().to_owned(),
                diff: line_diff(&old_manifest, &new_manifest, 3, 1),
                risk: Risk::Install,
                remember: Remember::Once,
            },
            Action::Worker(Box::new(move |_| {
                let result = (|| -> std::io::Result<()> {
                    if let Some(version) = &previous {
                        let keep = installed.join(HISTORY).join(sanitize(version));
                        if keep.exists() {
                            std::fs::remove_dir_all(&keep)?;
                        }
                        copy_tree(&installed, &keep)?;
                        clear_except_history(&installed)?;
                    }
                    copy_tree(&staged, &installed)?;
                    std::fs::remove_dir_all(&staged)
                })();
                match result {
                    Ok(()) => ToolOutput {
                        content: format!(
                            "Installed {id}. It appears in the Command Palette and the Plugins menu; the user \
                             approves it on first run."
                        ),
                        is_error: false,
                        effects: vec![HostEffect::ReloadPlugins],
                    },
                    Err(e) => ToolOutput::error(format!("install failed: {e}")),
                }
            })),
        ))
    }
}

fn sanitize(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

struct ExtRollback;
impl Tool for ExtRollback {
    fn spec(&self) -> ToolSpec {
        spec(
            "ext_rollback",
            "Restore a previously installed version of an extension (see ext_list for saved versions).",
            id_schema(json!({"version": {"type": "string"}, "required": ["version"]})),
            Risk::Install,
        )
    }
    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let env = env(ctx)?.clone();
        let id = id_arg(args)?;
        let version = sanitize(args["version"].as_str().unwrap_or_default());
        let installed = env.plugins_dir.join(&id);
        let saved = installed.join(HISTORY).join(&version);
        if !saved.join("plugin.json").is_file() {
            return Err(format!("no saved version {version} of {id}"));
        }
        let current = Plugin::load(&installed).map_err(|e| format!("{e:#}"))?;
        Ok(Prepared::Ask(
            Approval {
                title: format!(
                    "Roll back \"{}\" {} -> {version}",
                    current.manifest.name, current.manifest.version
                ),
                detail: "The current version is kept and can be restored the same way.".into(),
                diff: Vec::new(),
                risk: Risk::Install,
                remember: Remember::Once,
            },
            Action::Worker(Box::new(move |_| {
                let result = (|| -> std::io::Result<()> {
                    let keep = installed
                        .join(HISTORY)
                        .join(sanitize(&current.manifest.version));
                    if !keep.exists() {
                        copy_tree(&installed, &keep)?;
                    }
                    clear_except_history(&installed)?;
                    copy_tree(&saved, &installed)
                })();
                match result {
                    Ok(()) => ToolOutput {
                        content: format!("Restored {id} {version}."),
                        is_error: false,
                        effects: vec![HostEffect::ReloadPlugins],
                    },
                    Err(e) => ToolOutput::error(format!("rollback failed: {e}")),
                }
            })),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{run, temp_dir};

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            ext: Some(ExtEnv {
                plugins_dir: root.join("plugins"),
                staging_dir: root.join("staging"),
                known_tools: vec!["read_buffer".into(), "edit_buffer".into()],
            }),
            ..Default::default()
        }
    }

    fn call(c: &ToolContext, tool: &dyn Tool, args: Value) -> ToolOutput {
        run(tool
            .prepare(c, &args)
            .unwrap_or_else(|e| panic!("{}: {e}", tool.spec().name)))
    }

    const RECIPE: &str = r#"{"api":0,"id":"tidy","name":"Tidy","version":"1.0.0","kind":"recipe",
        "commands":[{"id":"run","title":"Tidy Lines","steps":["trim_trailing_whitespace","sort_lines"]}]}"#;

    #[test]
    fn create_test_install_modify_and_roll_back_a_recipe() {
        let root = temp_dir("ext");
        let c = ctx(&root);
        assert!(
            ExtList
                .prepare(&ToolContext::default(), &json!({}))
                .unwrap_err()
                .contains("Extension Builder")
        );
        call(
            &c,
            &ExtWrite,
            json!({"id": "tidy", "path": "plugin.json", "content": RECIPE}),
        );
        let out = call(&c, &ExtValidate, json!({"id": "tidy"}));
        assert!(out.content.starts_with("OK: tidy"), "{}", out.content);
        assert!(out.content.contains("only the text it is given"));
        let out = call(
            &c,
            &ExtTest,
            json!({"id": "tidy", "command": "run", "inputs": ["b \na\n"]}),
        );
        assert!(out.content.contains("a\\nb\\n"), "{}", out.content);
        // Install always asks; it reloads plugins and clears staging.
        let prepared = ExtInstall
            .prepare(&c, &json!({"id": "tidy", "summary": "Sorts"}))
            .unwrap();
        let Prepared::Ask(approval, _) = &prepared else {
            panic!("ask");
        };
        assert_eq!(approval.risk, Risk::Install);
        assert!(
            approval
                .title
                .starts_with("Install extension \"Tidy\" 1.0.0")
        );
        let out = run(prepared);
        assert_eq!(out.effects, vec![HostEffect::ReloadPlugins]);
        assert!(root.join("plugins/tidy/plugin.json").is_file());
        assert!(!root.join("staging/tidy").exists());
        // Modify: staging starts from the installed copy; same version with
        // different content is refused until the version changes.
        let changed = RECIPE.replace("sort_lines", "unique_lines");
        call(
            &c,
            &ExtWrite,
            json!({"id": "tidy", "path": "plugin.json", "content": changed}),
        );
        assert!(
            ExtInstall
                .prepare(&c, &json!({"id": "tidy"}))
                .unwrap_err()
                .contains("increase `version`")
        );
        call(
            &c,
            &ExtWrite,
            json!({"id": "tidy", "path": "plugin.json", "content": changed.replace("1.0.0", "1.1.0")}),
        );
        let prepared = ExtInstall.prepare(&c, &json!({"id": "tidy"})).unwrap();
        let Prepared::Ask(approval, _) = &prepared else {
            panic!("ask")
        };
        assert!(approval.title.contains("1.0.0 -> 1.1.0"));
        run(prepared);
        assert!(
            root.join("plugins/tidy/.history/1.0.0/plugin.json")
                .is_file()
        );
        let list = call(&c, &ExtList, json!({}));
        assert!(list.content.contains("tidy \"Tidy\" 1.1.0 (recipe)"));
        assert!(list.content.contains("saved versions: 1.0.0"));
        // Roll back (asks), restoring 1.0.0 and keeping 1.1.0.
        let out = run(ExtRollback
            .prepare(&c, &json!({"id": "tidy", "version": "1.0.0"}))
            .unwrap());
        assert!(!out.is_error, "{}", out.content);
        let restored = Plugin::load(&root.join("plugins/tidy")).unwrap();
        assert_eq!(restored.manifest.version, "1.0.0");
        assert!(
            root.join("plugins/tidy/.history/1.1.0/plugin.json")
                .is_file()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staging_is_confined_and_agents_tools_are_checked() {
        let root = temp_dir("extsafe");
        let c = ctx(&root);
        assert!(
            run(ExtWrite
                .prepare(
                    &c,
                    &json!({"id": "x", "path": "../escape.txt", "content": ""})
                )
                .unwrap())
            .is_error
        );
        assert!(
            ExtWrite
                .prepare(&c, &json!({"id": "../x", "path": "a", "content": ""}))
                .is_err()
        );
        let agent = r#"{"api":0,"id":"helper","name":"Helper","version":"1.0.0","kind":"agent",
            "system_prompt":"prompt.md","tools":["read_buffer","run_command"],"commands":[{"id":"open","title":"Open Helper"}]}"#;
        call(
            &c,
            &ExtWrite,
            json!({"id": "helper", "path": "plugin.json", "content": agent}),
        );
        call(
            &c,
            &ExtWrite,
            json!({"id": "helper", "path": "prompt.md", "content": "You help."}),
        );
        let out = run(ExtValidate.prepare(&c, &json!({"id": "helper"})).unwrap());
        assert!(
            out.is_error && out.content.contains("unknown tools [\"run_command\"]"),
            "{}",
            out.content
        );
        assert!(
            ExtTest
                .prepare(
                    &c,
                    &json!({"id": "helper", "command": "open", "inputs": []})
                )
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_extensions_need_approval_to_test() {
        let root = temp_dir("extproc");
        let c = ctx(&root);
        let manifest = r#"{"api":0,"id":"p","name":"P","version":"1","kind":"process","command":["python","p.py"],
            "commands":[{"id":"run","title":"Run"}]}"#;
        call(
            &c,
            &ExtWrite,
            json!({"id": "p", "path": "plugin.json", "content": manifest}),
        );
        let prepared = ExtTest
            .prepare(&c, &json!({"id": "p", "command": "run", "inputs": ["x"]}))
            .unwrap();
        assert!(matches!(prepared, Prepared::Ask(ref a, _) if a.risk == Risk::Process));
        std::fs::remove_dir_all(root).unwrap();
    }
}
