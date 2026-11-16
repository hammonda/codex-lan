use crate::*;
use codex_lan_core::transport::{self, MonitorEvent, ServerConfig};
use eframe::egui;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot};

struct LiveSession {
    client_id: String,
    client_label: String,
    name: String,
    workspace: String,
    model: Option<String>,
    effort: Option<String>,
    usage: crate::usage::ThreadUsage,
    observer: crate::client::SessionObserver,
}

impl LiveSession {
    fn model_label(&self) -> String {
        let model = self.model.as_deref().unwrap_or("Codex default");
        self.effort
            .as_ref()
            .map_or_else(|| model.into(), |effort| format!("{model} {effort}"))
    }
}

fn live_session_ui(
    ui: &mut egui::Ui,
    id: &str,
    session: &LiveSession,
    selected: bool,
    closed: bool,
) -> bool {
    let mut inspect = false;
    ui.horizontal(|ui| {
        ui.label(mono(
            if closed { "○" } else { "●" },
            10.0,
            if closed { MUTED } else { MINT },
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(mono(
                session.usage.label(session.model.as_deref()),
                10.0,
                AMBER,
            ))
            .on_hover_text(format!(
                "{}\nUpdates when Codex reports token usage; cumulative for this session.",
                session.usage.tooltip(session.model.as_deref())
            ));
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let name = terminal_label(
                    ui,
                    &session.name,
                    (ui.available_width() * 0.45).min(220.0),
                    12.0,
                    if selected { CYAN } else { INK },
                )
                .interact(egui::Sense::click());
                name.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Button, true, &session.name)
                });
                inspect |= name
                    .on_hover_text(format!(
                        "Inspect both directions of this session\n{}\nNative thread: {id}",
                        session.name
                    ))
                    .clicked();
                terminal_label(ui, &session.client_label, ui.available_width(), 10.0, CYAN)
                    .on_hover_text(format!(
                        "{}\nConnection: {}",
                        session.client_label, session.client_id
                    ));
            });
        });
    });
    ui.indent("session_details", |ui| {
        ui.horizontal(|ui| {
            let model = session.model_label();
            terminal_label(
                ui,
                &model,
                (ui.available_width() * 0.3).min(200.0),
                10.0,
                MUTED,
            )
            .on_hover_text(&model);
            terminal_label(
                ui,
                &session.workspace,
                ui.available_width() - 26.0,
                10.0,
                MUTED,
            )
            .on_hover_text(&session.workspace);
            if icon(ui, "⧉", "Copy session directory").clicked() {
                ui.ctx().copy_text(session.workspace.clone());
            }
        });
    });
    inspect
}

struct LogEntry {
    time: String,
    kind: &'static str,
    text: String,
}

fn default_workspace_root() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from("/data/scratch")
    }
}

fn preferences_path() -> PathBuf {
    state_directory().join("server-preferences.json")
}

fn load_workspace_root(path: &Path) -> io::Result<Option<String>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let settings: serde_json::Value = serde_json::from_slice(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    settings
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .map(|path| Some(path.to_owned()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Missing workspace_root setting"))
}

fn load_connection_file(path: &Path) -> Option<PathBuf> {
    let settings: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    settings["claude_connection_file"]
        .as_str()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn default_connection_file() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(state_directory)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            })
    }
    .join("codex-lan/connection.json")
}

fn save_workspace_root(
    path: &Path,
    workspace: &str,
    connection_file: Option<&Path>,
) -> io::Result<()> {
    let mut settings = serde_json::json!({"workspace_root": workspace});
    if let Some(connection_file) = connection_file {
        settings["claude_connection_file"] = serde_json::json!(connection_file);
    }
    write_private_json(path, &settings)
}

fn save_local_connection(path: &Path, address: &str, pairing_code: &str) -> io::Result<()> {
    let mut address: std::net::SocketAddr = address
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    write_private_json(
        path,
        &serde_json::json!({"address":address.to_string(),"pairing_code":pairing_code}),
    )
}

