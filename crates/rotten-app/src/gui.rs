#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Cermin GUI: pick a TV, connect, mirror. The mirror session itself runs on a
//! dedicated thread with its own Tokio runtime; the UI only sends commands and
//! drains status events.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Layout, Margin, Pos2,
    Rect, RichText, ScrollArea, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use rotten_core::config::{
    HwAccel, MirrorCipherMode, MirrorConfig, StreamConfig, resolve_credentials_path,
};
use rotten_core::device::AirPlayDevice;
use rotten_discovery::discover_for;
use rotten_pairing::PairingManager;

// ---- palette (matches the mockup) -----------------------------------------

const BG: Color32 = Color32::from_rgb(0xF1, 0xF5, 0xFB);
const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
const BORDER: Color32 = Color32::from_rgb(0xE3, 0xE8, 0xF2);
const ACCENT: Color32 = Color32::from_rgb(0x1F, 0x6F, 0xE5);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x1A, 0x60, 0xC8);
const TEXT: Color32 = Color32::from_rgb(0x14, 0x1B, 0x2B);
const MUTED: Color32 = Color32::from_rgb(0x64, 0x6E, 0x80);
const FAINT: Color32 = Color32::from_rgb(0x94, 0x9C, 0xAB);
const DISABLED_BG: Color32 = Color32::from_rgb(0xE7, 0xEB, 0xF1);
const DISABLED_FG: Color32 = Color32::from_rgb(0xA0, 0xA8, 0xB4);
const SEL_BG: Color32 = Color32::from_rgb(0xE7, 0xF0, 0xFD);
const GREEN: Color32 = Color32::from_rgb(0x22, 0xB1, 0x5A);
const AMBER: Color32 = Color32::from_rgb(0xE0, 0x9B, 0x1E);
const RED: Color32 = Color32::from_rgb(0xDC, 0x35, 0x45);
const LOG_BG: Color32 = Color32::from_rgb(0xF8, 0xFA, 0xFC);

// ---- commands / events ------------------------------------------------------

enum Cmd {
    Search,
    Connect(Box<AirPlayDevice>, Option<String>),
    Stop,
}

enum Ev {
    Searching,
    Devices(Vec<AirPlayDevice>),
    SessionStarting(String),
    Streaming,
    Stopped(String),
    Error(String),
}

fn timestamp() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

fn main() -> Result<(), eframe::Error> {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let worker = spawn_worker(cmd_rx, ev_tx);

    rotten_protocol::set_tv_volume_percent(35);

    let mut app = CerminApp::new(cmd_tx, ev_rx);
    let _ = app.cmd_tx.send(Cmd::Search);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 820.0])
            .with_min_inner_size([560.0, 560.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Cermin",
        options,
        Box::new(|_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    );
    // Closing the window drops the command sender. Wait for the worker to stop
    // the session and restore local audio before the GUI process exits.
    let _ = worker.join();
    result
}

fn spawn_worker(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Ev>) -> std::thread::JoinHandle<()> {
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

        let mut session: Option<(std::thread::JoinHandle<Result<(), String>>, Arc<AtomicBool>)> =
            None;

        loop {
            if session
                .as_ref()
                .is_some_and(|(handle, _)| handle.is_finished())
            {
                let (handle, _) = session.take().expect("finished session");
                let reason = match handle.join() {
                    Ok(Ok(())) => "session ended".to_string(),
                    Ok(Err(error)) => error,
                    Err(_) => "session worker panicked".to_string(),
                };
                // Announce completion only after releasing session ownership,
                // so Connect can always accept a retry following Stopped.
                let _ = ev_tx.send(Ev::Stopped(reason));
            }

            let cmd = match cmd_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
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
                    let ev_stream = ev_tx.clone();
                    let name = device.name.clone();
                    // Publish this before launching the session: immediate
                    // failures and first-frame events must follow Starting.
                    let _ = ev_tx.send(Ev::SessionStarting(name));
                    let handle = std::thread::Builder::new()
                        .name("mirror-session".into())
                        .spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build();
                            let first_frame: Option<Box<dyn FnOnce() + Send>> =
                                Some(Box::new(move || {
                                    let _ = ev_stream.send(Ev::Streaming);
                                }));
                            match rt {
                                Ok(rt) => run_on_session_runtime(
                                    rt,
                                    run_session(*device, pin, stop_thread, first_frame),
                                ),
                                Err(e) => Err(format!("runtime: {e}")),
                            }
                        });
                    match handle {
                        Ok(handle) => session = Some((handle, stop)),
                        Err(error) => {
                            let _ = ev_tx.send(Ev::Stopped(format!(
                                "could not start session worker: {error}"
                            )));
                        }
                    }
                }
                Cmd::Stop => {
                    if let Some((_, stop)) = &session {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        if let Some((handle, stop)) = session {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    })
}

