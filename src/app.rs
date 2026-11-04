use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, Layout, Margin, RichText, Stroke, Ui,
    UiBuilder, Vec2, ViewportCommand, WindowLevel,
};

use crate::autostart;
use crate::config::{self, Config, RunMode};
use crate::devices::{DeviceId, DeviceInfo, DeviceManager, Link, ReadMode};
use crate::theme::{self, ThemeMode};
use crate::tray::{self, Tray, TrayAction};
use crate::ui::mini;
use crate::ui::widgets::{
    chip, divider, section_heading, shadow_card, split_row, toggle_row, CARD_RADIUS,
};
use crate::ui::{chrome, config_page, home};

/// Configuration pages stay narrower than the workbench: full-width cards on a
/// 960px window push a label and its control so far apart that they stop
/// reading as one row.
const PAGE_W: f32 = 620.0;

/// 配置页浮层提示的停留与淡出时长。展示节奏由这里管理，而不是跟随
/// last_error 的出现/清空——设备侧一条成功消息就会把它清空，直接跟随
/// 会让提示闪烁，也回不到"读得完"的体验。
const ERR_TOAST_HOLD: Duration = Duration::from_secs(5);
const ERR_TOAST_FADE: Duration = Duration::from_secs(1);

/// 配置页浮层提示的状态：展示哪条设备错误、从何时开始计时。
struct ErrToast {
    msg: String,
    at: Instant,
}

const MAIN_SIZE: Vec2 = Vec2::new(960.0, 680.0);
const MAIN_MIN: Vec2 = Vec2::new(540.0, 480.0);
/// Stands in for "no upper bound": winit rejects infinity here.
const MAIN_MAX: f32 = 16384.0;

/// Gap between the widget and the screen edge when it first appears.
const MINI_MARGIN: f32 = 24.0;

/// What the main window is showing. Configuration used to open a second
/// viewport; keeping it as a route means one window, one titlebar, and a back
/// path that is always on screen.
#[derive(Clone)]
enum Route {
    Home,
    Config(DeviceId),
}

pub struct Panel {
    manager: DeviceManager,
    cfg: Config,
    applied_theme: Option<ThemeMode>,
    settings_open: bool,
    route: Route,
    /// When the current page was entered, so it can settle in instead of
    /// snapping into place.
    page_entered: Instant,
    tray: Option<Tray>,
    tray_inbox: Arc<Mutex<Vec<TrayAction>>>,
    autostart: bool,
    quitting: bool,
    error: Option<(String, Instant)>,
    /// 配置页的浮层设备错误：最近一次展示的消息与首次出现时间。设备离线
    /// 提示不经过它——离线是持续状态，浮层画到恢复在线为止。
    err_toast: Option<ErrToast>,
    last_rescan: Option<Instant>,
    /// Native handle of this window, kept so the frame style can be stripped.
    hwnd: Option<isize>,
    /// Height we last asked the window to be while in mini mode, so the resize
    /// command is only sent when the content really changed size.
    mini_h: f32,
    /// Whether the widget has been placed once this run.
    mini_placed: bool,
    /// 当前外壳的任务栏图标是否已隐藏；`None` 表示还没同步过。
    taskbar_hidden: Option<bool>,
}

impl Panel {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        setup_fonts(ctx);
        let hwnd = window_shell(cc);
        let mut cfg = config::load();
        // 与 main.rs 的视口创建保持一致：启动即浮窗，不读取上次保存的模式。
        cfg.mode = RunMode::Mini;
        // 显示器/分辨率可能变过：上次保存的坐标若已经不在任何一块屏上，
        // 启动后窗口就再也找不回来（浮窗尤其致命，托盘里没有"重置位置"）。
        if let Some([x, y]) = cfg.main_pos {
            let size = cfg.main_size.map(|[w, h]| vec2(w, h)).unwrap_or(MAIN_SIZE);
            if !is_reachable(ctx, egui::pos2(x, y), size) {
                cfg.main_pos = None;
            }
        }
        if let Some([x, y]) = cfg.mini_pos {
            let size = mini::window_size(mini::WIDTH, mini::height(1, 0));
            if !is_reachable(ctx, egui::pos2(x, y), size) {
                cfg.mini_pos = None;
            }
        }
        theme::apply(ctx, cfg.theme);
        let manager = DeviceManager::new(ctx);

        let mut autostart_on = autostart::is_enabled();
        if autostart_on {
            // Rewrite the value so a renamed/moved exe still autostarts correctly.
            let _ = autostart::set_enabled(true);
        }
        autostart_on = autostart::is_enabled();

        let inbox = Arc::new(Mutex::new(Vec::new()));
        let mut error = None;
        let tray = match tray::install(
            ctx,
            cfg.theme == ThemeMode::Dark,
            autostart_on,
            cfg.mode == RunMode::Mini,
            inbox.clone(),
        ) {
            Ok(t) => Some(t),
            Err(e) => {
                error = Some((e, Instant::now()));
                cfg.mode = RunMode::Desktop;
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                None
            }
        };

