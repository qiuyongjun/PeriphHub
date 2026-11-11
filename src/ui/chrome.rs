use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Rect, Response, RichText, Sense, Stroke,
    StrokeKind, Ui, UiBuilder, Vec2,
};

use crate::theme::{alpha, heading, Palette};
use crate::ui::widgets::mix;

pub struct Bar<'a> {
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    pub show_actions: bool,
    pub window_buttons: bool,
    pub maximized: bool,
    /// Sub-pages swap the app monogram for a back arrow, the way a settings
    /// app does, so leaving a page never depends on scrolling back up.
    pub back: bool,
}

#[derive(Default)]
pub struct BarAction {
    pub minimize: bool,
    pub maximize: bool,
    pub close: bool,
    pub settings: bool,
    pub toggle_theme: bool,
    pub back: bool,
    /// 收起为主窗口外的浮窗卡片。
    pub mini: bool,
}

/// 为标题和工具动作预留独立空间，并保留原生拖动手势。
pub fn titlebar(ui: &mut Ui, p: &Palette, cfg: Bar<'_>) -> BarAction {
    use crate::ui::widgets::CARD_RADIUS;

    let mut action = BarAction::default();
    let width = ui.available_width();
    let (bar, _) = ui.allocate_exact_size(Vec2::new(width, 54.0), Sense::hover());
    // 标题栏铺满圆角卡片的整个宽度，顶部两角跟随卡片圆角，否则方角会从
    // 卡片圆弧里戳出去；最大化时卡片无圆角，标题栏也回到方角。
    let top = if cfg.maximized { 0 } else { CARD_RADIUS };
    ui.painter().rect_filled(
        bar,
        CornerRadius {
            nw: top,
            ne: top,
            sw: 0,
            se: 0,
        },
        p.titlebar,
    );
    let border = Rect::from_min_max(
        egui::pos2(bar.left(), bar.bottom() - 1.0),
        bar.right_bottom(),
    );
    ui.painter().rect_filled(border, 0.0, alpha(p.border, 130));

    let inner = bar.shrink2(Vec2::new(20.0, 0.0));
    // 右侧两组的预留宽度（含组内间距与分隔线，实际约 143 / 106），后面再
    // 统一留 18 px 余量。改 `window_button` / `icon_button` 的尺寸时同步这里，
    // 否则标题会悄悄压到按钮上。
    let controls_w =
        if cfg.window_buttons { 134.0 } else { 0.0 } + if cfg.show_actions { 108.0 } else { 0.0 };
    let title_rect = Rect::from_min_max(
        inner.min,
        egui::pos2(
            (inner.right() - controls_w - 18.0).max(inner.left()),
            inner.bottom(),
        ),
    );
    // Where the drag strip may start. The back button is clickable, and the
    // drag interaction is registered after it, so the strip has to begin past
    // it — otherwise dragging wins the click and the button never fires.
    let mut drag_left = bar.left();
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(title_rect)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if cfg.back {
                let back = back_button(ui, p);
                if back.clicked() {
                    action.back = true;
                }
                drag_left = back.rect.right() + 4.0;
            } else {
                crate::theme::brand_badge(ui, 28.0, p);
            }
            ui.add_space(7.0);
            ui.add(egui::Label::new(heading(cfg.title, 14.5, p.text)).truncate());
            if let Some(sub) = cfg.subtitle.filter(|_| ui.available_width() > 100.0) {
                ui.add_space(8.0);
                ui.label(RichText::new(sub).size(11.0).color(p.subtle));
            }
        },
    );

    let mut buttons_left = bar.right() - 10.0;
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if cfg.window_buttons {
                if window_button(ui, p, WinBtn::Close).clicked() {
                    action.close = true;
                }
                if window_button(ui, p, WinBtn::Maximize(cfg.maximized)).clicked() {
                    action.maximize = true;
                }
                if window_button(ui, p, WinBtn::Minimize).clicked() {
                    action.minimize = true;
                }
                ui.add_space(10.0);
                let (rule, _) = ui.allocate_exact_size(Vec2::new(1.0, 18.0), Sense::hover());
                ui.painter().rect_filled(rule, 0.0, alpha(p.border, 170));
                ui.add_space(10.0);
            }
            if cfg.window_buttons || cfg.show_actions {
                if cfg.show_actions {
                    if icon_button(ui, p, "设置", paint_gear).clicked() {
                        action.settings = true;
                    }
                    let theme_tip = if p.dark {
                        "切换到浅色模式"
                    } else {
                        "切换到深色模式"
                    };
                    let dark = p.dark;
                    let titlebar = p.titlebar;
                    if icon_button(ui, p, theme_tip, move |painter, rect, color| {
                        paint_theme(painter, rect, color, dark, titlebar)
                    })
                    .clicked()
                    {
                        action.toggle_theme = true;
                    }
                    if icon_button(ui, p, "切换为浮窗模式", paint_mini).clicked() {
                        action.mini = true;
                    }
                }
                buttons_left = ui.min_rect().left() - 6.0;
            }
        },
    );

    let drag = Rect::from_min_max(
        egui::pos2(drag_left, bar.top()),
        egui::pos2(buttons_left.max(drag_left + 8.0), bar.bottom()),
    );
    let resp = ui.interact(
        drag,
        ui.scope_id().with("titlebar_drag"),
        Sense::click_and_drag(),
    );
    if resp.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if resp.double_clicked() && cfg.window_buttons {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!cfg.maximized));
    }
    action
}

