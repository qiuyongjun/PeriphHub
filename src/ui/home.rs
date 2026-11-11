use std::time::Instant;

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke, StrokeKind, Ui,
    Vec2,
};

use crate::devices::{DeviceId, DeviceManager, Entry, Link};
use crate::driver::{self, DeviceProfile};
use crate::theme::{
    alpha, battery_color, battery_label, battery_level, battery_text, brand_badge, card_shadow,
    heading, Palette,
};
use crate::ui::widgets::{
    action_link, connection_chip_colored, connection_color, device_tile, hairline, material_shape,
    metric_col, mix, pill, primary_button, short_model, skeleton, split_row, status_dot, surface,
    vrule, CARD_RADIUS, RESCAN_HINT,
};

/// Height of a device card's body block; every variant pads to it so cards in
/// the same row keep their feet on one line.
const BODY_H: f32 = 90.0;
/// Inner content height of every device card: header + body + rule + footer.
const CARD_H: f32 = 214.0;
/// Side of the letter tile in card headers. The title block must reserve the
/// same height, or egui centers a zero-height rect on the row's midline and
/// the two text lines hang below the tile (they did: ~14px too low).
const TILE: f32 = 50.0;

#[derive(Default)]
pub struct HomeAction {
    pub open_config: Option<DeviceId>,
    pub rescan: bool,
}

/// 工作台概览与自适应设备网格，保留现有状态排序和页面入口。
pub fn view(
    ui: &mut Ui,
    p: &Palette,
    manager: &DeviceManager,
    recently_scanned: bool,
    last_scan: Option<Instant>,
) -> HomeAction {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut action = HomeAction::default();
    let entries = manager.list();
    let total = entries.len();
    let online = entries
        .iter()
        .filter(|e| !matches!(e.state.link, Link::Offline(_)))
        .count();
    let attention = entries
        .iter()
        .filter(|e| {
            matches!(e.state.link, Link::Offline(_))
                || matches!(
                    battery_level(e.state.battery, e.info.connection.is_wired()),
                    crate::theme::BatteryLevel::Critical | crate::theme::BatteryLevel::Low
                )
        })
        .count();

    surface(ui, p, Margin::symmetric(24, 20), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        header(ui, p, &mut action, recently_scanned);
        ui.add_space(20.0);
        hairline(ui, p, 140);
        ui.add_space(16.0);
        overview(ui, p, total, online, attention);
    });
    ui.add_space(26.0);

    split_row(
        ui,
        |ui| {
            status_dot(ui, p.ok, false);
            ui.label(
                RichText::new(sync_text(last_scan))
                    .size(11.0)
                    .color(p.subtle),
            );
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(heading("我的设备", 17.0, p.text));
            pill(ui, p, &format!("{total} 台"), p.dim);
            if manager.is_demo() {
                pill(ui, p, "演示数据", p.info);
            }
        },
    );
    ui.add_space(14.0);

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

/// 页首强调工作台身份，扫描动作保持唯一的实色主按钮。
fn header(ui: &mut Ui, p: &Palette, action: &mut HomeAction, recently_scanned: bool) {
    split_row(
        ui,
        |ui| {
            let label = if recently_scanned {
                "已重新扫描"
            } else {
                "重新扫描"
            };
            if primary_button(
                ui,
                p,
                label,
                &format!("重新检测已连接的外设（{RESCAN_HINT}）"),
            )
            .clicked()
            {
                action.rescan = true;
            }
        },
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(
                    RichText::new("PERIPHHUB  /  WORKSPACE")
                        .size(10.0)
                        .extra_letter_spacing(1.4)
                        .color(p.subtle),
                );
                ui.add_space(7.0);
                ui.label(heading("设备工作台", 30.0, p.text));
                ui.add_space(5.0);
                ui.label(
                    RichText::new("连接、状态与性能，一处掌控")
                        .size(12.5)
                        .color(p.dim),
                );
            });
        },
    );
}