        ctx.request_repaint();
        let applied_theme = Some(cfg.theme);
        let mut panel = Self {
            manager,
            cfg,
            applied_theme,
            settings_open: false,
            route: Route::Home,
            page_entered: Instant::now(),
            tray,
            tray_inbox: inbox,
            autostart: autostart_on,
            quitting: false,
            error,
            err_toast: None,
            last_rescan: None,
            hwnd,
            mini_h: 0.0,
            mini_placed: false,
            taskbar_hidden: None,
        };
        // The viewport builder only knows the mode it was created with; this
        // puts the window into the shell the config actually asks for.
        panel.apply_shell(ctx);
        panel
    }

    fn save(&mut self) {
        if let Err(e) = config::save(&self.cfg) {
            crate::log::line("ERROR", &format!("保存配置失败：{e}"));
            self.fail(e);
        }
    }

    /// Rescan and remember when it happened, so the header button can flash
    /// an honest "already rescanned" confirmation instead of doing nothing.
    fn do_rescan(&mut self, ctx: &egui::Context) {
        self.manager.rescan();
        self.last_rescan = Some(Instant::now());
        ctx.request_repaint();
    }

    fn fail(&mut self, msg: String) {
        self.error = Some((msg, Instant::now()));
    }

    /// 首次读到某台鼠标的 DPI 档位时记下来，作为配置页“恢复初始值”的基线。
    fn capture_dpi_baselines(&mut self) {
        let pending: Vec<(String, Vec<u16>)> = self
            .manager
            .list()
            .iter()
            .filter_map(|e| {
                let snap = e.state.snap.as_ref()?;
                if !snap.caps.dpi || snap.dpi.is_empty() {
                    return None;
                }
                let key = dpi_baseline_key(&e.info);
                (!self.cfg.dpi_baseline.contains_key(&key)).then(|| (key, snap.dpi.clone()))
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        for (key, values) in pending {
            self.cfg.dpi_baseline.insert(key, values);
        }
        self.save();
    }

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.cfg.theme = match self.cfg.theme {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        };
        theme::apply(ctx, self.cfg.theme);
        self.applied_theme = Some(self.cfg.theme);
        if let Some(tray) = &self.tray {
            tray.set_theme_dark(self.cfg.theme == ThemeMode::Dark);
        }
        self.save();
        ctx.request_repaint();
    }

    fn set_autostart(&mut self, ctx: &egui::Context, enabled: bool) {
        match autostart::set_enabled(enabled) {
            Ok(()) => {
                self.autostart = enabled;
                if let Some(tray) = &self.tray {
                    tray.set_autostart(enabled);
                }
            }
            Err(e) => {
                self.fail(e);
                // muda 点击时已把勾选翻过去了；注册表没写成就要把它拨回来，
                // 否则菜单会一直显示与实际相反的状态。
                if let Some(tray) = &self.tray {
                    tray.set_autostart(self.autostart);
                }
            }
        }
        ctx.request_repaint();
    }

    /// Remember where and how big the main window was, so leaving the widget
    /// puts the user back exactly where they were. A maximized window is left
    /// out: its rect is the whole screen, and restoring that as a normal size
    /// would give a full-screen window with no title bar state.
    fn remember_main(&mut self, ctx: &egui::Context) {
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        if maximized {
            return;
        }
        if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
            self.cfg.main_pos = Some([r.left(), r.top()]);
            self.cfg.main_size = Some([r.width(), r.height()]);
        }
    }

    fn show_main(&mut self, ctx: &egui::Context) {
        // A minimized window stays minimized through `Focus`: without an
        // explicit restore, "打开主界面" from the tray would silently do
        // nothing whenever the workbench had been minimized to the taskbar.
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        if self.cfg.mode == RunMode::Desktop {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            return;
        }
        self.cfg.mode = RunMode::Desktop;
        self.save();
        // Leave whatever the widget was sitting on as its parking spot.
        if let Some(t) = &self.tray {
            t.set_mini(false);
        }
        self.apply_shell(ctx);
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn go_mini(&mut self, ctx: &egui::Context) {
        if self.cfg.mode == RunMode::Mini {
            // muda 的勾选项在点击时自己翻转，这里补一次，否则菜单上的
            // "桌面浮窗"会显示成未勾选，而窗口其实还是浮窗。
            if let Some(t) = &self.tray {
                t.set_mini(true);
            }
            return;
        }
        self.remember_main(ctx);
        // Next time the big window comes back it should open on the workbench,
        // not on a page for a device that may have been unplugged meanwhile.
        self.route = Route::Home;
        self.settings_open = false;
        self.cfg.mode = RunMode::Mini;
        self.mini_placed = false;
        self.save();
        if let Some(t) = &self.tray {
            t.set_mini(true);
        }
        self.apply_shell(ctx);
    }

    fn mini_height(&self) -> f32 {
        mini::height(
            self.manager.list().len(),
            mini::attention_count(&self.manager),
        )
    }

    /// Where the main window should sit: last time's spot if it is still on
    /// screen, otherwise centred. Coming back from the widget the window is
    /// still parked in the corner the widget was in, and growing it there
    /// would push most of it off the desktop.
    fn main_position(&self, ctx: &egui::Context) -> Option<egui::Pos2> {
        let monitor = ctx.input(|i| i.viewport().monitor_size)?;
        let size = self
            .cfg
            .main_size
            .map(|[w, h]| vec2(w, h))
            .unwrap_or(MAIN_SIZE);
        // 窗口坐标是跨显示器的绝对坐标，不能拿"当前显示器尺寸"去夹：一块
        // 1920 宽的副屏上的窗口在 x=2000，夹回 [0, 1800] 会把它拽回主屏。
        // 只有整个窗口都跑出虚拟桌面（原来那块屏被拔掉了）才回到中央。
        let centered = egui::pos2((monitor.x - size.x) / 2.0, (monitor.y - size.y) / 2.0);
        let Some([x, y]) = self.cfg.main_pos else {
            return Some(centered);
        };
        let pos = egui::pos2(x, y);
        Some(if is_reachable(ctx, pos, size) {
            pos
        } else {
            centered
        })
    }

    /// Push the window shape for the current mode. One window, two shells —
    /// no second native window, no hidden state to get stuck in.
    fn apply_shell(&mut self, ctx: &egui::Context) {
        match self.cfg.mode {
            RunMode::Desktop => {
                ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
                ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(MAIN_MIN));
                ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(Vec2::splat(MAIN_MAX)));
                let size = self
                    .cfg
                    .main_size
                    .map(|[w, h]| vec2(w, h))
                    .unwrap_or(MAIN_SIZE);
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                if let Some(pos) = self.main_position(ctx) {
                    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos));
                }
            }
            RunMode::Mini => {
                let h = self.mini_height();
                // 窗口比卡片大一圈透明边距，容纳自绘投影（见 mini::SHADOW_PAD）。
                let size = mini::window_size(mini::WIDTH, h);
                // winit 不会自己退出最大化/最小化：从最大化状态点"收起为浮窗"，
                // 窗口仍是 maximized，浮窗会以整屏尺寸铺开；此时再把 Min/Max 内
                // 尺寸钉成 340×N 也救不回来，必须先还原。
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if self.cfg.mini_pinned {
                    WindowLevel::AlwaysOnTop
                } else {
                    WindowLevel::Normal
                }));
                ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
                // Pin min and max to the exact size: the widget sizes itself to
                // its content and must not be dragged into a different shape.
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(size));
                ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(size));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                self.mini_h = h;
                if let Some([x, y]) = self.cfg.mini_pos {
                    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(egui::pos2(x, y)));
                }
            }
        }
        ctx.request_repaint();
    }

    /// Windows 10 paints a 1px hairline in the system's light/dark colour
    /// around any top-level window that carries a caption or border frame, and
    /// offers no attribute to switch it off (the Windows 11 `DWMWA_*` ones are
    /// rejected). Dropping those style bits leaves the window without a frame
    /// for the compositor to decorate, so the only outline left is the one the
    /// UI paints.
    ///
    /// `WS_SIZEBOX` is kept: winit starts an edge resize by posting
    /// `WM_NCLBUTTONDOWN`, which the OS only acts on when the window declares a
    /// sizing border.
    ///
    /// winit rewrites the whole style whenever its own flags change (shell
    /// switch, pin, resize), so this runs once a frame; it returns immediately
    /// once the style is already bare.
    fn strip_window_frame(&self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_BORDER,
            WS_CAPTION, WS_EX_WINDOWEDGE,
        };

        let Some(hwnd) = self.hwnd else {
            return;
        };
        let hwnd = hwnd as *mut core::ffi::c_void;
        // SAFETY: `hwnd` is this app's window; the calls only rewrite its own
        // style bits, and the GL surface survives a frame refresh.
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            // `WS_POPUP` is deliberately not set: it would take the shell out
            // of the taskbar and Alt+Tab, which is the opposite of what the
            // workbench is for.
            let bare = style & !(WS_CAPTION | WS_BORDER);
            let bare_ex = ex & !WS_EX_WINDOWEDGE;
            if style == bare && ex == bare_ex {
                return;
            }
            SetWindowLongPtrW(hwnd, GWL_STYLE, bare as isize);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, bare_ex as isize);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }

    /// 浮窗只留托盘图标，主界面恢复任务栏按钮。winit 的运行时接口走
    /// ITaskbarList，不会去碰窗口样式，因此和 `strip_window_frame` 不冲突。
    fn sync_taskbar(&mut self, frame: &eframe::Frame) {
        let hide = self.cfg.mode == RunMode::Mini;
        if self.taskbar_hidden == Some(hide) {
            return;
        }
        let Some(window) = frame.winit_window() else {
            return;
        };
        use winit::platform::windows::WindowExtWindows as _;
        window.set_skip_taskbar(hide);
        self.taskbar_hidden = Some(hide);
    }

    fn set_pinned(&mut self, ctx: &egui::Context, pinned: bool) {
        self.cfg.mini_pinned = pinned;
        self.save();
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if pinned {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        }));
        ctx.request_repaint();
    }

    /// First run with the widget: park it near the bottom-right corner of the
    /// screen, where a status card does not cover anything the user is doing.
    fn place_mini(&mut self, ctx: &egui::Context) {
        if self.mini_placed || self.cfg.mini_pos.is_some() {
            self.mini_placed = true;
            return;
        }
        let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) else {
            return;
        };
        let h = mini::window_size(mini::WIDTH, self.mini_height());
        let pos = egui::pos2(
            (monitor.x - h.x - MINI_MARGIN).max(MINI_MARGIN),
            (monitor.y - h.y - MINI_MARGIN * 3.0).max(MINI_MARGIN),
        );
        self.cfg.mini_pos = Some([pos.x, pos.y]);
        self.mini_placed = true;
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos));
        ctx.request_repaint();
    }

    /// Remember where the widget was dropped. Sampling the window rect every
    /// frame looked simpler but records the transient positions the OS reports
    /// while the window is being created or moved, so it is done once, on
    /// release.
    fn remember_mini_pos(&mut self, ctx: &egui::Context) {
        let Some(r) = ctx.input(|i| i.viewport().outer_rect) else {
            return;
        };
        let pos = [r.left(), r.top()];
        if self.cfg.mini_pos != Some(pos) {
            self.cfg.mini_pos = Some(pos);
            self.save();
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        if self.cfg.mode == RunMode::Desktop {
            // 从托盘直接退出时不会经过 go_mini，几何要在这里补记一次。
            self.remember_main(ctx);
        }
        self.save();
        // 让采集线程把手上的写序列做完再退；否则可能正好截断在 EEPROM 的
        // commit 之前，留下一个 CRC 对不上的扇区。
        self.manager.shutdown();
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn go_home(&mut self) {
        self.route = Route::Home;
        self.err_toast = None;
    }

    fn drain_tray(&mut self, ctx: &egui::Context) {
        let actions: Vec<TrayAction> = {
            // 同 tray.rs：托盘回调线程 panic 后锁中毒时不能跟着 panic。
            let mut queue = self.tray_inbox.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *queue)
        };
        for action in actions {
            match action {
                TrayAction::ShowDesktop => self.show_main(ctx),
                TrayAction::EnterMini => self.go_mini(ctx),
                TrayAction::ToggleTheme => self.toggle_theme(ctx),
                TrayAction::ToggleAutostart => {
                    let next = !self.autostart;
                    self.set_autostart(ctx, next);
                }
                TrayAction::Rescan => {
                    self.do_rescan(ctx);
                }
                TrayAction::Quit => self.quit(ctx),
            }
        }
    }

    fn open_config(&mut self, id: &DeviceId) {
        if self.manager.entry(id).is_some() {
            self.settings_open = false;
            self.page_entered = Instant::now();
            // 换设备后旧设备的失败原因与新页面无关，直接不展示。
            self.err_toast = None;
            self.route = Route::Config(id.clone());
        }
    }

    /// Device configuration as a page inside the main window. The page keeps
    /// the titlebar (with its back arrow) and its own scroll position per
    /// device, so returning to a device resumes where the user left off.
    fn show_config_page(&mut self, ui: &mut Ui, id: &DeviceId, p: &'static theme::Palette) {
        let entry = self
            .manager
            .entry(id)
            .map(|e| (e.info.clone(), e.state.clone()));
        let baseline = entry
            .as_ref()
            .and_then(|(info, _)| self.cfg.dpi_baseline.get(&dpi_baseline_key(info)).cloned());

        // A short settle-in: the page drifts up from where the card sat, which
        // reads as a push rather than a hard cut between two screens.
        let t = (self.page_entered.elapsed().as_secs_f32() / 0.16).clamp(0.0, 1.0);
        let ease = 1.0 - (1.0 - t).powi(3);
        if t < 1.0 {
            ui.ctx().request_repaint();
        }

        let width = ui.available_width().min(PAGE_W);
        let pad = ((ui.available_width() - width) / 2.0).max(0.0);
        let mut cmds = Vec::new();
        let mut back = false;
        let mut rescan = false;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt(("config-page", id.0.as_ref()))
            .show(ui, |ui| {
                ui.add_space((1.0 - ease) * 10.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.allocate_ui_with_layout(
                        Vec2::new(width, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(width);
                            match &entry {
                                Some((info, state)) => {
                                    cmds =
                                        config_page::view(ui, p, info, state, baseline.as_deref())
                                            .cmds;
                                }
                                None => {
                                    let action = config_page::missing(ui, p);
                                    back = action.back;
                                    rescan = action.rescan;
                                }
                            }
                        },
                    );
                });
            });

        // 断开/写入失败的浮层提示。以前是滚动内容顶部的一块文档流通知，
        // 跟着设备状态偶发地插入又消失，把视口下方的内容顶下又顶回——
        // 用户停在 LOD/防抖这类行上不动，光标下的控件却换了，看起来就像
        // 页面自己跳了一下。浮层画在前景层、不占布局，谁出现谁消失都不
        // 再移动页面内容，而且始终钉在页首，滚动到哪里都看得见。
        let mut toast: Option<(&str, String, f32)> = None;
        if let Some((_, state)) = &entry {
            if let Link::Offline(err) = &state.link {
                // 离线是持续状态：一直显示到恢复在线为止，不做淡出。
                toast = Some(("设备已断开", err.clone(), 1.0));
            } else {
                if let Some(err) = &state.last_error {
                    // 同一条错误还在展示期内时不重置计时，否则持续存在的
                    // 错误会把提示钉死在满不透明度上，永远走不完淡出。
                    let fresh = match &self.err_toast {
                        Some(t) if t.msg == *err && t.at.elapsed() < ERR_TOAST_HOLD => t.at,
                        _ => Instant::now(),
                    };
                    self.err_toast = Some(ErrToast {
                        msg: err.clone(),
                        at: fresh,
                    });
                }
                let expired = self
                    .err_toast
                    .as_ref()
                    .is_some_and(|t| t.at.elapsed() >= ERR_TOAST_HOLD + ERR_TOAST_FADE);
                if expired {
                    self.err_toast = None;
                }
                if let Some(t) = &self.err_toast {
                    let elapsed = t.at.elapsed();
                    let alpha = if elapsed < ERR_TOAST_HOLD {
                        1.0
                    } else {
                        1.0 - (elapsed - ERR_TOAST_HOLD).as_secs_f32()
                            / ERR_TOAST_FADE.as_secs_f32()
                    };
                    if alpha < 1.0 {
                        ui.ctx().request_repaint_after(Duration::from_millis(50));
                    }
                    toast = Some(("操作失败", t.msg.clone(), alpha));
                }
            }
        }
        if let Some((title, body, alpha)) = toast {
            config_page::toast_overlay(ui, p, title, &body, alpha);
        }

        if rescan {
            self.do_rescan(ui.ctx());
        }
        if back {
            self.go_home();
        }
        for cmd in cmds {
            self.manager.send(id, cmd);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let p = theme::palette(self.cfg.theme);
        let mut close = false;
        egui::Window::new("设置")
            // egui's own title bar paints a grey band that fights the card
            // styling, so the popover gets a hand-drawn header instead.
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::RIGHT_TOP, [-16.0, 56.0])
            .frame(
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(16))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 6],
                        blur: 20,
                        spread: 0,
                        color: Color32::from_black_alpha(if p.dark { 90 } else { 26 }),
                    }),
            )
            .show(ctx, |ui| {
                ui.set_width(300.0);
                split_row(
                    ui,
                    |ui| {
                        if close_button(ui, p) {
                            close = true;
                        }
                    },
                    |ui| {
                        ui.label(RichText::new("设置").size(14.5).strong().color(p.text));
                    },
                );
                ui.add_space(12.0);
                divider(ui, p);
                ui.add_space(12.0);
                section_heading(ui, p, "外观", "选择更适合当前环境的界面主题");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let dark = self.cfg.theme == ThemeMode::Dark;
                    if chip(ui, p, "深色模式", dark).clicked() && !dark {
                        self.toggle_theme(ctx);
                    }
                    if chip(ui, p, "浅色模式", !dark).clicked() && dark {
                        self.toggle_theme(ctx);
                    }
                });
                ui.add_space(14.0);
                divider(ui, p);
                ui.add_space(12.0);
                section_heading(ui, p, "应用行为", "管理启动方式和后台运行状态");
                ui.add_space(8.0);
                let autostart = self.autostart;
                toggle_row(
                    ui,
                    p,
                    "开机自启动",
                    "登录 Windows 后自动启动，开机就能看到设备电量",
                    "常驻开启",
                    autostart,
                    |v| {
                        self.set_autostart(ctx, v);
                    },
                );
                ui.add_space(10.0);
                split_row(
                    ui,
                    |ui| {
                        if chip(ui, p, "收起为浮窗", false).clicked() {
                            self.go_mini(ctx);
                        }
                    },
                    |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new("桌面浮窗").size(13.0).color(p.text));
                            ui.label(
                                RichText::new("变成小窗常驻桌面，关闭主界面也能看到状态")
                                    .size(10.5)
                                    .color(p.dim),
                            );
                        });
                    },
                );
                ui.add_space(14.0);
                divider(ui, p);
                ui.add_space(12.0);
                let device_count = self.manager.list().len();
                split_row(
                    ui,
                    |ui| {
                        if chip(ui, p, "重新扫描", false)
                            .on_hover_text("重新检测已连接的外设（F5 或 Ctrl+R）")
                            .clicked()
                        {
                            self.do_rescan(ctx);
                        }
                    },
                    |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new("设备检测").size(13.0).color(p.text));
                            ui.label(
                                RichText::new(format!("当前已发现 {device_count} 台设备"))
                                    .size(10.5)
                                    .color(p.dim),
                            );
                        });
                    },
                );
                ui.add_space(12.0);
                divider(ui, p);
                ui.add_space(8.0);
                split_row(
                    ui,
                    |ui| {
                        ui.label(
                            RichText::new(concat!("版本 ", env!("CARGO_PKG_VERSION")))
                                .size(10.5)
                                .color(p.dim),
                        );
                    },
                    |ui| {
                        ui.label(RichText::new("PeriphHub").size(10.5).strong().color(p.dim));
                    },
                );
            });
        if close {
            self.settings_open = false;
        }
    }

    fn error_strip(&mut self, ui: &mut Ui, p: &'static theme::Palette) {
        let Some((msg, at)) = &self.error else {
            return;
        };
        if at.elapsed() > Duration::from_secs(12) {
            self.error = None;
            return;
        }
        let msg = msg.clone();
        let color = p.danger;
        let mut dismissed = false;
        egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                20,
            ))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.4)))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(10, 7))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(msg.as_str()).size(11.0).color(color));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("忽略").size(10.0).color(p.dim))
                                    .fill(Color32::TRANSPARENT)
                                    .stroke(Stroke::NONE),
                            )
                            .clicked()
                        {
                            dismissed = true;
                        }
                    });
                });
            });
        if dismissed {
            self.error = None;
        }
        ui.add_space(8.0);
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
}

