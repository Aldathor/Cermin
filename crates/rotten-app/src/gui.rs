#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Cermin GUI: pick a TV, connect, mirror. The mirror session itself runs on a
//! dedicated thread with its own Tokio runtime; the UI only sends commands and
//! drains status events.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use eframe::egui;
use rotten_core::config::{
    HwAccel, MirrorCipherMode, MirrorConfig, StreamConfig, resolve_credentials_path,
};
use rotten_core::device::AirPlayDevice;
use rotten_discovery::discover_for;
use rotten_pairing::PairingManager;

enum Cmd {
    Search,
    Connect(Box<AirPlayDevice>, Option<String>),
    Stop,
}

enum Ev {
    Searching,
    Devices(Vec<AirPlayDevice>),
    SessionStarting(String),
    Stopped(String),
    Error(String),
}

fn main() -> Result<(), eframe::Error> {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    spawn_worker(cmd_rx, ev_tx);

    rotten_protocol::set_tv_volume_percent(35);

    let mut app = CerminApp::new(cmd_tx, ev_rx);
    let _ = app.cmd_tx.send(Cmd::Search);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 520.0])
            .with_min_inner_size([420.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Cermin",
        options,
        Box::new(|_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    )
}

fn spawn_worker(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Ev>) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ev_tx.send(Ev::Error(format!("runtime: {e}")));
                return;
            }
        };

        let mut session: Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> = None;

        while let Ok(cmd) = cmd_rx.recv() {
            if let Some((handle, _)) = &session {
                if handle.is_finished() {
                    session = None;
                }
            }

            match cmd {
                Cmd::Search => {
                    let _ = ev_tx.send(Ev::Searching);
                    match rt.block_on(discover_for(Duration::from_secs(5))) {
                        Ok(devices) => {
                            let _ = ev_tx.send(Ev::Devices(devices));
                        }
                        Err(e) => {
                            let _ = ev_tx.send(Ev::Error(format!("discovery failed: {e}")));
                        }
                    }
                }
                Cmd::Connect(device, pin) => {
                    if session.is_some() {
                        continue;
                    }
                    let stop = Arc::new(AtomicBool::new(false));
                    let stop_thread = stop.clone();
                    let ev = ev_tx.clone();
                    let name = device.name.clone();
                    let handle = std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build();
                        let result = match rt {
                            Ok(rt) => rt.block_on(run_session(*device, pin, stop_thread)),
                            Err(e) => Err(format!("runtime: {e}")),
                        };
                        let reason = match result {
                            Ok(()) => "session ended".to_string(),
                            Err(e) => e,
                        };
                        let _ = ev.send(Ev::Stopped(reason));
                    });
                    session = Some((handle, stop));
                    let _ = ev_tx.send(Ev::SessionStarting(name));
                }
                Cmd::Stop => {
                    if let Some((handle, stop)) = session.take() {
                        stop.store(true, Ordering::Relaxed);
                        std::thread::spawn(move || {
                            let _ = handle.join();
                        });
                    }
                }
            }
        }
    });
}

async fn run_session(
    device: AirPlayDevice,
    pin: Option<String>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let config = MirrorConfig {
        stream: StreamConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 0,
        },
        pin,
        force_pair: false,
        test_mode: false,
        audio: true,
        hw_accel: HwAccel::Auto,
        credentials_path: resolve_credentials_path(None),
        display_index: None,
        virtual_display_only: false,
        no_encrypt: false,
        cipher: MirrorCipherMode::ChaCha,
    };
    rotten_app::mirror::run_mirror(device, config, stop)
        .await
        .map_err(|e| e.to_string())
}

struct CerminApp {
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    devices: Vec<AirPlayDevice>,
    selected: usize,
    status: String,
    log: Vec<String>,
    searching: bool,
    session_active: bool,
    needs_pin: bool,
    pin: String,
    volume: u32,
}

