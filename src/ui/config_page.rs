//! The device configuration page. It is a route inside the main window, not a
//! separate viewport, so the titlebar keeps its back affordance and the user
//! never loses the device list.

use eframe::egui::{self, Align, Layout, Margin, RichText, Stroke, Ui, Vec2};

use crate::devices::{Cmd, DeviceInfo, DeviceState, EditorState, Link, Snapshot};
use crate::driver::Limits;
use crate::sysinput;
use crate::theme::{battery_color, battery_label, battery_level, battery_text, heading, Palette};
use crate::ui::widgets::{
    battery_bar, card, chip, chip_small, connection_chip, connection_color, device_tile, divider,
    dpi_chip, link_action, paint_refresh, primary_button, section_heading, shadow_card, split_row,
    stat, step_button, stepper_row, toggle_row,
};

#[derive(Default)]
pub struct ViewAction {
    pub cmds: Vec<Cmd>,
}

/// 设备摘要和参数分组沿用同一材质，写入行为由原有能力位控制。
pub fn view(
    ui: &mut Ui,
    p: &Palette,
    info: &DeviceInfo,
    state: &DeviceState,
    dpi_baseline: Option<&[u16]>,
) -> ViewAction {
    let mut action = ViewAction::default();
    let snap = state.snap.clone();
    let ed = state.editor;
    let writable = info.has_config && !matches!(state.link, Link::Offline(_)) && snap.is_some();

    card(ui, p, |ui| {
        split_row(
            ui,
            |ui| {
                connection_chip(ui, p, info.connection);
                if info.has_config {
                    ui.add_space(4.0);
                    if link_action(ui, p, "刷新配置", "重新读取设备上的当前配置", paint_refresh)
                        .clicked()
                    {
                        action.cmds.push(Cmd::Refresh);
                    }
                }
            },
            |ui| {
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
                    56.0,
                );
                ui.add_space(10.0);
                let title_w = ui.available_width().max(60.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(title_w, 56.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(title_w);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            RichText::new(info.brand.to_uppercase())
                                .size(10.5)
                                .strong()
                                .color(p.subtle),
                        );
                        ui.add(egui::Label::new(heading(&info.model, 22.0, p.text)).truncate())
                            .on_hover_text(&info.model);
                    },
                );
            },
        );
        ui.add_space(16.0);
        divider(ui, p);
        ui.add_space(16.0);
        let level = battery_level(state.battery, info.connection.is_wired());
        let color = battery_color(level, p);
        let battery_text = battery_text(state.battery, info.connection.is_wired());
        let has_polling = snap.as_ref().is_some_and(|s| s.caps.polling);
        let has_dpi = snap.as_ref().is_some_and(|s| s.caps.dpi);
        egui::Frame::new()
            .fill(p.inset)
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.columns(1 + has_polling as usize + has_dpi as usize, |cols| {
                    stat(&mut cols[0], p, "电量", &battery_text);
                    if has_polling {
                        stat(
                            &mut cols[1],
                            p,
                            "回报率",
                            &snap
                                .as_ref()
                                .map(|s| format!("{} Hz", 1000 / s.polling_ms.max(1) as u32))
                                .unwrap_or_else(|| "--".into()),
                        );
                    }
                    if has_dpi {
                        stat(
                            &mut cols[1 + has_polling as usize],
                            p,
                            "当前 DPI",
                            &snap
                                .as_ref()
                                .and_then(|s| s.dpi.get(s.stage_active as usize).copied())
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "--".into()),
                        );
                    }
                });
                ui.add_space(12.0);
                battery_bar(ui, p, state.battery, info.connection.is_wired(), color);
            });
        ui.add_space(12.0);
        split_row(
            ui,
            |ui| {
                ui.label(RichText::new(battery_label(level)).size(11.0).color(color));
            },
            |ui| {
                if info.has_config {
                    ui.label(
                        RichText::new("配置更改即时生效 · 无需手动保存")
                            .size(11.0)
                            .color(p.dim),
                    );
                }
            },
        );
    });

    // 断开与写入失败不再以文档流通知出现：它们跟着设备状态偶发地插入又
    // 消失，会把视口下方的内容整体顶下又顶回——用户停在一行设置上不动，
    // 光标下的控件却换了，看起来就像页面自己跳了一下。两者改走 app.rs 里
    // show_config_page 的浮层提示（toast_overlay），固定悬在页首、不占布局。
    // 只读设备是设备静态属性，恒在或恒不在，留在文档流里是安全的。
    if !info.has_config {
        ui.add_space(14.0);
        notice(
            ui,
            p,
            "只读设备",
            info.reason
                .unwrap_or("未找到该设备的厂商配置接口，无法读写参数。"),
            p.dim,
        );
    }

    let Some(snap) = snap else {
        // A read-only device already got the "只读设备" notice above; a second
        // card saying it has no configuration would just repeat it.
        if info.has_config && !matches!(state.link, Link::Offline(_)) {
            ui.add_space(14.0);
            card(ui, p, |ui| {
                ui.label(RichText::new("正在读取设备配置…").size(13.5).color(p.dim));
            });
        }
        return action;
    };

    if snap.caps.dpi {
        ui.add_space(14.0);
        dpi_card(ui, p, &snap, writable, dpi_baseline, &mut action);
    }
    if snap.caps.polling {
        ui.add_space(14.0);
        polling_card(ui, p, &snap, writable, info.limits.polling, &mut action);
    }
    if snap.caps.sensor {
        ui.add_space(14.0);
        settings_card(ui, p, state, ed, info.limits, writable, &mut action);
    }
    if !snap.caps.polling && !snap.caps.sensor {
        ui.add_space(14.0);
        limited_card(ui, p, &snap);
    }
    ui.add_space(14.0);
    system_card(ui, p);
    action
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
                ui.label(heading(format!("当前 {current}"), 12.0, p.accent));
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
                        if dpi_chip(ui, p, *v, selected).clicked() && !selected {
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
                    .size(10.5)
                    .color(p.dim),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let base = next.unwrap_or(current);
            let step = if base < 1000 { 50i32 } else { 100 };
            if step_button(ui, p, "+", &format!("按住可连续增加（每次 +{step}）")) {
                next = Some((base as i32 + step).clamp(min as i32, max as i32) as u16);
            }
            if step_button(ui, p, "−", &format!("按住可连续减少（每次 −{step}）")) {
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
        .size(10.5)
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

    let response = ui.add(
        egui::TextEdit::singleline(&mut draft.text)
            .id(id)
            .desired_width(76.0)
            .char_limit(5)
            .font(egui::FontId::proportional(18.0))
            .align(egui::Align2::RIGHT_CENTER)
            .text_color(p.text)
            .margin(egui::Margin::symmetric(8, 6))
            .hint_text("DPI"),
    );
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
                    ui.label(RichText::new(note).size(10.5).color(p.dim));
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
        row(ui, "电量", true, "每 20 秒读一次");
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
                "8 ms",
                &format!(
                    "{} ms",
                    ed.deb_press.clamp(limits.debounce.0, limits.debounce.1)
                ),
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
                "8 ms",
                &format!(
                    "{} ms",
                    ed.deb_release.clamp(limits.debounce.0, limits.debounce.1)
                ),
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
                "10 分钟",
                &format!(
                    "{} 分钟",
                    ed.sleep_min.clamp(limits.sleep.0, limits.sleep.1)
                ),
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
            stepper_row(
                ui,
                p,
                "静默高度 LOD",
                "鼠标离开桌面多少毫米后停止追踪。越低，抬鼠归位后指针越不容易漂",
                "1.0–1.2 mm",
                &lod_text(ed.lod.clamp(limits.lod.0, limits.lod.1)),
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
                ui.label(RichText::new("正在连接设备…").size(11.0).color(p.dim));
            }
        });
    });
}

/// Small explanatory footnote, used where a whole row of settings shares one
/// piece of advice.
fn setting_note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(p.dim));
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
fn system_card(ui: &mut Ui, p: &Palette) {
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
            &format!("{speed} / 20"),
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
            stepper_row(
                ui,
                p,
                "滚轮行数",
                "滚一格滚多少行；网页和表格里 3 行最常见，调大滚得更快",
                "3 行",
                &sysinput::wheel_label(lines),
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
            "这两项点完立即生效，所有程序一起变，和当前这台鼠标无关",
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

fn lod_text(code: u8) -> String {
    let mm = 0.9 + code as f32 / 10.0;
    format!("{mm:.1} mm")
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
        ui.label(
            RichText::new("设备已移除")
                .size(15.0)
                .strong()
                .color(p.text),
        );
        ui.add_space(3.0);
        ui.label(
            RichText::new("该设备已断开或拔出，当前没有可读取的配置。")
                .size(11.5)
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
            if primary_button(ui, p, "重新扫描", "重新检测已连接的外设").clicked() {
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
            ui.label(RichText::new(title).size(12.5).strong().color(color));
            ui.add_space(2.0);
            ui.label(RichText::new(body).size(11.5).color(p.dim));
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
