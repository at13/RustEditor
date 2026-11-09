//! Provider settings UI. Keys never enter conversations, config JSON or logs.
use super::*;
use re_agent::setup::Kind;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Key,
    Model,
}

pub(super) struct Settings {
    pub open: bool,
    pub kind: Kind,
    pub model: String,
    // Deliberately no Debug implementation: holds a secret until Save/Cancel.
    key: String,
    pub field: Option<Field>,
    pub busy: bool,
    pub notice: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            open: false,
            kind: Kind::OpenAi,
            model: Kind::OpenAi.model().into(),
            key: String::new(),
            field: None,
            busy: false,
            notice: String::new(),
        }
    }
}

impl Workspace {
    pub(super) fn provider_select(&mut self, kind: Kind, cx: &mut Context<Self>) {
        if self.agents.settings.busy {
            return;
        }
        if matches!(self.command, Some(CommandMode::ProviderSetting)) {
            self.cancel_command(cx);
        }
        let model = self
            .agents
            .providers
            .iter()
            .find(|p| p.config.id == kind.id())
            .map(|p| p.config.model.clone())
            .unwrap_or_else(|| kind.model().into());
        self.agents.settings = Settings {
            open: true,
            kind,
            model,
            ..Default::default()
        };
        // Own input even when no field is active; typing must not fall
        // through into the document after applying a key/model.
        self.command = Some(CommandMode::ProviderSetting);
        self.content.clear();
        self.selected = 0..0;
        self.layout = None;
        self.marked = None;
        cx.notify();
    }

    pub(super) fn provider_edit(
        &mut self,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.agents.settings.busy {
            return;
        }
        if self.command.is_some() {
            self.cancel_command(cx);
        }
        self.agents.settings.field = Some(field);
        self.start_command(CommandMode::ProviderSetting, window, cx);
        self.content = if field == Field::Model {
            self.agents.settings.model.clone()
        } else {
            String::new()
        };
        self.selected = 0..self.content.len();
        cx.notify();
    }

    pub(super) fn provider_apply_field(&mut self, cx: &mut Context<Self>) {
        let value = std::mem::take(&mut self.content);
        match self.agents.settings.field.take() {
            Some(Field::Key) => {
                if !value.is_ascii() || value.contains(['\r', '\n']) || value.len() > 2048 {
                    self.agents.settings.notice =
                        "Invalid API key (must be a single ASCII line)".into();
                } else {
                    self.agents.settings.key = value.trim().into();
                }
            }
            Some(Field::Model) => self.agents.settings.model = value.trim().into(),
            None => {}
        }
        self.selected = 0..0;
        self.marked = None;
        self.layout = None;
        cx.notify();
    }

    pub(super) fn provider_settings_close(&mut self, cx: &mut Context<Self>) {
        if self.agents.settings.busy {
            return;
        }
        if matches!(self.command, Some(CommandMode::ProviderSetting)) {
            self.cancel_command(cx);
        }
        self.agents.settings = Settings::default();
        cx.notify();
    }

