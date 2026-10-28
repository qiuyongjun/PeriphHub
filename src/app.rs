use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke, Ui, Vec2,
};

use crate::hid::{
    Device, Snapshot, A_DEB_PRESS, A_DPI_X, A_DPI_Y, A_FLAGS, A_LOD, A_POLL, A_STAGE_ACTIVE,
};

const BG: Color32 = Color32::from_rgb(10, 13, 18);
const CARD: Color32 = Color32::from_rgb(19, 24, 32);
const TRACK: Color32 = Color32::from_rgb(30, 37, 47);
const TEXT: Color32 = Color32::from_rgb(229, 235, 243);
const DIM: Color32 = Color32::from_rgb(128, 140, 156);
const ACCENT: Color32 = Color32::from_rgb(64, 224, 208);
const DANGER: Color32 = Color32::from_rgb(240, 96, 96);

const FULL_EVERY: Duration = Duration::from_secs(4);
const BATTERY_EVERY: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
pub enum Cmd {
    Refresh,
    Polling(u8),
    Motion(bool),
    Line(bool),
    Wave(bool),
    Debounce(u8, bool),
    Sleep(u8),
    Lod(u8),
    Activate(u8),
    Dpi(u8, u16),
}

/// Which block needs re-reading after a command, so we avoid a full snapshot.
#[derive(Clone, Copy)]
enum Change {
    Poll,
    Lm,
    Deb,
    Active,
    Dpi(u8),
}

struct Msg {
    snap: Option<Snapshot>,
    battery: Option<u8>,
    connected: bool,
    error: Option<String>,
}

pub struct Panel {
    tx: Sender<Cmd>,
    rx: Receiver<Msg>,
    snap: Option<Snapshot>,
    battery: u8,
    connected: bool,
    error: Option<String>,
    synced: Option<Instant>,
    // local mirrors for the numeric steppers
    lod: u8,
    deb_press: u8,
    deb_release: u8,
    sleep_min: u8,
}

fn apply(dev: &Device, cmd: Cmd) -> Result<Option<Change>, String> {
    let change = match cmd {
        Cmd::Refresh => return Ok(None),
        Cmd::Polling(ms) => {
            let mut b = dev.read(A_POLL, 4)?;
            b[0] = ms;
            dev.write_commit(A_POLL, &b)?;
            Change::Poll
        }
        Cmd::Motion(on) => {
            let mut b = dev.read(A_LOD, 4)?;
            b[1] = on as u8;
            dev.write_commit(A_LOD, &b)?;
            Change::Lm
        }
        Cmd::Lod(v) => {
            let mut b = dev.read(A_LOD, 4)?;
            b[0] = v;
            dev.write_commit(A_LOD, &b)?;
            Change::Lm
        }
        Cmd::Line(on) => {
            set_flag(dev, 0x01, !on)?;
            Change::Deb
        }
        Cmd::Wave(on) => {
            set_flag(dev, 0x02, !on)?;
            Change::Deb
        }
        Cmd::Debounce(v, press) => {
            let mut b = dev.read(A_DEB_PRESS, 4)?;
            b[if press { 0 } else { 1 }] = v;
            dev.write_commit(A_DEB_PRESS, &b)?;
            Change::Deb
        }
        Cmd::Sleep(m) => {
            let mut b = dev.read(A_DEB_PRESS, 4)?;
            b[2] = m;
            dev.write_commit(A_DEB_PRESS, &b)?;
            Change::Deb
        }
        Cmd::Activate(i) => {
            let mut b = dev.read(A_STAGE_ACTIVE, 4)?;
            b[0] = i;
            dev.write_commit(A_STAGE_ACTIVE, &b)?;
            Change::Active
        }
        Cmd::Dpi(stage, value) => {
            let bytes = value.to_le_bytes();
            dev.write_field(A_DPI_X + stage as u32 * 2, &bytes)?;
            dev.write_field(A_DPI_Y + stage as u32 * 2, &bytes)?;
            dev.commit()?;
            Change::Dpi(stage)
        }
    };
    Ok(Some(change))
}

fn set_flag(dev: &Device, bit: u8, disabled: bool) -> Result<(), String> {
    let mut b = dev.read(A_FLAGS - 3, 4)?;
    if disabled {
        b[3] |= bit;
    } else {
        b[3] &= !bit;
    }
    dev.write_commit(A_FLAGS - 3, &b)
}