fn run_on_session_runtime<T>(
    runtime: tokio::runtime::Runtime,
    session: impl std::future::Future<Output = T>,
) -> T {
    let result = runtime.block_on(session);
    // Session/audio cleanup is complete. A cancelled blocking capture/PIN read
    // must not hold window close or a subsequent connection open forever.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn completed_session_does_not_wait_forever_for_blocking_work() {
        let (release_tx, release_rx) = channel::<()>();
        let (started_tx, started_rx) = channel();
        let (done_tx, done_rx) = channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = run_on_session_runtime(runtime, async {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    ready_tx.send(()).unwrap();
                    // Simulate a capture driver or stdin read that ignores
                    // cancellation after the session has finished cleanup.
                    let _ = release_rx.recv();
                });
                ready_rx.await.unwrap();
                started_tx.send(()).unwrap();
                Err::<(), _>("expected session failure")
            });
            done_tx.send(result).unwrap();
        });

        let started = started_rx.recv_timeout(Duration::from_secs(5));
        let result = done_rx.recv_timeout(Duration::from_secs(5));
        // Release the simulated driver even if the shutdown regression returns,
        // so a failing test leaves no stuck worker behind.
        let _ = release_tx.send(());
        worker.join().unwrap();
        started.expect("blocking work did not start");
        assert_eq!(
            result.expect("completed session was held open by blocking work"),
            Err("expected session failure")
        );
    }
}

