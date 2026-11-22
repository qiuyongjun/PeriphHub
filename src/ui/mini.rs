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
use crate::theme::{alpha, battery_color, battery_level, heading_font, paint_brand, text, Palette};
use crate::ui::chrome::{paint_gear, paint_theme};
use crate::ui::widgets::{
    activated, connection_color, focus_ring, mix, paint_device_glyph, shadow_card, short_model,
    CARD_RADIUS,
};

/// Widget card width. The native window is this plus `SHADOW_PAD` on each side.
pub const WIDTH: f32 = 320.0;
/// 卡片四周的透明边距：egui 只能在窗口矩形内作画，系统级阴影在 Win10 上
/// 又拿不到（DWM 缝线无法隐藏），所以投影由自己画在卡片后面，这段边距
/// 就是阴影的落脚处。它由 `crate::shadow` 的独立分层窗口绘制——主窗口会被
/// 裁成卡片形状，边距上的点击才能穿透到桌面与其他应用（见那边的模块注释）。
///
/// 取 16 而不是更宽：σ=8 那瓣**看得见**的环（alpha ≥ 2/255）到 12px 就没了，
/// 加上 3px 下移正好 15px；再往外只剩 1/255 的尾巴，被窗口边缘裁掉也看不出
/// 接缝。再往上加只会让窗口矩形比卡片多出一圈纯空白——贴边、Alt-Tab 缩略图、
/// 命中测试都按窗口矩形算，那圈空白都是要付账的（`visible_shadow_stays_inside_the_padding`
/// 把这条约束钉住了）。
pub const SHADOW_PAD: f32 = 16.0;

/// 浮窗原生窗口尺寸 = 卡片内容尺寸 + 四周 `SHADOW_PAD`。
pub fn window_size(content_w: f32, content_h: f32) -> Vec2 {
    Vec2::new(content_w + 2.0 * SHADOW_PAD, content_h + 2.0 * SHADOW_PAD)
}

/// 行高按两行文字（13 + 11）加 4px 余量取整，比上一版收紧，浮窗整体更小。
const HEAD_H: f32 = 50.0;
const LIST_GAP: f32 = 8.0;
const ROW_H: f32 = 52.0;
/// Past this many devices the widget stops growing and scrolls instead, so it
/// never eats more desktop than it is worth.
const MAX_ROWS: usize = 5;
/// 扫描完成/失败后的短反馈窗口，与首页概览保持一致。
const SCAN_NOTICE_SECS: u64 = 2;

#[derive(Default)]
pub struct MiniAction {
    pub open_main: bool,
    pub open_config: Option<DeviceId>,
    pub open_settings: bool,
    pub toggle_pin: bool,
    pub toggle_theme: bool,
    pub rescan: bool,
    pub quit: bool,
}

/// Exact window height for the current content. `app` compares it against what
/// the window currently is and only resizes when it really changed.
pub fn height(devices: usize) -> f32 {
    let rows = devices.clamp(1, MAX_ROWS) as f32;
    HEAD_H + LIST_GAP + rows * ROW_H + 10.0
}

/// 浮窗是状态窗，不是设备清单：只列当前在线的设备。离线与连接中的设备
/// 只在主界面出现，那里才有完整的身份、原因与数量概览。
pub fn is_visible(link: &Link) -> bool {
    matches!(link, Link::Online)
}

pub fn online_only(entries: &[Entry]) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|e| is_visible(&e.state.link))
        .collect()
}

/// 标题下的状态词：只说状态，不报任何设备数量（数量是主界面的职责）。
pub fn status_word(online: usize, connecting: usize, total: usize, scanning: bool) -> &'static str {
    if scanning {
        "正在扫描…"
    } else if total == 0 {
        "无设备"
    } else if online > 0 {
        "设备在线"
    } else if connecting > 0 {
        "正在连接"
    } else {
        "设备离线"
    }
}

/// 状态点颜色：扫描/连接中用警示色，在线用成功色，其余退回次级色。
fn status_color(p: &Palette, online: usize, connecting: usize, scanning: bool) -> Color32 {
    if scanning || connecting > 0 {
        p.warn
    } else if online > 0 {
        p.ok
    } else {
        p.dim
    }
}

