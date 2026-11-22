use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, Painter, Pos2, Rect, Response,
    RichText, Sense, Stroke, Ui, Vec2,
};

use crate::driver::DeviceType;
use crate::hid::ConnectionType;
use crate::theme::{
    alpha, card_shadow, ctl, heading_font, meta, micro, numeral, numeral_font, text, Palette,
};

/// Shortcut shown in rescan tooltips; must match the binding in `app.rs`.
pub const RESCAN_HINT: &str = "F5 或 Ctrl+R";

/// 窗口级卡片的圆角半径，主界面与浮窗共用一个值以保持观感一致。
pub const CARD_RADIUS: u8 = 16;

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
/// σ=8、offsetY=3 时看得见的部分（alpha ≥ 2/255）到 12px 就结束，加上下移
/// 是 15px，压在调用方留的 `SHADOW_PAD`（16px）内。两者是一对：动 pad 之前
/// 先看这里最外一环还剩多少 alpha，`visible_shadow_stays_inside_the_padding`
/// 就是这条约束的看门人。
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
    for (sigma, dy, peak) in shadow_lobes(dark, focused) {
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

/// 投影的两瓣高斯：`(σ, 下移, 峰值不透明度)`。紧阴影贴着边，环境阴影铺得
/// 远而淡；失焦时整体收淡一档，和系统窗口"不活动时影子更浅"的行为对齐，
/// 否则切到别的应用后这块卡片会显得浮在桌面上面，比前台窗口还重。
///
/// 浮窗的投影现在由 `shadow` 模块画在一个独立的分层窗口里（见那边的模块
/// 注释：主窗口要裁成卡片形状，投影不能再画在它里面），两边必须用同一组
/// 参数，否则切主题或失焦时会看到影子换了个形状。
pub(crate) fn shadow_lobes(dark: bool, focused: bool) -> [(f32, f32, f32); 2] {
    let lobes: [(f32, f32, f32); 2] = if dark {
        [(2.4, 1.0, 0.16), (8.0, 3.0, 0.20)]
    } else {
        [(2.4, 1.0, 0.07), (8.0, 3.0, 0.08)]
    };
    let lift = if focused { 1.0 } else { 0.78 };
    lobes.map(|(sigma, dy, peak)| (sigma, dy, peak * lift))
}

/// 环 `r`（距卡片边缘的像素数）那一圈要叠的 alpha，0 表示这一环画了也看不见。
///
/// 抽成纯函数是为了让"透明边距够不够"能被离线验证：只要有一环落在窗口外，
/// 卡片边缘就会留下一条被裁断的硬边。见 `visible_shadow_stays_inside_the_padding`。
/// `shadow` 模块画分层窗口时也用它，两条绘制路径必须逐环一致。
pub(crate) fn ring_alpha(sigma: f32, peak: f32, r: i32) -> u8 {
    let density = |r: i32| {
        if r <= 0 {
            peak
        } else {
            peak * (-((r as f32) / sigma).powi(2)).exp()
        }
    };
    let a = 1.0 - (1.0 - density(r)) / (1.0 - density(r + 1));
    (a * 255.0).round() as u8
}

/// 反解每层透明度，使阴影连续衰减到透明边距内。
fn paint_shadow_lobe(painter: &Painter, rect: Rect, radius: u8, sigma: f32, peak: f32) {
    let reach = (2.7 * sigma).ceil() as i32;
    for r in (1..=reach).rev() {
        let alpha = ring_alpha(sigma, peak, r);
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

/// 共用表面负责圆角与统一留白；瓷白材质不描边，层次交给底色差与柔投影。
pub fn surface<R>(
    ui: &mut Ui,
    p: &Palette,
    margin: Margin,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    let mut frame = egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(margin)
        .shadow(card_shadow(p))
        .begin(ui);
    frame
        .content_ui
        .set_width(frame.content_ui.available_width());
    frame.content_ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
    let inner = add(&mut frame.content_ui);
    let response = frame.end(ui);
    egui::InnerResponse::new(inner, response)
}

/// 标准配置卡片沿用共用表面与统一留白。
pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    surface(ui, p, Margin::same(18), add).inner
}

/// 键盘焦点环：只画在控件内侧，不改变布局，也不和悬停描边抢位置。
/// 自绘控件（档位、步进、开关、主按钮）要把自己注册进 Tab 顺序才有这个环。
/// 是否处于"键盘导航"状态：只有用键盘移动焦点时才画焦点环。
///
/// 鼠标点过的控件同样会拿到焦点（egui 的默认行为）。如果照画不误，点一下
/// 已经选中的档位按钮，就会在它的 1px 描边外面再套一圈 2px 蓝环——看起来
/// 像按钮被套了两层边框。Windows / macOS 的通行做法是只在键盘导航时显示
/// 焦点指示（focus-visible），鼠标操作不显示。
fn keyboard_nav(ui: &Ui) -> bool {
    let ctx = ui.ctx();
    let frame = ctx.cumulative_frame_nr();
    let id = egui::Id::unique("ph-keyboard-nav");
    let (last, mode) = ctx
        .data(|d| d.get_temp::<(u64, bool)>(id))
        .unwrap_or((0, false));
    if last == frame {
        return mode; // 同一帧里多个控件调用，只算一次
    }
    let key_moved = ctx.input(|i| {
        [
            egui::Key::Tab,
            egui::Key::ArrowUp,
            egui::Key::ArrowDown,
            egui::Key::ArrowLeft,
            egui::Key::ArrowRight,
        ]
        .iter()
        .any(|k| i.key_pressed(*k))
    });
    let clicked = ctx.input(|i| i.pointer.any_click());
    let mode = if key_moved {
        true
    } else if clicked {
        false
    } else {
        mode
    };
    ctx.data_mut(|d| d.insert_temp(id, (frame, mode)));
    mode
}

/// 键盘焦点环：偏移在控件**外侧**，中间留一圈底色。
///
/// 以前画在内侧，2px 环紧贴控件自己的 1px 描边，就成了"深蓝边 + 浅蓝边"
/// 两道；即使挪到外侧，只要还贴着也是两层。所以既要外置、也要留缝。
pub fn focus_ring(ui: &Ui, rect: Rect, p: &Palette, radius: u8) {
    if !keyboard_nav(ui) {
        return;
    }
    ui.painter().rect_stroke(
        rect.expand(3.0),
        CornerRadius::same(radius.saturating_add(3)),
        Stroke::new(1.5, alpha(p.accent, 130)),
        egui::StrokeKind::Middle,
    );
}

/// 自绘控件不经过 egui 的 Button，禁用态不会自动压暗——只是点不动，
/// 看起来仍然可点。所有自绘颜色都要过一遍这个函数。
fn faded(c: Color32, enabled: bool) -> Color32 {
    if enabled {
        c
    } else {
        c.gamma_multiply(0.45)
    }
}

/// 返回"这次是否被键盘激活"。焦点本身由 egui 按 `Sense` 自动分配（`click()`
/// 带 FOCUSABLE），这里只负责把回车/空格翻译成一次点击，并在点击后把焦点
/// 留在控件上——否则键盘用户点过一次就丢失当前位置。
fn keyboard_activated(ui: &mut Ui, resp: &Response) -> bool {
    let focused = resp.has_focus();
    if resp.clicked() {
        resp.request_focus();
    }
    focused && ui.input(|i| i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter))
}

/// 鼠标、回车与空格共用同一激活路径，供完全自绘的卡片和图标按钮复用。
pub fn activated(ui: &mut Ui, resp: &Response) -> bool {
    let by_key = keyboard_activated(ui, resp);
    resp.clicked() || by_key
}

/// 档位选择是胶囊：未选中落在底色差上，选中整块实心强调色，不再靠描边标识。
/// 自绘以便悬停wash画在文字下面（egui Button 的 fill 覆盖会连悬停态一起盖掉）。
pub fn chip(ui: &mut Ui, p: &Palette, text: &str, selected: bool) -> Response {
    let en = ui.is_enabled();
    let fg = faded(if selected { p.on_accent } else { p.dim }, en);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(text::BODY), fg);
    let size = Vec2::new((galley.size().x + 28.0).max(54.0), ctl::H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.08);
    let fill = if selected {
        mix(p.accent, p.accent_hover, t * 0.5)
    } else {
        mix(p.chip_bg, p.track, t * 0.55)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(ctl::PILL), faded(fill, en));
    painter.galley(rect.center() - galley.size() / 2.0, galley, fg);
    if resp.has_focus() {
        focus_ring(ui, rect, p, ctl::PILL);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// 行内的次要动作——比如卡头部"当前 1600"旁边一个"恢复初始值"——用比
/// `chip` 小一号的尺寸：字号和高度都跟着头部小字对齐，不喧宾夺主。
pub fn chip_small(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    let resp = ui.add(
        egui::Button::new(meta(text, p.text))
            .fill(p.chip_bg)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(ctl::H_SM as u8 / 2))
            .min_size(Vec2::new(0.0, ctl::H_SM)),
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_stroke(
            resp.rect,
            CornerRadius::same(ctl::H_SM as u8 / 2),
            Stroke::new(1.0, alpha(p.accent, 120)),
            egui::StrokeKind::Inside,
        );
    }
    if resp.has_focus() {
        focus_ring(ui, resp.rect, p, ctl::H_SM as u8 / 2);
    }
    resp
}

/// DPI 按钮保持固定宽度，数值变化不引起重排；描边与色彩共同标识当前档。
/// 返回"这次是否被激活"：鼠标点击与键盘回车/空格同等对待。
pub fn dpi_chip(ui: &mut Ui, p: &Palette, value: u16, selected: bool) -> bool {
    let size = Vec2::new(62.0, ctl::H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let by_key = keyboard_activated(ui, &resp);
    let focused = resp.has_focus();
    let t = ui.ctx().animate_value_with_time(
        resp.id.with("hover"),
        if resp.hovered() { 1.0 } else { 0.0 },
        0.08,
    );

    let en = ui.is_enabled();
    let fg = faded(
        if selected {
            p.on_accent
        } else {
            mix(p.dim, p.text, t * 0.75)
        },
        en,
    );
    let galley = ui
        .painter()
        .layout_no_wrap(value.to_string(), numeral_font(14.0), fg);
    let painter = ui.painter();
    let fill = if selected {
        mix(p.accent, p.accent_hover, t * 0.5)
    } else {
        mix(p.chip_bg, p.track, t * 0.55)
    };
    painter.rect_filled(rect, CornerRadius::same(ctl::PILL), faded(fill, en));
    painter.galley(rect.center() - galley.size() / 2.0, galley, fg);
    if focused {
        focus_ring(ui, rect, p, ctl::PILL);
    }
    let clicked = resp.clicked();
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        resp.on_hover_text(format!("{value} DPI"));
    }
    clicked || by_key
}

/// Fires immediately on press, then auto-repeats while the button is held,
/// so users do not have to click dozens of times to cross a wide range.
///
/// 加减号是画出来的两段线，不再用字体里的 `+` / `−`：符号字符随字体基线
/// 落位，26px 的方框里会偏上 1px，而且笔画粗细跟旁边的箭头对不上。
pub fn step_button(ui: &mut Ui, p: &Palette, plus: bool, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let by_key = keyboard_activated(ui, &resp);
    let focused = resp.has_focus();
    let t = ui.ctx().animate_value_with_time(
        resp.id.with("hover"),
        if resp.hovered() { 1.0 } else { 0.0 },
        0.08,
    );
    let en = ui.is_enabled();
    let painter = ui.painter();
    if t > 0.01 {
        painter.rect_filled(
            rect,
            CornerRadius::same(ctl::RADIUS_SM),
            alpha(p.chip_bg, (t * 235.0) as u8),
        );
    }
    painter.rect_stroke(
        rect,
        CornerRadius::same(ctl::RADIUS_SM),
        Stroke::new(1.0, faded(mix(p.border, p.accent, t * 0.55), en)),
        egui::StrokeKind::Inside,
    );
    let c = rect.center();
    let stroke = Stroke::new(1.5, faded(mix(p.dim, p.text, 0.45 + t * 0.55), en));
    let arm = 4.4;
    painter.line_segment(
        [egui::pos2(c.x - arm, c.y), egui::pos2(c.x + arm, c.y)],
        stroke,
    );
    if plus {
        painter.line_segment(
            [egui::pos2(c.x, c.y - arm), egui::pos2(c.x, c.y + arm)],
            stroke,
        );
    }
    if focused {
        focus_ring(ui, rect, p, ctl::RADIUS_SM);
    }
    let fired = hold_repeat(ui, resp.id, &resp) || by_key;
    if fired || resp.hovered() {
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

/// 保留按下即触发和长按重复的交互，状态按控件隔离。
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

/// 连接标签使用与设备图标一致的语义色。
pub fn connection_chip(ui: &mut Ui, p: &Palette, conn: ConnectionType) {
    connection_chip_colored(ui, p, conn, connection_color(p, conn));
}

/// Same chip, but the caller decides the hue — offline devices need a muted
/// one rather than a colour that promises a live link.
pub fn connection_chip_colored(ui: &mut Ui, p: &Palette, conn: ConnectionType, color: Color32) {
    let bg = alpha(color, if p.dark { 26 } else { 18 });
    let font = egui::FontId::proportional(text::MICRO);
    let text = conn.label();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let size = galley.size() + Vec2::new(18.0, 8.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let pnt = ui.painter();
    let pill = (size.y / 2.0) as u8;
    pnt.rect_filled(rect, CornerRadius::same(pill), bg);
    pnt.rect_stroke(
        rect,
        CornerRadius::same(pill),
        Stroke::new(1.0, alpha(color, if p.dark { 42 } else { 32 })),
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
    let painter = ui.painter();
    let radius = (size * 0.3) as u8;
    painter.rect_filled(rect, CornerRadius::same(radius), p.chip_bg);
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
            if extent >= 18.0 {
                painter.rect_filled(
                    body,
                    CornerRadius::same((extent * 0.28) as u8),
                    alpha(color, 24),
                );
            }
            painter.rect_stroke(
                body,
                CornerRadius::same((extent * 0.28) as u8),
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment([pt(0.0, -0.5), pt(0.0, -0.36)], stroke);
            painter.rect_filled(
                Rect::from_center_size(pt(0.0, -0.28), Vec2::new(extent * 0.10, extent * 0.19)),
                CornerRadius::same(2),
                color,
            );
        }
        DeviceType::Keyboard => {
            // 横向机身 + 两排键位（下排三颗，读起来像空格行的起点）。
            let body = Rect::from_min_max(pt(-0.47, -0.3), pt(0.47, 0.3));
            if extent >= 18.0 {
                painter.rect_filled(
                    body,
                    CornerRadius::same((extent * 0.1) as u8),
                    alpha(color, 24),
                );
            }
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
///
/// 数值字号固定：以前按列宽在 15–19 之间自适应，同一排三个指标会因为数字宽度
/// 不同而出现三种字号，整行看起来像没对齐。18 号在 340px 卡片的三列里放得下
/// 五位数，溢出交给 `truncate`，不靠缩字号救。
#[allow(clippy::too_many_arguments)]
pub fn metric_col(
    ui: &mut Ui,
    p: &Palette,
    width: f32,
    label: &str,
    value: &str,
    unit: &str,
    value_color: Color32,
    tip: &str,
) {
    let out =
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            // Default row spacing would double every gap below.
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add(egui::Label::new(meta(label, p.dim)).truncate());
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add(egui::Label::new(numeral(value, text::METRIC, value_color)).truncate());
                if !unit.is_empty() {
                    ui.add_space(3.0);
                    ui.label(micro(unit, p.dim));
                }
            });
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
    ui.painter().rect_filled(line, 0, alpha(p.border, 170));
}

/// 主按钮用于空态等唯一关键动作；深色下用近白填充，浅色下用强调色。
pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str, tip: &str) -> bool {
    refresh_button_impl(ui, p, text, tip, true)
}

/// 带刷新图标的次要按钮，用于已有内容页，避免与设备卡片争夺视觉重心。
pub fn refresh_button(ui: &mut Ui, p: &Palette, text: &str, tip: &str) -> bool {
    refresh_button_impl(ui, p, text, tip, false)
}

fn refresh_button_impl(ui: &mut Ui, p: &Palette, text: &str, tip: &str, primary: bool) -> bool {
    let (fill, ink) = if primary {
        (p.accent, p.on_accent)
    } else {
        (p.chip_bg, p.text)
    };
    // 颜色先压暗再排版：galley 里存的是当时的颜色，事后再 faded 只改得了图标。
    let en = ui.is_enabled();
    let (fill, ink) = (faded(fill, en), faded(ink, en));
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), heading_font(text::BODY), ink);
    let size = Vec2::new(galley.size().x + 44.0, ctl::H_BTN);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let by_key = keyboard_activated(ui, &resp);
    let focused = resp.has_focus();
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    // 按下时只让内容下沉 1px，底不跟着动：整块位移会像窗口抖了一下。
    let shift = if resp.is_pointer_button_down_on() {
        1.0
    } else {
        0.0
    };
    let painter = ui.painter();
    let fill = faded(
        mix(fill, p.accent, t * if p.dark { 0.16 } else { 0.10 }),
        en,
    );
    painter.rect_filled(rect, CornerRadius::same(ctl::RADIUS), fill);
    if !primary {
        painter.rect_stroke(
            rect,
            CornerRadius::same(ctl::RADIUS),
            Stroke::new(1.0, faded(mix(p.border, p.accent, t * 0.55), en)),
            egui::StrokeKind::Inside,
        );
    }
    let icon_c = egui::pos2(rect.left() + 18.0, rect.center().y + shift);
    paint_refresh(painter, icon_c, 5.4, ink);
    painter.galley(
        egui::pos2(
            rect.left() + 31.0,
            rect.center().y - galley.size().y / 2.0 + shift,
        ),
        galley,
        ink,
    );
    if focused {
        focus_ring(ui, rect, p, ctl::RADIUS);
    }
    let clicked = resp.clicked();
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if !tip.is_empty() {
        resp.on_hover_text(tip);
    }
    clicked || by_key
}

/// 先为右侧动作预留宽度，左侧内容只能使用剩余区域，避免窄窗口相互挤压。
pub fn split_row(ui: &mut Ui, right: impl FnOnce(&mut Ui), left: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            right(ui);
            ui.add_space(12.0);
            ui.with_layout(Layout::left_to_right(Align::Center), left);
        });
    });
}

