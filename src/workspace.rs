use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};
use eframe::egui::{self, Align, Color32, Frame, Layout, Margin, Panel, RichText, Stroke};
use rfd::FileDialog;

use crate::{
    app::{RustEditorApp, configure_style},
    document::Document,
    instance::SingleInstanceServer,
};

const BORDER: Color32 = Color32::from_rgb(205, 205, 205);

struct LoadMessage {
    tab_id: u64,
    result: Result<Document>,
}

enum TabState {
    Loading,
    Ready(Box<RustEditorApp>),
    Failed(String),
}

struct FileTab {
    id: u64,
    path: Option<PathBuf>,
    state: TabState,
}

impl FileTab {
    fn label(&self) -> String {
        let name = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned());

        match &self.state {
            TabState::Loading => format!("{name} (loading...)"),
            TabState::Ready(editor) if editor.is_dirty() => {
                format!("{} *", editor.display_name())
            }
            TabState::Ready(editor) => editor.display_name(),
            TabState::Failed(_) => format!("{name} (failed)"),
        }
    }
}

pub struct WorkspaceApp {
    tabs: Vec<FileTab>,
    active_tab: usize,
    next_tab_id: u64,
    load_sender: Sender<LoadMessage>,
    load_receiver: Receiver<LoadMessage>,
    incoming_paths: Receiver<Vec<PathBuf>>,
}

impl WorkspaceApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial_paths: Vec<PathBuf>,
        instance_server: Option<SingleInstanceServer>,
    ) -> Self {
        configure_style(&cc.egui_ctx);
        let (load_sender, load_receiver) = mpsc::channel();
        let (incoming_sender, incoming_paths) = mpsc::channel();
        if let Some(server) = instance_server
            && let Err(error) = server.start(incoming_sender, cc.egui_ctx.clone())
        {
            eprintln!("RustEditor instance listener failed: {error}");
        }
        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            load_sender,
            load_receiver,
            incoming_paths,
        };

        if initial_paths.is_empty() {
            app.new_blank_tab();
        } else {
            for path in initial_paths {
                app.open_async(path, &cc.egui_ctx);
            }
        }
        app
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    fn new_blank_tab(&mut self) {
        let id = self.next_id();
        self.tabs.push(FileTab {
            id,
            path: None,
            state: TabState::Ready(Box::new(RustEditorApp::blank())),
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn open_async(&mut self, path: PathBuf, ctx: &egui::Context) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.path.as_deref() == Some(path.as_path()))
        {
            self.active_tab = index;
            return;
        }

        let tab_id = self.next_id();
        self.tabs.push(FileTab {
            id: tab_id,
            path: Some(path.clone()),
            state: TabState::Loading,
        });
        self.active_tab = self.tabs.len() - 1;

        let sender = self.load_sender.clone();
        let repaint = ctx.clone();
        let thread_name = format!("rusteditor-loader-{tab_id}");
        let spawn_result = thread::Builder::new().name(thread_name).spawn(move || {
            let result = load_document(&path);
            let _ = sender.send(LoadMessage { tab_id, result });
            repaint.request_repaint();
        });

        if let Err(error) = spawn_result
            && let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id)
        {
            tab.state = TabState::Failed(format!("cannot start file loader: {error}"));
        }
    }

    fn receive_load_results(&mut self) {
        while let Ok(message) = self.load_receiver.try_recv() {
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == message.tab_id) else {
                continue;
            };
            tab.state = match message.result {
                Ok(document) => TabState::Ready(Box::new(RustEditorApp::new(document))),
                Err(error) => TabState::Failed(format!("{error:#}")),
            };
        }
    }

    fn receive_external_paths(&mut self, ctx: &egui::Context) {
        while let Ok(paths) = self.incoming_paths.try_recv() {
            for path in paths {
                self.open_async(path, ctx);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let Some(paths) = FileDialog::new().set_title("Open files").pick_files() else {
            return;
        };
        for path in paths {
            self.open_async(path, ctx);
        }
    }

    fn active_editor_mut(&mut self) -> Option<&mut RustEditorApp> {
        match self.tabs.get_mut(self.active_tab).map(|tab| &mut tab.state) {
            Some(TabState::Ready(editor)) => Some(editor),
            _ => None,
        }
    }

    fn menu_bar(&mut self, root_ui: &mut egui::Ui, ctx: &egui::Context) {
        Panel::top("workspace_menu")
            .frame(Frame::new().fill(Color32::from_rgb(245, 245, 245)))
            .show(root_ui, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui.button("New\tCtrl+N").clicked() {
                            ui.close();
                            self.new_blank_tab();
                        }
                        if ui.button("Open...\tCtrl+O").clicked() {
                            ui.close();
                            self.open_dialog(ctx);
                        }
                        let ready = matches!(
                            self.tabs.get(self.active_tab).map(|tab| &tab.state),
                            Some(TabState::Ready(_))
                        );
                        let writable = matches!(
                            self.tabs.get(self.active_tab).map(|tab| &tab.state),
                            Some(TabState::Ready(editor)) if !editor.is_read_only()
                        );
                        if ui
                            .add_enabled(writable, egui::Button::new("Save\tCtrl+S"))
                            .clicked()
                        {
                            ui.close();
                            if let Some(editor) = self.active_editor_mut() {
                                editor.save();
                            }
                        }
                        if ui
                            .add_enabled(ready, egui::Button::new("Save As..."))
                            .clicked()
                        {
                            ui.close();
                            if let Some(editor) = self.active_editor_mut() {
                                editor.save_as();
                            }
                        }
                    });
                    if let Some(editor) = self.active_editor_mut() {
                        editor.tools_menu(ui);
                    }
                });
            });
    }

    fn tab_bar(&mut self, root_ui: &mut egui::Ui) {
        let mut create_blank = false;
        Panel::top("workspace_tabs")
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(225, 225, 225))
                    .inner_margin(Margin::symmetric(6, 3))
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(root_ui, |ui| {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (index, tab) in self.tabs.iter().enumerate() {
                                let selected = index == self.active_tab;
                                if ui.selectable_label(selected, tab.label()).clicked() {
                                    self.active_tab = index;
                                }
                            }
                            if ui
                                .small_button("+")
                                .on_hover_text("Create a new file")
                                .clicked()
                            {
                                create_blank = true;
                            }
                        });
                    });
            });
        if create_blank {
            self.new_blank_tab();
        }
    }

    fn loading_or_error(&mut self, root_ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        egui::CentralPanel::default()
            .frame(Frame::new().fill(Color32::WHITE))
            .show(root_ui, |ui| {
                ui.with_layout(
                    Layout::top_down(Align::Center).with_cross_justify(true),
                    |ui| {
                        ui.add_space(80.0);
                        match &tab.state {
                            TabState::Loading => {
                                ui.spinner();
                                ui.heading("Loading file in the background");
                                if let Some(path) = &tab.path {
                                    ui.label(path.display().to_string());
                                }
                                ui.label("Other tabs remain available while this file loads.");
                                ctx.request_repaint_after(Duration::from_millis(100));
                            }
                            TabState::Failed(error) => {
                                ui.heading(
                                    RichText::new("File could not be loaded")
                                        .color(Color32::from_rgb(180, 35, 35)),
                                );
                                if let Some(path) = &tab.path {
                                    ui.label(path.display().to_string());
                                }
                                ui.separator();
                                ui.label(error);
                            }
                            TabState::Ready(_) => {}
                        }
                    },
                );
            });
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::N)) {
            self.new_blank_tab();
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::O)) {
            self.open_dialog(ctx);
        }
    }

    fn update_title(&self, ctx: &egui::Context) {
        let title = self
            .tabs
            .get(self.active_tab)
            .map(FileTab::label)
            .unwrap_or_else(|| "RustEditor".to_owned());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "{title} - RustEditor"
        )));
    }
}