/// 紧凑浮窗用双行身份和设备图标保持可读性；设置入口不再依赖主界面。
pub fn view(ui: &mut Ui, p: &Palette, manager: &DeviceManager, pinned: bool) -> MiniAction {
    let mut action = MiniAction::default();
    let window = ui.ctx().content_rect();
    let card = window.shrink(SHADOW_PAD);
    let entries = manager.list();
    let visible = online_only(entries);
    let online = visible.len();
    let connecting = entries
        .iter()
        .filter(|e| matches!(e.state.link, Link::Connecting))
        .count();

    paint_card(ui, p, card);

    let inner = Rect::from_min_max(
        Pos2::new(card.left() + 14.0, card.top()),
        Pos2::new(card.right() - 14.0, card.bottom() - 10.0),
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
        status_word(online, connecting, entries.len(), manager.is_scanning()),
        status_color(p, online, connecting, manager.is_scanning()),
    );
    body.add_space(LIST_GAP);

    let list_h = visible.len().clamp(1, MAX_ROWS) as f32 * ROW_H;
    if visible.is_empty() {
        // 完成/失败反馈只展示最新一次结果；失败会覆盖旧的“已更新”。
        let completed = manager.last_scan_completed();
        let failed = manager.last_scan_failed();
        let scan_failed = failed.is_some_and(|f| {
            completed.is_none_or(|c| f > c) && f.elapsed().as_secs() < SCAN_NOTICE_SECS
        });
        let recently_scanned = completed.is_some_and(|c| {
            failed.is_none_or(|f| c > f) && c.elapsed().as_secs() < SCAN_NOTICE_SECS
        });
        empty_row(
            &mut body,
            p,
            &mut action,
            recently_scanned,
            scan_failed,
            manager.is_scanning(),
            !entries.is_empty(),
            list_h,
        );
    } else if visible.len() > MAX_ROWS {
        egui::ScrollArea::vertical()
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(&mut body, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for entry in visible {
                    row(ui, p, entry, &mut action);
                }
            });
    } else {
        for entry in visible {
            row(&mut body, p, entry, &mut action);
        }
    }

    action
}

/// Card over the window. Inset from the window edge so the shadow has room to
/// spread; the shadow itself is what separates the card from the desktop, so the
/// card carries no stroke — a hard 1px outline read as a stray line at the top
/// of the widget.
///
/// 投影平时画在卡片外面那个独立的分层窗口里（`crate::shadow`）：主窗口被裁成
/// 卡片形状，边距上的点击才能穿透到桌面与其他应用，而裁剪同样会裁掉绘制。
/// 只有那个窗口建不起来时才退回在这里自绘。
fn paint_card(ui: &Ui, p: &Palette, card: Rect) {
    if !crate::shadow::attached() {
        let focused = ui.ctx().input(|i| i.focused);
        shadow_card(ui.painter(), card, p.card, CARD_RADIUS, p.dark, focused);
        return;
    }
    // 卡片要铺满整块：留出半像素缝会从窗口区域的边缘透出桌面。
    ui.painter()
        .rect_filled(card, CornerRadius::same(CARD_RADIUS), p.card);
}