async fn run_session(
    device: AirPlayDevice,
    pin: Option<String>,
    stop: Arc<AtomicBool>,
    on_first_frame: Option<Box<dyn FnOnce() + Send + 'static>>,
) -> Result<(), String> {
    let config = MirrorConfig {
        stream: StreamConfig {
            width: 0,
            height: 0,
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
    rotten_app::mirror::run_mirror(device, config, stop, on_first_frame)
        .await
        .map_err(|e| e.to_string())
}

// ---- icons (painted, no font dependency) ------------------------------------

#[derive(Clone, Copy)]
enum Icon {
    Monitor,
    Magnifier,
    Refresh,
    Speaker,
    Trash,
}

fn paint_icon(p: &egui::Painter, icon: Icon, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.7, color);
    match icon {
        Icon::Monitor => {
            let body = Rect::from_center_size(
                pos2(rect.center().x, rect.center().y - rect.height() * 0.10),
                vec2(rect.width() * 0.86, rect.height() * 0.60),
            );
            p.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
            let stand_y = body.bottom();
            p.line_segment(
                [
                    pos2(body.center().x, stand_y),
                    pos2(body.center().x, stand_y + rect.height() * 0.12),
                ],
                stroke,
            );
            p.line_segment(
                [
                    pos2(
                        body.center().x - rect.width() * 0.20,
                        stand_y + rect.height() * 0.12,
                    ),
                    pos2(
                        body.center().x + rect.width() * 0.20,
                        stand_y + rect.height() * 0.12,
                    ),
                ],
                stroke,
            );
        }
        Icon::Magnifier => {
            let c = pos2(
                rect.center().x - rect.width() * 0.08,
                rect.center().y - rect.height() * 0.10,
            );
            let r = rect.width() * 0.27;
            p.circle_stroke(c, r, stroke);
            p.line_segment(
                [
                    pos2(c.x + r * 0.72, c.y + r * 0.72),
                    pos2(
                        rect.right() - rect.width() * 0.16,
                        rect.bottom() - rect.height() * 0.16,
                    ),
                ],
                stroke,
            );
        }
        Icon::Refresh => {
            let c = rect.center();
            let r = rect.width() * 0.30;
            let start = 45f32.to_radians();
            let end = 330f32.to_radians();
            let mut points: Vec<Pos2> = Vec::new();
            for i in 0..=24 {
                let a = start + (end - start) * (i as f32 / 24.0);
                points.push(pos2(c.x + r * a.cos(), c.y + r * a.sin()));
            }
            p.add(Shape::line(points, stroke));
            let tip = pos2(c.x + r * start.cos(), c.y + r * start.sin());
            let s = rect.width() * 0.17;
            let tangent = vec2(-start.sin(), start.cos());
            let inward = vec2(-start.cos(), -start.sin());
            p.add(Shape::convex_polygon(
                vec![
                    tip + tangent * s * 0.9,
                    tip - tangent * s * 0.9,
                    tip + inward * s * 1.1,
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Speaker => {
            let cone = Rect::from_center_size(
                pos2(rect.center().x - rect.width() * 0.24, rect.center().y),
                vec2(rect.width() * 0.20, rect.height() * 0.34),
            );
            p.rect_filled(cone, CornerRadius::same(1), color);
            p.add(Shape::convex_polygon(
                vec![
                    pos2(cone.right(), cone.top()),
                    pos2(
                        rect.center().x + rect.width() * 0.04,
                        rect.top() + rect.height() * 0.18,
                    ),
                    pos2(
                        rect.center().x + rect.width() * 0.04,
                        rect.bottom() - rect.height() * 0.18,
                    ),
                    pos2(cone.right(), cone.bottom()),
                ],
                color,
                Stroke::NONE,
            ));
            let c = pos2(rect.center().x, rect.center().y);
            for rr in [rect.width() * 0.18, rect.width() * 0.32] {
                let mut points: Vec<Pos2> = Vec::new();
                for k in 0..=12 {
                    let a = (-50.0 + 100.0 * (k as f32 / 12.0)).to_radians();
                    points.push(pos2(c.x + rr * a.cos(), c.y + rr * a.sin()));
                }
                p.add(Shape::line(points, Stroke::new(1.5, color)));
            }
        }
        Icon::Trash => {
            let w = rect.width() * 0.56;
            let h = rect.height() * 0.52;
            let body = Rect::from_center_size(
                pos2(rect.center().x, rect.center().y + rect.height() * 0.10),
                vec2(w, h),
            );
            p.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
            let lid_y = body.top() - rect.height() * 0.06;
            p.line_segment(
                [
                    pos2(rect.center().x - w * 0.75, lid_y),
                    pos2(rect.center().x + w * 0.75, lid_y),
                ],
                stroke,
            );
            p.line_segment(
                [
                    pos2(rect.center().x - w * 0.22, lid_y - rect.height() * 0.10),
                    pos2(rect.center().x + w * 0.22, lid_y - rect.height() * 0.10),
                ],
                stroke,
            );
            for dx in [-w * 0.20, w * 0.20] {
                p.line_segment(
                    [
                        pos2(rect.center().x + dx, body.top() + h * 0.18),
                        pos2(rect.center().x + dx, body.bottom() - h * 0.18),
                    ],
                    Stroke::new(1.3, color),
                );
            }
        }
    }
}

fn draw_logo(p: &egui::Painter, rect: Rect) {
    let blue = Color32::from_rgb(0x2E, 0x86, 0xF0);
    let arrow_blue = Color32::from_rgb(0x14, 0x4E, 0xA6);
    p.rect_filled(rect, CornerRadius::same(12), blue);
    let screen = Rect::from_center_size(
        pos2(
            rect.center().x + rect.width() * 0.06,
            rect.center().y - rect.height() * 0.07,
        ),
        vec2(rect.width() * 0.58, rect.height() * 0.42),
    );
    p.rect_filled(screen, CornerRadius::same(3), Color32::WHITE);
    let stand = Rect::from_center_size(
        pos2(screen.center().x, screen.bottom() + rect.height() * 0.10),
        vec2(rect.width() * 0.24, rect.height() * 0.08),
    );
    p.rect_filled(stand, CornerRadius::same(2), Color32::WHITE);
    // arrow entering the screen from the lower left (dark blue on the screen)
    let tip = pos2(
        screen.left() + screen.width() * 0.62,
        screen.center().y + screen.height() * 0.05,
    );
    let t = screen.height() * 0.62;
    p.add(Shape::convex_polygon(
        vec![
            tip,
            tip + vec2(-t * 1.15, -t * 0.30),
            tip + vec2(-t * 1.15, t * 0.40),
        ],
        arrow_blue,
        Stroke::NONE,
    ));
    p.rect_filled(
        Rect::from_min_size(
            pos2(tip.x - t * 2.1, tip.y - t * 0.02),
            vec2(t * 1.0, t * 0.14),
        ),
        CornerRadius::same(1),
        arrow_blue,
    );
}

// ---- reusable widgets -------------------------------------------------------

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .show(ui, add)
        .inner
}

fn action_button(
    ui: &mut Ui,
    label: &str,
    icon: Option<Icon>,
    primary: bool,
    enabled: bool,
    min_width: f32,
) -> egui::Response {
    let font = FontId::proportional(14.5);
    let color = if !enabled {
        DISABLED_FG
    } else if primary {
        Color32::WHITE
    } else {
        MUTED
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), color);
    let icon_w = if icon.is_some() { 17.0 } else { 0.0 };
    let gap = if icon.is_some() { 8.0 } else { 0.0 };
    let content_w = galley.size().x + icon_w + gap;
    let height = 38.0;
    let width = min_width.max(content_w + 30.0);
    let (rect, response) = ui.allocate_exact_size(
        vec2(width, height),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );

    let hovered = enabled && response.hovered();
    let fill = if !enabled {
        DISABLED_BG
    } else if primary {
        if hovered { ACCENT_HOVER } else { ACCENT }
    } else if hovered {
        Color32::from_rgb(0xEC, 0xF0, 0xF7)
    } else {
        Color32::from_rgb(0xF3, 0xF5, 0xF9)
    };

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(9), fill);
    if !primary {
        p.rect_stroke(
            rect,
            CornerRadius::same(9),
            Stroke::new(1.0, BORDER),
            StrokeKind::Inside,
        );
    }

    let mut x = rect.center().x - content_w / 2.0;
    if let Some(icon) = icon {
        paint_icon(
            p,
            icon,
            Rect::from_center_size(
                pos2(x + icon_w / 2.0, rect.center().y),
                vec2(icon_w, icon_w),
            ),
            color,
        );
        x += icon_w + gap;
    }
    p.text(
        pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        color,
    );

    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

fn icon_button(ui: &mut Ui, icon: Icon, size: f32, enabled: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::splat(size),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = enabled && response.hovered();
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(9),
        if hovered {
            Color32::from_rgb(0xEC, 0xF0, 0xF7)
        } else {
            Color32::from_rgb(0xF3, 0xF5, 0xF9)
        },
    );
    p.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        icon,
        rect.shrink(size * 0.24),
        if enabled { MUTED } else { DISABLED_FG },
    );
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

fn dot(ui: &mut Ui, color: Color32, diameter: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(diameter + 4.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), diameter / 2.0, color);
}

fn pill(ui: &mut Ui, label: &str, color: Color32, bg: Color32) {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(100))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color, 9.0);
                ui.label(RichText::new(label).size(12.5).color(color));
            });
        });
}