fn write_private_json(path: &Path, value: &serde_json::Value) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Settings path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let contents = serde_json::to_vec_pretty(value)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".codex-lan-settings-{}-{nonce}.tmp",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(&contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) struct ServerApp {
    runtime: Runtime,
    worker: Option<Worker>,
    receiver: Option<mpsc::UnboundedReceiver<MonitorEvent>>,
    workspace: String,
    bind: String,
    codex_path: String,
    model: String,
    state_dir: String,
    connection_file: Option<PathBuf>,
    ready: bool,
    stopping: bool,
    ready_address: String,
    pairing: String,
    reveal_pairing: bool,
    show_settings: bool,
    clients: BTreeMap<String, String>,
    sessions: BTreeMap<String, LiveSession>,
    recent: VecDeque<(String, LiveSession)>,
    inspected: Option<String>,
    logs: Vec<LogEntry>,
    clock: Instant,
    error: Option<String>,
    close_requested: bool,
    demo: bool,
    // Release the single-instance lock after the runtime and workers are gone.
    instance: Option<crate::instance::ServerInstance>,
}

impl ServerApp {
    pub(crate) fn new(demo: bool) -> Self {
        let saved_workspace = load_workspace_root(&preferences_path());
        let mut app = Self {
            instance: None,
            runtime: runtime(),
            worker: None,
            receiver: None,
            workspace: saved_workspace
                .as_ref()
                .ok()
                .and_then(|workspace| workspace.clone())
                .unwrap_or_else(|| default_workspace_root().display().to_string()),
            bind: "0.0.0.0:7331".into(),
            codex_path: "codex".into(),
            model: String::new(),
            state_dir: state_directory().display().to_string(),
            connection_file: load_connection_file(&preferences_path()),
            ready: false,
            stopping: false,
            ready_address: String::new(),
            pairing: String::new(),
            reveal_pairing: false,
            show_settings: !demo,
            clients: BTreeMap::new(),
            sessions: BTreeMap::new(),
            recent: VecDeque::new(),
            inspected: None,
            logs: vec![],
            clock: Instant::now(),
            error: None,
            close_requested: false,
            demo,
        };
        app.log(
            "SYSTEM",
            "Server console ready. Select a workspace root to begin.",
        );
        if let Err(error) = saved_workspace {
            app.log(
                "ERROR",
                format!("Could not load the workspace preference; using the default: {error}"),
            );
        }
        if demo {
            app.ready = true;
            app.ready_address = "0.0.0.0:7331".into();
            app.workspace = "/data/scratch".into();
            app.pairing = "DEMO-PAIRING-CODE-NOT-A-REAL-CREDENTIAL".into();
            app.log(
                "CODEX",
                "App-server initialized. Workspace permissions applied.",
            );
            app.log("LISTEN", "Encrypted listener ready on 0.0.0.0:7331.");
            for (id, hostname, peer, name, model, input, output) in [
                (
                    "demo-01",
                    "WORKSTATION",
                    "192.168.1.88:49231",
                    "Explore the workspace",
                    "gpt-6.1-sol",
                    48000,
                    3400,
                ),
                (
                    "demo-02",
                    "LAPTOP",
                    "192.168.1.89:51842",
                    "Review the build",
                    "gpt-6.1-terra",
                    12500,
                    1800,
                ),
            ] {
                app.handle(MonitorEvent::ClientConnected {
                    id: id.into(),
                    peer: peer.into(),
                    hostname: Some(hostname.into()),
                });
                app.handle(MonitorEvent::SessionOpened {
                    client_id: id.into(),
                    session_id: id.into(),
                    name: name.into(),
                    workspace: format!("/data/scratch/codex-lan-threads/session-{id}"),
                    model: Some(model.into()),
                    effort: Some("medium".into()),
                });
                app.handle(MonitorEvent::SessionUsage {
                    session_id: id.into(),
                    params: serde_json::json!({"tokenUsage":{"total":{
                        "inputTokens":input, "cachedInputTokens":input / 2,
                        "outputTokens":output, "reasoningOutputTokens":output / 2,
                        "totalTokens":input + output
                    }}}),
                });
            }
            if std::env::var_os("CODEX_LAN_DEMO_INSPECTOR").is_some() {
                app.show_settings = true;
                let session = app.sessions.get_mut("demo-01").unwrap();
                session.name = "Claude / Summarize prepared context".into();
                session.observer.demo_traffic();
                app.inspected = Some("demo-01".into());
            }
            app.log("CODEX", "Ready to receive prompts.");
        }
        app
    }

