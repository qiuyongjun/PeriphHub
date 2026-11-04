//! The mini widget: a small always-visible card that answers one question —
//! "are my peripherals okay?" — without opening the main window.
//!
//! It is the same native window as the main UI, only resized and repainted as
//! a floating card. Everything is painted by hand: at this size a single
//! stray layout margin is visible, and the card has to read at a glance from
//! across a desk.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, Vec2,
};

use crate::devices::{DeviceId, DeviceManager, Entry, Link};
use crate::theme::{alpha, battery_color, battery_label, battery_level, Palette};
use crate::ui::widgets::{
    connection_color, mix, paint_device_glyph, shadow_card, short_model, CARD_RADIUS,
};

/// Widget card width. The native window is this plus `SHADOW_PAD` on each side.
pub const WIDTH: f32 = 296.0;
/// 卡片四周的透明边距：egui 只能在窗口矩形内作画，系统级阴影在 Win10 上
/// 又拿不到（DWM 缝线无法隐藏），所以投影由自己在卡片后面画，这段边距
/// 就是阴影的落脚处。可见衰减约 14px（σ=8 + 3px 下移），留到 22 是为了
/// 让阴影在窗口边缘之前就已经淡到 0，不会出现被裁断的硬边。
pub const SHADOW_PAD: f32 = 22.0;

/// 浮窗原生窗口尺寸 = 卡片内容尺寸 + 四周 `SHADOW_PAD`。
pub fn window_size(content_w: f32, content_h: f32) -> Vec2 {
    Vec2::new(content_w + 2.0 * SHADOW_PAD, content_h + 2.0 * SHADOW_PAD)
}

const HEAD_H: f32 = 36.0;
const LIST_GAP: f32 = 6.0;
const ROW_H: f32 = 42.0;
const FOOT_H: f32 = 30.0;
/// Past this many devices the widget stops growing and scrolls instead, so it
/// never eats more desktop than it is worth.
const MAX_ROWS: usize = 5;

#[derive(Default)]
pub struct MiniAction {
    pub open_main: bool,
    pub open_config: Option<DeviceId>,
    pub toggle_pin: bool,
    pub toggle_theme: bool,
    pub rescan: bool,
    pub quit: bool,
}

/// Exact window height for the current content. `app` compares it against what
/// the window currently is and only resizes when it really changed.
pub fn height(devices: usize, attention: usize) -> f32 {
    let rows = devices.clamp(1, MAX_ROWS) as f32;
    let foot = if attention > 0 && devices > 0 {
        LIST_GAP + FOOT_H
    } else {
        0.0
    };
    HEAD_H + LIST_GAP + rows * ROW_H + foot + 8.0
}

fn needs_attention(entry: &Entry) -> bool {
    matches!(entry.state.link, Link::Offline(_))
        || matches!(
            battery_level(entry.state.battery, entry.info.connection.is_wired()),
            crate::theme::BatteryLevel::Critical | crate::theme::BatteryLevel::Low
        )
}

pub fn attention_count(manager: &DeviceManager) -> usize {
    manager.list().iter().filter(|e| needs_attention(e)).count()
}

pub fn view(
    ui: &mut Ui,
    p: &Palette,
    manager: &DeviceManager,
    pinned: bool,
    recently_scanned: bool,
) -> MiniAction {
    let mut action = MiniAction::default();
    let window = ui.ctx().content_rect();
    let card = window.shrink(SHADOW_PAD);
    let entries = manager.list();
    let attention = entries.iter().filter(|e| needs_attention(e)).count();
    let online = entries
        .iter()
        .filter(|e| !matches!(e.state.link, Link::Offline(_)))
        .count();

    paint_card(ui, p, card);

    let inner = Rect::from_min_max(
        Pos2::new(card.left() + 10.0, card.top()),
        Pos2::new(card.right() - 10.0, card.bottom() - 8.0),
    );
    let mut body = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(egui::Align::Min)),
    );
    body.set_clip_rect(card);
    body.spacing_mut().item_spacing = Vec2::ZERO;

    header(
        &mut body,
        p,
        &mut action,
        pinned,
        online,
        entries.len(),
        attention,
    );
    body.add_space(LIST_GAP);

    let list_h = entries.len().clamp(1, MAX_ROWS) as f32 * ROW_H;
    if entries.is_empty() {
        empty_row(&mut body, p, &mut action, recently_scanned, list_h);
    } else if entries.len() > MAX_ROWS {
        egui::ScrollArea::vertical()
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(&mut body, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for entry in entries {
                    row(ui, p, entry, &mut action);
                }
            });
    } else {
        for entry in entries {
            row(&mut body, p, entry, &mut action);
        }
    }

    if attention > 0 && !entries.is_empty() {
        body.add_space(LIST_GAP);
        footer(&mut body, p, &mut action, attention);
    }

    action
}