/// 概览共享一块表面，均分宽度避免状态数字挤在左侧。
fn overview(ui: &mut Ui, p: &Palette, total: usize, online: usize, attention: usize) {
    let gap = 20.0;
    let cell_w = (ui.available_width() - 2.0 - gap * 4.0) / 3.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        stat_cell(ui, p, cell_w, &total.to_string(), "台", "设备总数", p.text);
        ui.add_space(gap);
        vrule(ui, p, 4.0, 42.0);
        ui.add_space(gap);
        stat_cell(
            ui,
            p,
            cell_w,
            &online.to_string(),
            "台",
            "当前在线",
            if online == 0 { p.dim } else { p.ok },
        );
        ui.add_space(gap);
        vrule(ui, p, 4.0, 42.0);
        ui.add_space(gap);
        stat_cell(
            ui,
            p,
            cell_w,
            &attention.to_string(),
            "项",
            "需要关注",
            if attention == 0 { p.dim } else { p.warn },
        );
    });
}

/// 数字与单位分级排版，单位不占用指标的主要视觉权重。
fn stat_cell(
    ui: &mut Ui,
    p: &Palette,
    width: f32,
    value: &str,
    unit: &str,
    label: &str,
    color: Color32,
) {
    ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(label).size(11.0).color(p.dim));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.label(heading(value, 28.0, color));
            ui.add_space(2.0);
            ui.label(RichText::new(unit).size(11.0).color(p.dim));
        });
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
            ui.label(heading(title, 18.0, p.text));
            ui.add_space(7.0);
            ui.label(RichText::new(note).size(12.5).color(p.dim));
            if !scanning {
                ui.add_space(4.0);
                // 诚实说明支持范围：未知 VID/PID 的设备在 enumerate 里被整个
                // 过滤掉了，用户手里的鼠标不在支持列表时，看到"未发现设备"
                // 最容易得出的结论是程序坏了，而不是型号不支持。文案从设备
                // 总表生成（见 supported_summary），新增型号不会漏改这里。
                ui.label(RichText::new(supported_summary()).size(11.5).color(p.dim));
                ui.add_space(20.0);
                if primary_button(
                    ui,
                    p,
                    "扫描设备",
                    &format!("重新检测已连接的外设（{RESCAN_HINT}）"),
                )
                .clicked()
                {
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
        alpha(p.border, 120)
    } else {
        alpha(p.border, 180)
    };

    let card_id = ui
        .scope_id()
        .with(("device-card", entry.info.id.0.as_ref()));
    let mut clicked_button = false;
    let mut material = None;
    let frame = egui::Frame::new()
        .fill(card_fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(20))
        .shadow(if offline {
            egui::Shadow::NONE
        } else {
            card_shadow(p)
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            material = Some(ui.painter().add(egui::Shape::Noop));
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
                        egui::FontId::proportional(11.5),
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
                                    .size(10.5)
                                    .strong()
                                    .color(p.subtle),
                            )
                            .truncate(),
                        );
                        let full = entry.info.model.as_str();
                        let title = short_model(full);
                        let label =
                            ui.add(egui::Label::new(heading(title, 20.0, text_col)).truncate());
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
            ui.add_space(14.0);

            let body_top = ui.cursor().top();
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), BODY_H),
                Layout::top_down(Align::Min),
                |ui| {
                    if !entry.info.has_config {
                        read_only_body(ui, p, entry.info.reason);
                    } else if entry.state.snap.is_some() {
                        egui::Frame::new()
                            .fill(if offline {
                                alpha(p.inset, 130)
                            } else {
                                p.inset
                            })
                            .corner_radius(CornerRadius::same(11))
                            .inner_margin(Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                metrics(ui, p, entry);
                            });
                    } else {
                        loading_body(ui, p, connecting);
                    }
                },
            );
            // Every body variant reserves the same slot, so a read-only card
            // still lines its footer up with the configurable card beside it.
            let used = ui.cursor().top() - body_top;
            if used < BODY_H {
                ui.add_space(BODY_H - used);
            }

            ui.add_space(14.0);
            hairline(ui, p, 150);
            ui.add_space(10.0);

            split_row(
                ui,
                |ui| {
                    let label = if !entry.info.has_config {
                        "查看详情"
                    } else if offline {
                        "重新连接"
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
                            .size(11.5)
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
    // Smoothly fade the highlight border in and out instead of popping.
    let t =
        ui.ctx()
            .animate_value_with_time(card_id, if response.hovered() { 1.0 } else { 0.0 }, 0.10);
    if let Some(material) = material {
        let top = if offline {
            card_fill
        } else {
            mix(p.card_top, p.card_hover, t * 0.5)
        };
        ui.painter().set(
            material,
            material_shape(rect.shrink(1.0), CARD_RADIUS - 1, top, card_fill),
        );
    }
    if t > 0.001 {
        let col = mix(p.border, p.accent, t * 0.55);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(CARD_RADIUS),
            Stroke::new(1.0, col),
            StrokeKind::Inside,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked() || clicked_button
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
    let dpi_text = current
        .map(|v| v.to_string())
        .unwrap_or_else(|| "--".into());
    let stage_note = if !snap.caps.dpi {
        "固件未开放".to_string()
    } else if snap.stage_count > 1 {
        format!(
            "第 {} / {} 档",
            snap.stage_active + 1,
            snap.stage_count.max(1)
        )
    } else {
        "单档".to_string()
    };
    let hz = 1000 / snap.polling_ms.max(1) as u32;

    // Only the columns this device can actually fill; a fixed "1000 Hz" on a
    // mouse whose report rate cannot be read would just be a guess.
    struct Col<'a> {
        label: &'a str,
        value: String,
        note: String,
        color: egui::Color32,
        tip: &'a str,
    }
    let cols = [
        Some(Col {
            label: "电量",
            value: battery_text,
            note: battery_label(level).to_string(),
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
            note: stage_note,
            color: if current.is_some() { value_col } else { p.dim },
            tip: "鼠标灵敏度，数值越高指针移动越快",
        }),
        snap.caps.polling.then(|| Col {
            label: "回报率",
            value: format!("{hz} Hz"),
            note: format!("{} ms 间隔", snap.polling_ms.max(1)),
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
                vrule(ui, p, 6.0, 44.0);
                ui.add_space(gap);
            }
            metric_col(ui, p, col_w, c.label, &c.value, &c.note, c.color, c.tip);
        }
    });
}

/// Devices without a vendor config interface: say so plainly instead of
/// showing three empty metric columns that look like a bug.
fn read_only_body(ui: &mut Ui, p: &Palette, reason: Option<&str>) {
    egui::Frame::new()
        .fill(alpha(p.chip_bg, if p.dark { 150 } else { 190 }))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(11, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(BODY_H - 18.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new("只读设备").size(11.5).strong().color(p.text));
                ui.label(
                    RichText::new(reason.unwrap_or("未检测到厂商配置接口，仅显示连接状态"))
                        .size(10.5)
                        .color(p.dim),
                );
            });
        });
}

/// 骨架占据与真实指标相同的位置，读取完成时卡片保持稳定。
fn loading_body(ui: &mut Ui, p: &Palette, connecting: bool) {
    let total = ui.available_width();
    let gap = 12.0;
    let col_w = (total - gap * 4.0 - 2.0).max(40.0) / 3.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for i in 0..3 {
            ui.allocate_ui_with_layout(Vec2::new(col_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(col_w);
                skeleton(ui, p, col_w * 0.45, 9.0);
                ui.add_space(7.0);
                skeleton(ui, p, col_w * 0.7, 16.0);
                ui.add_space(7.0);
                skeleton(ui, p, col_w * 0.55, 9.0);
            });
            if i < 2 {
                ui.add_space(gap);
                vrule(ui, p, 6.0, 44.0);
                ui.add_space(gap);
            }
        }
    });
    if connecting {
        ui.add_space(6.0);
        ui.label(RichText::new("正在读取设备配置…").size(10.5).color(p.dim));
    }
}

/// 显示最近扫描的实际时间。
fn sync_text(last: Option<Instant>) -> String {
    match last {
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
        None => "启动时已扫描".into(),
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
        assert!(text.contains("AULA F87、RK98、BY Tech Gaming Keyboard 仅识别"));
    }
}