/// Small round close affordance for the settings popover.
fn close_button(ui: &mut Ui, p: &theme::Palette) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.08);
    if t > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(7),
            theme::alpha(p.track, (t * 255.0) as u8),
        );
    }
    let c = rect.center();
    let stroke = Stroke::new(1.3, p.dim.gamma_multiply(0.7 + t * 0.3));
    for (a, b) in [
        (egui::vec2(-3.4, -3.4), egui::vec2(3.4, 3.4)),
        (egui::vec2(3.4, -3.4), egui::vec2(-3.4, 3.4)),
    ] {
        ui.painter().line_segment([c + a, c + b], stroke);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

impl eframe::App for Panel {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.quitting {
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.cfg.mode == RunMode::Mini {
                // Alt+F4 on the widget means the same as its close button.
                self.quit(ctx);
                return;
            }
            // Closing the big window folds it back into the widget instead of
            // making the app disappear.
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.go_mini(ctx);
        }
        self.strip_window_frame();
        self.drain_tray(ctx);
        self.sync_taskbar(frame);
        // 浮窗只读电量；主页摘要 20 秒一刷；配置页才需要 4 秒级全量读，
        // 避免每台设备在无关页面被频繁唤醒、反复访问 EEPROM。
        let read_mode = match (self.cfg.mode, &self.route) {
            (RunMode::Mini, _) => ReadMode::BatteryOnly,
            (RunMode::Desktop, Route::Config(_)) => ReadMode::Full,
            (RunMode::Desktop, _) => ReadMode::Summary,
        };
        self.manager.set_read_mode(read_mode);
        if self.manager.poll() {
            // 基线只可能在"刚读到新快照"时出现，跟着 poll 的结果做就够了；
            // 以前每帧都扫一遍设备列表、为每台设备拼一次 key，纯属空转。
            self.capture_dpi_baselines();
            ctx.request_repaint();
        }
        // "Connecting" status dots breathe; give them frames to breathe in.
        let connecting = self
            .manager
            .list()
            .iter()
            .any(|e| matches!(e.state.link, Link::Connecting));
        if self.cfg.mode == RunMode::Mini {
            self.place_mini(ctx);
            if ctx.input(|i| i.pointer.any_released()) {
                self.remember_mini_pos(ctx);
            }
            // The widget is exactly as tall as its content; a device dropping
            // off the bus should shrink it rather than leave a gap.
            let h = self.mini_height();
            if (h - self.mini_h).abs() > 0.5 {
                let size = mini::window_size(mini::WIDTH, h);
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(size));
                ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(size));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                self.mini_h = h;
            }
            ctx.request_repaint_after(if connecting {
                Duration::from_millis(140)
            } else {
                Duration::from_millis(600)
            });
        } else if connecting {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        if self.quitting {
            return;
        }
        let ctx = ui.ctx().clone();
        if self.applied_theme != Some(self.cfg.theme) {
            theme::apply(&ctx, self.cfg.theme);
            self.applied_theme = Some(self.cfg.theme);
        }
        let rescan_pressed = ctx.input(|i| {
            i.key_pressed(egui::Key::F5) || (i.modifiers.ctrl && i.key_pressed(egui::Key::R))
        });
        if rescan_pressed {
            self.do_rescan(&ctx);
        }
        let p = theme::palette(self.cfg.theme);
        match self.cfg.mode {
            RunMode::Mini => self.mini_ui(ui, p),
            RunMode::Desktop => self.desktop_ui(ui, &ctx, p),
        }
        // 本帧内刚切换外壳时立刻更新任务栏，不等下一次 logic。
        self.sync_taskbar(frame);
    }

    /// A transparent window background lets the widget's rounded card float;
    /// both shells paint their own opaque surface inside it.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save();
    }
}