/// 无边框窗口沿用系统缩放行为，最大化时停用边缘手柄。
pub fn resize_handles(ui: &Ui, enabled: bool) {
    if !enabled {
        return;
    }
    let ctx = ui.ctx();
    let view = ctx.content_rect();
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let edge = 7.0;
    let left = pos.x <= view.left() + edge;
    let right = pos.x >= view.right() - edge;
    let top = pos.y <= view.top() + edge;
    let bottom = pos.y >= view.bottom() - edge;
    let dir = match (left, right, top, bottom) {
        (true, _, true, _) => Some((
            egui::ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNorthWest,
        )),
        (_, true, true, _) => Some((
            egui::ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNorthEast,
        )),
        (true, _, _, true) => Some((
            egui::ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeSouthWest,
        )),
        (_, true, _, true) => Some((
            egui::ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeSouthEast,
        )),
        (true, _, _, _) => Some((egui::ResizeDirection::West, egui::CursorIcon::ResizeWest)),
        (_, true, _, _) => Some((egui::ResizeDirection::East, egui::CursorIcon::ResizeEast)),
        (_, _, true, _) => Some((egui::ResizeDirection::North, egui::CursorIcon::ResizeNorth)),
        (_, _, _, true) => Some((egui::ResizeDirection::South, egui::CursorIcon::ResizeSouth)),
        _ => None,
    };
    if let Some((dir, cursor)) = dir {
        ctx.set_cursor_icon(cursor);
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
    }
}

enum WinBtn {
    Minimize,
    Maximize(bool),
    Close,
}

/// 窗口控制提供轻量悬停反馈，关闭按钮保留明确的危险色。
fn window_button(ui: &mut Ui, p: &Palette, kind: WinBtn) -> Response {
    let close = matches!(kind, WinBtn::Close);
    let width = if close { 40.0 } else { 36.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
    // Ease the hover state in and out instead of snapping.
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.07);
    let hover_bg = if close { p.close_hover } else { p.track };
    if t > 0.001 {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), mix(p.titlebar, hover_bg, t));
    }
    let color = if close {
        mix(p.dim, Color32::WHITE, t)
    } else {
        mix(p.dim, p.text, t * 0.6)
    };
    let stroke = Stroke::new(1.3, color);
    let c = rect.center();
    let painter = ui.painter();
    match kind {
        WinBtn::Minimize => {
            painter.line_segment(
                [egui::pos2(c.x - 4.5, c.y), egui::pos2(c.x + 4.5, c.y)],
                stroke,
            );
        }
        WinBtn::Maximize(maximized) => {
            if maximized {
                painter.rect_stroke(
                    Rect::from_min_max(
                        egui::pos2(c.x - 5.0, c.y - 3.0),
                        egui::pos2(c.x + 3.5, c.y + 5.0),
                    ),
                    CornerRadius::same(1),
                    stroke,
                    StrokeKind::Inside,
                );
                painter.rect_stroke(
                    Rect::from_min_max(
                        egui::pos2(c.x - 2.5, c.y - 5.0),
                        egui::pos2(c.x + 5.0, c.y + 2.5),
                    ),
                    CornerRadius::same(1),
                    stroke,
                    StrokeKind::Inside,
                );
            } else {
                painter.rect_stroke(
                    Rect::from_center_size(c, Vec2::splat(9.0)),
                    CornerRadius::same(1),
                    stroke,
                    StrokeKind::Inside,
                );
            }
        }
        WinBtn::Close => {
            painter.line_segment(
                [
                    egui::pos2(c.x - 4.5, c.y - 4.5),
                    egui::pos2(c.x + 4.5, c.y + 4.5),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x + 4.5, c.y - 4.5),
                    egui::pos2(c.x - 4.5, c.y + 4.5),
                ],
                stroke,
            );
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let tip = match kind {
        WinBtn::Minimize => "最小化",
        WinBtn::Maximize(true) => "还原窗口",
        WinBtn::Maximize(false) => "最大化",
        WinBtn::Close => "关闭窗口",
    };
    resp.on_hover_text(tip)
}

