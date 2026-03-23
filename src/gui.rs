//! GUI application with GDB debugger integration.
//!
//! This module provides the GUI thread that manages GDB connections
//! and handles user interactions.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use flume::{Receiver, Sender};
use gdb_client::{GDB, GDBCmd, GDBConnectResult, GDBResponse, GDBSource};

/// GDB event loop running in a dedicated thread.
///
/// This function runs the GDB client in a separate thread from the GUI.
/// Communication happens via channels:
/// - `app_to_gdb`: Commands from GUI to GDB
/// - `gdb_to_app`: Responses from GDB to GUI
///
/// The thread handles:
/// - Synchronous and asynchronous connection (TCP/serial)
/// - Command execution with proper error handling
/// - Monitor thread spawning for async stop detection when target runs
pub fn gdb_thread(app_to_gdb: Receiver<GDBCmd>, gdb_to_app: Sender<GDBResponse>) {
    let mut gdb = GDB::default();
    let target_running = Arc::new(AtomicBool::new(false));

    type ConnectRx = std::sync::mpsc::Receiver<Result<GDB, gdb_client::GDBError>>;
    let mut pending_connect: Option<ConnectRx> = None;

    loop {
        // Poll pending connect and/or wait for next command.
        let cmd: GDBCmd;

        if let Some(ref rx) = pending_connect {
            // Check if the TCP connect finished
            match rx.try_recv() {
                Ok(Ok(new_gdb)) => {
                    pending_connect = None;
                    gdb = new_gdb;
                    match gdb.connect_and_init() {
                        Ok(_) => {
                            let _ = gdb_to_app.send(GDBResponse::Connected);
                        }
                        Err(e) => {
                            let _ = gdb_to_app.send(GDBResponse::Error(e.to_string()));
                        }
                    }
                    continue;
                }
                Ok(Err(e)) => {
                    pending_connect = None;
                    let _ = gdb_to_app.send(GDBResponse::Error(e.to_string()));
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    pending_connect = None;
                    let _ = gdb_to_app.send(GDBResponse::Error("connect thread died".into()));
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still connecting — poll for commands without blocking
                    match app_to_gdb.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(c) => {
                            // Cancel the in-progress connect, process new command below
                            pending_connect = None;
                            cmd = c;
                        }
                        Err(flume::RecvTimeoutError::Timeout) => continue,
                        Err(flume::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        } else {
            // No pending connect — block for next command
            cmd = match app_to_gdb.recv() {
                Ok(c) => c,
                Err(_) => return,
            };
        }

        // ── Process command ─────────────────────────────────────────────
        let is_halt = matches!(cmd, GDBCmd::Halt);
        let is_continue = matches!(cmd, GDBCmd::Continue);
        let is_disconnect = matches!(cmd, GDBCmd::Disconnect);

        if is_disconnect {
            target_running.store(false, Ordering::SeqCst);
        }

        if is_halt && target_running.load(Ordering::SeqCst) {
            if let Err(e) = gdb.send_interrupt() {
                let _ = gdb_to_app.send(GDBResponse::Error(e.to_string()));
            }
            continue;
        }

        // Async connect: spawn TCP connect in background thread
        if let GDBCmd::ConnectAndInit(source) = &cmd {
            let is_serial = matches!(source, GDBSource::Serial { .. });
            if is_serial {
                match gdb.execute_cmd(GDBCmd::ConnectAndInit(source.clone())) {
                    Ok(GDBResponse::Connected) => {
                        let _ = gdb_to_app.send(GDBResponse::Connected);
                    }
                    Ok(resp) => {
                        let _ = gdb_to_app.send(resp);
                    }
                    Err(e) => {
                        let _ = gdb.execute_cmd(GDBCmd::Disconnect);
                        let _ = gdb_to_app.send(GDBResponse::Error(e.to_string()));
                    }
                }
                continue;
            }
            if let GDBSource::Network((_ip, _port)) = source {
                match GDB::connect_async(source) {
                    GDBConnectResult::InProgress(rx) => {
                        pending_connect = Some(rx);
                        continue;
                    }
                    GDBConnectResult::Connected(new_gdb) => {
                        gdb = new_gdb;
                        match gdb.connect_and_init() {
                            Ok(_) => {
                                let _ = gdb_to_app.send(GDBResponse::Connected);
                            }
                            Err(e) => {
                                let _ = gdb_to_app.send(GDBResponse::Error(e.to_string()));
                            }
                        }
                        continue;
                    }
                    GDBConnectResult::Error(e) => {
                        let _ = gdb_to_app.send(GDBResponse::Error(e.to_string()));
                        continue;
                    }
                }
            }
        }

        let response = match gdb.execute_cmd(cmd) {
            Ok(resp) => resp,
            Err(e) => GDBResponse::Error(e.to_string()),
        };

        if is_continue && matches!(response, GDBResponse::Continued) {
            target_running.store(true, Ordering::SeqCst);
            if let Ok(mut gdb_clone) = gdb.try_clone_stream() {
                let tx = gdb_to_app.clone();
                let running = target_running.clone();
                std::thread::spawn(move || {
                    let packet = gdb_clone.read_packet();
                    match packet {
                        Some(_) => {
                            running.store(false, Ordering::SeqCst);
                            let _ = tx.send(GDBResponse::Halted);
                        }
                        None => {
                            running.store(false, Ordering::SeqCst);
                            let _ =
                                tx.send(GDBResponse::Error("RSP monitor: connection lost".into()));
                        }
                    }
                });
            }
        }

        if gdb_to_app.send(response).is_err() {
            return;
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
            let (ip, port) = if let Some(storage) = _cc.storage {
                let ip = storage
                    .get_string("RSPIpAddr")
                    .and_then(|string| std::net::Ipv4Addr::from_str(&string).ok())
                    .map(|ip| ip.to_bits().to_be_bytes())
                    .unwrap_or_default();
                let port = storage
                    .get_string("RSPPort")
                    .and_then(|string| u16::from_str(&string).ok())
                    .unwrap_or(2159);
                (ip, port)
            } else {
                ([0u8; 4], 2159)
            };
            Ok(Box::new(PenumbraApp {
                port,
                to_gdb: app_to_gdb,
                from_gdb: gdb_to_app,
                ip,
                connexion: Default::default(),
                running: false,
                error_msg: None,
                use_serial: false,
                serial_path: if cfg!(windows) {
                    "COM1".to_string()
                } else {
                    "/dev/ttyUSB0".to_string()
                },
                serial_baud: 115200,
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
    to_gdb: Sender<GDBCmd>,
    from_gdb: Receiver<GDBResponse>,
    error_msg: Option<String>,
    use_serial: bool,
    serial_path: String,
    serial_baud: u32,
}

impl PenumbraApp {
    fn send_cmd(&self, cmd: GDBCmd, ctx: &egui::Context) {
        if self.to_gdb.send(cmd).is_err() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for PenumbraApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Process responses from GDB thread
        let mut got_response = false;
        for resp in self.from_gdb.try_iter() {
            got_response = true;
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
                    // Ignore monitor shutdown errors (expected on disconnect)
                    if e.contains("connection lost") {
                        continue;
                    }
                    self.error_msg = Some(e);
                    self.connexion = Connexion::Disconnected;
                    self.running = false;
                }
            }
        }
        if got_response {
            ctx.request_repaint();
        }
        // Poll for async GDB responses (e.g. monitor thread stop-replies)
        if self.connexion >= Connexion::Connecting {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
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

            // Connection type selector
            ui.add_enabled_ui(self.connexion == Connexion::Disconnected, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Connection: ");
                    ui.radio_value(&mut self.use_serial, false, "Network");
                    ui.radio_value(&mut self.use_serial, true, "Serial");
                });
            });

            // Connection settings — disabled when not disconnected
            ui.add_enabled_ui(self.connexion == Connexion::Disconnected, |ui| {
                if self.use_serial {
                    ui.horizontal(|ui| {
                        ui.label("Serial port: ");
                        ui.text_edit_singleline(&mut self.serial_path);
                        ui.label("Baud: ");
                        ui.add(egui::DragValue::new(&mut self.serial_baud).range(300..=921600));
                    });
                } else {
                    ui.horizontal(|ui| {
                        let ip_label = ui.label("IP address: ");
                        let ip_formated = format!(
                            "{}.{}.{}.{}",
                            self.ip[0], self.ip[1], self.ip[2], self.ip[3]
                        );
                        for byte in self.ip.iter_mut() {
                            let field = ui.add(egui::DragValue::new(byte));
                            if field.gained_focus() || field.lost_focus() || field.changed() {
                                if let Some(storage) = frame.storage_mut() {
                                    storage.set_string("RSPIpAddr", ip_formated.clone());
                                }
                            }
                            field.labelled_by(ip_label.id);
                        }
                        ui.label(":");
                        let port = ui.add(egui::DragValue::new(&mut self.port));
                        if port.gained_focus() || port.lost_focus() || port.changed() {
                            if let Some(storage) = frame.storage_mut() {
                                storage.set_string("RSPPort", format!("{}", self.port));
                            }
                        }
                    });
                }
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
                    let source = if self.use_serial {
                        GDBSource::Serial {
                            path: std::path::PathBuf::from(&self.serial_path),
                            baud_rate: self.serial_baud,
                        }
                    } else {
                        GDBSource::Network((
                            std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                                self.ip[0], self.ip[1], self.ip[2], self.ip[3],
                            )),
                            self.port,
                        ))
                    };
                    self.send_cmd(GDBCmd::ConnectAndInit(source), ctx);
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