/// Card over the window. Inset from the window edge so the shadow (painted on
/// the same transparent surface) has room to spread; the shadow itself is what
/// separates the card from the desktop, so the card carries no stroke — a hard
/// 1px outline read as a stray line at the top of the widget.
fn paint_card(ui: &Ui, p: &Palette, card: Rect) {
    let focused = ui.ctx().input(|i| i.focused);
    shadow_card(ui.painter(), card, p.card, CARD_RADIUS, p.dark, focused);
}

fn header(
    body: &mut Ui,
    p: &Palette,
    action: &mut MiniAction,
    pinned: bool,
    online: usize,
    total: usize,
    attention: usize,
) {
    let width = body.available_width();
    let (rect, _) = body.allocate_exact_size(Vec2::new(width, HEAD_H), Sense::hover());
    // Owned clone: the buttons below take `&mut Ui`, which cannot coexist with
    // a borrow of the painter.
    let painter = body.painter().clone();

    // Buttons live on the right, laid out from the edge inward.
    let mut x = rect.right();
    let mut slots = [Rect::ZERO; 4];
    for slot in slots.iter_mut() {
        let r = Rect::from_min_size(Pos2::new(x - 22.0, rect.top() + 7.0), Vec2::splat(22.0));
        *slot = r;
        x = r.left() - 2.0;
    }
    let quit = icon_button(body, slots[0], p, "退出 PeriphHub", paint_close, false);
    let open = icon_button(body, slots[1], p, "打开主界面", paint_open, false);
    let pin = icon_button(
        body,
        slots[2],
        p,
        if pinned {
            "取消置顶"
        } else {
            "置顶显示在窗口最前"
        },
        paint_pin,
        pinned,
    );
    // Same sun/moon glyph as the titlebar, so the control reads identically on
    // both surfaces. The crescent knocks itself out with the card colour.
    let theme_tip = if p.dark {
        "切换到浅色模式"
    } else {
        "切换到深色模式"
    };
    let (dark, card) = (p.dark, p.card);
    let theme = icon_button(
        body,
        slots[3],
        p,
        theme_tip,
        move |painter, c, color| paint_theme(painter, c, color, dark, card),
        false,
    );
    if quit.clicked() {
        action.quit = true;
    }
    if open.clicked() {
        action.open_main = true;
    }
    if pin.clicked() {
        action.toggle_pin = true;
    }
    if theme.clicked() {
        action.toggle_theme = true;
    }

    // Monogram, name, and the one number that matters.
    let badge = Rect::from_min_size(
        Pos2::new(rect.left() + 2.0, rect.center().y - 9.0),
        Vec2::splat(18.0),
    );
    painter.rect_filled(badge, CornerRadius::same(5), p.accent);
    painter.text(
        badge.center(),
        Align2::CENTER_CENTER,
        "P",
        FontId::proportional(11.5),
        p.on_accent,
    );

    let title_pos = Pos2::new(badge.right() + 7.0, rect.center().y);
    painter.text(
        title_pos,
        Align2::LEFT_CENTER,
        "PeriphHub",
        FontId::proportional(11.5),
        p.text,
    );
    let title_w = painter
        .layout_no_wrap("PeriphHub".into(), FontId::proportional(11.5), p.text)
        .size()
        .x;

    let (dot_color, summary, summary_color) = if total == 0 {
        (p.dim, "无设备".to_string(), p.dim)
    } else if attention > 0 {
        (p.warn, format!("{attention} 项需关注"), p.warn)
    } else {
        (p.ok, format!("{online}/{total} 在线"), p.dim)
    };
    let dot_c = Pos2::new(title_pos.x + title_w + 12.0, rect.center().y);
    painter.circle_filled(dot_c, 2.6, dot_color);
    painter.text(
        Pos2::new(dot_c.x + 7.0, rect.center().y),
        Align2::LEFT_CENTER,
        summary,
        FontId::proportional(10.5),
        summary_color,
    );

    // Drag strip: everything left of the buttons. Registered after them so a
    // click on a button still wins.
    let drag = Rect::from_min_max(
        rect.left_top(),
        Pos2::new(slots[3].left() - 4.0, rect.bottom()),
    );
    let resp = body.interact(
        drag,
        body.scope_id().with("mini-drag"),
        Sense::click_and_drag(),
    );
    if resp.drag_started() {
        body.ctx()
            .send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        action.open_main = true;
    }
}