fn device_row(ui: &mut Ui, device: &AirPlayDevice, selected: bool) -> egui::Response {
    let narrow = ui.available_width() < 330.0;
    let ir = Frame::new()
        .fill(if selected { SEL_BG } else { CARD })
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                paint_icon(
                    ui.painter(),
                    Icon::Monitor,
                    rect.shrink(2.0),
                    if selected { ACCENT } else { MUTED },
                );
                ui.add_space(6.0);
                let subtitle = {
                    let mut line = format!("{}:{}", device.host, device.port);
                    if let (true, Some(model)) = (narrow, device.model.as_ref()) {
                        line.push_str(&format!("  ·  {model}"));
                    }
                    line
                };
                if narrow {
                    ui.vertical(|ui| {
                        ui.add_space(1.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(&device.name).size(15.0).strong().color(TEXT),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(subtitle).size(12.5).color(MUTED))
                                .truncate(),
                        );
                    });
                } else {
                    let badge_width = 78.0;
                    let info_width = (ui.available_width() - badge_width - 8.0).max(90.0);
                    ui.allocate_ui_with_layout(
                        vec2(info_width, 36.0),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.add_space(1.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&device.name).size(15.0).strong().color(TEXT),
                                )
                                .truncate(),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(subtitle).size(12.5).color(MUTED))
                                    .truncate(),
                            );
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(model) = &device.model {
                            ui.label(RichText::new(model).size(12.5).color(FAINT));
                        }
                    });
                }
            });
        });

    let response = ir.response.interact(Sense::click());
    if response.hovered() && !selected {
        ui.painter().rect_filled(
            ir.response.rect,
            CornerRadius::same(9),
            Color32::from_rgba_unmultiplied(31, 111, 229, 10),
        );
    }
    if selected {
        let r = ir.response.rect;
        ui.painter().rect_filled(
            Rect::from_min_size(pos2(r.left(), r.top() + 7.0), vec2(3.0, r.height() - 14.0)),
            CornerRadius::same(2),
            ACCENT,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

// ---- app --------------------------------------------------------------------

struct CerminApp {
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    devices: Vec<AirPlayDevice>,
    selected: usize,
    log: Vec<String>,
    searching: bool,
    session_active: bool,
    streaming: bool,
    needs_pin: bool,
    focus_pin: bool,
    pin: String,
    volume: u32,
    status_title: String,
    status_desc: String,
    status_color: Color32,
    styled: bool,
}

impl CerminApp {
    fn new(cmd_tx: Sender<Cmd>, ev_rx: Receiver<Ev>) -> Self {
        let mut app = Self {
            cmd_tx,
            ev_rx,
            devices: Vec::new(),
            selected: 0,
            log: Vec::new(),
            searching: false,
            session_active: false,
            streaming: false,
            needs_pin: false,
            focus_pin: false,
            pin: String::new(),
            volume: 35,
            status_title: "Ready".into(),
            status_desc: "Press Search to look for AirPlay TVs.".into(),
            status_color: GREEN,
            styled: false,
        };
        app.push_log("Cermin started");
        app
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(format!("{}  {}", timestamp(), line.into()));
        if self.log.len() > 300 {
            self.log.remove(0);
        }
    }

    fn search(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Search);
    }

    fn disconnect(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        self.status_title = "Stopping".into();
        self.status_desc = "Closing the session...".into();
        self.status_color = MUTED;
    }

    fn connect(&mut self) {
        let Some(device) = self.devices.get(self.selected).cloned() else {
            self.status_title = "No TV selected".into();
            self.status_desc = "Pick a TV from the list first.".into();
            self.status_color = AMBER;
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
                self.focus_pin = true;
                self.status_title = "AirPlay code needed".into();
                self.status_desc = "Enter the code shown on your TV.".into();
                self.status_color = AMBER;
                return;
            }
            Some(entered)
        };

        self.needs_pin = false;
        let _ = self.cmd_tx.send(Cmd::Connect(Box::new(device), pin));
    }

    fn handle_events(&mut self) {
        loop {
            match self.ev_rx.try_recv() {
                Ok(Ev::Searching) => {
                    self.searching = true;
                    self.devices.clear();
                    self.selected = 0;
                    self.status_title = "Searching".into();
                    self.status_desc = "Looking for AirPlay TVs on your network...".into();
                    self.status_color = ACCENT;
                    self.push_log("Searching for TVs on your network...");
                }
                Ok(Ev::Devices(devices)) => {
                    self.searching = false;
                    self.devices = devices;
                    self.selected = 0;
                    if self.devices.is_empty() {
                        self.status_title = "No TVs found".into();
                        self.status_desc =
                            "Check that the TV is on and on the same Wi-Fi, then search again."
                                .into();
                        self.status_color = AMBER;
                        self.push_log("Found 0 device(s).");
                    } else {
                        self.status_title = "Ready".into();
                        self.status_desc = format!(
                            "Found {} device(s). Select one and press Connect.",
                            self.devices.len()
                        );
                        self.status_color = GREEN;
                        self.push_log(format!(
                            "Found {} device(s). Select one and press Connect.",
                            self.devices.len()
                        ));
                    }
                }
                Ok(Ev::SessionStarting(name)) => {
                    self.session_active = true;
                    self.streaming = false;
                    self.status_title = "Connecting".into();
                    self.status_desc = format!("Setting up the session with {name}...");
                    self.status_color = ACCENT;
                    self.push_log(format!("Connecting to {name}..."));
                }
                Ok(Ev::Streaming) => {
                    self.streaming = true;
                    self.status_title = "Mirroring".into();
                    self.status_desc = "Your screen is on the TV.".into();
                    self.status_color = GREEN;
                    self.push_log("Streaming started.");
                }
                Ok(Ev::Stopped(reason)) => {
                    self.session_active = false;
                    self.streaming = false;
                    self.status_title = "Stopped".into();
                    self.status_desc = reason.clone();
                    self.status_color = MUTED;
                    self.push_log(format!("Session stopped: {reason}"));
                }
                Ok(Ev::Error(e)) => {
                    self.searching = false;
                    self.status_title = "Error".into();
                    self.status_desc = e.clone();
                    self.status_color = RED;
                    self.push_log(e);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    // ---- sections -----------------------------------------------------------

    fn header(&mut self, ui: &mut Ui) {
        let show_tagline = ui.available_width() > 620.0;
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
            draw_logo(ui.painter(), rect);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new("Cermin").size(30.0).strong().color(TEXT));
                ui.label(
                    RichText::new("AirPlay mirroring — no cables")
                        .size(13.5)
                        .color(MUTED),
                );
            });
            if show_tagline {
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.add_space(7.0);
                        ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                            ui.label(
                                RichText::new("Your PC screen. On your TV.")
                                    .size(13.0)
                                    .color(MUTED),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                            ui.label(
                                RichText::new("No cables. No accounts. Just works.")
                                    .size(13.0)
                                    .color(MUTED),
                            );
                        });
                    });
                });
            }
        });
    }

    fn devices_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            let wide = ui.available_width() > 430.0;
            if wide {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("TVs found on your network")
                                .size(16.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new("Select a TV and press Connect.")
                                .size(12.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let enabled = !self.searching && !self.session_active;
                        if action_button(ui, "Search", Some(Icon::Magnifier), true, enabled, 104.0)
                            .clicked()
                        {
                            self.search();
                        }
                        if icon_button(ui, Icon::Refresh, 36.0, enabled).clicked() {
                            self.search();
                        }
                    });
                });
            } else {
                ui.label(
                    RichText::new("TVs found on your network")
                        .size(16.0)
                        .strong()
                        .color(TEXT),
                );
                ui.label(
                    RichText::new("Select a TV and press Connect.")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let enabled = !self.searching && !self.session_active;
                    if action_button(ui, "Search", Some(Icon::Magnifier), true, enabled, 104.0)
                        .clicked()
                    {
                        self.search();
                    }
                    if icon_button(ui, Icon::Refresh, 36.0, enabled).clicked() {
                        self.search();
                    }
                });
            }
            ui.add_space(8.0);
            ScrollArea::vertical()
                .max_height(280.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.devices.is_empty() {
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("Press Search to look for AirPlay TVs.")
                                .size(13.0)
                                .color(FAINT),
                        );
                        ui.add_space(10.0);
                    }
                    for i in 0..self.devices.len() {
                        let selected = i == self.selected;
                        let device = self.devices[i].clone();
                        if device_row(ui, &device, selected).clicked() {
                            self.selected = i;
                            self.needs_pin = false;
                        }
                        ui.add_space(2.0);
                    }
                });
        });
    }

    fn connection_card(&mut self, ui: &mut Ui) {
        let selected_device = self.devices.get(self.selected).cloned();
        let session_active = self.session_active;
        let streaming = self.streaming;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Connection").size(16.0).strong().color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if streaming {
                        pill(ui, "Mirroring", GREEN, Color32::from_rgb(0xE8, 0xF7, 0xEE));
                    } else if session_active {
                        pill(
                            ui,
                            "Connecting",
                            ACCENT,
                            Color32::from_rgb(0xE8, 0xF0, 0xFD),
                        );
                    } else {
                        pill(
                            ui,
                            "Not connected",
                            MUTED,
                            Color32::from_rgb(0xF1, 0xF3, 0xF6),
                        );
                    }
                });
            });
            ui.add_space(10.0);

            match &selected_device {
                Some(device) => {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::hover());
                        paint_icon(ui.painter(), Icon::Monitor, rect.shrink(3.0), ACCENT);
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&device.name).size(17.0).strong().color(TEXT));
                            let mut line = format!("{}:{}", device.host, device.port);
                            if let Some(model) = &device.model {
                                line.push_str(&format!("  ·  {model}"));
                            }
                            ui.label(RichText::new(line).size(13.0).color(MUTED));
                            ui.label(
                                RichText::new(format!("Device ID: {}", device.device_id))
                                    .size(12.0)
                                    .color(FAINT),
                            );
                        });
                    });
                }
                None => {
                    ui.label(
                        RichText::new("No TV selected — pick one from the list.")
                            .size(13.0)
                            .color(FAINT),
                    );
                }
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            if ui.available_width() > 430.0 {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("AirPlay code")
                            .size(13.5)
                            .strong()
                            .color(TEXT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new("First time: enter the code shown on your TV.")
                                .size(11.5)
                                .color(FAINT),
                        );
                    });
                });
            } else {
                ui.label(
                    RichText::new("AirPlay code")
                        .size(13.5)
                        .strong()
                        .color(TEXT),
                );
                ui.label(
                    RichText::new("First time: enter the code shown on your TV.")
                        .size(11.5)
                        .color(FAINT),
                );
            }
            ui.add_space(4.0);
            let text_edit = egui::TextEdit::singleline(&mut self.pin)
                .hint_text("Enter 4-digit code (e.g. 1234)")
                .font(FontId::proportional(14.0));
            let width = ui.available_width();
            let response = ui.add_sized([width, 36.0], text_edit);
            if self.focus_pin {
                response.request_focus();
                self.focus_pin = false;
            }

            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                let total = ui.available_width();
                let gap = 10.0;
                let connect_w = ((total - gap) * 0.56).max(140.0);
                let disconnect_w = (total - gap - connect_w).max(110.0);
                let can_connect = selected_device.is_some() && !session_active && !self.searching;
                if action_button(
                    ui,
                    "Connect",
                    Some(Icon::Monitor),
                    true,
                    can_connect,
                    connect_w,
                )
                .clicked()
                {
                    self.connect();
                }
                if action_button(ui, "Disconnect", None, false, session_active, disconnect_w)
                    .clicked()
                {
                    self.disconnect();
                }
            });
        });
    }

    fn volume_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                paint_icon(ui.painter(), Icon::Speaker, rect.shrink(1.0), TEXT);
                ui.add_space(6.0);
                ui.label(RichText::new("TV volume").size(16.0).strong().color(TEXT));
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let value_width = 70.0;
                let slider_width = (ui.available_width() - value_width - 14.0).max(60.0);
                let mut volume = self.volume;
                let response = ui.add_sized(
                    [slider_width, 22.0],
                    egui::Slider::new(&mut volume, 0..=100).show_value(false),
                );
                if response.changed() {
                    self.volume = volume;
                    rotten_protocol::set_tv_volume_percent(volume);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} %", self.volume))
                            .size(20.0)
                            .strong()
                            .color(TEXT),
                    );
                });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("Sets the TV's volume for this session (0 – 100 %).")
                    .size(11.5)
                    .color(FAINT),
            );
        });
    }

    fn status_card(&mut self, ui: &mut Ui) {
        let title = self.status_title.clone();
        let desc = self.status_desc.clone();
        let color = self.status_color;
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                dot(ui, color, 13.0);
                ui.add_space(2.0);
                ui.label(RichText::new(title).size(16.0).strong().color(TEXT));
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(21.0);
                ui.label(RichText::new(desc).size(13.0).color(MUTED));
            });
        });
    }

    fn log_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Log").size(15.0).strong().color(TEXT));
                if action_button(ui, "Clear", Some(Icon::Trash), false, true, 92.0).clicked() {
                    self.log.clear();
                }
            });
            ui.add_space(4.0);
            Frame::new()
                .fill(LOG_BG)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .max_height(120.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if self.log.is_empty() {
                                ui.label(
                                    RichText::new("No events yet.")
                                        .font(FontId::monospace(11.5))
                                        .color(FAINT),
                                );
                            }
                            for line in &self.log {
                                ui.label(
                                    RichText::new(line)
                                        .font(FontId::monospace(11.5))
                                        .color(MUTED),
                                );
                            }
                        });
                });
        });
    }
}

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::light();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = CARD;
        style.visuals.extreme_bg_color = Color32::WHITE;
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.spacing.button_padding = vec2(12.0, 7.0);
        style.spacing.slider_width = 200.0;
    });
}

