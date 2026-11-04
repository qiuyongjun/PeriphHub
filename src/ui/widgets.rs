use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, Painter, Pos2, Rect, Response,
    RichText, Sense, Stroke, Ui, Vec2,
};

use crate::driver::DeviceType;
use crate::hid::ConnectionType;
use crate::theme::{alpha, card_shadow, Palette};

/// Shortcut shown in rescan tooltips; must match the binding in `app.rs`.
pub const RESCAN_HINT: &str = "F5 或 Ctrl+R";

/// 窗口级圆角卡片的圆角半径，主界面与浮窗共用一个值以保持观感一致。
pub const CARD_RADIUS: u8 = 14;

/// 圆角卡片 + 手绘投影，画在窗口矩形内的透明边距里。
///
/// Win10 拿不到无边框的系统级阴影（见 `app::window_shell`），只能用像素
/// 画。画法不是"叠几层同浓度的矩形"，而是按高斯反解每一环的透明度：
/// 圆角矩形 `rect.expand(r)` 从外向内逐像素叠，环 r 的 alpha 由
/// `(1 - a_r)(1 - D(r+1)) = 1 - D(r)` 解出，叠完之后半径 r 处的累积浓度
/// 正好等于 `D(r) = peak * exp(-(r/σ)²)`。这样得到的是连续衰减：
/// 相邻像素的差值只有 1~9/255（原先 6 层等浓度是每 3px 跳 12~13 级），
/// 既没有等间距台阶，也不会在窗口边缘留下一条"忽然截断"的硬边。
///
/// 两瓣叠加做出方向感：紧贴边缘的小 σ 给接触阴影，大 σ 给周围的环境阴影，
/// 两者都向下偏移一点，光看起来来自上方（系统阴影也是这个味道）。
///
/// σ=8、offsetY=3 时可见衰减在 14px 内就归零，压在调用方留的 `SHADOW_PAD`（22px）内。
///
/// `focused` 来自 `InputState::focused`：窗口失焦时整体收淡一档，和系统窗口
/// "不活动时影子更浅"的行为对齐，否则切到别的应用后这块卡片会显得浮在桌面
/// 上面，比前台窗口还重。
pub fn shadow_card(
    painter: &Painter,
    rect: Rect,
    fill: Color32,
    radius: u8,
    dark: bool,
    focused: bool,
) {
    // (sigma, 下移, 峰值不透明度)：紧阴影贴着边，环境阴影铺得远而淡。
    let lobes: [(f32, f32, f32); 2] = if dark {
        [(2.4, 1.0, 0.22), (8.0, 3.0, 0.26)]
    } else {
        [(2.4, 1.0, 0.09), (8.0, 3.0, 0.10)]
    };
    let lift = if focused { 1.0 } else { 0.78 };
    let lobes = lobes.map(|(sigma, dy, peak)| (sigma, dy, peak * lift));
    for (sigma, dy, peak) in lobes {
        paint_shadow_lobe(
            painter,
            rect.translate(egui::vec2(0.0, dy)),
            radius,
            sigma,
            peak,
        );
    }
    painter.rect_filled(rect, CornerRadius::same(radius), fill);
}

fn paint_shadow_lobe(painter: &Painter, rect: Rect, radius: u8, sigma: f32, peak: f32) {
    let reach = (2.7 * sigma).ceil() as i32;
    let density = |r: i32| {
        if r <= 0 {
            peak
        } else {
            peak * (-((r as f32) / sigma).powi(2)).exp()
        }
    };
    for r in (1..=reach).rev() {
        let a = 1.0 - (1.0 - density(r)) / (1.0 - density(r + 1));
        let alpha = (a * 255.0).round() as u8;
        if alpha == 0 {
            continue;
        }
        painter.rect_filled(
            rect.expand(r as f32),
            CornerRadius::same(radius.saturating_add(r as u8)),
            Color32::from_black_alpha(alpha),
        );
    }
}

pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .shadow(card_shadow(p))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn chip(ui: &mut Ui, p: &Palette, text: &str, selected: bool) -> Response {
    let (fill, fg, stroke) = if selected {
        (p.accent, p.on_accent, Stroke::NONE)
    } else {
        (p.chip_bg, p.text, Stroke::new(1.0, p.border))
    };
    ui.add(
        egui::Button::new(RichText::new(text).size(12.5).color(fg))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(52.0, 30.0)),
    )
}