impl CerminApp {
    fn new(cmd_tx: Sender<Cmd>, ev_rx: Receiver<Ev>) -> Self {
        Self {
            cmd_tx,
            ev_rx,
            devices: Vec::new(),
            selected: 0,
            status: "Starting...".into(),
            log: Vec::new(),
            searching: false,
            session_active: false,
            needs_pin: false,
            pin: String::new(),
            volume: 35,
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn handle_events(&mut self) {
        loop {
            match self.ev_rx.try_recv() {
                Ok(Ev::Searching) => {
                    self.searching = true;
                    self.devices.clear();
                    self.selected = 0;
                    self.status = "Searching for TVs on your network...".into();
                }
                Ok(Ev::Devices(devices)) => {
                    self.searching = false;
                    self.status = if devices.is_empty() {
                        "No AirPlay TVs found. Check that the TV is on and on the same Wi-Fi.".into()
                    } else {
                        format!("Found {} device(s). Select one and press Connect.", devices.len())
                    };
                    self.devices = devices;
                    self.selected = 0;
                }
                Ok(Ev::SessionStarting(name)) => {
                    self.session_active = true;
                    self.status = format!("Connecting to {name}...");
                    self.push_log(format!("connecting to {name}"));
                }
                Ok(Ev::Stopped(reason)) => {
                    self.session_active = false;
                    self.status = format!("Stopped: {reason}");
                    self.push_log(format!("session stopped: {reason}"));
                }
                Ok(Ev::Error(e)) => {
                    self.searching = false;
                    self.status = format!("Error: {e}");
                    self.push_log(e);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn connect(&mut self) {
        let Some(device) = self.devices.get(self.selected).cloned() else {
            self.status = "Select a TV first.".into();
            return;
        };

        let has_credentials = PairingManager::load(resolve_credentials_path(None))
            .map(|m| m.has_credentials(&device.device_id))
            .unwrap_or(false);

        let pin = if has_credentials {
            None
        } else {
            let entered = self.pin.trim().to_string();
            if entered.is_empty() {
                self.needs_pin = true;
                self.status = "First time: enter the AirPlay code shown on the TV.".into();
                return;
            }
            Some(entered)
        };

        self.needs_pin = false;
        let _ = self.cmd_tx.send(Cmd::Connect(Box::new(device), pin));
    }
}

impl CerminApp {
    fn draw(&mut self, ui: &mut egui::Ui) {
        self.handle_events();

        {
            ui.horizontal(|ui| {
                ui.heading("Cermin");
                ui.weak("AirPlay mirroring — no cables");
            });
            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.searching && !self.session_active, egui::Button::new("Search"))
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Cmd::Search);
                }
                if self.session_active {
                    if ui.button("Disconnect").clicked() {
                        let _ = self.cmd_tx.send(Cmd::Stop);
                        self.status = "Stopping...".into();
                    }
                } else if ui
                    .add_enabled(!self.devices.is_empty() && !self.searching, egui::Button::new("Connect"))
                    .clicked()
                {
                    self.connect();
                }
            });

            ui.add_space(6.0);
            ui.label("TVs found:");
            egui::ScrollArea::vertical()
                .max_height(130.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if self.devices.is_empty() {
                        ui.weak("Press Search to look for AirPlay TVs.");
                    }
                    for (i, device) in self.devices.iter().enumerate() {
                        let label = format!("{}   ({}:{})", device.name, device.host, device.port);
                        if ui
                            .selectable_label(self.selected == i, label)
                            .clicked()
                        {
                            self.selected = i;
                            self.needs_pin = false;
                        }
                    }
                });

            if self.needs_pin {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("AirPlay code:");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.pin)
                            .desired_width(90.0)
                            .hint_text("1234"),
                    );
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.connect();
                    }
                });
            }

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("TV volume");
                let slider = ui.add(
                    egui::Slider::new(&mut self.volume, 0..=100)
                        .suffix("%")
                        .fixed_decimals(0),
                );
                if slider.changed() {
                    rotten_protocol::set_tv_volume_percent(self.volume);
                }
                if slider.drag_stopped() {
                    self.push_log(format!("TV volume set to {}%", self.volume));
                }
            });

            ui.add_space(4.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Status:");
                ui.strong(&self.status);
            });

            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.weak(line);
                    }
                });
        }
    }
}

impl eframe::App for CerminApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}
