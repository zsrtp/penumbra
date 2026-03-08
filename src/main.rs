use std::time::Duration;

use clap::Parser;
use egui::Sense;
use flume::{Receiver, SendError, Sender, TryRecvError};
use gdb_client::GDBCmd;

#[derive(Debug, Parser)]
struct Args {
    #[arg()]
    command: Option<String>,
}

fn gdb_thread(app_to_gdb: flume::Receiver<GDBCmd>, gdb_to_app: flume::Sender<()>) {
    loop {
        match app_to_gdb.try_recv() {
            Ok(cmd) => {
                std::thread::sleep(Duration::from_secs(2));
                match gdb_to_app.send(()) {
                    Ok(_) => {},
                    Err(_) => return,
                }
            },
            Err(TryRecvError::Disconnected) => return,
            Err(_) => continue,
        }
    }
}

fn run_gui(app_to_gdb: flume::Sender<GDBCmd>, gdb_to_app: flume::Receiver<()>) -> std::thread::Result<()> {
    let options = eframe::NativeOptions {
        hardware_acceleration: eframe::HardwareAcceleration::Preferred,
        ..Default::default()
    };
    let _ = eframe::run_native("Penumbra", options, Box::new(|_cc| {
        Ok(Box::<PenumbraApp>::new(PenumbraApp { port: 2159, to_gdb: app_to_gdb, from_gdb: gdb_to_app, ip: Default::default(), connexion: Default::default(), max_health: 0 }))
    }));
    Ok(())
}

fn main() -> std::thread::Result<()> {
    let args = Args::parse();

    if args.command.is_none() {
        let (snd_to_gdb, rcv_from_app) = flume::unbounded();
        let (snd_to_app, rcv_from_gdb) = flume::unbounded();
        let gdb_thread_handle = std::thread::spawn(move || gdb_thread(rcv_from_app, snd_to_app));
        run_gui(snd_to_gdb, rcv_from_gdb)?;
        gdb_thread_handle.join()?;
    }

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
    max_health: u8,
    to_gdb: Sender<GDBCmd>,
    from_gdb: Receiver<()>,
}

impl eframe::App for PenumbraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Penumbra");
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
            let connexion_btn = ui.add_enabled(self.connexion != Connexion::Connecting, egui::Button::new(if self.connexion == Connexion::Connected {"Disconnect"} else {"Connect"}));
            if connexion_btn.clicked() {
                if self.connexion == Connexion::Disconnected {
                    self.connexion = Connexion::Connecting;
                    match self.to_gdb.send(GDBCmd::Connect) {
                        Ok(_) => {},
                        Err(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    }
                } else {
                    self.connexion = Connexion::Disconnected;
                }
            }
        });

        if self.from_gdb.is_disconnected() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        for _ in self.from_gdb.try_iter() {
            self.connexion = Connexion::Connected;
        }
    }
}