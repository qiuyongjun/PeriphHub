use std::time::Instant;

use eframe::egui::{
    self, Align, CornerRadius, Layout, Margin, RichText, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::devices::{DeviceId, DeviceManager, Entry, Link};
use crate::driver::{self, DeviceProfile};
use crate::theme::{
    battery_color, battery_level, battery_text, brand_badge, card_shadow, heading, meta, text,
    Palette,
};
use crate::ui::widgets::{
    action_link, activated, connection_chip_colored, connection_color, device_tile, focus_ring,
    hairline, metric_col, mix, pill, primary_button, refresh_button, short_model, skeleton,
    split_row, status_dot, surface, vrule, CARD_RADIUS, RESCAN_HINT,
};

/// Height of a device card's body block; every variant pads to it so cards in
/// the same row keep their feet on one line.
///
/// 指标只剩"标签 + 数值"两行（约 45）时，这里必须跟着收：固定 60 会在数值
/// 和分隔线之间留出一段谁也用不上的空白。
const BODY_H: f32 = 46.0;
/// Inner content height of every device card: header + body + rule + footer.
const CARD_H: f32 = 158.0;
/// Side of the letter tile in card headers. The title block must reserve the
/// same height, or egui centers a zero-height rect on the row's midline and
/// the two text lines hang below the tile (they did: ~14px too low).
const TILE: f32 = 38.0;

#[derive(Default)]
pub struct HomeAction {
    pub open_config: Option<DeviceId>,
    pub rescan: bool,
}

/// 扫描完成/失败后短时间展示结果反馈；与浮窗共用同一节奏。
const SCAN_NOTICE_SECS: u64 = 2;

/// 工作台概览与自适应设备网格，保留现有状态排序和页面入口。
pub fn view(ui: &mut Ui, p: &Palette, manager: &DeviceManager) -> HomeAction {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut action = HomeAction::default();
    let entries = manager.list();
    let total = entries.len();
    let online = entries
        .iter()
        .filter(|e| matches!(e.state.link, Link::Online))
        .count();
    let connecting = entries
        .iter()
        .filter(|e| matches!(e.state.link, Link::Connecting))
        .count();
    let scanning = manager.is_scanning();
    let completed = manager.last_scan_completed();
    let failed = manager.last_scan_failed();
    let recently_scanned = completed
        .is_some_and(|c| failed.is_none_or(|f| c > f) && c.elapsed().as_secs() < SCAN_NOTICE_SECS);

    header(ui, p, &mut action, recently_scanned, scanning);
    ui.add_space(16.0);
    overview(
        ui,
        p,
        total,
        online,
        connecting,
        completed,
        failed,
        manager.auto_detect_enabled(),
        scanning,
    );
    ui.add_space(20.0);

    split_row(
        ui,
        |ui| {
            if !entries.is_empty() {
                ui.label(meta("点击卡片进入配置", p.dim));
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(heading("我的设备", text::SUBTITLE, p.text));
            if manager.is_demo() {
                pill(ui, p, "演示数据", p.info);
            }
        },
    );
    ui.add_space(12.0);

    if entries.is_empty() {
        empty_state(ui, p, &mut action, manager.is_scanning());
        return action;
    }

    let gap = 18.0;
    // 三列只在宽窗口启用，确保长型号和回报率在常用尺寸下完整可读。
    let min_w = 340.0;
    let avail = ui.available_width();
    let cols = (((avail + gap) / (min_w + gap)).floor() as usize).clamp(1, 3);
    let card_w = (avail - gap * (cols.saturating_sub(1)) as f32) / cols as f32;

    // Offline devices drop to the end: what still works should be read first.
    let mut order: Vec<&Entry> = entries.iter().collect();
    order.sort_by_key(|e| matches!(e.state.link, Link::Offline(_)) as u8);

    for row in order.chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (i, entry) in row.iter().enumerate() {
                if i > 0 {
                    ui.add_space(gap);
                }
                ui.allocate_ui_with_layout(
                    Vec2::new(card_w, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(card_w);
                        if device_card(ui, p, entry) {
                            action.open_config = Some(entry.info.id.clone());
                        }
                    },
                );
            }
        });
        ui.add_space(gap);
    }
    action
}