    pub(crate) fn launch(
        demo: bool,
        start: bool,
        connection_file: Option<PathBuf>,
        instance: Option<crate::instance::ServerInstance>,
    ) -> Self {
        let mut app = Self::new(demo);
        app.instance = instance;
        if connection_file.is_some() {
            app.connection_file = connection_file;
        }
        if start && !demo {
            app.start();
        }
        app
    }

    fn log(&mut self, kind: &'static str, message: impl Into<String>) {
        let elapsed = self.clock.elapsed().as_secs();
        self.logs.push(LogEntry {
            time: format!(
                "{:02}:{:02}:{:02}",
                elapsed / 3600,
                elapsed / 60 % 60,
                elapsed % 60
            ),
            kind,
            text: message.into(),
        });
        if self.logs.len() > 500 {
            self.logs.drain(..self.logs.len() - 500);
        }
    }

    fn save_for_claude(&mut self) {
        if !self.ready || self.demo {
            return;
        }
        let path = self
            .connection_file
            .clone()
            .unwrap_or_else(default_connection_file);
        let result =
            save_local_connection(&path, &self.ready_address, &self.pairing).and_then(|()| {
                self.connection_file = Some(path.clone());
                save_workspace_root(&preferences_path(), &self.workspace, Some(&path))
            });
        match result {
            Ok(()) => self.log(
                "LOCAL",
                format!(
                    "Claude pairing saved: {} · refreshes on server start",
                    path.display()
                ),
            ),
            Err(error) => self.error = Some(format!("Could not save Claude pairing: {error}")),
        }
    }