/// 标准尺寸（30px 高）的 chip 是给选中态/档位用的；行内的次要动作——
/// 比如卡头部"当前 1600"旁边一个"恢复初始值"——用同款会喧宾夺主，
/// 这里收小一号：字号和高度都跟着头部小字对齐。
pub fn chip_small(ui: &mut Ui, p: &Palette, text: &str, selected: bool) -> Response {
    let (fill, fg, stroke) = if selected {
        (p.accent, p.on_accent, Stroke::NONE)
    } else {
        (p.chip_bg, p.text, Stroke::new(1.0, p.border))
    };
    ui.add(
        egui::Button::new(RichText::new(text).size(11.0).color(fg))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(6))
            .min_size(Vec2::new(0.0, 22.0)),
    )
}

/// A DPI stage chip. The number is the whole story here, so there is no
/// magnitude bar underneath: a stage's DPI has no ceiling to be measured
/// against, and the bar only invited false comparisons between stages.
///
/// The width is fixed rather than fitted to the digits: a stage going from 400
/// to 26000 must not widen its chip and re-flow the row under the cursor.
pub fn dpi_chip(ui: &mut Ui, p: &Palette, value: u16, selected: bool) -> Response {
    let (fill, fg, stroke) = if selected {
        (p.accent, p.on_accent, Stroke::NONE)
    } else {
        (p.chip_bg, p.text, Stroke::new(1.0, p.border))
    };
    let galley = ui
        .painter()
        .layout_no_wrap(value.to_string(), FontId::proportional(12.5), fg);
    let size = Vec2::new(62.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, fg);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if !selected {
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(8),
                Stroke::new(1.0, p.dim.gamma_multiply(0.7)),
                egui::StrokeKind::Inside,
            );
        }
        resp.on_hover_text(format!("{value} DPI"))
    } else {
        resp
    }
}

/// Fires immediately on press, then auto-repeats while the button is held,
/// so users do not have to click dozens of times to cross a wide range.
pub fn step_button(ui: &mut Ui, p: &Palette, label: &str, tip: &str) -> bool {
    let resp = ui.add(
        egui::Button::new(RichText::new(label).size(15.0).color(p.text))
            .fill(p.chip_bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(7))
            .min_size(Vec2::new(28.0, 28.0)),
    );
    let fired = hold_repeat(ui, resp.id, &resp);
    if fired {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip);
    fired
}

#[derive(Clone, Copy, Default)]
struct HoldState {
    press: Option<f64>,
    last: f64,
    /// The current press already produced a step, so the release that follows
    /// must not produce a second one.
    fired: bool,
}

const HOLD_DELAY: f64 = 0.45;
const REPEAT_EVERY: f64 = 0.1;

fn hold_repeat(ui: &mut Ui, id: egui::Id, resp: &Response) -> bool {
    let now = ui.input(|i| i.time);
    let mut state = ui
        .ctx()
        .data(|d| d.get_temp::<HoldState>(id))
        .unwrap_or_default();
    let fire = hold_step(
        &mut state,
        resp.is_pointer_button_down_on(),
        resp.clicked(),
        now,
    );
    ui.ctx().data_mut(|d| d.insert_temp(id, state));
    if state.press.is_some() {
        ui.ctx().request_repaint_after(Duration::from_millis(80));
    }
    fire
}

/// The decision half of `hold_repeat`, kept free of egui so the click-vs-hold
/// rules can be tested: a press must fire exactly once, holding must auto
/// repeat, and a press+release squeezed into one frame must still count.
fn hold_step(state: &mut HoldState, down: bool, clicked: bool, now: f64) -> bool {
    let mut fire = false;
    if down {
        match state.press {
            None => {
                state.press = Some(now);
                state.last = now;
                state.fired = true;
                fire = true;
            }
            Some(press) => {
                if now - press >= HOLD_DELAY && now - state.last >= REPEAT_EVERY {
                    state.last = now;
                    fire = true;
                }
            }
        }
    } else {
        // A press and release can land in the same frame — a quick click, or a
        // frame that took long enough to swallow both events. Without this the
        // step button would look dead for exactly those clicks. `fired` keeps a
        // normal click (press seen in an earlier frame) from counting twice.
        if clicked && !state.fired {
            fire = true;
        }
        state.press = None;
        state.fired = false;
    }
    fire
}