/// 页首只保留页面身份与唯一的检测动作，让设备内容尽早上屏。
fn header(
    ui: &mut Ui,
    p: &Palette,
    action: &mut HomeAction,
    recently_scanned: bool,
    scanning: bool,
) {
    split_row(
        ui,
        |ui| {
            let label = if scanning {
                "正在扫描…"
            } else if recently_scanned {
                "已更新"
            } else {
                "重新扫描"
            };
            ui.add_enabled_ui(!scanning, |ui| {
                if refresh_button(
                    ui,
                    p,
                    label,
                    &format!("重新检测已连接的外设（{RESCAN_HINT}）"),
                ) {
                    action.rescan = true;
                }
            });
        },
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(heading("设备工作台", text::TITLE, p.text));
                ui.add_space(5.0);
                ui.label(meta("连接状态一目了然，设备配置触手可及", p.dim));
            });
        },
    );
}

/// 用一条紧凑概览替代重复的大数字区块，同时呈现扫描、失败与连接过程。
#[allow(clippy::too_many_arguments)]
fn overview(
    ui: &mut Ui,
    p: &Palette,
    total: usize,
    online: usize,
    connecting: usize,
    completed: Option<Instant>,
    failed: Option<Instant>,
    auto_detect: bool,
    scanning: bool,
) {
    surface(ui, p, Margin::symmetric(16, 11), |ui| {
        split_row(
            ui,
            |ui| {
                let failed_recent = failed.is_some_and(|f| {
                    completed.is_none_or(|c| f > c) && f.elapsed().as_secs() < SCAN_NOTICE_SECS
                });
                // 状态点代表“整条检测链路是否可信”：自动检测不可用、扫描失败、
                // 插拔合并中或设备连接中都算需要留意；全部离线不是正常态。
                let color = if scanning || connecting > 0 || failed_recent || !auto_detect {
                    p.warn
                } else if total == 0 {
                    p.dim
                } else if online == 0 {
                    p.danger
                } else {
                    p.ok
                };
                status_dot(ui, color, scanning);
                ui.add_space(2.0);
                ui.label(meta(
                    sync_text(completed, failed, auto_detect, scanning),
                    p.dim,
                ));
            },
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.label(heading(total.to_string(), text::HEADING, p.text));
                    ui.label(meta("台设备", p.dim));
                    ui.add_space(13.0);
                    ui.label(heading(
                        online.to_string(),
                        text::HEADING,
                        if online > 0 { p.ok } else { p.dim },
                    ));
                    ui.label(meta("台在线", p.dim));
                    if connecting > 0 {
                        ui.add_space(7.0);
                        pill(ui, p, &format!("{connecting} 连接中"), p.warn);
                    }
                });
            },
        );
    });
}

/// 空态区分扫描中与扫描结束，并保留真实支持范围。
fn empty_state(ui: &mut Ui, p: &Palette, action: &mut HomeAction, scanning: bool) {
    surface(ui, p, Margin::same(24), |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.add_space(32.0);
            brand_badge(ui, 48.0, p);
            ui.add_space(16.0);
            // 首次扫描在后台跑：这期间说"还没有检测到设备"是错的，
            // 用户会以为设备没被认出来。
            let (title, note) = if scanning {
                ("正在扫描设备…", "正在检测 USB 与蓝牙上的外设，请稍候")
            } else {
                (
                    "还没有检测到设备",
                    "请连接设备、接收器或 USB 数据线，然后重新扫描",
                )
            };
            ui.label(heading(title, text::HEADING, p.text));
            ui.add_space(7.0);
            ui.label(RichText::new(note).size(12.0).color(p.dim));
            if !scanning {
                ui.add_space(4.0);
                // 诚实说明支持范围：未知 VID/PID 的设备在 enumerate 里被整个
                // 过滤掉了，用户手里的鼠标不在支持列表时，看到"未发现设备"
                // 最容易得出的结论是程序坏了，而不是型号不支持。文案从设备
                // 总表生成（见 supported_summary），新增型号不会漏改这里。
                ui.label(RichText::new(supported_summary()).size(12.0).color(p.dim));
                ui.add_space(20.0);
                if primary_button(
                    ui,
                    p,
                    "扫描设备",
                    &format!("重新检测已连接的外设（{RESCAN_HINT}）"),
                ) {
                    action.rescan = true;
                }
            }
            ui.add_space(28.0);
        });
    });
}