/// 标题与状态词分成两行，避免状态文字覆盖右侧工具按钮。
fn header(
    body: &mut Ui,
    p: &Palette,
    action: &mut MiniAction,
    pinned: bool,
    summary: &str,
    dot_color: Color32,
) {
    let width = body.available_width();
    let (rect, _) = body.allocate_exact_size(Vec2::new(width, HEAD_H), Sense::hover());
    // Owned clone: the buttons below take `&mut Ui`, which cannot coexist with
    // a borrow of the painter.
    let painter = body.painter().clone();

    // 30px 工具按钮兼顾紧凑与触达面积；设置放在最左侧，浮窗内即可直达。
    // ID 用稳定 key，不跟 tooltip/置顶状态变化，否则键盘焦点会在切换时丢失。
    let mut x = rect.right();
    let mut slots = [Rect::ZERO; 5];
    for slot in slots.iter_mut() {
        let r = Rect::from_min_size(
            Pos2::new(x - 30.0, rect.center().y - 15.0),
            Vec2::splat(30.0),
        );
        *slot = r;
        x = r.left() - 2.0;
    }
    if icon_button(
        body,
        slots[0],
        p,
        "quit",
        "退出 PeriphHub",
        paint_close,
        false,
    ) {
        action.quit = true;
    }
    if icon_button(body, slots[1], p, "open", "打开主界面", paint_open, false) {
        action.open_main = true;
    }
    if icon_button(
        body,
        slots[2],
        p,
        "pin",
        if pinned {
            "取消置顶"
        } else {
            "置顶显示在窗口最前"
        },
        paint_pin,
        pinned,
    ) {
        action.toggle_pin = true;
    }
    // Same contrast glyph as the titlebar, so the control reads identically on
    // both surfaces and keeps one optical size in either theme.
    let theme_tip = if p.dark {
        "切换到浅色模式"
    } else {
        "切换到深色模式"
    };
    if icon_button(
        body,
        slots[3],
        p,
        "theme",
        theme_tip,
        |painter, c, color| {
            paint_theme(painter, Rect::from_center_size(c, Vec2::splat(16.0)), color)
        },
        false,
    ) {
        action.toggle_theme = true;
    }
    if icon_button(
        body,
        slots[4],
        p,
        "settings",
        "打开设置与系统指针",
        // chrome 的齿轮以矩形中心为基准；浮窗按钮回调给的是中心点，包一层即可。
        |painter, c, color| {
            paint_gear(painter, Rect::from_center_size(c, Vec2::splat(16.0)), color)
        },
        false,
    ) {
        action.open_settings = true;
    }

    // Monogram, name, and a plain status word — counts belong to the main window.
    let badge = Rect::from_min_size(
        Pos2::new(rect.left() + 2.0, rect.center().y - 13.0),
        Vec2::splat(26.0),
    );
    paint_brand(&painter, badge, p);

    let title_pos = Pos2::new(badge.right() + 8.0, rect.top() + 9.0);
    painter.text(
        title_pos,
        Align2::LEFT_TOP,
        "PeriphHub",
        heading_font(13.0),
        p.text,
    );
    let dot_c = Pos2::new(title_pos.x + 2.6, rect.top() + 33.0);
    painter.circle_filled(dot_c, 2.6, dot_color);
    painter.text(
        Pos2::new(dot_c.x + 7.0, dot_c.y),
        Align2::LEFT_CENTER,
        summary,
        FontId::proportional(11.0),
        p.dim,
    );
    painter.line_segment(
        [
            Pos2::new(rect.left(), rect.bottom()),
            Pos2::new(rect.right(), rect.bottom()),
        ],
        Stroke::new(1.0, p.border),
    );

    // 拖拽热区止于最左侧按钮（settings = slots[4]）之前。它注册在按钮之后，
    // 重叠区域内后注册的交互胜出，所以热区一旦盖住按钮，按钮就再也点不动了。
    let drag = Rect::from_min_max(
        rect.left_top(),
        Pos2::new(slots[4].left() - 4.0, rect.bottom()),
    );
    let resp = body.interact(
        drag,
        body.scope_id().with("mini-drag"),
        Sense::CLICK | Sense::DRAG,
    );
    if resp.drag_started() {
        body.ctx()
            .send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        action.open_main = true;
    }
}