/// Quiet secondary action used in card footers: tinted accent fill, accent
/// text, and a chevron that steps right on hover.
pub fn action_link(ui: &mut Ui, p: &Palette, text: &str, warn: bool) -> Response {
    let color = if warn { p.warn } else { p.accent };
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::proportional(text::META), color);
    let size = Vec2::new(galley.size().x + 34.0, 26.0);
    // 与整卡点击等价，属于重复入口：不进 Tab，键盘用户直接停在卡片上。
    let (rect, resp) = ui.allocate_exact_size(size, Sense::CLICK);
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    // Ghost at rest, tinted only while hovered: repeated footer actions should
    // read as quiet affordances, not as four primary buttons.
    if t > 0.001 {
        let bg = alpha(color, (t * if p.dark { 34.0 } else { 22.0 }) as u8);
        ui.painter().rect_filled(rect, CornerRadius::same(13), bg);
    }
    let painter = ui.painter();
    painter.galley(
        egui::pos2(rect.left() + 11.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    let cx = rect.right() - 13.0 + t * 2.0;
    let cy = rect.center().y;
    let stroke = Stroke::new(1.15, color);
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
        .layout_no_wrap(text.to_owned(), FontId::proportional(11.0), color);
    let size = galley.size() + Vec2::new(16.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same((size.y / 2.0) as u8),
        alpha(color, if p.dark { 26 } else { 18 }),
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
    let font = FontId::proportional(text::META);
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
    if resp.has_focus() {
        focus_ring(ui, rect, p, ctl::RADIUS_SM);
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
    let stroke = Stroke::new(1.2, color);
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
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if !desc.is_empty() {
            ui.label(RichText::new(desc).size(12.0).color(p.dim));
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
/// than as the selected state of a control.
fn rec_tag(ui: &mut Ui, p: &Palette, rec: &str) {
    let color = if p.dark {
        mix(p.info, Color32::WHITE, 0.3)
    } else {
        p.info
    };
    let text = format!("推荐 {rec}");
    // 不低于 12px：11px 下"读"这类十来笔的汉字，笔画在 egui 的无 hinting
    // 灰度抗锯齿里会并成一团，比正文小一号的代价是看不清字。
    let galley = ui
        .painter()
        .layout_no_wrap(text, FontId::proportional(12.0), color);
    let size = galley.size() + Vec2::new(10.0, 5.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(6),
        p.info.gamma_multiply(if p.dark { 0.22 } else { 0.10 }),
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.on_hover_text("这是多数场景下好用的取值，可以按手感再调");
}

/// 标签与开关共享触发行为，圆形滑块随真实状态平滑移动。
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
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(44.0, 24.0), Sense::click());
            let by_key = keyboard_activated(ui, &resp);
            let focused = resp.has_focus();
            let t = ui.ctx().animate_bool(resp.id, value);
            let hover = ui.ctx().animate_value_with_time(
                resp.id.with("hover"),
                if resp.hovered() { 1.0 } else { 0.0 },
                0.08,
            );
            let en = ui.is_enabled();
            let track = if value {
                mix(p.accent, p.accent_hover, hover)
            } else {
                mix(p.track, p.dim, hover * 0.12)
            };
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(12), faded(track, en));
            let cx = rect.left() + 12.0 + t * 20.0;
            // 滑块始终是白色：这是 Windows / iOS 开关的共同约定，深色主题下
            // 浅蓝轨道配白滑块也够区分，靠下面那层投影把滑块托起来。
            let knob = faded(Color32::WHITE, en);
            painter.circle_filled(
                egui::pos2(cx, rect.center().y + 0.6),
                9.4,
                Color32::from_black_alpha(if p.dark { 70 } else { 24 }),
            );
            painter.circle_filled(egui::pos2(cx, rect.center().y), 9.0, knob);
            // 关闭态的滑块压在浅色轨道上，补一圈描边才立得起来。
            painter.circle_stroke(
                egui::pos2(cx, rect.center().y),
                9.0,
                Stroke::new(
                    1.0,
                    if value {
                        alpha(p.on_accent, 0)
                    } else {
                        alpha(p.dim, if p.dark { 0 } else { 70 })
                    },
                ),
            );
            if focused {
                focus_ring(ui, rect, p, 12);
            }
            if resp.hovered() || label_response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() || by_key {
                on_change(!value);
            }
        });
    });
    setting_hint(ui, p, desc, rec);
    ui.add_space(8.0);
}

