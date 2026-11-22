//! The device configuration page. It is a route inside the main window, not a
//! separate viewport, so the titlebar keeps its back affordance and the user
//! never loses the device list.
//!
//! 这是鼠标配置页。键盘目前是只读设备，只会显示“只读设备”提示，不会进入
//! 下面的设置卡；新增键盘灯效/改键应另建设置模型，而不是扩展 `Snapshot`。

use eframe::egui::{self, Align, Layout, RichText, Stroke, Ui, Vec2};

use crate::devices::{Cmd, DeviceId, DeviceInfo, DeviceState, EditorState, Link, Snapshot};
use crate::driver::{Limits, LodScale};
use crate::sysinput;
use crate::theme::{
    alpha, battery_color, battery_level, heading, meta, micro, numeral, text, Palette,
};
use crate::ui::widgets::{
    battery_bar, card, chip, chip_small, connection_chip, connection_color, device_tile, divider,
    dpi_chip, link_action, paint_refresh, primary_button, section_heading, shadow_card, split_row,
    step_button, stepper_row, toggle_row,
};

#[derive(Default)]
pub struct ViewAction {
    pub cmds: Vec<Cmd>,
}

/// 左侧固定宽度摘要，右侧设置列表；窄窗口退回单列。
const RAIL_W: f32 = 268.0;
const TWO_COL_MIN: f32 = 760.0;

/// 设备摘要与设置分组：宽窗口用左栏放身份与状态，设置留在右栏。
pub fn view(
    ui: &mut Ui,
    p: &Palette,
    info: &DeviceInfo,
    state: &DeviceState,
    dpi_baseline: Option<&[u16]>,
    scroll_id: &DeviceId,
) -> ViewAction {
    let mut action = ViewAction::default();
    let snap = state.snap.clone();
    let ed = state.editor;
    // 同步异常时仍允许编辑：命令由采集线程排队、合并，等基准配置读回来
    // 再执行；页面顶部的“同步异常”会说明当前显示的是上次成功的值。
    let writable = info.has_config && !matches!(state.link, Link::Offline(_)) && snap.is_some();

    split_row(
        ui,
        |ui| {
            if info.has_config
                && link_action(ui, p, "刷新配置", "重新读取设备上的当前配置", paint_refresh)
                    .clicked()
            {
                action.cmds.push(Cmd::Refresh);
            }
        },
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(heading("设备配置", text::HEADING, p.text));
                ui.add_space(3.0);
                let status = if state.pending_confirm {
                    "写入已提交，正在等待设备确认…"
                } else if state.writing {
                    "正在写入设备…"
                } else if state.write_unconfirmed {
                    "写入结果未确认；已重新读取设备实际配置"
                } else if state.sync.stale {
                    // 说过头会比不说更误导：同步异常期间用户可能已经改过值
                    // （命令还在排队或已被放弃），显示的不一定是设备实际值。
                    "同步异常；显示值可能不是设备当前配置"
                } else {
                    "更改即时写入设备，无需手动保存"
                };
                ui.label(RichText::new(status).size(12.0).color(p.dim));
            });
        },
    );
    ui.add_space(16.0);

    // 断开与写入失败不再以文档流通知出现：它们跟着设备状态偶发地插入又
    // 消失，会把视口下方的内容整体顶下又顶回——用户停在一行设置上不动，
    // 光标下的控件却换了，看起来就像页面自己跳了一下。两者改走 app.rs 里
    // show_config_page 的浮层提示（toast_overlay），固定悬在页首、不占布局。
    // 只读设备是设备静态属性，恒在或恒不在，留在文档流里是安全的。
    if !info.has_config {
        notice(
            ui,
            p,
            "只读设备",
            info.reason
                .unwrap_or("未找到该设备的厂商配置接口，无法读写参数。"),
            p.dim,
        );
        return action;
    }

    if state.sync.stale {
        notice(
            ui,
            p,
            "同步异常",
            state
                .sync
                .error
                .as_deref()
                .unwrap_or("设备暂时没有返回配置，仍会按较低频率重试。"),
            p.warn,
        );
        ui.add_space(14.0);
    }

    let Some(snap) = snap else {
        // A read-only device already got the "只读设备" notice above; a second
        // card saying it has no configuration would just repeat it.
        if !matches!(state.link, Link::Offline(_)) {
            card(ui, p, |ui| {
                ui.label(RichText::new("正在读取设备配置…").size(13.0).color(p.dim));
            });
        }
        return action;
    };

    // 周期落盘失败是持续状态：设置仍在运行态生效，但断电后会回到旧值。
    // 放在文档流里而不是 toast，用户滚到哪一行都能看到它还没保存。
    if let Some(err) = &snap.save_error {
        notice(ui, p, "尚未保存到设备", err, p.warn);
    }

    if ui.available_width() >= TWO_COL_MIN {
        let body_h = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(RAIL_W, body_h),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(RAIL_W);
                    status_card(ui, p, info, state, &snap);
                },
            );
            ui.add_space(18.0);
            let right_w = ui.available_width();
            ui.allocate_ui_with_layout(
                Vec2::new(right_w, body_h),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(right_w);
                    // 只有设置列滚动：顶部标题与左栏摘要始终留在原地。
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .id_salt(("config-settings", scroll_id.0.as_ref()))
                        .show(ui, |ui| {
                            settings_column(
                                ui,
                                p,
                                info,
                                state,
                                ed,
                                &snap,
                                dpi_baseline,
                                writable,
                                &mut action.cmds,
                            );
                            ui.add_space(8.0);
                        });
                },
            );
        });
    } else {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt(("config-settings", scroll_id.0.as_ref()))
            .show(ui, |ui| {
                status_card(ui, p, info, state, &snap);
                ui.add_space(14.0);
                settings_column(
                    ui,
                    p,
                    info,
                    state,
                    ed,
                    &snap,
                    dpi_baseline,
                    writable,
                    &mut action.cmds,
                );
                ui.add_space(8.0);
            });
    }
    action
}