    fn start(&mut self) {
        self.error = None;
        self.stopping = false;
        self.pairing.clear();
        let config = ServerConfig {
            bind: self.bind.trim().into(),
            workspace: PathBuf::from(self.workspace.trim()),
            codex_path: PathBuf::from(self.codex_path.trim()),
            state_dir: PathBuf::from(self.state_dir.trim()),
            model: if self.model.trim().is_empty() {
                None
            } else {
                Some(self.model.trim().into())
            },
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        self.runtime.spawn(async move {
            let result = transport::run_server(config, tx, task_stop).await;
            let _ = done_tx.send(result.map_err(|e| format!("{e:#}")));
        });
        self.receiver = Some(rx);
        self.worker = Some(Worker {
            stop,
            done: done_rx,
        });
        self.log(
            "SYSTEM",
            "Starting Codex and checking pending thread cleanup…",
        );
    }

    fn stop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.stop.cancel();
        }
        if !self.stopping {
            self.stopping = true;
            self.log(
                "CLEANUP",
                "Stopping sessions and deleting their Codex threads…",
            );
        }
    }

    fn drain(&mut self) {
        let mut events = Vec::new();
        if let Some(receiver) = &mut self.receiver {
            for _ in 0..4096 {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(_) => break,
                }
            }
        }
        for event in events {
            self.handle(event);
        }
        let finished = self
            .worker
            .as_mut()
            .and_then(|worker| match worker.done.try_recv() {
                Ok(result) => Some(result),
                Err(oneshot::error::TryRecvError::Closed) => {
                    Some(Err("Server task stopped unexpectedly.".into()))
                }
                Err(oneshot::error::TryRecvError::Empty) => None,
            });
        if let Some(result) = finished {
            if let Err(error) = result {
                self.log("ERROR", error.clone());
                self.error = Some(error);
            }
            self.worker = None;
            self.receiver = None;
            self.ready = false;
            self.stopping = false;
            self.pairing.clear();
            self.clients.clear();
            self.sessions.clear();
        }
    }

    fn handle(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::Ready {
                address,
                pairing_code,
                workspace,
            } => {
                self.ready_address = address;
                self.pairing = pairing_code;
                self.workspace = workspace;
                self.ready = true;
                if let Err(error) = save_workspace_root(
                    &preferences_path(),
                    &self.workspace,
                    self.connection_file.as_deref(),
                ) {
                    self.log(
                        "ERROR",
                        format!("Server is running, but the workspace preference could not be saved: {error}"),
                    );
                }
                if let Some(path) = self.connection_file.clone() {
                    match save_local_connection(&path, &self.ready_address, &self.pairing) {
                        Ok(()) => self.log(
                            "LOCAL",
                            format!("MCP connection refreshed: {}", path.display()),
                        ),
                        Err(error) => {
                            let message =
                                format!("Could not write the local MCP connection file: {error}");
                            self.log("ERROR", &message);
                            self.error = Some(message);
                        }
                    }
                }
                self.log(
                    "LISTEN",
                    format!(
                        "Ready on {}. Share the pairing code with your client.",
                        self.ready_address
                    ),
                );
            }
            MonitorEvent::ClientConnected { id, peer, hostname } => {
                let label = hostname
                    .map_or_else(|| peer.clone(), |hostname| format!("{hostname} ({peer})"));
                self.log("CLIENT", format!("Connected: {label}"));
                self.clients.insert(id, label);
            }
            MonitorEvent::ClientDisconnected { id } => {
                let label = self.clients.remove(&id).unwrap_or(id);
                self.log("CLIENT", format!("Disconnected: {label}"));
            }
            MonitorEvent::SessionOpened {
                client_id,
                session_id,
                name,
                workspace,
                model,
                effort,
            } => {
                let session = LiveSession {
                    observer: crate::client::SessionObserver::new(
                        session_id.clone(),
                        name.clone(),
                        workspace.clone(),
                    ),
                    client_label: self
                        .clients
                        .get(&client_id)
                        .cloned()
                        .unwrap_or_else(|| client_id.clone()),
                    client_id,
                    name,
                    workspace,
                    model,
                    effort,
                    usage: Default::default(),
                };
                self.log(
                    "THREAD",
                    format!(
                        "Opened: {} · {} · {}",
                        session.name,
                        session.client_label,
                        session.model_label()
                    ),
                );
                self.sessions.insert(session_id, session);
            }
            MonitorEvent::SessionUsage { session_id, params } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.usage.update(&params);
                }
            }
            MonitorEvent::SessionCommand {
                session_id,
                command,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.observer.command(command);
                }
            }
            MonitorEvent::SessionEvent(event) => {
                if let Some(session) = event.session_id().and_then(|id| self.sessions.get_mut(id)) {
                    session.observer.event(event);
                }
            }
            MonitorEvent::SessionClosed { session_id } => {
                if let Some(session) = self.sessions.remove(&session_id) {
                    self.log(
                        "CLEANUP",
                        format!(
                            "Deleted: {} · {} · {}",
                            session.name,
                            session.client_label,
                            session.usage.label(session.model.as_deref())
                        ),
                    );
                    self.recent.push_back((session_id, session));
                    while self.recent.len() > 8 {
                        self.recent.pop_front();
                    }
                }
            }
            MonitorEvent::Log(message) => self.log("CODEX", message),
            MonitorEvent::Stopped => {
                self.ready = false;
                self.pairing.clear();
                self.log("SYSTEM", "Server stopped.");
            }
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("server_settings").default_size(280.0).size_range(220.0..=420.0).frame(panel_frame()).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("server_settings_scroll").show(ui, |ui| {
                ui.add_enabled_ui(self.worker.is_none() && !self.demo, |ui| {
                    field(ui, "workspace root", &mut self.workspace, &default_workspace_root().display().to_string());
                    ui.label(mono("Clients may choose a folder inside this root.", 10.0, MUTED)).on_hover_text("The root is saved after the server starts. Blank client directory creates a unique folder under codex-lan-threads/ inside this root. Each thread gets write access to its selected folder. Closing a session deletes its native Codex thread immediately, while its files remain.");
                    field(ui, "listen", &mut self.bind, "0.0.0.0:7331");
                    egui::CollapsingHeader::new("advanced").show(ui, |ui| {
                        field(ui, "codex executable", &mut self.codex_path, "codex");
                        field(ui, "default model", &mut self.model, "Codex default");
                        field(ui, "cleanup state", &mut self.state_dir, "Local journal directory");
                    });
                });
                ui.add_space(4.0);
                if self.worker.is_some() {
                    if ui.add_enabled(!self.stopping, egui::Button::new("■ stop")).on_hover_text("Stop server and delete all owned native Codex threads").clicked() { self.stop(); }
                } else if ui.add_enabled(!self.demo && !self.workspace.trim().is_empty() && !self.codex_path.trim().is_empty(), egui::Button::new("start →")).clicked() { self.start(); }
                if self.ready {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(mono("pairing", 11.0, MUTED));
                        if icon(ui,"⧉","Copy pairing code").clicked() { ui.ctx().copy_text(self.pairing.clone()); }
                        if icon(ui,"◉","Show or hide pairing code").clicked() { self.reveal_pairing = !self.reveal_pairing; }
                    });
                    ui.add(egui::TextEdit::singleline(&mut self.pairing).password(!self.reveal_pairing).interactive(false).desired_width(f32::INFINITY));
                    ui.label(mono("Use this host’s LAN IP in the client.",10.0,MUTED)).on_hover_text("The pairing code changes each time the server starts.");
                }
            });
        });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(mono(
                        format!(
                            "{} clients · {} sessions",
                            self.clients.len(),
                            self.sessions.len()
                        ),
                        11.0,
                        MUTED,
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon(ui, "⧉", "Copy workspace root").clicked() {
                            ui.ctx().copy_text(self.workspace.clone());
                        }
                        ui.add(egui::Label::new(mono(&self.workspace, 11.0, MUTED)).truncate())
                            .on_hover_text(&self.workspace);
                    });
                });
                if !self.sessions.is_empty() || !self.recent.is_empty() {
                    egui::ScrollArea::vertical()
                        .id_salt("active_sessions")
                        .max_height(150.0)
                        .show(ui, |ui| {
                            for (id, session) in &self.sessions {
                                ui.push_id(id, |ui| {
                                    if live_session_ui(
                                        ui,
                                        id,
                                        session,
                                        self.inspected.as_ref() == Some(id),
                                        false,
                                    ) {
                                        self.inspected = Some(id.clone());
                                    }
                                });
                            }
                            if !self.recent.is_empty() {
                                egui::CollapsingHeader::new("recently closed").show(ui, |ui| {
                                    for (id, session) in self.recent.iter().rev() {
                                        ui.push_id(id, |ui| {
                                            if live_session_ui(
                                                ui,
                                                id,
                                                session,
                                                self.inspected.as_ref() == Some(id),
                                                true,
                                            ) {
                                                self.inspected = Some(id.clone());
                                            }
                                        });
                                    }
                                });
                            }
                        });
                }
                ui.separator();
                if let Some(id) = self.inspected.clone() {
                    let closed = !self.sessions.contains_key(&id);
                    let selected = self.sessions.get_mut(&id).or_else(|| {
                        self.recent
                            .iter_mut()
                            .find(|(recent_id, _)| recent_id == &id)
                            .map(|(_, session)| session)
                    });
                    if let Some(session) = selected {
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if icon(ui, "×", "Close inspector and show server events")
                                        .clicked()
                                    {
                                        self.inspected = None;
                                    }
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            terminal_label(
                                                ui,
                                                &session.name,
                                                ui.available_width() * 0.6,
                                                12.0,
                                                CYAN,
                                            )
                                            .on_hover_text(&session.name);
                                            terminal_label(
                                                ui,
                                                &session.client_label,
                                                ui.available_width(),
                                                10.0,
                                                MUTED,
                                            )
                                            .on_hover_text(&session.client_label);
                                        },
                                    );
                                },
                            );
                        });
                        session.observer.ui(ui, closed);
                        return;
                    }
                    self.inspected = None;
                }
                ui.horizontal(|ui| {
                    ui.label(mono("events", 11.0, MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon(ui, "×", "Clear event display").clicked() {
                            self.logs.clear();
                        }
                        if icon(ui, "⧉", "Copy event log").clicked() {
                            ui.ctx().copy_text(
                                self.logs
                                    .iter()
                                    .map(|log| format!("{} {} {}", log.time, log.kind, log.text))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                        }
                    });
                });
                egui::ScrollArea::vertical()
                    .id_salt("event_stream")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (index, log) in self.logs.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal_top(|ui| {
                                    ui.label(mono(&log.time, 10.0, MUTED));
                                    let color = match log.kind {
                                        "ERROR" => RED,
                                        "CLEANUP" => AMBER,
                                        "CLIENT" => CYAN,
                                        "THREAD" => MINT,
                                        _ => MUTED,
                                    };
                                    terminal_label(ui, &log.kind.to_lowercase(), 58.0, 10.0, color);
                                    ui.add(
                                        egui::Label::new(mono(
                                            &log.text,
                                            12.0,
                                            if log.kind == "ERROR" { RED } else { INK },
                                        ))
                                        .selectable(true)
                                        .wrap(),
                                    );
                                });
                            });
                        }
                    });
            });
    }
}