impl Panel {
    fn mini_ui(&mut self, ui: &mut Ui, p: &'static theme::Palette) {
        let ctx = ui.ctx().clone();
        let recently_scanned = self
            .last_rescan
            .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        let action = mini::view(ui, p, &self.manager, self.cfg.mini_pinned, recently_scanned);
        if action.toggle_pin {
            let next = !self.cfg.mini_pinned;
            self.set_pinned(&ctx, next);
        }
        if action.toggle_theme {
            self.toggle_theme(&ctx);
        }
        if action.rescan {
            self.do_rescan(&ctx);
        }
        if let Some(id) = action.open_config {
            self.show_main(&ctx);
            self.open_config(&id);
        } else if action.open_main {
            self.show_main(&ctx);
        }
        if action.quit {
            self.quit(&ctx);
        }
    }

    fn desktop_ui(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &'static theme::Palette) {
        let ctx = ctx.clone();
        // Escape unwinds one layer at a time: the settings popover first, then
        // the configuration page.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.settings_open {
                self.settings_open = false;
            } else {
                self.go_home();
            }
        }
        // Alt+← and the mouse back button both mean "leave this page".
        let back_pressed = ctx.input(|i| {
            (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft))
                || i.pointer.button_pressed(egui::PointerButton::Extra1)
        });
        if back_pressed {
            self.go_home();
        }
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));

        let route = self.route.clone();
        let on_page = !matches!(route, Route::Home);
        // The titlebar carries the page identity now that there is no second
        // window title to do it.
        let (title, subtitle) = match &route {
            Route::Home => ("PeriphHub".to_string(), "外设中枢".to_string()),
            Route::Config(id) => self
                .manager
                .entry(id)
                .map(|e| {
                    (
                        format!("{} {}", e.info.brand, e.info.model),
                        e.info.connection.label().to_string(),
                    )
                })
                .unwrap_or_else(|| ("设备已移除".to_string(), "返回设备列表".to_string())),
        };

        // 主界面外壳与浮窗同款：窗口四周留透明边距，圆角卡片 + 手绘多层
        // 投影画在窗口内（Win10 拿不到无边框的系统阴影，见 widgets::shadow_card）。
        // 最大化时铺满工作区、去圆角去投影，与系统窗口行为一致。
        let window = ui.max_rect();
        let pad = if maximized { 0.0 } else { mini::SHADOW_PAD };
        let card = window.shrink(pad);
        if maximized {
            ui.painter().rect_filled(card, 0.0, p.bg);
        } else {
            shadow_card(
                ui.painter(),
                card,
                p.bg,
                CARD_RADIUS,
                p.dark,
                ctx.input(|i| i.focused),
            );
        }
        let mut card_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(card)
                .layout(Layout::top_down(Align::Min)),
        );
        card_ui.set_clip_rect(card);

        let bar = chrome::titlebar(
            &mut card_ui,
            p,
            chrome::Bar {
                title: &title,
                subtitle: Some(&subtitle),
                show_actions: true,
                window_buttons: true,
                maximized,
                back: on_page,
            },
        );
        if bar.minimize {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        if bar.maximize {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
        }
        if bar.settings {
            self.settings_open = !self.settings_open;
        }
        if bar.toggle_theme {
            self.toggle_theme(&ctx);
        }
        if bar.close || bar.mini {
            self.go_mini(&ctx);
        }
        if bar.back {
            self.go_home();
        }

        egui::Frame::new()
            .inner_margin(Margin {
                left: 20,
                right: 20,
                top: 20,
                bottom: 14,
            })
            .show(&mut card_ui, |ui| {
                self.error_strip(ui, p);
                match &route {
                    Route::Home => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let recently_scanned = self
                                    .last_rescan
                                    .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                                let action = home::view(
                                    ui,
                                    p,
                                    &self.manager,
                                    recently_scanned,
                                    self.last_rescan,
                                );
                                if action.rescan {
                                    self.do_rescan(&ctx);
                                }
                                if let Some(id) = action.open_config {
                                    self.open_config(&id);
                                }
                                ui.add_space(18.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("PeriphHub · 2.4G / 蓝牙 / 有线外设统一管理")
                                            .size(10.0)
                                            .color(p.dim),
                                    );
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(
                                            RichText::new("关闭窗口后收起为桌面浮窗")
                                                .size(10.0)
                                                .color(p.dim),
                                        );
                                    });
                                });
                            });
                    }
                    Route::Config(id) => self.show_config_page(ui, id, p),
                }
            });

        chrome::resize_handles(ui, !maximized);
        self.settings_window(&ctx);
    }
}