/// 左栏：设备身份、电量与三项只读状态。
fn status_card(ui: &mut Ui, p: &Palette, info: &DeviceInfo, state: &DeviceState, snap: &Snapshot) {
    card(ui, p, |ui| {
        ui.horizontal(|ui| {
            let letter = info
                .brand
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "P".into());
            device_tile(
                ui,
                p,
                info.kind,
                &letter,
                connection_color(p, info.connection),
                38.0,
            );
            ui.add_space(10.0);
            // 连接标签在本行之后才布局，必须先预留它的宽度：否则长型号会把
            // 整行占满，标签被挤出卡片范围并与型号文字叠在一起。
            let chip_w = ui
                .painter()
                .layout_no_wrap(
                    info.connection.label().to_owned(),
                    egui::FontId::proportional(text::MICRO),
                    p.text,
                )
                .size()
                .x
                + 18.0;
            let title_w = (ui.available_width() - chip_w - 10.0).max(60.0);
            ui.allocate_ui_with_layout(
                Vec2::new(title_w, 38.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(title_w);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    // 与首页卡片同序：型号在上、厂家在下。
                    // egui 0.37 的 Label 对省略文本会自动弹一条完整文本提示，
                    // 与下面显式的 on_hover_text 叠加会同时出现两条同样的
                    // tooltip；这里关掉自动提示，只保留手写的那条。
                    ui.add(
                        egui::Label::new(heading(&info.model, text::SUBTITLE, p.text))
                            .truncate()
                            .show_tooltip_when_elided(false),
                    )
                    .on_hover_text(&info.model);
                    ui.label(
                        RichText::new(info.brand.to_uppercase())
                            .size(11.0)
                            .extra_letter_spacing(0.6)
                            .color(p.subtle),
                    );
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                connection_chip(ui, p, info.connection);
            });
        });

        ui.add_space(14.0);
        divider(ui, p);

        // "有线"不是电量读数：连接方式已经在身份行和标题栏里写明了，
        // 这里没有百分比就不摆电量区，避免把状态词当成数值展示。
        let wired = info.connection.is_wired();
        // 能力位说没有电量，就不摆一列永远读不到的 "--"。
        let show_battery = snap.caps.battery && (state.battery.is_some() || !wired);
        ui.add_space(if show_battery { 14.0 } else { 10.0 });

        if show_battery {
            let level = battery_level(state.battery, wired);
            let color = battery_color(level, p);
            let value = state
                .battery
                .map(|b| b.min(100).to_string())
                .unwrap_or_else(|| "--".into());
            // 百分比跟在"电量"标签同一行的右端，紧贴进度条右上角：
            // 原来竖直堆成 标签 / 28px 数字 / 状态 / 进度条 四行，
            // 一个数字占掉整张卡三分之一的竖向空间。
            ui.horizontal(|ui| {
                ui.label(meta("电量", p.dim));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    if state.battery.is_some() {
                        ui.label(micro("%", p.dim));
                        ui.add_space(2.0);
                    }
                    ui.label(numeral(value, text::METRIC, color));
                });
            });
            ui.add_space(7.0);
            battery_bar(ui, p, state.battery, wired, color);

            // 进度条下面不再有状态文字，留白跟着收一档，否则条和分隔线之间空得发虚。
            ui.add_space(10.0);
            divider(ui, p);
            ui.add_space(10.0);
        }

        let hz = 1000 / snap.polling_ms.max(1) as u32;
        if snap.caps.polling {
            status_row(ui, p, "回报率", &hz.to_string(), "Hz");
        }
        if snap.caps.dpi {
            let active = snap.stage_active as usize;
            let current = snap.dpi.get(active).copied().unwrap_or(0);
            status_row(ui, p, "当前 DPI", &current.to_string(), "");
        }
    });
}