pub fn connection_chip(ui: &mut Ui, p: &Palette, conn: ConnectionType) {
    connection_chip_colored(ui, p, conn, connection_color(p, conn));
}

/// Same chip, but the caller decides the hue — offline devices need a muted
/// one rather than a colour that promises a live link.
pub fn connection_chip_colored(ui: &mut Ui, _p: &Palette, conn: ConnectionType, color: Color32) {
    let bg = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 26);
    let font = egui::FontId::proportional(11.0);
    let text = conn.label();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let size = galley.size() + Vec2::new(16.0, 7.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let pnt = ui.painter();
    pnt.rect_filled(rect, CornerRadius::same(6), bg);
    pnt.rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0, color.gamma_multiply(0.45)),
        egui::StrokeKind::Inside,
    );
    let pos = rect.center() - galley.size() / 2.0;
    pnt.galley(pos, galley, color);
}

/// One colour per link type, so a device's badge, chip and border all agree.
pub fn connection_color(p: &Palette, conn: ConnectionType) -> Color32 {
    match conn {
        ConnectionType::Wireless24G => p.accent,
        ConnectionType::Bluetooth => p.info,
        ConnectionType::Wired => p.ok,
        ConnectionType::Unknown => p.dim,
    }
}

/// Device badge: the device-type glyph on a wash of its link colour. The type
/// is what the eye scans for ("which one is the keyboard?"); the model name
/// next to it is what tells two keyboards apart.
pub fn device_tile(
    ui: &mut Ui,
    p: &Palette,
    kind: DeviceType,
    letter: &str,
    tint: Color32,
    size: f32,
) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let wash = alpha(tint, if p.dark { 44 } else { 26 });
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same((size * 0.3) as u8), wash);
    painter.rect_stroke(
        rect,
        CornerRadius::same((size * 0.3) as u8),
        Stroke::new(1.0, alpha(tint, if p.dark { 70 } else { 40 })),
        egui::StrokeKind::Inside,
    );
    if kind == DeviceType::Unknown {
        // 没有可信形态时退回品牌首字母，总比一个含义不明的图形好。
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            letter,
            FontId::proportional(size * 0.46),
            tint,
        );
    } else {
        paint_device_glyph(painter, rect.center(), kind, tint, size * 0.55);
    }
    resp.on_hover_text(kind.label());
}