/// 稳定的图标和双行文本，电量用数值与短条共同呈现。
fn row(ui: &mut Ui, p: &Palette, entry: &Entry, action: &mut MiniAction) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
    let hover =
        ui.ctx()
            .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
    let offline = matches!(entry.state.link, Link::Offline(_));
    let painter = ui.painter();

    if hover > 0.001 {
        painter.rect_filled(
            rect,
            CornerRadius::same(11),
            alpha(p.chip_bg, (hover * 215.0) as u8),
        );
    }

    let tint = if offline {
        p.dim
    } else {
        connection_color(p, entry.info.connection)
    };
    let tile = Rect::from_min_size(
        Pos2::new(rect.left() + 2.0, rect.center().y - 16.0),
        Vec2::splat(32.0),
    );
    painter.rect_filled(tile, CornerRadius::same(10), p.chip_bg);

    let wired = entry.info.connection.is_wired();
    // 能力位说没有电量就不显示读数，哪怕采集线程刚好留下过旧值。
    let battery = entry
        .state
        .snap
        .as_ref()
        .filter(|s| s.caps.battery)
        .and(entry.state.battery);
    let level = battery_level(battery, wired);
    // 右侧这一列是电量读数；没有读数就留空。连接方式已经写在左侧注释里，
    // 再放一个"有线/在线"会和百分比抢同一列，也重复了信息。
    let (value, value_color, metric) = match &entry.state.link {
        Link::Offline(_) => ("离线".to_string(), p.danger, false),
        Link::Connecting => ("连接中".to_string(), p.warn, false),
        Link::Online => match battery {
            Some(b) => (format!("{}%", b.min(100)), battery_color(level, p), true),
            None => (String::new(), p.dim, false),
        },
    };
    let value_font = if metric {
        heading_font(text::BODY)
    } else {
        FontId::proportional(text::META)
    };
    let value_top = if metric {
        rect.top() + 10.0
    } else {
        rect.top() + 12.0
    };
    let value_w = if value.is_empty() {
        0.0
    } else {
        painter
            .layout_no_wrap(value.clone(), value_font.clone(), value_color)
            .size()
            .x
    };
    if !value.is_empty() {
        painter.text(
            Pos2::new(rect.right() - 10.0, value_top),
            Align2::RIGHT_TOP,
            value,
            value_font,
            value_color,
        );
    }

    // 形态图标独立成块，小尺寸下也能分辨键盘与鼠标。
    paint_device_glyph(painter, tile.center(), entry.info.kind, tint, 18.0);
    let text_left = rect.left() + 44.0;
    let title_col = if offline { p.dim } else { p.text };
    let title_w = (rect.right() - 12.0 - value_w - 8.0) - text_left;
    let model = ellipsize(
        painter,
        short_model(&entry.info.model),
        FontId::proportional(13.0),
        title_w,
    );
    painter.text(
        Pos2::new(text_left, rect.top() + 10.0),
        Align2::LEFT_TOP,
        model,
        FontId::proportional(13.0),
        title_col,
    );

    // 第二行只描述"设备是什么/连得上吗"。电量状态不在这里重复：百分比
    // 已经用颜色和进度条表达过一次，再写一行"电量低"就是第三种说法。
    let note = match &entry.state.link {
        Link::Offline(_) => "点击查看离线原因".to_string(),
        Link::Connecting => "正在建立连接".to_string(),
        // 同步异常只是配置读取失败，连接仍然有效，设备行保留。
        Link::Online if entry.state.sync.stale => "同步异常 · 显示上次读数".to_string(),
        Link::Online => format!(
            "{} · {}",
            entry.info.brand.to_uppercase(),
            entry.info.connection.label()
        ),
    };
    let note_color = p.dim;
    let has_gauge = matches!(entry.state.link, Link::Online) && battery.is_some();
    let note = ellipsize(
        painter,
        &note,
        FontId::proportional(text::META),
        rect.right() - text_left - if has_gauge { 56.0 } else { 10.0 },
    );
    painter.text(
        Pos2::new(text_left, rect.top() + 30.0),
        Align2::LEFT_TOP,
        note,
        FontId::proportional(text::META),
        note_color,
    );
    if has_gauge {
        if let Some(battery) = battery {
            let gauge = Rect::from_min_size(
                Pos2::new(rect.right() - 44.0, rect.top() + 36.0),
                Vec2::new(34.0, 3.0),
            );
            painter.rect_filled(gauge, CornerRadius::same(2), p.track);
            if battery > 0 {
                let mut fill = gauge;
                fill.set_width(gauge.width() * battery.min(100) as f32 / 100.0);
                painter.rect_filled(fill, CornerRadius::same(2), value_color);
            }
        }
    }

    if resp.has_focus() {
        focus_ring(ui, rect, p, 10);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if activated(ui, &resp) {
        action.open_config = Some(entry.info.id.clone());
    }
}

/// 空态保留扫描入口，并区分后台扫描过程、扫描结论与最近一次失败。
#[allow(clippy::too_many_arguments)]
fn empty_row(
    body: &mut Ui,
    p: &Palette,
    action: &mut MiniAction,
    recently_scanned: bool,
    scan_failed: bool,
    scanning: bool,
    has_devices: bool,
    height: f32,
) {
    // 禁用态交给 egui：不会进 Tab，也不会响应回车/空格；不再画一个看起来
    // 可激活却吞掉键盘动作的焦点环。
    body.add_enabled_ui(!scanning, |ui| {
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let hover =
            ui.ctx()
                .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.10);
        let painter = ui.painter();
        if hover > 0.001 {
            painter.rect_filled(
                rect,
                CornerRadius::same(11),
                alpha(p.chip_bg, (hover * 215.0) as u8),
            );
        }
        // 后台首轮扫描期间不能报"未发现设备"：那是结论，不是过程。
        // 有设备但都不在线时，浮窗不列离线设备，只说明去向。
        let (title, note) = if scanning {
            ("正在扫描…", "正在检测已连接的外设")
        } else if scan_failed {
            ("上次扫描失败", "检查设备后点击重新扫描")
        } else if has_devices {
            ("没有在线设备", "离线设备请在主界面查看")
        } else if recently_scanned {
            ("仍未发现设备", "检查接收器或数据线后重新扫描")
        } else {
            ("未发现设备", "点击扫描已连接的外设")
        };
        painter.text(
            Pos2::new(rect.center().x, rect.center().y - 8.0),
            Align2::CENTER_CENTER,
            title,
            FontId::proportional(text::BODY),
            if scan_failed { p.warn } else { p.text },
        );
        painter.text(
            Pos2::new(rect.center().x, rect.center().y + 10.0),
            Align2::CENTER_CENTER,
            note,
            FontId::proportional(text::META),
            p.dim,
        );
        if resp.has_focus() {
            focus_ring(ui, rect, p, 10);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if activated(ui, &resp) {
            action.rescan = true;
        }
    });
}