/// 只读状态行：标签靠左、数值靠右，读起来像一张规格表。
///
/// 单位单独用微标签排版，和首页卡片的指标列保持同一套“数值大、单位小”
/// 的关系；整串交给数字字族时，单位会跟着数字一起变粗变高。
fn status_row(ui: &mut Ui, p: &Palette, label: &str, value: &str, unit: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(p.dim));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if !unit.is_empty() {
                ui.label(micro(unit, p.dim));
                ui.add_space(3.0);
            }
            ui.label(numeral(value, 15.0, p.text));
        });
    });
    ui.add_space(7.0);
}

/// 右栏：可写设置按能力位排列，系统设置单独成卡。
#[allow(clippy::too_many_arguments)]
fn settings_column(
    ui: &mut Ui,
    p: &Palette,
    info: &DeviceInfo,
    state: &DeviceState,
    ed: EditorState,
    snap: &Snapshot,
    dpi_baseline: Option<&[u16]>,
    writable: bool,
    cmds: &mut Vec<Cmd>,
) {
    let mut action = ViewAction { cmds: Vec::new() };
    if snap.caps.dpi {
        dpi_card(ui, p, snap, writable, dpi_baseline, &mut action);
        ui.add_space(14.0);
    }
    if snap.caps.polling {
        polling_card(ui, p, snap, writable, info.limits.polling, &mut action);
        ui.add_space(14.0);
    }
    if snap.caps.sensor {
        settings_card(ui, p, state, ed, info.limits, writable, &mut action);
        ui.add_space(14.0);
    }
    if !snap.caps.polling && !snap.caps.sensor {
        limited_card(ui, p, snap);
        ui.add_space(14.0);
    }
    cmds.append(&mut action.cmds);
}

/// 档位与当前值分层展示，恢复入口仍仅在偏离设备基线时出现。
fn dpi_card(
    ui: &mut Ui,
    p: &Palette,
    snap: &Snapshot,
    enabled: bool,
    baseline: Option<&[u16]>,
    action: &mut ViewAction,
) {
    let active = snap.stage_active as usize;
    let current = snap.dpi.get(active).copied().unwrap_or(0);
    let stages = snap.caps.dpi_stages;
    // 只收集真正偏离基线的档位；没有偏离时不显示恢复入口，正常状态下少一个按钮。
    let diffs: Vec<(usize, u16)> = baseline
        .map(|base| {
            snap.dpi
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let b = base.get(i).copied()?;
                    (b != *v).then_some((i, b.clamp(snap.dpi_min, snap.dpi_max)))
                })
                .collect()
        })
        .unwrap_or_default();
    card(ui, p, |ui| {
        split_row(
            ui,
            |ui| {
                ui.label(
                    RichText::new(format!("当前 {current}"))
                        .size(13.0)
                        .color(p.accent),
                );
                // 设备离线/只读时这个按钮点了也只会把命令丢进死队列，索性不出现。
                if !diffs.is_empty() && enabled {
                    ui.add_space(8.0);
                    if chip_small(ui, p, "恢复初始值")
                        .on_hover_text("把偏离首次读取值的档位改回原值")
                        .clicked()
                    {
                        for (i, base) in &diffs {
                            action.cmds.push(Cmd::Dpi(*i as u8, *base));
                        }
                    }
                }
            },
            |ui| {
                ui.vertical(|ui| {
                    section_heading(
                        ui,
                        p,
                        if stages { "DPI 档位" } else { "DPI" },
                        if stages {
                            "点击数值切换档位，使用加减按钮微调当前档位"
                        } else {
                            "直接设定鼠标当前灵敏度，写入后立即生效"
                        },
                    );
                });
            },
        );
        ui.add_space(12.0);
        ui.add_enabled_ui(enabled, |ui| {
            if stages {
                ui.horizontal_wrapped(|ui| {
                    // 折行时两行档位之间要有间距（surface 已把垂直间距清零）。
                    ui.spacing_mut().item_spacing.y = 8.0;
                    for (i, v) in snap.dpi.iter().enumerate() {
                        let selected = i == active;
                        if dpi_chip(ui, p, *v, selected) && !selected {
                            action.cmds.push(Cmd::Activate(i as u8));
                        }
                    }
                });
                ui.add_space(10.0);
            }
            dpi_editor(ui, p, snap, baseline, &mut action.cmds);
        });
    });
}