fn row(ui: &mut Ui, p: &Palette, entry: &Entry, action: &mut MiniAction) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
    let hover =
        ui.ctx()
            .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    let offline = matches!(entry.state.link, Link::Offline(_));
    let connecting = matches!(entry.state.link, Link::Connecting);
    let painter = ui.painter();

    if hover > 0.001 {
        painter.rect_filled(
            rect,
            CornerRadius::same(9),
            alpha(p.chip_bg, (hover * 215.0) as u8),
        );
    }

    // A colour bar instead of a dot: it survives being read at a glance and
    // keeps the row's left edge steady whatever the state is.
    let tint = if offline {
        p.dim
    } else {
        connection_color(p, entry.info.connection)
    };
    let bar = Rect::from_min_size(
        Pos2::new(rect.left() + 2.0, rect.center().y - 9.0),
        Vec2::new(3.0, 18.0),
    );
    painter.rect_filled(
        bar,
        CornerRadius::same(2),
        tint.gamma_multiply(if connecting { 0.45 } else { 0.95 }),
    );

    let wired = entry.info.connection.is_wired();
    let level = battery_level(entry.state.battery, wired);
    let (value, value_color) = match &entry.state.link {
        Link::Offline(_) => ("离线".to_string(), p.danger),
        Link::Connecting => ("连接中".to_string(), p.warn),
        Link::Online => match entry.state.battery {
            Some(b) => (format!("{b}%"), battery_color(level, p)),
            None if wired => ("有线".to_string(), p.info),
            None => ("在线".to_string(), p.ok),
        },
    };
    let value_font = FontId::proportional(12.5);
    let value_w = painter
        .layout_no_wrap(value.clone(), value_font.clone(), value_color)
        .size()
        .x;
    painter.text(
        Pos2::new(rect.right() - 9.0, rect.top() + 8.0),
        Align2::RIGHT_TOP,
        value,
        value_font,
        value_color,
    );

    // 形态图标放在色条与文字之间：一眼分出键盘和鼠标，不必先读型号。
    paint_device_glyph(
        painter,
        Pos2::new(rect.left() + 16.0, rect.center().y),
        entry.info.kind,
        tint,
        14.0,
    );
    let text_left = rect.left() + 28.0;
    let title_col = if offline { p.dim } else { p.text };
    let title_w = (rect.right() - 9.0 - value_w - 8.0) - text_left;
    let model = ellipsize(
        painter,
        short_model(&entry.info.model),
        FontId::proportional(12.5),
        title_w,
    );
    painter.text(
        Pos2::new(text_left, rect.top() + 8.0),
        Align2::LEFT_TOP,
        model,
        FontId::proportional(12.5),
        title_col,
    );

    // Second line: what the device is when all is well, what is wrong when it
    // is not. A low battery is more urgent than the link type.
    let (note, note_color) = match &entry.state.link {
        Link::Offline(_) => ("断开连接，点击查看".to_string(), p.dim),
        Link::Connecting => ("正在建立连接".to_string(), p.dim),
        Link::Online => match level {
            crate::theme::BatteryLevel::Critical | crate::theme::BatteryLevel::Low => {
                (battery_label(level).to_string(), value_color)
            }
            _ => (
                format!(
                    "{} · {}",
                    entry.info.brand.to_uppercase(),
                    entry.info.connection.label()
                ),
                p.dim,
            ),
        },
    };
    let note = ellipsize(
        painter,
        &note,
        FontId::proportional(9.5),
        rect.width() - 37.0,
    );
    painter.text(
        Pos2::new(text_left, rect.top() + 25.0),
        Align2::LEFT_TOP,
        note,
        FontId::proportional(9.5),
        note_color,
    );

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        action.open_config = Some(entry.info.id.clone());
    }
}

fn empty_row(
    body: &mut Ui,
    p: &Palette,
    action: &mut MiniAction,
    recently_scanned: bool,
    height: f32,
) {
    let width = body.available_width();
    let (rect, resp) = body.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let hover =
        body.ctx()
            .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    let painter = body.painter();
    if hover > 0.001 {
        painter.rect_filled(
            rect,
            CornerRadius::same(9),
            alpha(p.chip_bg, (hover * 215.0) as u8),
        );
    }
    let (title, note) = if recently_scanned {
        ("仍未发现设备", "检查接收器或数据线后重新扫描")
    } else {
        ("未发现设备", "点击扫描已连接的外设")
    };
    painter.text(
        Pos2::new(rect.center().x, rect.center().y - 8.0),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(12.0),
        p.text,
    );
    painter.text(
        Pos2::new(rect.center().x, rect.center().y + 9.0),
        Align2::CENTER_CENTER,
        note,
        FontId::proportional(9.5),
        p.dim,
    );
    if resp.hovered() {
        body.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        action.rescan = true;
    }
}