/// 统一身份、指标与动作区；离线表面降低明度，悬停只增强边界。
fn device_card(ui: &mut Ui, p: &Palette, entry: &Entry) -> bool {
    let offline = matches!(entry.state.link, Link::Offline(_));
    let connecting = matches!(entry.state.link, Link::Connecting);
    let tint = connection_color(p, entry.info.connection);

    // Offline devices sink back into the background instead of looking
    // just as alive as connected ones.
    let card_fill = if offline {
        mix(p.card, p.bg, 0.55)
    } else {
        p.card
    };
    let text_col = if offline { p.dim } else { p.text };
    let border = if offline {
        p.border.gamma_multiply(0.7)
    } else {
        p.border
    };

    let card_id = ui
        .scope_id()
        .with(("device-card", entry.info.id.0.as_ref()));
    let mut clicked_button = false;
    let frame = egui::Frame::new()
        .fill(card_fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(16))
        .shadow(card_shadow(p))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let card_top = ui.cursor().top();

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let letter = entry
                    .info
                    .brand
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "P".into());
                device_tile(ui, p, entry.info.kind, &letter, tint, TILE);
                ui.add_space(10.0);
                // The link chip is laid out after this block, so its width has
                // to be reserved up front: otherwise a long product name eats
                // the row and the chip slides out of the card.
                let chip_w = ui
                    .painter()
                    .layout_no_wrap(
                        entry.info.connection.label().to_owned(),
                        egui::FontId::proportional(text::MICRO),
                        tint,
                    )
                    .size()
                    .x
                    + 18.0;
                let title_w = (ui.available_width() - chip_w - 10.0).max(60.0);
                ui.allocate_ui_with_layout(
                    // Height must match the tile: with 0 egui centers an empty
                    // rect on the row's midline and the text hangs below it.
                    Vec2::new(title_w, TILE),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(title_w);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.add(
                            egui::Label::new(
                                RichText::new(entry.info.brand.to_uppercase())
                                    .size(11.0)
                                    .extra_letter_spacing(0.7)
                                    .color(p.subtle),
                            )
                            .truncate(),
                        );
                        let full = entry.info.model.as_str();
                        let title = short_model(full);
                        let label = ui.add(
                            egui::Label::new(heading(title, text::SUBTITLE, text_col)).truncate(),
                        );
                        // The shortened title is a convenience; the device's
                        // own name stays one hover away.
                        if title != full {
                            label.on_hover_text(full);
                        }
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let chip_color = if offline { p.dim } else { tint };
                    connection_chip_colored(ui, p, entry.info.connection, chip_color);
                });
            });
            ui.add_space(12.0);
            hairline(ui, p, 165);
            ui.add_space(12.0);

            let body_top = ui.cursor().top();
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), BODY_H),
                Layout::top_down(Align::Min),
                |ui| {
                    if !entry.info.has_config {
                        read_only_body(ui, p, entry.info.reason);
                    } else if entry.state.snap.is_some() {
                        metrics(ui, p, entry);
                    } else {
                        loading_body(ui, p);
                    }
                },
            );
            // Every body variant reserves the same slot, so a read-only card
            // still lines its footer up with the configurable card beside it.
            let used = ui.cursor().top() - body_top;
            if used < BODY_H {
                ui.add_space(BODY_H - used);
            }

            ui.add_space(12.0);
            hairline(ui, p, 165);
            ui.add_space(9.0);

            split_row(
                ui,
                |ui| {
                    let label = if !entry.info.has_config {
                        "查看详情"
                    } else if offline {
                        "查看离线原因"
                    } else if connecting {
                        "查看状态"
                    } else {
                        "设备配置"
                    };
                    if action_link(ui, p, label, offline).clicked() {
                        clicked_button = true;
                    }
                },
                |ui| {
                    let (text, status_color) = match &entry.state.link {
                        Link::Offline(_) => ("设备离线", p.danger),
                        Link::Connecting => ("正在连接", p.warn),
                        Link::Online => match entry.state.synced {
                            Some(t) if t.elapsed().as_secs() < 2 => ("刚刚同步", p.ok),
                            Some(_) => ("设备在线", p.ok),
                            None => ("设备已连接", p.ok),
                        },
                    };
                    status_dot(ui, status_color, connecting);
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(text)
                            .size(12.0)
                            .color(if offline || connecting {
                                status_color
                            } else {
                                p.dim
                            }),
                    );
                },
            );
            // Pad to a fixed content height so every card in a row ends on the
            // same line, whatever variant its body rendered.
            let used = ui.cursor().top() - card_top;
            if used < CARD_H {
                ui.add_space(CARD_H - used);
            }
        });

    let rect = frame.response.rect;
    let response = ui.interact(rect, card_id, Sense::click());
    let t =
        ui.ctx()
            .animate_value_with_time(card_id, if response.hovered() { 1.0 } else { 0.0 }, 0.10);
    if t > 0.001 {
        let col = mix(p.border, p.accent, t * 0.42);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(CARD_RADIUS),
            Stroke::new(1.0, col),
            StrokeKind::Inside,
        );
    }
    if response.has_focus() {
        focus_ring(ui, rect, p, CARD_RADIUS);
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    activated(ui, &response) || clicked_button
}