fn patch(dev: &Device, change: Change, s: &mut Snapshot) -> Result<(), String> {
    match change {
        Change::Poll => {
            let b = dev.read(A_POLL, 4)?;
            s.polling_ms = b[0];
        }
        Change::Lm => {
            let b = dev.read(A_LOD, 4)?;
            s.lod = b[0];
            s.motion = b[1] != 0;
        }
        Change::Deb => {
            let b = dev.read(A_DEB_PRESS, 8)?;
            s.debounce_press = b[0];
            s.debounce_release = b[1];
            s.sleep_min = b[2];
            s.line_corr = b[3] & 0x01 == 0;
            s.wave_corr = b[3] & 0x02 == 0;
        }
        Change::Active => {
            let b = dev.read(A_STAGE_ACTIVE, 4)?;
            s.stage_active = b[0].min(s.stage_count.saturating_sub(1));
        }
        Change::Dpi(i) => {
            let b = dev.read(A_DPI_X + i as u32 * 2, 2)?;
            if let Some(d) = s.dpi.get_mut(i as usize) {
                *d = u16::from_le_bytes([b[0], b[1]]);
            }
        }
    }
    Ok(())
}

fn worker(rx: Receiver<Cmd>, tx: Sender<Msg>) {
    let mut dev: Option<Device> = None;
    let mut snap: Option<Snapshot> = None;
    let mut last_full = Instant::now();
    let mut last_batt = Instant::now();

    loop {
        if dev.is_none() {
            if let Ok(d) = Device::open() {
                dev = Some(d);
                last_full = Instant::now() - FULL_EVERY;
                last_batt = Instant::now() - BATTERY_EVERY;
            }
        }

        let cmd = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => Some(c),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let Some(d) = dev.as_ref() else {
            let _ = tx.send(Msg {
                snap: None,
                battery: None,
                connected: false,
                error: Some("未检测到 VT1 Pro，请检查接收器".into()),
            });
            std::thread::sleep(Duration::from_millis(600));
            continue;
        };

        let mut error = None;
        let mut changed = false;

        if let Some(cmd) = cmd {
            if let Cmd::Refresh = cmd {
                last_full = Instant::now() - FULL_EVERY;
            } else {
                match apply(d, cmd) {
                    Ok(Some(change)) => {
                        if let Some(s) = snap.as_mut() {
                            match patch(d, change, s) {
                                Ok(()) => changed = true,
                                Err(e) => error = Some(e),
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => error = Some(e),
                }
            }
        }

        if snap.is_none() || last_full.elapsed() >= FULL_EVERY {
            match d.snapshot() {
                Ok(s) => {
                    snap = Some(s);
                    last_full = Instant::now();
                    changed = true;
                }
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        }

        let mut battery = None;
        if last_batt.elapsed() >= BATTERY_EVERY {
            match d.battery() {
                Ok(b) => {
                    battery = Some(b);
                    last_batt = Instant::now();
                }
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        }

        if snap.is_none() {
            dev = None;
        }

        let _ = tx.send(Msg {
            snap: if changed { snap.clone() } else { None },
            battery,
            connected: dev.is_some(),
            error,
        });
    }
}

impl Panel {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let (tx, cmd_rx) = std::sync::mpsc::channel();
        let (msg_tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || worker(cmd_rx, msg_tx));
        Self {
            tx,
            rx,
            snap: None,
            battery: 0,
            connected: false,
            error: None,
            synced: None,
            lod: 2,
            deb_press: 8,
            deb_release: 8,
            sleep_min: 10,
        }
    }

    /// Send a command and immediately mirror it locally so the UI never waits on the device.
    fn send(&mut self, cmd: Cmd) {
        if let Some(s) = self.snap.as_mut() {
            match cmd {
                Cmd::Polling(ms) => s.polling_ms = ms,
                Cmd::Motion(v) => s.motion = v,
                Cmd::Line(v) => s.line_corr = v,
                Cmd::Wave(v) => s.wave_corr = v,
                Cmd::Debounce(v, press) => {
                    if press {
                        s.debounce_press = v;
                    } else {
                        s.debounce_release = v;
                    }
                }
                Cmd::Sleep(m) => s.sleep_min = m,
                Cmd::Lod(v) => s.lod = v,
                Cmd::Activate(i) => s.stage_active = i.min(s.stage_count.saturating_sub(1)),
                Cmd::Dpi(stage, v) => {
                    if let Some(d) = s.dpi.get_mut(stage as usize) {
                        *d = v;
                    }
                }
                Cmd::Refresh => {}
            }
            self.lod = s.lod;
            self.deb_press = s.debounce_press;
            self.deb_release = s.debounce_release;
            self.sleep_min = s.sleep_min;
        }
        let _ = self.tx.send(cmd);
    }

    fn poll(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(m) => {
                    self.connected = m.connected;
                    if let Some(b) = m.battery {
                        self.battery = b;
                    }
                    self.error = m.error;
                    if let Some(s) = m.snap {
                        self.lod = s.lod;
                        self.deb_press = s.debounce_press;
                        self.deb_release = s.debounce_release;
                        self.sleep_min = s.sleep_min;
                        self.snap = Some(s);
                        self.synced = Some(Instant::now());
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn header(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("VT1 PRO").size(19.0).strong().color(TEXT));
            ui.label(RichText::new("控制面板").size(12.0).color(DIM).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (color, text) = if self.connected {
                    (ACCENT, "已连接")
                } else {
                    (DANGER, "未连接")
                };
                ui.label(RichText::new(text).size(12.0).color(color));
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, color);
            });
        });
    }

    fn battery_card(&self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("电量").size(11.0).color(DIM));
                    let pct = if self.connected && self.battery > 0 {
                        format!("{}%", self.battery)
                    } else {
                        "--".into()
                    };
                    ui.label(RichText::new(pct).size(34.0).strong().color(ACCENT));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.vertical(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new("2.4G").size(11.0).color(DIM));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new("无线连接").size(13.0).color(TEXT));
                        });
                    });
                });
            });
            ui.add_space(8.0);
            let width = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 7.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 3.5, TRACK);
            if self.connected && self.battery > 0 {
                let mut filled = rect;
                filled.set_width(rect.width() * (self.battery.min(100) as f32 / 100.0));
                p.rect_filled(filled, 3.5, ACCENT);
            }
        });
    }

    fn dpi_card(&mut self, ui: &mut Ui, active: usize, dpi: &[u16]) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DPI 档位").size(13.0).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("当前 {}", dpi.get(active).copied().unwrap_or(0)))
                            .size(12.0)
                            .color(ACCENT),
                    );
                });
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                for (i, v) in dpi.iter().enumerate() {
                    let selected = i == active;
                    if chip(ui, &v.to_string(), selected).clicked() && !selected {
                        self.send(Cmd::Activate(i as u8));
                    }
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("微调").size(11.0).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let cur = dpi.get(active).copied().unwrap_or(0);
                    let mut delta = 0i32;
                    if ui
                        .add(egui::Button::new("+").min_size(Vec2::new(26.0, 26.0)))
                        .clicked()
                    {
                        delta = if cur < 1000 { 50 } else { 100 };
                    }
                    if ui
                        .add(egui::Button::new("−").min_size(Vec2::new(26.0, 26.0)))
                        .clicked()
                    {
                        delta = if cur < 1000 { -50 } else { -100 };
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(cur.to_string()).size(16.0).strong());
                    if delta != 0 {
                        let next = (cur as i32 + delta).clamp(100, 26000) as u16;
                        self.send(Cmd::Dpi(active as u8, next));
                    }
                });
            });
        });
    }

    fn polling_card(&mut self, ui: &mut Ui, polling_ms: u8) {
        card(ui, |ui| {
            ui.label(RichText::new("回报率").size(13.0).color(DIM));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for ms in [8u8, 4, 2, 1] {
                    let hz = 1000 / ms as u32;
                    let selected = polling_ms == ms;
                    if chip(ui, &format!("{hz} Hz"), selected).clicked() && !selected {
                        self.send(Cmd::Polling(ms));
                    }
                }
            });
        });
    }

    fn settings_card(&mut self, ui: &mut Ui, s: Snapshot) {
        card(ui, |ui| {
            toggle_row(ui, "移动同步", s.motion, |v| self.send(Cmd::Motion(v)));
            ui.add_space(6.0);
            toggle_row(ui, "直线修正", s.line_corr, |v| self.send(Cmd::Line(v)));
            ui.add_space(6.0);
            toggle_row(ui, "波纹修正", s.wave_corr, |v| self.send(Cmd::Wave(v)));
            ui.add_space(10.0);
            divider(ui);
            ui.add_space(10.0);

            let (dp, dr) = (self.deb_press, self.deb_release);
            stepper_row(ui, "按下防抖", &format!("{dp} ms"), |m, p| {
                if m {
                    self.send(Cmd::Debounce(dp.saturating_sub(1).max(1), true));
                }
                if p {
                    self.send(Cmd::Debounce((dp + 1).min(32), true));
                }
            });
            stepper_row(ui, "抬起防抖", &format!("{dr} ms"), |m, p| {
                if m {
                    self.send(Cmd::Debounce(dr.saturating_sub(1).max(1), false));
                }
                if p {
                    self.send(Cmd::Debounce((dr + 1).min(32), false));
                }
            });
            let sl = self.sleep_min;
            stepper_row(ui, "休眠等待", &format!("{sl} 分钟"), |m, p| {
                if m {
                    self.send(Cmd::Sleep(sl.saturating_sub(1).max(5)));
                }
                if p {
                    self.send(Cmd::Sleep((sl + 1).min(120)));
                }
            });
            let lo = self.lod;
            stepper_row(ui, "静默高度 LOD", &lod_text(lo), |m, p| {
                if m {
                    self.send(Cmd::Lod(lo.saturating_sub(1).max(1)));
                }
                if p {
                    self.send(Cmd::Lod((lo + 1).min(11)));
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let when = match self.synced {
                Some(t) => {
                    let secs = t.elapsed().as_secs();
                    if secs < 2 {
                        "刚刚同步".to_string()
                    } else {
                        format!("同步于 {secs} 秒前")
                    }
                }
                None => "等待同步".to_string(),
            };
            ui.label(RichText::new(when).size(11.0).color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("刷新").size(12.0))
                            .fill(Color32::from_rgb(26, 33, 42)),
                    )
                    .clicked()
                {
                    self.send(Cmd::Refresh);
                }
            });
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).size(11.0).color(DANGER));
        }
    }
}