/// One editable line for the current stage: type an exact value, or nudge it
/// with the step buttons. Editing any stage is "click that chip above, then
/// type here" — the chip click doubles as the switch, which is what the mouse
/// actually does, so there is no third hidden state to keep in sync.
///
/// Everything the editor needs (active stage, its value, whether stages exist
/// at all) is derived from `snap` here rather than passed in — the caller used
/// to hand over six loose values that were all readable from the snapshot.
fn dpi_editor(
    ui: &mut Ui,
    p: &Palette,
    snap: &Snapshot,
    baseline: Option<&[u16]>,
    cmds: &mut Vec<Cmd>,
) {
    let stage = snap.stage_active;
    let active = stage as usize;
    let current = snap.dpi.get(active).copied().unwrap_or(0);
    let stages = snap.caps.dpi_stages;
    // 被改动过才把初始值摆出来：用户随时能看出“原来是多少”，又不会在没改
    // 的时候多一行干扰信息。
    let initial = baseline.and_then(|b| b.get(active).copied());
    let (min, max) = (snap.dpi_min, snap.dpi_max);
    let mut next = None;
    ui.horizontal(|ui| {
        if stages {
            ui.label(
                RichText::new(format!("第 {} 档", stage as usize + 1))
                    .size(11.0)
                    .color(p.dim),
            );
            ui.add_space(6.0);
        }
        if let Some(v) = dpi_input(ui, p, stage, current, min, max) {
            next = Some(v);
        }
        ui.add_space(4.0);
        ui.label(RichText::new("DPI").size(11.0).color(p.dim));
        if let Some(base) = initial.filter(|b| *b != current) {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("初始 {base}"))
                    .size(11.0)
                    .color(p.dim),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let base = next.unwrap_or(current);
            let step = if base < 1000 { 50i32 } else { 100 };
            if step_button(ui, p, true, &format!("按住可连续增加（每次 +{step}）")) {
                next = Some((base as i32 + step).clamp(min as i32, max as i32) as u16);
            }
            if step_button(ui, p, false, &format!("按住可连续减少（每次 −{step}）")) {
                next = Some((base as i32 - step).clamp(min as i32, max as i32) as u16);
            }
        });
    });
    if let Some(v) = next {
        if v != current {
            cmds.push(Cmd::Dpi(stage, v));
        }
    }
    ui.add_space(6.0);
    ui.label(
        RichText::new(if stages {
            format!("点上方档位切换要改的档；数值可直接输入，回车或移开光标后生效（{min}–{max}）")
        } else {
            format!("数值可直接输入，回车或移开光标后生效（{min}–{max}）")
        })
        .size(12.0)
        .color(p.dim),
    );
}

/// Typing buffer for the DPI field, kept per stage in egui's temp storage.
#[derive(Clone)]
struct DpiDraft {
    text: String,
    /// Last value the field was showing, so a change made elsewhere (step
    /// button, device poll, another stage) can refresh the text without
    /// fighting the caret while the user types.
    seen: u16,
}

/// Returns the value to commit when the field loses focus.
fn dpi_input(ui: &mut Ui, p: &Palette, stage: u8, current: u16, min: u16, max: u16) -> Option<u16> {
    let id = ui.scope_id().with(("dpi-input", stage));
    let mut draft = ui.data(|d| d.get_temp::<DpiDraft>(id)).unwrap_or(DpiDraft {
        text: current.to_string(),
        seen: current,
    });
    let focused = ui.memory(|m| m.has_focus(id));
    if !focused && draft.seen != current {
        draft.text = current.to_string();
        draft.seen = current;
    }

    // 比上方档位标签（62×32）略宽，5 位 DPI 数字不贴边；但仍不做成 92px
    // 的独立表单字段。字号跟档位数字走（展示字族 14），不用 16 号正文。
    let response = egui::Frame::new()
        .fill(p.inset)
        .stroke(Stroke::new(
            1.0,
            if focused {
                alpha(p.accent, 190)
            } else {
                p.border
            },
        ))
        .corner_radius(egui::CornerRadius::same(crate::theme::ctl::RADIUS_SM))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut draft.text)
                    .id(id)
                    .desired_width(64.0)
                    .char_limit(5)
                    .font(crate::theme::heading_font(14.0))
                    .align(egui::Align2::CENTER_CENTER)
                    .text_color(p.text)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::symmetric(7, 4))
                    .hint_text("DPI"),
            )
        })
        .inner;
    if response.changed() {
        let digits: String = draft.text.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits != draft.text {
            draft.text = digits;
        }
    }

    let mut commit = None;
    if response.lost_focus() {
        match draft.text.trim().parse::<u16>() {
            Ok(v) => {
                let v = v.clamp(min, max);
                draft.text = v.to_string();
                draft.seen = v;
                if v != current {
                    commit = Some(v);
                }
            }
            // Empty or half-typed input falls back to what the device has.
            Err(_) => draft.text = current.to_string(),
        }
    }

    ui.data_mut(|d| d.insert_temp(id, draft));
    commit
}