/// Hand-drawn shape per device type. `extent` is the glyph's height; the width
/// follows the silhouette, so one drawing serves both the 40px card tile and
/// the 14px widget row. All offsets are in units of `extent`, which keeps the
/// strokes optically consistent at every size.
pub fn paint_device_glyph(
    painter: &Painter,
    center: Pos2,
    kind: DeviceType,
    color: Color32,
    extent: f32,
) {
    let stroke = Stroke::new(if extent >= 18.0 { 1.25 } else { 1.0 }, color);
    let pt = |x: f32, y: f32| center + Vec2::new(x * extent, y * extent);
    match kind {
        DeviceType::Mouse => {
            // 俯视鼠标：竖长的胶囊 + 顶部滚轮，顶部一小段缝表示左右键分界。
            let body = Rect::from_min_max(pt(-0.28, -0.5), pt(0.28, 0.5));
            painter.rect_stroke(
                body,
                CornerRadius::same((extent * 0.28) as u8),
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment([pt(0.0, -0.5), pt(0.0, -0.36)], stroke);
            painter.circle_filled(pt(0.0, -0.28), extent * 0.07, color);
        }
        DeviceType::Keyboard => {
            // 横向机身 + 两排键位（下排三颗，读起来像空格行的起点）。
            let body = Rect::from_min_max(pt(-0.47, -0.3), pt(0.47, 0.3));
            painter.rect_stroke(
                body,
                CornerRadius::same((extent * 0.1) as u8),
                stroke,
                egui::StrokeKind::Inside,
            );
            for x in [-0.29, -0.10, 0.10, 0.29] {
                painter.circle_filled(pt(x, -0.09), extent * 0.045, color);
            }
            for x in [-0.20, 0.0, 0.20] {
                painter.circle_filled(pt(x, 0.14), extent * 0.045, color);
            }
        }
        DeviceType::Receiver => {
            // 竖插的 USB 接收器：窄插头在上，宽机身在下，插头里两条触点。
            let body = Rect::from_min_max(pt(-0.28, -0.18), pt(0.28, 0.5));
            painter.rect_stroke(
                body,
                CornerRadius::same((extent * 0.08) as u8),
                stroke,
                egui::StrokeKind::Inside,
            );
            let plug = Rect::from_min_max(pt(-0.14, -0.5), pt(0.14, -0.18));
            painter.rect_stroke(
                plug,
                CornerRadius::same((extent * 0.04) as u8),
                stroke,
                egui::StrokeKind::Inside,
            );
            for y in [-0.42, -0.32] {
                painter.line_segment([pt(-0.07, y), pt(0.07, y)], stroke);
            }
        }
        DeviceType::Unknown => {
            let body = Rect::from_min_max(pt(-0.3, -0.3), pt(0.3, 0.3));
            painter.rect_stroke(
                body,
                CornerRadius::same((extent * 0.1) as u8),
                stroke,
                egui::StrokeKind::Inside,
            );
        }
    }
}

/// Label / value / note stack. Bare columns read faster than boxed tiles and
/// scale down better. No magnitude bar: DPI and polling rate have no ceiling
/// to fill, so a bar only implied a ratio that does not exist.
#[allow(clippy::too_many_arguments)]
pub fn metric_col(
    ui: &mut Ui,
    p: &Palette,
    width: f32,
    label: &str,
    value: &str,
    note: &str,
    value_color: Color32,
    tip: &str,
) {
    let out =
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            // Default row spacing would double every gap below.
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(label).size(10.5).color(p.dim));
            ui.add_space(4.0);
            ui.label(RichText::new(value).size(18.0).strong().color(value_color));
            ui.add_space(3.0);
            ui.label(RichText::new(note).size(10.5).color(p.dim));
        });
    if !tip.is_empty() {
        out.response.on_hover_text(tip);
    }
}

/// Placeholder block for values that are still being fetched. A pulsing bar
/// is honest about "loading" in a way that three "--" strings are not.
pub fn skeleton(ui: &mut Ui, p: &Palette, width: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    let phase = (t * 1.7).sin() * 0.5 + 0.5;
    let fill = mix(p.track, p.chip_bg, 0.25 + phase * 0.55);
    ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
    ui.ctx().request_repaint_after(Duration::from_millis(60));
}

/// Hairline used to separate a card's body from its footer.
pub fn hairline(ui: &mut Ui, p: &Palette, strength: u8) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, alpha(p.border, strength));
}

/// Vertical rule between columns. Rows are top-aligned (see `horizontal_top`
/// call sites), so the rule paints itself `top` px down instead of relying on
/// cross-axis centring — which would drag the rule and the row out of sync.
pub fn vrule(ui: &mut Ui, p: &Palette, top: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, top + height), Sense::hover());
    let line = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.top() + top),
        Vec2::new(1.0, height),
    );
    ui.painter().rect_filled(line, 0, alpha(p.border, 200));
}

/// The one filled button in the app. Painted by hand so the hover state is a
/// real colour change instead of the flat fill egui's `Button` keeps.
pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str, tip: &str) -> Response {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::proportional(12.5), p.on_accent);
    let size = Vec2::new(galley.size().x + 46.0, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(10),
        mix(p.accent, p.accent_hover, t),
    );
    let icon_c = egui::pos2(rect.left() + 19.0, rect.center().y);
    paint_refresh(painter, icon_c, 5.6, p.on_accent);
    painter.galley(
        egui::pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0),
        galley,
        p.on_accent,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    }
}