fn lod_text(code: u8) -> String {
    let mm = 0.9 + code as f32 / 10.0;
    format!("{mm:.1} mm")
}

fn chip(ui: &mut Ui, text: &str, selected: bool) -> egui::Response {
    let (fill, fg, stroke) = if selected {
        (ACCENT, BG, Stroke::NONE)
    } else {
        (
            Color32::from_rgb(26, 33, 42),
            TEXT,
            Stroke::new(1.0, Color32::from_rgb(38, 46, 58)),
        )
    };
    ui.add(
        egui::Button::new(RichText::new(text).size(13.0).color(fg))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(50.0, 28.0)),
    )
}

fn toggle_row(ui: &mut Ui, label: &str, value: bool, mut on_change: impl FnMut(bool)) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(14.0).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut v = value;
            if ui.add(egui::Checkbox::without_text(&mut v)).changed() {
                on_change(v);
            }
        });
    });
}

fn stepper_row(ui: &mut Ui, label: &str, value: &str, mut on_click: impl FnMut(bool, bool)) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(13.0).color(DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut plus = false;
            let mut minus = false;
            if ui
                .add(egui::Button::new("+").min_size(Vec2::new(24.0, 24.0)))
                .clicked()
            {
                plus = true;
            }
            if ui
                .add(egui::Button::new("−").min_size(Vec2::new(24.0, 24.0)))
                .clicked()
            {
                minus = true;
            }
            ui.add_space(4.0);
            ui.label(RichText::new(value).size(13.0).color(TEXT));
            if plus || minus {
                on_click(minus, plus);
            }
        });
    });
}