/// 设备基线键：container id 在重插后仍稳定，型号能区分同一接收器下换过的
/// 鼠标，两者组合后既不会跨设备串值，也不会因换鼠标误用别人的初始档位。
fn dpi_baseline_key(info: &DeviceInfo) -> String {
    format!("{}|{}", info.id.0, info.model)
}

/// 整个虚拟桌面（所有显示器合并后的区域），单位与 egui 的窗口坐标一致（点）。
///
/// egui 只暴露"窗口所在那块显示器"的尺寸，判断一个坐标是否还在屏幕上够用不了
/// 多屏场景，所以直接问系统要虚拟桌面的物理范围再换算成点。
fn virtual_screen(ctx: &egui::Context) -> Option<egui::Rect> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    // SAFETY: 纯查询，无参数前置条件；返回值 <= 0 表示读不到。
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return None;
    }
    let scale = ctx.pixels_per_point().max(0.1);
    // 混合 DPI 的多屏环境里各屏缩放不同，这里只有一个 scale（当前窗口所在
    // 显示器的），换算出的虚拟桌面边界在其它屏上会有偏差。`is_reachable`
    // 用它做的是"窗口是否还有一部分留在桌面上"的粗判并留了抓取余量，偏差
    // 可容忍；宁可保守——误判为可达只是不搬窗口，误判为不可达会把还能用
    // 的坐标强行拽回屏幕中央。
    Some(egui::Rect::from_min_size(
        egui::pos2(x as f32 / scale, y as f32 / scale),
        vec2(w as f32 / scale, h as f32 / scale),
    ))
}