/// 回报率沿用设备提供的有效档位，并用统一选择控件展示。
fn polling_card(
    ui: &mut Ui,
    p: &Palette,
    snap: &Snapshot,
    enabled: bool,
    options: &'static [u8],
    action: &mut ViewAction,
) {
    card(ui, p, |ui| {
        section_heading(
            ui,
            p,
            "回报率",
            "鼠标每秒向电脑上报位置的次数。数值越高越跟手，也越费电",
        );
        ui.add_space(10.0);
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                for &ms in options {
                    let hz = 1000 / ms.max(1) as u32;
                    let selected = snap.polling_ms == ms;
                    if chip(ui, p, &format!("{hz} Hz"), selected).clicked() && !selected {
                        action.cmds.push(Cmd::Polling(ms));
                    }
                }
            });
        });
        ui.add_space(8.0);
        setting_note(
            ui,
            p,
            "竞技射击用 1000 Hz，办公或想省电用 500 Hz；125/250 Hz 主要给老设备或排障用",
        );
    });
}

/// Devices whose firmware only exposes DPI get an honest summary instead of
/// cards full of dead switches: what is adjustable here, and what is not.
fn limited_card(ui: &mut Ui, p: &Palette, snap: &Snapshot) {
    card(ui, p, |ui| {
        section_heading(
            ui,
            p,
            "这台设备能改什么",
            "固件只开放了下面几项，其余参数要改请用官方驱动",
        );
        ui.add_space(10.0);
        let row = |ui: &mut Ui, label: &str, yes: bool, note: &str| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if yes { "✓" } else { "—" })
                        .size(12.0)
                        .strong()
                        .color(if yes { p.ok } else { p.dim }),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(label).size(12.0).color(p.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(note).size(12.0).color(p.dim));
                });
            });
        };
        row(
            ui,
            "DPI",
            snap.caps.dpi,
            &format!("{}–{}", snap.dpi_min, snap.dpi_max),
        );
        // `surface` 把垂直间距清零了，能力行之间要显式留白。
        ui.add_space(6.0);
        row(
            ui,
            "电量",
            snap.caps.battery,
            if snap.caps.battery {
                "后台定期读取"
            } else {
                "固件未上报"
            },
        );
        ui.add_space(6.0);
        row(ui, "回报率", snap.caps.polling, "固件固定，不可调");
        ui.add_space(6.0);
        row(ui, "静默高度 / 防抖 / 休眠", snap.caps.sensor, "未开放");
        ui.add_space(6.0);
        setting_note(
            ui,
            p,
            "这些限制来自设备固件本身，不是面板没做：换官方驱动也只能改同一批参数。",
        );
    });
}