/// 只展示设备真实开放的指标，未知值继续使用占位而非推测。
fn metrics(ui: &mut Ui, p: &Palette, entry: &Entry) {
    let Some(snap) = entry.state.snap.as_ref() else {
        return;
    };
    let wired = entry.info.connection.is_wired();
    let level = battery_level(entry.state.battery, wired);
    let color = battery_color(level, p);
    let active = snap.stage_active as usize;
    let current = snap.dpi.get(active).copied();
    let offline = matches!(entry.state.link, Link::Offline(_));
    let value_col = if offline { p.dim } else { p.text };

    let battery_text = battery_text(entry.state.battery, wired);
    let (battery_text, battery_unit) = if entry.state.battery.is_some() {
        (battery_text.trim_end_matches('%').to_string(), "%")
    } else {
        (battery_text, "")
    };
    let dpi_text = current
        .map(|v| v.to_string())
        .unwrap_or_else(|| "--".into());
    let hz = 1000 / snap.polling_ms.max(1) as u32;

    // Only the columns this device can actually fill; a fixed "1000 Hz" on a
    // mouse whose report rate cannot be read would just be a guess.
    struct Col<'a> {
        label: &'a str,
        value: String,
        unit: &'a str,
        color: egui::Color32,
        tip: &'a str,
    }
    let cols = [
        // "有线"不是读数：连接方式已经写在卡头，没读到电量就不占一列。
        (entry.state.battery.is_some() || !wired).then_some(Col {
            label: "电量",
            value: battery_text,
            unit: battery_unit,
            color: if offline { p.dim } else { color },
            // 插线时设备照样报电量（而且多半在充电），所以提示只能看这一轮
            // 到底有没有读数，不能只看连接方式——否则会出现"71%"配一句
            // "不显示电量"。
            tip: match entry.state.battery {
                Some(_) => "剩余电量，低于 20% 建议充电",
                None if wired => "设备由数据线供电，这一轮没有读到电量",
                None => "还没读到电量，无线鼠标休眠时常见",
            },
        }),
        snap.caps.dpi.then(|| Col {
            label: "当前 DPI",
            value: dpi_text,
            unit: "",
            color: if current.is_some() { value_col } else { p.dim },
            tip: "鼠标灵敏度，数值越高指针移动越快",
        }),
        snap.caps.polling.then(|| Col {
            label: "回报率",
            value: hz.to_string(),
            unit: "Hz",
            color: value_col,
            tip: "每秒向电脑上报位置的次数，越高越跟手",
        }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    let total = ui.available_width();
    let gap = 12.0;
    let n = cols.len().max(1) as f32;
    let col_w = (total - gap * (n - 1.0) * 2.0 - 2.0).max(40.0) / n;

    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                ui.add_space(gap);
                vrule(ui, p, 4.0, 38.0);
                ui.add_space(gap);
            }
            metric_col(ui, p, col_w, c.label, &c.value, c.unit, c.color, c.tip);
        }
    });
}