impl eframe::App for ServerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(instance) = &mut self.instance {
            instance.poll(ctx);
        }
        self.drain();
        closing(ctx, &mut self.close_requested, &self.worker);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, color) = if self.demo {
            ("demo", AMBER)
        } else if self.stopping || self.close_requested {
            ("stopping", AMBER)
        } else if self.ready {
            ("listening", MINT)
        } else if self.worker.is_some() {
            ("starting", CYAN)
        } else {
            ("offline", MUTED)
        };
        egui::Panel::top("app_header")
            .frame(panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if icon(ui, "☰", "Show or hide host settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    ui.label(mono("codex / host", 13.0, INK));
                    ui.label(mono(concat!("v", env!("CARGO_PKG_VERSION")), 11.0, CYAN))
                        .on_hover_text("Version of this running server");
                    ui.label(mono(format!("● {status}"), 10.0, color));
                    if self.ready {
                        ui.label(mono(&self.ready_address, 11.0, MUTED));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.ready && icon(ui, "⧉", "Copy pairing code").clicked() {
                            ui.ctx().copy_text(self.pairing.clone());
                        }
                        if ui.add_enabled(self.ready && !self.demo, egui::Button::new(mono("Claude ↓", 11.0, CYAN)))
                            .on_hover_text(format!("Save pairing for Claude\n{}\nAutomatically refreshes after server restarts.", self.connection_file.clone().unwrap_or_else(default_connection_file).display())).clicked() {
                            self.save_for_claude();
                        }
                        ui.add_enabled_ui(!self.demo && !self.stopping, |ui| {
                            if self.worker.is_some() {
                                if icon(ui, "■", "Stop server and delete its native threads")
                                    .clicked()
                                {
                                    self.stop();
                                }
                            } else if icon(ui, "▶", "Start server").clicked() {
                                self.start();
                            }
                        });
                    });
                });
            });
        if let Some(error) = self.error.clone() {
            egui::Panel::top("error_banner")
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(mono(error, 12.0, RED));
                        if icon(ui, "×", "Dismiss error").clicked() {
                            self.error = None;
                        }
                    });
                });
        }
        if self.show_settings {
            self.settings(ui);
        }
        self.dashboard(ui);
    }
}

impl Drop for ServerApp {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.stop.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