impl eframe::App for WorkspaceApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive_external_paths(&ctx);
        self.receive_load_results();
        self.handle_shortcuts(&ctx);
        self.menu_bar(ui, &ctx);
        self.tab_bar(ui);

        if let Some(editor) = self.active_editor_mut() {
            editor.show(ui);
        } else {
            self.loading_or_error(ui, &ctx);
        }
        self.update_title(&ctx);
    }
}

fn load_document(path: &Path) -> Result<Document> {
    catch_unwind(AssertUnwindSafe(|| Document::open(path))).map_err(|_| {
        anyhow!(
            "the file loader panicked while processing {}",
            path.display()
        )
    })?
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::load_document;
    use crate::document::Document;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("rusteditor-workspace-{name}-{nonce}.txt"))
    }

    #[test]
    fn documents_can_move_from_loader_threads_to_the_ui_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Document>();
    }

    #[test]
    fn corrupted_utf8_fails_without_panicking() {
        let path = temp_file("invalid-utf8");
        fs::write(&path, [0xff, 0xfe, 0xfd]).expect("test file should be created");

        let error = match load_document(&path) {
            Ok(_) => panic!("invalid UTF-8 should fail"),
            Err(error) => error,
        };

        assert!(format!("{error:#}").contains("not valid UTF-8"));
        fs::remove_file(path).expect("test file should be removed");
    }
}