/// 窗口是否还有一部分留在屏幕上：至少留出一条能抓住标题栏的区域，否则用户
/// 既拖不动也看不到。读不到屏幕信息时一律当作可见，宁可不搬别乱搬。
fn is_reachable(ctx: &egui::Context, pos: egui::Pos2, size: Vec2) -> bool {
    const GRAB_W: f32 = 120.0;
    const GRAB_H: f32 = 80.0;

    let Some(screen) = virtual_screen(ctx) else {
        return true;
    };
    pos.x + size.x > screen.left() + GRAB_W
        && pos.x < screen.right() - GRAB_W
        && pos.y + size.y > screen.top()
        && pos.y < screen.bottom() - GRAB_H
}

/// Best-effort compositor setup for the frameless window.
///
/// 系统阴影一律不用，投影全部由 UI 自绘（见 `ui::widgets::shadow_card`）：
/// - winit 的无边框阴影靠在窗口顶部留 1px 非客户区换 DWM 投影，那 1px 会被
///   DWM 画成一条改不掉的缝线（Win10 没有 `DWMWA_BORDER_COLOR`）；
/// - 而且它投的是"整块窗口矩形"，这个窗口比卡片大一圈 `SHADOW_PAD`，
///   叠上自绘阴影就成了两圈错位的影子。
///
/// 所以这里只做两件事：关掉 winit 的阴影；在 Win11 上把 DWM 给透明窗口补的
/// 那圈 1px 边框色关掉（Win10 没有该属性，调用会被静默拒绝）。
///
/// 最大化时 `strip_window_frame` 继续兜底 caption/border 样式缝线。
fn window_shell(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if let Some(window) = cc.winit_window() {
        use winit::platform::windows::WindowExtWindows;
        // 始终关掉：它的 DWM 投影必须留 1px 非客户区，那 1px 是一条
        // 按窗口改不掉颜色的缝线，投影交给 `shadow_card` 自绘。
        window.set_undecorated_shadow(false);
    }

    let Ok(handle) = cc.window_handle() else {
        return None;
    };
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return None;
    };
    let hwnd = win.hwnd.get() as *mut core::ffi::c_void;
    let set = |attr: u32, value: u32| {
        // SAFETY: `hwnd` came from winit for this very window and the value
        // outlives the call; DWM only reads `size` bytes from it.
        unsafe {
            windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
                hwnd,
                attr,
                &value as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
        }
    };
    // Win11（build >= 22000）：透明窗口仍会被 DWM 补一圈 1px 边框，关掉它，
    // 卡片的轮廓只由自绘的圆角和描边决定。Win10 拒绝这些属性，无需处理。
    if is_win11() {
        set(
            windows_sys::Win32::Graphics::Dwm::DWMWA_BORDER_COLOR as u32,
            windows_sys::Win32::Graphics::Dwm::DWMWA_COLOR_NONE,
        );
        set(
            windows_sys::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            windows_sys::Win32::Graphics::Dwm::DWMWCP_ROUND as u32,
        );
    }
    Some(win.hwnd.get())
}

/// CurrentBuildNumber >= 22000 即 Win11；读不到时按 Win11 处理——那些
/// DWMWA 属性在不支持的系统上只是被静默拒绝，没有副作用。
fn is_win11() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let sub: Vec<u16> = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "CurrentBuildNumber"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = [0u16; 16];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
        )
    };
    if rc != 0 {
        return true;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
        .trim()
        .parse::<u32>()
        .map(|build| build >= 22000)
        .unwrap_or(true)
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // 系统盘不一定是 C:，字体目录跟着 %WINDIR% 走。
    let dir = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("C:/Windows"))
        .join("Fonts");
    for name in ["simhei.ttf", "msyh.ttc"] {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
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
}