/// 置顶状态用轻微色彩标识，所有工具保持一致的触达面积。
fn icon_button(
    ui: &mut Ui,
    rect: Rect,
    p: &Palette,
    key: &'static str,
    tip: &str,
    paint: impl FnOnce(&egui::Painter, Pos2, Color32),
    accent: bool,
) -> bool {
    let resp = ui.interact(rect, ui.scope_id().with(key), Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(resp.id, if resp.hovered() { 1.0 } else { 0.0 }, 0.07);
    if t > 0.001 || accent {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(7),
            if accent {
                alpha(p.accent, (24.0 + t * 20.0) as u8)
            } else {
                alpha(p.chip_bg, (t * 255.0) as u8)
            },
        );
    }
    let base = if accent { p.accent } else { p.dim };
    paint(ui.painter(), rect.center(), mix(base, p.text, t * 0.6));
    if resp.has_focus() {
        focus_ring(ui, rect, p, 7);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let triggered = activated(ui, &resp);
    resp.on_hover_text(tip);
    triggered
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

/// 主界面就是一扇窗：外框加顶部标题栏。原来的“箭头钻出方框”在 14px 下
/// 只剩几根短线，既像退出又像外链，看不出是打开主窗口。
fn paint_open(painter: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.25, color);
    let body = Rect::from_center_size(c, Vec2::new(13.0, 11.0));
    painter.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
    let bar_y = body.top() + 3.4;
    painter.line_segment(
        [
            Pos2::new(body.left() + 1.0, bar_y),
            Pos2::new(body.right() - 1.0, bar_y),
        ],
        stroke,
    );
}

/// 置顶：向上箭头 + 顶部横线。
///
/// 原来的图钉由帽檐、钉身和细针三段组成，缩到 14px 只剩一个"T"形，
/// 顶上的帽檐和钉身糊在一起，更容易被读成漏斗或帐篷。箭头到顶线是
/// 系统里通行的"固定在最前"符号，只有两条实线和三根短线，小尺寸下
/// 每一笔都还认得出来；图钉的语义由 tooltip 补充。
fn paint_pin(painter: &egui::Painter, c: Pos2, color: Color32) {
    // 顶线略粗：它承担"到顶"那一半语义，粗细差让箭头不至于和它连成一把伞。
    painter.line_segment(
        [
            Pos2::new(c.x - 4.9, c.y - 5.5),
            Pos2::new(c.x + 4.9, c.y - 5.5),
        ],
        Stroke::new(1.6, color),
    );
    let stroke = Stroke::new(1.3, color);
    painter.line_segment(
        [Pos2::new(c.x, c.y - 2.4), Pos2::new(c.x, c.y + 5.5)],
        stroke,
    );
    let tip = Pos2::new(c.x, c.y - 2.4);
    painter.line_segment([Pos2::new(c.x - 2.9, c.y + 0.3), tip], stroke);
    painter.line_segment([tip, Pos2::new(c.x + 2.9, c.y + 0.3)], stroke);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mini_lists_only_online_devices() {
        assert!(is_visible(&Link::Online));
        assert!(!is_visible(&Link::Connecting));
    }

    #[test]
    fn status_word_never_reports_counts() {
        for word in [
            status_word(4, 0, 4, false),
            status_word(0, 0, 4, false),
            status_word(0, 2, 4, false),
            status_word(0, 0, 0, false),
            status_word(0, 0, 0, true),
        ] {
            assert!(
                !word.chars().any(|c| c.is_ascii_digit()),
                "浮窗状态词不该带数量：{word}"
            );
        }
        assert_eq!(status_word(3, 0, 5, false), "设备在线");
        assert_eq!(status_word(0, 0, 5, false), "设备离线");
        assert_eq!(status_word(0, 1, 5, false), "正在连接");
        assert_eq!(status_word(0, 0, 0, false), "无设备");
        assert_eq!(status_word(0, 0, 3, true), "正在扫描…");
    }

    #[test]
    fn height_follows_visible_rows() {
        assert_eq!(height(0), height(1));
        assert!(height(3) > height(2));
        assert_eq!(height(9), height(MAX_ROWS));
    }
}