    /// User-triggered Save / Sign in / Remove. All credential I/O happens on
    /// a worker and never from tool prepare or the model.
    pub(super) fn provider_save(&mut self, login: bool, remove: bool, cx: &mut Context<Self>) {
        if self.agents.settings.busy {
            return;
        }
        if matches!(self.command, Some(CommandMode::ProviderSetting)) {
            self.provider_apply_field(cx);
        }
        let Some(root) = self.agents.paths.providers.clone() else {
            self.status = "No provider config directory".into();
            cx.notify();
            return;
        };
        let kind = self.agents.settings.kind;
        let helper = std::env::current_exe().ok().and_then(|p| {
            p.parent()
                .map(|d| d.join(format!("rusteditor-openai{}", std::env::consts::EXE_SUFFIX)))
        });
        if !remove && helper.as_ref().is_none_or(|p| !p.is_file()) {
            self.agents.settings.notice =
                "Provider helper is missing. Build/install rusteditor-openai alongside RustEditor."
                    .into();
            cx.notify();
            return;
        }
        if !remove
            && kind == Kind::Copilot
            && helper
                .as_ref()
                .is_some_and(|p| !Kind::copilot_script(p).is_file())
        {
            self.agents.settings.notice = "Copilot SDK is missing. Reinstall with the Copilot SDK package (Node.js required).".into();
            cx.notify();
            return;
        }
        let config = kind.config(
            &self.agents.settings.model,
            helper.as_deref().unwrap_or(Path::new("rusteditor-openai")),
        );
        let config = match config {
            Ok(c) => c,
            Err(e) => {
                self.agents.settings.notice = e;
                cx.notify();
                return;
            }
        };
        let key = std::mem::take(&mut self.agents.settings.key);
        let sender = self.agents.sender.clone();
        self.agents.settings.busy = true;
        self.agents.settings.notice = if login {
            "Complete GitHub sign-in in the browser/CLI window..."
        } else {
            "Saving..."
        }
        .into();
        let started = re_tasks::spawn(
            "rusteditor-provider-settings",
            None,
            move || {
                if remove {
                    // Remove the profile first; OS key deletion failures are reported.
                    re_agent::setup::remove(&root, kind)?;
                    if kind != Kind::Copilot {
                        re_platform::credentials::delete(kind.id())?;
                    }
                    return Ok(
                        "Provider removed. Copilot authentication, if any, is managed by the CLI."
                            .to_owned(),
                    );
                }
                if login {
                    let mut child = re_platform::credentials::copilot_login()?;
                    if !child.wait()?.success() {
                        anyhow::bail!(
                            "Copilot sign-in did not complete. Install the Copilot CLI and retry."
                        );
                    }
                }
                if !key.is_empty() {
                    re_platform::credentials::set(kind.id(), &key)?;
                }
                re_platform::paths::ensure_private_dir(&root)?;
                re_agent::setup::save(&root, &config)?;
                Ok(if login {
                    "Copilot sign-in command completed. Provider enabled."
                } else {
                    "Saved. Key, if supplied, is stored in the OS credential store."
                }
                .to_owned())
            },
            move |result| {
                let _=sender.send(agent::AgentMessage::Settings {id:kind.id().into(),result:result.map_err(|_|"Could not update provider. Check the OS credential service or Copilot installation; no key was written to a config file.".to_owned())});
            },
        );
        if let Err(e) = started {
            self.agents.settings.busy = false;
            self.agents.settings.notice = format!("Cannot start setup: {e}");
        }
        cx.notify();
    }

    pub(super) fn provider_check_copilot(&mut self, cx: &mut Context<Self>) {
        if self.agents.settings.busy {
            return;
        }
        let Some(helper) = std::env::current_exe().ok().and_then(|p| {
            p.parent()
                .map(|d| d.join(format!("rusteditor-openai{}", std::env::consts::EXE_SUFFIX)))
        }) else {
            return;
        };
        let script = Kind::copilot_script(&helper);
        if !helper.is_file() || !script.is_file() {
            self.agents.settings.notice =
                "Copilot SDK provider is not installed. Reinstall the provider helpers.".into();
            cx.notify();
            return;
        }
        self.agents.settings.busy = true;
        self.agents.settings.notice =
            "Checking the Copilot SDK runtime and authentication (no document text is sent)..."
                .into();
        let sender = self.agents.sender.clone();
        let started = re_tasks::spawn(
            "rusteditor-copilot-sdk-auth",
            None,
            move || {
                let mut command = std::process::Command::new(helper);
                command
                    .args(["--backend", "copilot", "--check-auth", "--copilot-sdk"])
                    .arg(script)
                    .stdin(std::process::Stdio::null());
                re_platform::child_lifetime::configure(&mut command);
                let output = command.output()?;
                let status = serde_json::from_slice::<serde_json::Value>(&output.stdout)
                    .ok()
                    .and_then(|v| {
                        v.get("is_authenticated")
                            .and_then(serde_json::Value::as_bool)
                    });
                Ok(match status {
                Some(true) if output.status.success() => "Copilot SDK connected and authenticated. Ready to use.",
                Some(false) => "Copilot SDK is not authenticated. Use Sign in with GitHub, complete authorization, then check again.",
                _ => "Copilot SDK connection failed. Check Node.js and the SDK installation; reinstall the provider if needed.",
            }.to_owned())
            },
            move |result: anyhow::Result<String>| {
                let result = result.unwrap_or_else(|_| {
                    "Cannot start the Copilot SDK helper. Check the installation.".into()
                });
                let _ = sender.send(agent::AgentMessage::ProviderAuth { result });
            },
        );
        if let Err(error) = started {
            self.agents.settings.busy = false;
            self.agents.settings.notice = format!("Cannot start check: {error}");
        }
        cx.notify();
    }