/// One row with `right` flush against the right edge and `left` taking the
/// rest. Doing it right-to-left first avoids the 8px item spacing that
/// `with_layout` would otherwise leave between the two halves.
pub fn split_row(ui: &mut Ui, right: impl FnOnce(&mut Ui), left: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        // A nested `with_layout` inherits one item spacing of inset; zeroing it
        // here is what makes the right group sit flush with the content edge.
        ui.spacing_mut().item_spacing.x = 0.0;
        left(ui);
        ui.add_space(12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| right(ui));
    });
}

/// Quiet secondary action used in card footers: tinted accent fill, accent
/// text, and a chevron that steps right on hover.
pub fn action_link(ui: &mut Ui, p: &Palette, text: &str, warn: bool) -> Response {
    let color = if warn { p.warn } else { p.accent };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(11.5), color);
    let size = Vec2::new(galley.size().x + 34.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    let base = if p.dark { 30.0 } else { 22.0 };
    let top = if p.dark { 54.0 } else { 40.0 };
    let bg = alpha(color, (base + (top - base) * t) as u8);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), bg);
    painter.galley(
        egui::pos2(rect.left() + 11.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    let cx = rect.right() - 13.0 + t * 2.0;
    let cy = rect.center().y;
    let stroke = Stroke::new(1.3, color);
    painter.line_segment(
        [egui::pos2(cx - 2.0, cy - 3.2), egui::pos2(cx + 1.2, cy)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(cx + 1.2, cy), egui::pos2(cx - 2.0, cy + 3.2)],
        stroke,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// Small count/label pill, e.g. "3 台".
pub fn pill(ui: &mut Ui, p: &Palette, text: &str, color: Color32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(10.5), color);
    let size = galley.size() + Vec2::new(14.0, 5.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(5),
        alpha(color, if p.dark { 34 } else { 24 }),
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
}

/// A quiet secondary action: icon plus label, no fill and no border at rest, a
/// soft wash on hover. Used where a bordered chip would compete with the
/// content around it (the device header already has one tinted badge).
pub fn link_action(
    ui: &mut Ui,
    p: &Palette,
    text: &str,
    tip: &str,
    icon: impl FnOnce(&egui::Painter, egui::Pos2, f32, Color32),
) -> Response {
    let font = FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, p.text);
    let size = Vec2::new(galley.size().x + 36.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.08);
    if t > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(7),
            alpha(p.chip_bg, (t * 255.0) as u8),
        );
    }
    let color = mix(p.dim, p.text, t);
    let painter = ui.painter();
    icon(
        painter,
        egui::pos2(rect.left() + 12.0, rect.center().y),
        5.0,
        color,
    );
    painter.galley(
        egui::pos2(rect.left() + 22.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip)
}

/// Circular "refresh" glyph: a 290° arc with a tangent arrowhead. Drawn by hand
/// because no single CJK-safe character reads as "refresh".
pub fn paint_refresh(painter: &egui::Painter, center: egui::Pos2, r: f32, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    let start = -0.35 * std::f32::consts::PI;
    let sweep = 1.65 * std::f32::consts::PI;
    let steps = 18;
    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let a = start + sweep * (i as f32 / steps as f32);
        pts.push(center + Vec2::angled(a) * r);
    }
    let head = *pts.last().unwrap();
    painter.add(egui::Shape::line(pts.clone(), stroke));
    // Arrowhead sits just past the arc's end, pointing along the sweep.
    let dir = {
        let prev = pts[steps.saturating_sub(1)];
        (head - prev).normalized()
    };
    let perp = Vec2::new(-dir.y, dir.x);
    let tip = head + dir * 3.4;
    painter.add(egui::Shape::convex_polygon(
        vec![
            tip,
            head + perp * 2.2 - dir * 0.6,
            head - perp * 2.2 - dir * 0.6,
        ],
        color,
        Stroke::NONE,
    ));
}

/// One line of explanation under a setting, followed by an optional
/// recommendation. Advanced switches (line correction, ripple control,
/// debounce) are meaningless without this text — the label alone does not
/// tell anyone whether to turn them on.
fn setting_hint(ui: &mut Ui, p: &Palette, desc: &str, rec: &str) {
    if desc.is_empty() && rec.is_empty() {
        return;
    }
    ui.add_space(3.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if !desc.is_empty() {
            ui.label(RichText::new(desc).size(10.5).color(p.dim));
        }
        if !rec.is_empty() {
            if !desc.is_empty() {
                ui.add_space(6.0);
            }
            rec_tag(ui, p, rec);
        }
    });
    ui.add_space(2.0);
}