fn footer(body: &mut Ui, p: &Palette, action: &mut MiniAction, attention: usize) {
    let width = body.available_width();
    let (rect, resp) = body.allocate_exact_size(Vec2::new(width, FOOT_H), Sense::click());
    let hover =
        body.ctx()
            .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    let painter = body.painter();
    painter.rect_filled(
        Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), 1.0)),
        0,
        alpha(p.border, 170),
    );
    if hover > 0.001 {
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(rect.left(), rect.top() + 1.0),
                rect.right_bottom(),
            ),
            CornerRadius::same(8),
            alpha(p.chip_bg, (hover * 215.0) as u8),
        );
    }
    painter.circle_filled(
        Pos2::new(rect.left() + 5.0, rect.center().y + 0.5),
        2.8,
        p.warn,
    );
    painter.text(
        Pos2::new(rect.left() + 13.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{attention} 台设备需要关注"),
        FontId::proportional(10.5),
        mix(p.dim, p.text, hover * 0.7),
    );
    painter.text(
        Pos2::new(rect.right() - 9.0, rect.center().y),
        Align2::RIGHT_CENTER,
        "查看",
        FontId::proportional(10.5),
        mix(p.dim, p.text, hover),
    );
    if resp.hovered() {
        body.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        action.open_main = true;
    }
}

fn icon_button(
    ui: &mut Ui,
    rect: Rect,
    p: &Palette,
    tip: &str,
    paint: impl FnOnce(&egui::Painter, Pos2, Color32),
    accent: bool,
) -> egui::Response {
    let resp = ui.interact(rect, ui.scope_id().with(tip), Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.07);
    if t > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(6),
            alpha(p.chip_bg, (t * 255.0) as u8),
        );
    }
    let base = if accent { p.text } else { p.dim };
    paint(ui.painter(), rect.center(), mix(base, p.text, t * 0.6));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip)
}

fn paint_close(painter: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    for (a, b) in [
        (Vec2::new(-3.6, -3.6), Vec2::new(3.6, 3.6)),
        (Vec2::new(3.6, -3.6), Vec2::new(-3.6, 3.6)),
    ] {
        painter.line_segment([c + a, c + b], stroke);
    }
}

/// Arrow leaving a frame: "open the big window".
fn paint_open(painter: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    painter.rect_stroke(
        Rect::from_min_max(
            Pos2::new(c.x - 5.0, c.y - 1.0),
            Pos2::new(c.x + 1.0, c.y + 5.0),
        ),
        CornerRadius::same(1),
        stroke,
        StrokeKind::Inside,
    );
    painter.line_segment(
        [
            Pos2::new(c.x - 1.0, c.y + 1.0),
            Pos2::new(c.x + 5.0, c.y - 5.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(c.x + 5.0, c.y - 5.0),
            Pos2::new(c.x + 1.4, c.y - 5.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(c.x + 5.0, c.y - 5.0),
            Pos2::new(c.x + 5.0, c.y - 1.4),
        ],
        stroke,
    );
}

/// Two stacked rectangles, the top one filled: "this window stays above the
/// others". A literal pushpin at 12 px reads as a funnel, so it is not used.
fn paint_pin(painter: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.2, color);
    painter.rect_filled(
        Rect::from_min_max(
            Pos2::new(c.x - 5.4, c.y - 5.0),
            Pos2::new(c.x + 1.6, c.y + 2.0),
        ),
        CornerRadius::same(1),
        color,
    );
    painter.rect_stroke(
        Rect::from_min_max(
            Pos2::new(c.x - 1.4, c.y - 1.0),
            Pos2::new(c.x + 5.4, c.y + 5.0),
        ),
        CornerRadius::same(1),
        stroke,
        StrokeKind::Inside,
    );
}

/// Sun while dark mode is active ("switch to light"), moon in light mode.
/// Scaled-down copy of the titlebar glyph so both toggles read as one control.
fn paint_theme(painter: &egui::Painter, c: Pos2, color: Color32, dark: bool, bg: Color32) {
    let stroke = Stroke::new(1.3, color);
    if dark {
        painter.circle_stroke(c, 3.4, stroke);
        for i in 0..8 {
            let dir = Vec2::angled(i as f32 * std::f32::consts::TAU / 8.0);
            painter.line_segment([c + dir * 5.0, c + dir * 6.4], stroke);
        }
    } else {
        // Crescent: a filled disc with a small offset disc knocked out.
        painter.circle_filled(c, 4.0, color);
        painter.circle_filled(c + Vec2::new(1.8, -1.8), 3.6, bg);
    }
}

/// Truncate with an ellipsis rather than clipping mid-glyph. Only runs when
/// the text is actually too wide, so the measurement cost is paid rarely.
fn ellipsize(painter: &egui::Painter, text: &str, font: FontId, max_w: f32) -> String {
    let measure = |s: &str| {
        painter
            .layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if max_w > 0.0 && measure(text) > max_w {
        let chars: Vec<char> = text.chars().collect();
        for n in (0..chars.len()).rev() {
            let mut s: String = chars[..n].iter().collect();
            s.push('…');
            if measure(&s) <= max_w {
                return s;
            }
        }
        return "…".into();
    }
    text.to_owned()
}