fn settings_card(
    ui: &mut Ui,
    p: &Palette,
    state: &DeviceState,
    ed: EditorState,
    limits: Limits,
    enabled: bool,
    action: &mut ViewAction,
) {
    let Some(snap) = state.snap.as_ref() else {
        return;
    };
    card(ui, p, |ui| {
        section_heading(ui, p, "传感器与按键", "调整追踪修正、按键响应和休眠行为");
        ui.add_space(12.0);
        ui.add_enabled_ui(enabled, |ui| {
            toggle_row(
                ui,
                p,
                "移动同步",
                "把鼠标的移动轨迹交给算法做平滑修正，让追踪更稳；关闭后指针更跟手、更接近原始输入",
                "竞技关闭",
                snap.motion,
                |v| {
                    action.cmds.push(Cmd::Motion(v));
                },
            );
            toggle_row(
                ui,
                p,
                "直线修正",
                "横向拖动时把轨迹“拉直”，消除手上的细微抖动；开启后画不出小幅斜线",
                "办公开启",
                snap.line_corr,
                |v| {
                    action.cmds.push(Cmd::Line(v));
                },
            );
            toggle_row(
                ui,
                p,
                "波纹修正",
                "在高 DPI 下抑制轨迹的波浪状抖动，让慢速移动更顺；低 DPI 时几乎感觉不到",
                "3200 以上开启",
                snap.wave_corr,
                |v| {
                    action.cmds.push(Cmd::Wave(v));
                },
            );
            ui.add_space(4.0);
            divider(ui, p);
            ui.add_space(10.0);

            stepper_row(
                ui,
                p,
                "按下防抖",
                "按下瞬间的过滤时长，用来挡掉触点抖动造成的误触双击",
                "常用 8 ms",
                &ed.deb_press
                    .clamp(limits.debounce.0, limits.debounce.1)
                    .to_string(),
                "ms",
                |m, c| {
                    let base = ed.deb_press.clamp(limits.debounce.0, limits.debounce.1);
                    if m {
                        action.cmds.push(Cmd::Debounce(
                            base.saturating_sub(1).max(limits.debounce.0),
                            true,
                        ));
                    }
                    if c {
                        action
                            .cmds
                            .push(Cmd::Debounce((base + 1).min(limits.debounce.1), true));
                    }
                },
            );
            stepper_row(
                ui,
                p,
                "抬起防抖",
                "松开瞬间的过滤时长；调得太大会让连点跟不上手速",
                "常用 8 ms",
                &ed.deb_release
                    .clamp(limits.debounce.0, limits.debounce.1)
                    .to_string(),
                "ms",
                |m, c| {
                    let base = ed.deb_release.clamp(limits.debounce.0, limits.debounce.1);
                    if m {
                        action.cmds.push(Cmd::Debounce(
                            base.saturating_sub(1).max(limits.debounce.0),
                            false,
                        ));
                    }
                    if c {
                        action
                            .cmds
                            .push(Cmd::Debounce((base + 1).min(limits.debounce.1), false));
                    }
                },
            );
            stepper_row(
                ui,
                p,
                "休眠等待",
                "闲置多久后进入休眠省电，唤醒要再点一下或晃一下鼠标；有线连接时不休眠",
                "常用 10 分钟",
                &ed.sleep_min
                    .clamp(limits.sleep.0, limits.sleep.1)
                    .to_string(),
                "分钟",
                |m, c| {
                    let base = ed.sleep_min.clamp(limits.sleep.0, limits.sleep.1);
                    if m {
                        action
                            .cmds
                            .push(Cmd::Sleep(base.saturating_sub(1).max(limits.sleep.0)));
                    }
                    if c {
                        action.cmds.push(Cmd::Sleep((base + 1).min(limits.sleep.1)));
                    }
                },
            );
            let lod_rec = limits
                .lod_recommended
                .map(|range| lod_range_text(range, limits.lod_scale))
                .unwrap_or_default();
            // "1.0 mm" / "—"：拆成数值与单位两段，单位和数字各用各的字族。
            let lod = lod_text(ed.lod.clamp(limits.lod.0, limits.lod.1), limits.lod_scale);
            let (lod_value, lod_unit) = lod.split_once(' ').unwrap_or((lod.as_str(), ""));
            stepper_row(
                ui,
                p,
                "静默高度 LOD",
                "鼠标离开桌面多少毫米后停止追踪。越低，抬鼠归位后指针越不容易漂",
                &lod_rec,
                lod_value,
                lod_unit,
                |m, c| {
                    let base = ed.lod.clamp(limits.lod.0, limits.lod.1);
                    if m {
                        action
                            .cmds
                            .push(Cmd::Lod(base.saturating_sub(1).max(limits.lod.0)));
                    }
                    if c {
                        action.cmds.push(Cmd::Lod((base + 1).min(limits.lod.1)));
                    }
                },
            );

            if matches!(state.link, Link::Connecting) {
                ui.add_space(8.0);
                ui.label(RichText::new("正在连接设备…").size(12.0).color(p.dim));
            }
        });
    });
}

/// Small explanatory footnote, used where a whole row of settings shares one
/// piece of advice.
fn setting_note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(p.dim));
}

/// 最近一次写入的系统值及其有效期。系统设置没有 `EditorState` 那样的乐观
/// 状态宿主：步进按钮连发时如果每帧直接读系统值当基准，同一帧内的多次
/// fire 会算出同一个数。把上次写入的值缓存两秒，连发在缓存上累加；窗口
/// 过后（或值被系统设置面板等别处修改）自动回落到系统实际值。
#[derive(Clone, Copy)]
struct SysEdit {
    v: u32,
    until: f64,
}

fn sys_value(ui: &Ui, id: egui::Id, actual: u32) -> u32 {
    let now = ui.input(|i| i.time);
    ui.data(|d| d.get_temp::<SysEdit>(id))
        .filter(|e| now < e.until)
        .map_or(actual, |e| e.v)
}

/// 写系统值并记住它。写失败（被策略/安全软件拦截）不记缓存，让数值回落
/// 到系统实际值——那正是 `refused` 提示想要传达的事实。
fn sys_write(ui: &Ui, id: egui::Id, v: u32, write: impl FnOnce(u32) -> bool) -> bool {
    if write(v) {
        let until = ui.input(|i| i.time) + 2.0;
        ui.data_mut(|d| d.insert_temp(id, SysEdit { v, until }));
        true
    } else {
        false
    }
}