fn divider(ui: &mut Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 0.0, Color32::from_rgb(32, 39, 49));
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in ["C:/Windows/Fonts/simhei.ttf", "C:/Windows/Fonts/msyh.ttc"] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.insert(0, "cjk".to_owned());
            }
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                f.push("cjk".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(15, 19, 25);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.3);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.inactive.bg_fill = Color32::from_rgb(26, 33, 42);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(26, 33, 42);
    v.widgets.hovered.bg_fill = Color32::from_rgb(34, 43, 54);
    v.widgets.active.bg_fill = Color32::from_rgb(40, 50, 62);
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.corner_radius = CornerRadius::same(8);
    ctx.set_visuals(v);

    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
    });
}

impl eframe::App for Panel {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll();
        ui.ctx().request_repaint_after(Duration::from_millis(400));

        egui::Frame::new()
            .fill(BG)
            .inner_margin(Margin::same(18))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    self.header(ui);
                    ui.add_space(4.0);
                    self.battery_card(ui);

                    let snap = self.snap.clone();
                    let active = snap.as_ref().map_or(0, |s| s.stage_active as usize);
                    let dpi = snap.as_ref().map(|s| s.dpi.clone()).unwrap_or_default();
                    let polling = snap.as_ref().map_or(1, |s| s.polling_ms);

                    ui.add_enabled_ui(self.connected, |ui| {
                        ui.add_space(2.0);
                        if dpi.is_empty() {
                            card(ui, |ui| {
                                ui.label(RichText::new("未读取到设备配置").size(13.0).color(DIM));
                            });
                        } else {
                            self.dpi_card(ui, active, &dpi);
                        }
                        ui.add_space(2.0);
                        self.polling_card(ui, polling);
                        ui.add_space(2.0);
                        if let Some(s) = &snap {
                            self.settings_card(ui, s.clone());
                        }
                    });

                    ui.add_space(4.0);
                    self.footer(ui);
                });
            });
    }
}
