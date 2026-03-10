use flume::{Receiver, Sender};
use gdb_client::{GDBCmd, GDBResponse, GDBSource, GDB};

pub fn gdb_thread(app_to_gdb: Receiver<GDBCmd>, gdb_to_app: Sender<GDBResponse>) {
    let mut gdb = GDB::default();
    loop {
        match app_to_gdb.recv() {
            Ok(cmd) => {
                let response = match gdb.execute_cmd(cmd) {
                    Ok(resp) => resp,
                    Err(e) => GDBResponse::Error(e.to_string()),
                };
                if gdb_to_app.send(response).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

pub fn run_gui(
    app_to_gdb: Sender<GDBCmd>,
    gdb_to_app: Receiver<GDBResponse>,
) -> std::thread::Result<()> {
    let options = eframe::NativeOptions {
        hardware_acceleration: eframe::HardwareAcceleration::Preferred,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Penumbra",
        options,
        Box::new(|_cc| {
            Ok(Box::new(PenumbraApp {
                port: 2159,
                to_gdb: app_to_gdb,
                from_gdb: gdb_to_app,
                ip: Default::default(),
                connexion: Default::default(),
                running: false,
                max_health: 0,
                error_msg: None,
            }))
        }),
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
enum Connexion {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

struct PenumbraApp {
    ip: [u8; 4],
    port: u16,
    connexion: Connexion,
    running: bool,
    max_health: u8,
    to_gdb: Sender<GDBCmd>,
    from_gdb: Receiver<GDBResponse>,
    error_msg: Option<String>,
}

impl PenumbraApp {
    fn send_cmd(&self, cmd: GDBCmd, ctx: &egui::Context) {
        if self.to_gdb.send(cmd).is_err() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for PenumbraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process responses from GDB thread
        for resp in self.from_gdb.try_iter() {
            match resp {
                GDBResponse::Connected => {
                    self.connexion = Connexion::Connected;
                    self.running = false;
                    self.error_msg = None;
                    // Server pauses CPU on connect; resume it immediately
                    self.send_cmd(GDBCmd::Continue, ctx);
                }
                GDBResponse::Disconnected => {
                    self.connexion = Connexion::Disconnected;
                    self.running = false;
                }
                GDBResponse::Halted => {
                    self.running = false;
                }
                GDBResponse::Continued => {
                    self.running = true;
                }
                GDBResponse::Error(e) => {
                    self.error_msg = Some(e);
                    self.connexion = Connexion::Disconnected;
                    self.running = false;
                }
            }
        }

        if self.from_gdb.is_disconnected() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Penumbra");

            // IP/port entry — disabled when not disconnected
            ui.add_enabled_ui(self.connexion == Connexion::Disconnected, |ui| {
                ui.horizontal(|ui| {
                    let ip_label = ui.label("IP address: ");
                    for byte in self.ip.iter_mut() {
                        let field = ui.add(egui::DragValue::new(byte));
                        field.labelled_by(ip_label.id);
                    }
                    ui.label(":");
                    ui.add(egui::DragValue::new(&mut self.port));
                });
            });

            // Connect / Disconnect button
            let btn_label = match self.connexion {
                Connexion::Disconnected => "Connect",
                Connexion::Connecting => "Connecting...",
                Connexion::Connected => "Disconnect",
            };
            let connexion_btn = ui.add_enabled(
                self.connexion != Connexion::Connecting,
                egui::Button::new(btn_label),
            );
            if connexion_btn.clicked() {
                if self.connexion == Connexion::Disconnected {
                    self.connexion = Connexion::Connecting;
                    self.error_msg = None;
                    let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                        self.ip[0], self.ip[1], self.ip[2], self.ip[3],
                    ));
                    self.send_cmd(GDBCmd::SetSource(GDBSource::Network((ip, self.port))), ctx);
                    self.send_cmd(GDBCmd::Connect, ctx);
                } else {
                    self.send_cmd(GDBCmd::Disconnect, ctx);
                }
            }

            // Halt / Continue buttons — only when connected, mutually exclusive
            if self.connexion == Connexion::Connected {
                ui.horizontal(|ui| {
                    let halt_btn = ui.add_enabled(self.running, egui::Button::new("Halt"));
                    if halt_btn.clicked() {
                        self.send_cmd(GDBCmd::Halt, ctx);
                    }
                    let cont_btn = ui.add_enabled(!self.running, egui::Button::new("Continue"));
                    if cont_btn.clicked() {
                        self.send_cmd(GDBCmd::Continue, ctx);
                    }
                });
            }

            // Error display
            if let Some(ref err) = self.error_msg {
                ui.colored_label(egui::Color32::RED, format!("Error: {}", err));
            }
        });
    }
}