/// Windows-level pointer and wheel settings. They are not stored on the mouse,
/// so they get their own card and say so — otherwise people expect them to
/// travel with the device the way DPI does.
pub fn system_card(ui: &mut Ui, p: &Palette) {
    // A refused write has no visible symptom (the number simply stays put), so
    // it is remembered for a few seconds and shown as a notice.
    let fail_id = ui.scope_id().with("sys-write-refused");
    let now = ui.input(|i| i.time);
    let mut refused = false;

    card(ui, p, |ui| {
        section_heading(
            ui,
            p,
            "系统指针",
            "改的是 Windows 全局设置，不写入鼠标；换台电脑不会跟着走",
        );
        ui.add_space(10.0);

        let speed_id = ui.scope_id().with("sys-speed");
        let speed = sys_value(ui, speed_id, sysinput::mouse_speed());
        let mut next_speed: Option<u32> = None;
        stepper_row(
            ui,
            p,
            "指针速度",
            "等于 Windows 鼠标设置里的滑块（1–20），10 是默认。和 DPI 是两回事：DPI 决定灵敏度，这个只是系统倍率",
            "10",
            &speed.to_string(),
            "/ 20",
            |m, c| {
                if m {
                    next_speed =
                        Some(speed.saturating_sub(1).max(sysinput::MOUSE_SPEED_MIN));
                }
                if c {
                    next_speed = Some((speed + 1).min(sysinput::MOUSE_SPEED_MAX));
                }
            },
        );
        if let Some(v) = next_speed {
            if !sys_write(ui, speed_id, v, sysinput::set_mouse_speed) {
                refused = true;
            }
        }

        let lines_id = ui.scope_id().with("sys-wheel");
        let lines = sys_value(ui, lines_id, sysinput::wheel_lines());
        let page = lines == sysinput::WHEEL_PAGESCROLL;
        ui.add_enabled_ui(!page, |ui| {
            let mut next_lines: Option<u32> = None;
            // "3 行" / "整屏"：同样是数值与单位分开，中文单位不进数字字族。
            let wheel = sysinput::wheel_label(lines);
            let (wheel_value, wheel_unit) = wheel.split_once(' ').unwrap_or((wheel.as_str(), ""));
            stepper_row(
                ui,
                p,
                "滚轮行数",
                "滚一格滚多少行；网页和表格里 3 行最常见，调大滚得更快",
                "3 行",
                wheel_value,
                wheel_unit,
                |m, c| {
                    if m {
                        next_lines = Some(lines.saturating_sub(1).max(1));
                    }
                    if c {
                        next_lines = Some((lines + 1).min(sysinput::WHEEL_LINES_MAX));
                    }
                },
            );
            if let Some(v) = next_lines {
                if !sys_write(ui, lines_id, v, sysinput::set_wheel_lines) {
                    refused = true;
                }
            }
        });
        let mut next_page: Option<bool> = None;
        toggle_row(
            ui,
            p,
            "整屏滚动",
            "滚一格翻一整屏，相当于 Windows 的「一次滚动一个屏幕」；关掉就回到按行滚动",
            "读长文开启",
            page,
            |v| next_page = Some(v),
        );
        if let Some(v) = next_page {
            let next = if v {
                sysinput::WHEEL_PAGESCROLL
            } else {
                sysinput::WHEEL_LINES_DEFAULT
            };
            if !sys_write(ui, lines_id, next, sysinput::set_wheel_lines) {
                refused = true;
            }
        }
        ui.add_space(2.0);
        setting_note(
            ui,
            p,
            "这些设置点完立即生效，所有程序一起变，和当前这台鼠标无关",
        );
    });

    if refused {
        ui.data_mut(|d| d.insert_temp::<f64>(fail_id, now + 5.0));
    }
    let until = ui.data(|d| d.get_temp::<f64>(fail_id)).unwrap_or(0.0);
    if now < until {
        ui.add_space(8.0);
        notice(
            ui,
            p,
            "这次修改没有生效",
            "Windows 拒绝了系统设置写入，可能被组策略或安全软件拦截；鼠标本身的参数不受影响。",
            p.warn,
        );
        // Keep ticking so the notice clears itself without another click.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn lod_text(code: u8, scale: LodScale) -> String {
    if scale == LodScale::UNKNOWN {
        return "—".into();
    }
    let mm = scale.tenths(code) as f32 / 10.0;
    format!("{mm:.1} mm")
}

/// LOD 推荐区间的展示文本，例如 VT1 Pro 的编码 1..3 → "1.0–1.2 mm"。
fn lod_range_text(range: (u8, u8), scale: LodScale) -> String {
    if scale == LodScale::UNKNOWN {
        return String::new();
    }
    let (lo, hi) = (scale.tenths(range.0), scale.tenths(range.1));
    if lo == hi {
        format!("{:.1} mm", lo as f32 / 10.0)
    } else {
        format!("{:.1}–{:.1} mm", lo as f32 / 10.0, hi as f32 / 10.0)
    }
}

#[derive(Default)]
pub struct MissingAction {
    pub rescan: bool,
    pub back: bool,
}

/// The device dropped out of the list while its page was still open. Say so in
/// place — bouncing the user back to the list would hide the reason.
pub fn missing(ui: &mut Ui, p: &Palette) -> MissingAction {
    let mut action = MissingAction::default();
    card(ui, p, |ui| {
        ui.label(heading("设备已移除", text::SUBTITLE, p.text));
        ui.add_space(3.0);
        ui.label(
            RichText::new("该设备已断开或拔出，当前没有可读取的配置。")
                .size(12.0)
                .color(p.dim),
        );
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if chip(ui, p, "返回列表", false)
                .on_hover_text("返回设备工作台（Esc）")
                .clicked()
            {
                action.back = true;
            }
            ui.add_space(6.0);
            if primary_button(ui, p, "重新扫描", "重新检测已连接的外设") {
                action.rescan = true;
            }
        });
    });
    action
}