/// "推荐 xxx" chip. Tinted with the info hue so it reads as advice rather
/// than as the selected state of a control. Dark mode lifts the hue toward
/// white first: a 10px run of mid-blue on a dark wash is barely legible.
fn rec_tag(ui: &mut Ui, p: &Palette, rec: &str) {
    let color = if p.dark {
        mix(p.info, Color32::WHITE, 0.3)
    } else {
        p.info
    };
    let text = format!("推荐 {rec}");
    let galley = ui
        .painter()
        .layout_no_wrap(text, FontId::proportional(10.5), color);
    let size = galley.size() + Vec2::new(10.0, 5.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(4),
        p.info.gamma_multiply(if p.dark { 0.22 } else { 0.10 }),
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.on_hover_text("这是多数场景下好用的取值，可以按手感再调");
}

pub fn toggle_row(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    desc: &str,
    rec: &str,
    value: bool,
    mut on_change: impl FnMut(bool),
) {
    ui.horizontal(|ui| {
        let label_response = ui.add(
            egui::Label::new(RichText::new(label).size(13.0).color(p.text)).sense(Sense::click()),
        );
        if label_response.clicked() {
            on_change(!value);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(42.0, 24.0), Sense::click());
            let t = ui.ctx().animate_bool(resp.id, value);
            let track = if value { p.accent } else { p.track };
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(12), track);
            let cx = rect.left() + 12.0 + t * 18.0;
            // The knob is always the pole opposite the track, so it reads on
            // both the monochrome accent and the neutral grey track.
            let knob = if value {
                p.on_accent
            } else {
                mix(p.dim, p.card, 0.65)
            };
            painter.circle_filled(egui::pos2(cx, rect.center().y), 9.0, knob);
            if resp.hovered() || label_response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                on_change(!value);
            }
        });
    });
    setting_hint(ui, p, desc, rec);
}

pub fn stepper_row(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    desc: &str,
    rec: &str,
    value: &str,
    mut on_click: impl FnMut(bool, bool),
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.5).color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut plus = false;
            let mut minus = false;
            if step_button(ui, p, "+", "按住可连续增加") {
                plus = true;
            }
            if step_button(ui, p, "−", "按住可连续减少") {
                minus = true;
            }
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(
                Vec2::new(72.0, 28.0),
                Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(RichText::new(value).size(12.5).strong().color(p.text));
                },
            );
            if plus || minus {
                on_click(minus, plus);
            }
        });
    });
    setting_hint(ui, p, desc, rec);
    ui.add_space(4.0);
}

pub fn section_heading(ui: &mut Ui, p: &Palette, title: &str, description: &str) {
    ui.label(RichText::new(title).size(14.0).strong().color(p.text));
    ui.add_space(2.0);
    ui.label(RichText::new(description).size(10.5).color(p.dim));
}

pub fn divider(ui: &mut Ui, p: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.border);
}

/// Battery bar that is honest about what it knows:
/// - a percentage fills proportionally in its status color,
/// - a wired device shows a full, muted bar (no misleading empty bar),
/// - an unknown level shows only the track.
pub fn battery_bar(ui: &mut Ui, p: &Palette, battery: Option<u8>, wired: bool, color: Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 7.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.5, p.track);
    let fill = match battery {
        Some(percent) => {
            let mut filled = rect;
            filled.set_width(rect.width() * (percent.min(100) as f32 / 100.0));
            if filled.width() > 0.5 {
                Some((filled, color))
            } else {
                None
            }
        }
        None if wired => Some((rect, p.info.gamma_multiply(0.45))),
        None => None,
    };
    if let Some((mut filled, color)) = fill {
        // Keep the fill's trailing edge rounded even at low percentages.
        filled.set_width(filled.width().max(7.0));
        painter.rect_filled(filled, 3.5, color);
    }
}