/// 数值与单位分开排：数字用数字字面（细体、拉丁），单位回到正文字族。
///
/// 数字字族只带规则的中文（回退到展示字族里的等线 Bold），把“10 分钟”
/// 整串交给它，中文单位就会突然加粗、和同一行的数字对不上。所以这里
/// 把单位拆出来单独排版，字号跟着说明文字走。
#[allow(clippy::too_many_arguments)]
pub fn stepper_row(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    desc: &str,
    rec: &str,
    value: &str,
    unit: &str,
    mut on_click: impl FnMut(bool, bool),
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(13.0).color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut plus = false;
            let mut minus = false;
            if step_button(ui, p, true, "按住可连续增加") {
                plus = true;
            }
            if step_button(ui, p, false, "按住可连续减少") {
                minus = true;
            }
            ui.add_space(6.0);
            // 固定单元格宽度，微调时按钮不随数值宽度左右移动；内容在手绘
            // 排版里居中，避免横向布局把数字顶到单元格左边。
            let (cell, _) = ui.allocate_exact_size(Vec2::new(72.0, 28.0), Sense::hover());
            let en = ui.is_enabled();
            let value_font = if value.is_ascii() {
                numeral_font(15.0)
            } else {
                FontId::proportional(text::BODY)
            };
            let painter = ui.painter();
            let value_gal = painter.layout_no_wrap(value.to_owned(), value_font, faded(p.text, en));
            let unit_gal = (!unit.is_empty()).then(|| {
                painter.layout_no_wrap(
                    unit.to_owned(),
                    FontId::proportional(text::META),
                    faded(p.dim, en),
                )
            });
            let gap = if unit_gal.is_some() { 3.0 } else { 0.0 };
            let total = value_gal.size().x + gap + unit_gal.as_ref().map_or(0.0, |g| g.size().x);
            let left = cell.center().x - total / 2.0;
            painter.galley(
                egui::pos2(left, cell.center().y - value_gal.size().y / 2.0),
                value_gal.clone(),
                p.text,
            );
            if let Some(gal) = unit_gal {
                painter.galley(
                    egui::pos2(
                        left + value_gal.size().x + gap,
                        cell.center().y - gal.size().y / 2.0,
                    ),
                    gal,
                    p.dim,
                );
            }
            if plus || minus {
                on_click(minus, plus);
            }
        });
    });
    setting_hint(ui, p, desc, rec);
    ui.add_space(9.0);
}