fn notice(ui: &mut Ui, p: &Palette, title: &str, body: &str, color: egui::Color32) {
    let bg = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 22);
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.4)))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(13.0).strong().color(color));
            ui.add_space(2.0);
            ui.label(RichText::new(body).size(12.0).color(p.dim));
        });
}

/// Floating toast for transient device errors and disconnects, painted on a
/// foreground layer above the scroll content. It deliberately takes no part
/// in layout: the old in-flow notice appeared and vanished with device
/// hiccups, shoving the page up and down under a stationary cursor.
/// `alpha` < 1 淡出中；调用方负责到点后停止绘制并驱动重绘。
pub fn toast_overlay(ui: &Ui, p: &Palette, title: &str, body: &str, alpha: f32) {
    if alpha <= 0.01 {
        return;
    }
    let fade = |c: egui::Color32| c.gamma_multiply(alpha);
    let host = ui.max_rect();
    let max_w = 460.0_f32.min(host.width() - 24.0);
    // `ui.painter()` 返回 `&Painter`；`with_layer_id` 按值消费，clone 一下
    // （内部是 Arc，代价可忽略）就能拿到前景层的独立 painter。
    let painter = ui.painter().clone().with_layer_id(egui::LayerId::new(
        egui::Order::Foreground,
        // Id::unique 是固定种子哈希：同一个字符串每帧得到同一个 id，图层
        // 不会逐帧漂移。（Id::new 在 0.37 已弃用，只是它的别名。）
        egui::Id::unique("config-toast"),
    ));
    let title_gal = painter.layout_no_wrap(
        title.to_owned(),
        egui::FontId::proportional(12.0),
        fade(p.danger),
    );
    let body_gal = painter.layout(
        body.to_owned(),
        egui::FontId::proportional(11.0),
        fade(p.text),
        max_w - 24.0,
    );
    // 换行后的 galley 宽度只反映最长一行，不能拿它当卡片宽度，否则卡片比
    // 文本换行宽度窄、字会溢出边框；只有单行时才按内容收缩。
    let w = if body_gal.rows.len() > 1 {
        max_w
    } else {
        (title_gal.size().x.max(body_gal.size().x) + 24.0).clamp(120.0, max_w)
    };
    let h = 10.0 + title_gal.size().y + 4.0 + body_gal.size().y + 10.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(host.center().x - w / 2.0, host.top() + 6.0),
        egui::vec2(w, h),
    );
    let focused = ui.ctx().input(|i| i.focused);
    shadow_card(&painter, rect, p.card, 10, p.dark, focused);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(10),
        egui::Color32::from_rgba_unmultiplied(p.danger.r(), p.danger.g(), p.danger.b(), 14)
            .gamma_multiply(alpha),
    );
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(10),
        Stroke::new(1.0, fade(p.danger.gamma_multiply(0.4))),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        egui::pos2(rect.left() + 12.0, rect.top() + 10.0),
        title_gal.clone(),
        p.text,
    );
    painter.galley(
        egui::pos2(rect.left() + 12.0, rect.top() + 14.0 + title_gal.size().y),
        body_gal,
        p.text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lod_text_uses_the_model_scale() {
        let scale = LodScale {
            base_tenths: 9,
            step_tenths: 1,
        };
        assert_eq!(lod_text(1, scale), "1.0 mm");
        assert_eq!(lod_text(11, scale), "2.0 mm");
        assert_eq!(lod_text(1, LodScale::UNKNOWN), "—");
    }

    #[test]
    fn lod_recommendation_formats_from_the_model_scale() {
        let scale = LodScale {
            base_tenths: 9,
            step_tenths: 1,
        };
        assert_eq!(lod_range_text((1, 3), scale), "1.0–1.2 mm");
        assert_eq!(lod_range_text((2, 2), scale), "1.1 mm");
        assert_eq!(lod_range_text((1, 3), LodScale::UNKNOWN), "");
    }
}