    pub(super) fn render_provider_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = &self.agents.settings;
        let configured = self
            .agents
            .providers
            .iter()
            .any(|p| p.config.id == settings.kind.id());
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child("Model providers")
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(Kind::ALL.into_iter().map(|kind| {
                        Self::toolbar_button(
                            kind.name(),
                            cx.listener(move |this, _, _, cx| this.provider_select(kind, cx)),
                        )
                    })),
            )
            .child(format!(
                "{} · {}",
                settings.kind.name(),
                if configured {
                    "configured"
                } else {
                    "not configured"
                }
            ))
            .child(format!("Model: {}", settings.model))
            .child(Self::toolbar_button(
                "Edit model",
                cx.listener(|this, _, window, cx| this.provider_edit(Field::Model, window, cx)),
            ));
        if settings.kind == Kind::Copilot {
            body=body.child("Uses the official Copilot SDK and your GitHub Copilot subscription. Sign in opens the CLI/browser OAuth flow; the SDK checks its shared authentication.")
                .child(Self::toolbar_button("Sign in with GitHub",cx.listener(|this,_,_,cx|this.provider_save(true,false,cx))))
                .child(Self::toolbar_button("Check Copilot connection",cx.listener(|this,_,_,cx|this.provider_check_copilot(cx))))
                .child("Requires Node.js and the installed SDK provider. Copilot CLI is needed for Sign in; already signed in? Save, then Check connection.");
        } else {
            body=body.child(if settings.key.is_empty() {"API key: existing stored key or environment variable (not displayed)"} else {"API key: replacement entered (not saved yet)"})
                .child(Self::toolbar_button("Enter / replace API key",cx.listener(|this,_,window,cx|this.provider_edit(Field::Key,window,cx))))
                .child("Stored securely in the OS credential store, not in JSON. Leave unchanged to use the existing key or API-key environment variable.");
        }
        if matches!(self.command, Some(CommandMode::ProviderSetting)) && settings.field.is_some() {
            body = body
                .child(if settings.field == Some(Field::Key) {
                    "API key (masked):"
                } else {
                    "Model name:"
                })
                .child(
                    div()
                        .flex()
                        .h(px(30.))
                        .bg(rgb(0xffffff))
                        .child(self.input_widget(cx)),
                )
                .child(Self::toolbar_button(
                    "Apply field",
                    cx.listener(|this, _, _, cx| this.provider_apply_field(cx)),
                ));
        }
        body = body.child(settings.notice.clone()).child(if settings.busy {
            "Please wait..."
        } else {
            ""
        });
        if !settings.busy {
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(Self::toolbar_button(
                        "Save provider",
                        cx.listener(|this, _, _, cx| this.provider_save(false, false, cx)),
                    ))
                    .child(Self::toolbar_button(
                        "Remove provider",
                        cx.listener(|this, _, _, cx| this.provider_save(false, true, cx)),
                    ))
                    .child(Self::toolbar_button(
                        "Done",
                        cx.listener(|this, _, _, cx| this.provider_settings_close(cx)),
                    )),
            );
        }
        body=body.child("Sending document text still requires a separate provider consent. API-key entry never sends a chat message.");
        div()
            .id("provider-settings")
            .key_context("Editor")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .w(px(agent::PANEL_WIDTH))
            .h_full()
            .flex_none()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(rgb(0xc8c8c8))
            .bg(rgb(0xf4f6fa))
            .child(body)
            .into_any_element()
    }

    pub(super) fn provider_secret_input(&self) -> bool {
        matches!(self.command, Some(CommandMode::ProviderSetting))
            && self.agents.settings.field == Some(Field::Key)
    }
}