/// Back arrow, sized exactly like the app monogram it replaces so the title
/// does not jump sideways when a page opens.
fn back_button(ui: &mut Ui, p: &Palette) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.07);
    if t > 0.001 {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(7), mix(p.titlebar, p.chip_bg, t));
    }
    let c = rect.center();
    let stroke = Stroke::new(1.5, mix(p.dim, p.text, t * 0.8));
    ui.painter().line_segment(
        [egui::pos2(c.x + 2.6, c.y - 4.6), egui::pos2(c.x - 2.4, c.y)],
        stroke,
    );
    ui.painter().line_segment(
        [egui::pos2(c.x - 2.4, c.y), egui::pos2(c.x + 2.6, c.y + 4.6)],
        stroke,
    );
    // A short tail turns the chevron into an arrow that reads as "go back".
    ui.painter().line_segment(
        [egui::pos2(c.x - 1.6, c.y), egui::pos2(c.x + 5.6, c.y)],
        stroke,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text("返回设备列表（Esc）")
}

/// 工具图标使用一致的点击面积与过渡。
fn icon_button(
    ui: &mut Ui,
    p: &Palette,
    tip: &str,
    paint: impl FnOnce(&egui::Painter, Rect, Color32),
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(34.0, 32.0), Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.07);
    if t > 0.001 {
        let bg = Color32::from_rgba_unmultiplied(
            p.chip_bg.r(),
            p.chip_bg.g(),
            p.chip_bg.b(),
            (t * 255.0) as u8,
        );
        ui.painter().rect_filled(rect, CornerRadius::same(8), bg);
    }
    // Icons brighten slightly as the hover fades in.
    paint(ui.painter(), rect, mix(p.dim, p.text, t * 0.7));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip)
}

/// Sun while dark mode is active ("switch to light"), moon in light mode.
fn paint_theme(painter: &egui::Painter, rect: Rect, color: Color32, dark: bool, bg: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.4, color);
    if dark {
        painter.circle_stroke(c, 4.6, stroke);
        for i in 0..8 {
            let a = i as f32 * std::f32::consts::TAU / 8.0;
            let dir = Vec2::angled(a);
            painter.line_segment([c + dir * 6.6, c + dir * 8.6], stroke);
        }
    } else {
        // Crescent: a filled disc with a small offset disc knocked out.
        painter.circle_filled(c, 5.4, color);
        painter.circle_filled(c + Vec2::new(2.4, -2.4), 4.8, bg);
    }
}

/// 窗口里嵌一张小卡片：表示“把这扇大窗收成桌面浮窗”。
fn paint_mini(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.3, color);
    painter.rect_stroke(
        Rect::from_min_max(
            egui::pos2(c.x - 6.2, c.y - 5.2),
            egui::pos2(c.x + 6.2, c.y + 5.2),
        ),
        CornerRadius::same(2),
        stroke,
        StrokeKind::Inside,
    );
    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(c.x + 0.2, c.y + 0.2),
            egui::pos2(c.x + 5.2, c.y + 4.2),
        ),
        CornerRadius::same(1),
        color,
    );
}

/// 滑杆形态使设置入口与主题按钮一眼可区分。
fn paint_gear(painter: &egui::Painter, rect: Rect, color: Color32) {
    // Three sliders read as "settings" instantly; the old radial ticks beside
    // the sun/moon toggle looked like a second theme button.
    let c = rect.center();
    let stroke = Stroke::new(1.4, color);
    let rows = [(-4.2f32, -2.2f32), (0.0, 2.4), (4.2, -0.6)];
    for (dy, knob) in rows {
        let y = c.y + dy;
        painter.line_segment([egui::pos2(c.x - 6.4, y), egui::pos2(c.x + 6.4, y)], stroke);
        painter.circle_filled(egui::pos2(c.x + knob, y), 1.9, color);
    }
}