/// A tiny status indicator. `pulse` makes it breathe, for transient states
/// such as "connecting"; it needs periodic repaints from the caller.
pub fn status_dot(ui: &mut Ui, color: Color32, pulse: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    let (radius, c) = if pulse {
        let phase = (t * 2.2).sin() * 0.5 + 0.5;
        let r = 2.2 + phase * 1.6;
        (r, color.gamma_multiply(0.45 + phase * 0.55))
    } else {
        (3.0, color)
    };
    ui.painter().circle_filled(rect.center(), radius, c);
}

pub fn stat(ui: &mut Ui, p: &Palette, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(11.0).color(p.dim));
        ui.label(RichText::new(value).size(15.0).strong().color(p.text));
    });
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// Marketing tails that vendor product strings carry: a card title needs the
/// model, not a description of what a mouse is. "G304 Lightspeed Wireless
/// Gaming Mouse" is a box label; "G304 Lightspeed" is a title. The full string
/// stays in the tooltip, and the tail is only dropped while something is left.
pub fn short_model(model: &str) -> &str {
    const TAIL: &[&str] = &[
        " Wireless Gaming Mouse",
        " Wireless Mouse",
        " Gaming Mouse",
        " Optical Mouse",
        " Lightspeed Mouse",
        " Mechanical Keyboard",
        " Wireless Keyboard",
        " Gaming Keyboard",
        " Keyboard",
        " Mouse",
    ];
    let mut s = model.trim();
    'strip: loop {
        let lower = s.to_ascii_lowercase();
        for tail in TAIL {
            if s.len() > tail.len() && lower.ends_with(&tail.to_ascii_lowercase()) {
                s = s[..s.len() - tail.len()].trim_end();
                continue 'strip;
            }
        }
        break;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Press in one frame, release in the next: exactly one step.
    #[test]
    fn click_steps_once() {
        let mut s = HoldState::default();
        assert!(hold_step(&mut s, true, false, 0.0), "press fires");
        assert!(!hold_step(&mut s, true, false, 0.05), "no repeat yet");
        assert!(
            !hold_step(&mut s, false, true, 0.10),
            "release must not fire again"
        );
        assert!(!hold_step(&mut s, false, false, 0.20));
    }

    /// Both edges inside a single frame must still produce one step.
    #[test]
    fn collapsed_click_steps_once() {
        let mut s = HoldState::default();
        assert!(hold_step(&mut s, false, true, 0.0));
        assert!(!hold_step(&mut s, false, false, 0.05));
    }

    /// Holding repeats after the delay, at the repeat interval.
    #[test]
    fn hold_repeats_after_delay() {
        let mut s = HoldState::default();
        assert!(hold_step(&mut s, true, false, 0.0));
        assert!(!hold_step(&mut s, true, false, HOLD_DELAY - 0.01));
        assert!(hold_step(&mut s, true, false, HOLD_DELAY + 0.01));
        assert!(!hold_step(
            &mut s,
            true,
            false,
            HOLD_DELAY + REPEAT_EVERY / 2.0
        ));
        assert!(hold_step(
            &mut s,
            true,
            false,
            HOLD_DELAY + REPEAT_EVERY + 0.01
        ));
    }

    #[test]
    fn drops_marketing_tail() {
        assert_eq!(
            short_model("G304 Lightspeed Wireless Gaming Mouse"),
            "G304 Lightspeed"
        );
        assert_eq!(short_model("MX Master 3S Wireless Mouse"), "MX Master 3S");
        assert_eq!(short_model("RK98 Mechanical Keyboard"), "RK98");
        assert_eq!(short_model("g304 wireless gaming mouse"), "g304");
    }

    #[test]
    fn keeps_names_that_are_already_short() {
        assert_eq!(short_model("VT1 Pro"), "VT1 Pro");
        assert_eq!(short_model("Lightspeed 接收器"), "Lightspeed 接收器");
    }

    /// A device that is literally called "Mouse" must not become nameless.
    #[test]
    fn never_strips_to_nothing() {
        assert_eq!(short_model("Mouse"), "Mouse");
        assert_eq!(short_model(" Keyboard "), "Keyboard");
    }
}