/// 分组标题用字重与留白区分层级。
pub fn section_heading(ui: &mut Ui, p: &Palette, title: &str, description: &str) {
    // 卡片小标题不再用展示字族：半粗在深色底上会显得过黑、过重，
    // 改成常规字重、比正文大一档，让页面标题与设备名保持唯一的重心。
    ui.label(RichText::new(title).size(15.0).color(p.text));
    ui.add_space(4.0);
    ui.label(meta(description, p.dim));
}

/// 卡片内分隔线保持低对比，避免与外部边界竞争。
pub fn divider(ui: &mut Ui, p: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, alpha(p.border, 190));
}

/// Battery bar that is honest about what it knows:
/// - a percentage fills proportionally in its status color,
/// - a wired device shows a full, muted bar (no misleading empty bar),
/// - an unknown level shows only the track.
pub fn battery_bar(ui: &mut Ui, p: &Palette, battery: Option<u8>, wired: bool, color: Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, p.track);
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
        None if wired => Some((rect, color.gamma_multiply(0.45))),
        None => None,
    };
    if let Some((mut filled, color)) = fill {
        // Keep the fill's trailing edge rounded even at low percentages.
        filled.set_width(filled.width().max(4.0));
        painter.rect_filled(filled, 2.0, color);
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

/// 在两种不透明表面色之间插值，供材质与悬停反馈共用。
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

    /// 透明边距必须盖住投影里"看得见"的那一段，否则卡片边缘会留下一条被裁断的
    /// 硬边。判据取 alpha ≥ 2/255：1/255 的那几环画在桌面上（浅色主题下更是
    /// 0.1 的峰值）人眼分不出来，允许被窗口裁掉——收窄边距的意义正在于此。
    /// 每个瓣都向下偏移 `dy`，所以底部要比环半径多留这一截。
    ///
    /// 22 那一版多留了 8px 纯空白；16 是"看得见的部分 + 下移"刚好装下的值。
    /// 改 `SHADOW_PAD` 或改 σ/dy 都要先过这条断言。
    #[test]
    fn visible_shadow_stays_inside_the_padding() {
        let pad = crate::ui::mini::SHADOW_PAD;
        // 取两瓣中最亮的一档（深色 + 聚焦）。
        let lobes = shadow_lobes(true, true);
        for (sigma, dy, peak) in lobes {
            let furthest = (1..=40)
                .filter(|&r| ring_alpha(sigma, peak, r) >= 2)
                .max()
                .expect("投影总得有看得见的环");
            assert!(
                furthest as f32 + dy <= pad,
                "σ={sigma} 的投影看得见到 {}px（含下移 {dy}px），超出 SHADOW_PAD（{pad}px）",
                furthest
            );
        }
        // 太短就失去"浮起"的观感：环境阴影只看得到 12px 以内时，卡片会贴死在桌面上。
        let reach = (1..=40)
            .filter(|&r| ring_alpha(8.0, 0.20, r) >= 2)
            .max()
            .unwrap();
        assert!(reach >= 10, "环境阴影只剩 {reach}px，卡片会贴死在桌面上");
    }

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