impl eframe::App for CerminApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            apply_style(ctx);
            self.styled = true;
        }
        self.handle_events();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: 16,
                        right: 16,
                        top: 6,
                        bottom: 10,
                    })
                    .show(ui, |ui| {
                        self.draw_content(ui);
                    });
            });

        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

impl CerminApp {
    fn draw_content(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        self.header(ui);
        ui.add_space(12.0);

        let total = ui.available_width();
        let gap = 12.0;
        // Below ~780 px the two columns get cramped and controls start to
        // collide, so stack the cards vertically instead.
        let stacked = total < 780.0;
        if stacked {
            self.devices_card(ui);
            ui.add_space(10.0);
            self.connection_card(ui);
            ui.add_space(10.0);
            self.volume_card(ui);
        } else {
            let mut left_width = (total - gap) * 0.46;
            let mut right_width = total - gap - left_width;
            if right_width < 360.0 {
                right_width = 360.0;
                left_width = total - gap - right_width;
            }
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(left_width);
                    self.devices_card(ui);
                });
                ui.add_space(gap);
                ui.vertical(|ui| {
                    ui.set_width(right_width);
                    self.connection_card(ui);
                    ui.add_space(10.0);
                    self.volume_card(ui);
                });
            });
        }

        ui.add_space(12.0);
        self.status_card(ui);
        ui.add_space(12.0);
        self.log_card(ui);
        ui.add_space(4.0);
    }
}