/// Devices without a vendor config interface: say so plainly instead of
/// showing three empty metric columns that look like a bug.
fn read_only_body(ui: &mut Ui, p: &Palette, reason: Option<&str>) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        ui.label(RichText::new("只读设备").size(12.0).color(p.text));
        ui.label(
            RichText::new(reason.unwrap_or("未检测到厂商配置接口，仅显示连接状态"))
                .size(12.0)
                .color(p.dim),
        );
    });
}

/// 骨架占据与真实指标相同的位置，读取完成时卡片保持稳定。
fn loading_body(ui: &mut Ui, p: &Palette) {
    let total = ui.available_width();
    let gap = 12.0;
    let col_w = (total - gap * 4.0 - 2.0).max(40.0) / 3.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for i in 0..3 {
            ui.allocate_ui_with_layout(Vec2::new(col_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(col_w);
                skeleton(ui, p, col_w * 0.45, 9.0);
                ui.add_space(6.0);
                skeleton(ui, p, col_w * 0.7, 15.0);
                ui.add_space(6.0);
                skeleton(ui, p, col_w * 0.55, 9.0);
            });
            if i < 2 {
                ui.add_space(gap);
                vrule(ui, p, 4.0, 38.0);
                ui.add_space(gap);
            }
        }
    });
}

/// 显示真实扫描进度、最近完成/失败时间和自动检测可用性；不把"刚发起"
/// 误报成"已更新"。
fn sync_text(
    completed: Option<Instant>,
    failed: Option<Instant>,
    auto_detect: bool,
    scanning: bool,
) -> String {
    if scanning {
        return "正在检测设备…".into();
    }
    let failed_is_newest = failed.is_some_and(|f| completed.is_none_or(|c| f > c));
    if failed_is_newest && failed.is_some_and(|f| f.elapsed().as_secs() < SCAN_NOTICE_SECS) {
        return "上次检测失败，请重新扫描".into();
    }
    match completed {
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s < 3 {
                "刚刚更新".into()
            } else if s < 60 {
                format!("{s} 秒前更新")
            } else {
                format!("{} 分钟前更新", s / 60)
            }
        }
        None if !auto_detect => "自动检测不可用，请手动扫描".into(),
        None => "设备自动检测已开启".into(),
    }
}

/// 空列表页那句"支持范围"文案，从设备总表生成。
fn supported_summary() -> String {
    let names = |writable: bool| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in driver::PROFILES
            .iter()
            .filter(|d| d.driver.is_some() == writable)
        {
            let name = display_name(d);
            // 同一型号的"接收器 / 插线"是两条 profile，但只该念一次。
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    };
    let mut text = format!("可配置 {}", names(true).join("、"));
    let read_only = names(false);
    if !read_only.is_empty() {
        text.push_str(&format!("，{} 仅识别", read_only.join("、")));
    }
    text.push_str("；其余型号暂不支持");
    text
}

/// 品牌名已在型号里（RK98）时不重复，避免出现"RK RK98"这种叠词。
fn display_name(d: &DeviceProfile) -> String {
    if d.model.contains(d.brand) {
        d.model.to_string()
    } else {
        format!("{} {}", d.brand, d.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 总表里每台设备都得在这句话里出现，别让新增型号在首页"隐身"。
    #[test]
    fn summary_lists_every_supported_device_once() {
        let text = supported_summary();
        assert!(text.starts_with("可配置 "));
        assert!(text.ends_with("；其余型号暂不支持"));
        for d in driver::PROFILES {
            assert!(text.contains(&display_name(d)), "缺少 {}", d.model);
        }
        // "RK98" 里已含品牌缩写，不能再叠出 "RK RK98"。
        assert!(!text.contains("RK RK98"));
        assert!(text.contains("可配置 Rapoo VT1 Pro、Logitech Lightspeed 接收器"));
        assert!(
            text.contains("AULA F87、RK98、BY Tech 通用有线键盘 仅识别"),
            "{text}"
        );
    }
}
