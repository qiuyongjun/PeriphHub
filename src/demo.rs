use std::sync::Arc;

use crate::devices::{Cmd, DeviceId, DeviceInfo, Snapshot};
use crate::driver::{Caps, DeviceType, Limits};
use crate::hid::ConnectionType;

pub struct DemoDevice {
    pub info: DeviceInfo,
    pub snap: Snapshot,
    pub battery: Option<u8>,
}

fn info(n: u8, brand: &str, model: &str, connection: ConnectionType) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId(Arc::from(format!("demo-{n}").as_str())),
        brand: brand.to_string(),
        model: model.to_string(),
        kind: DeviceType::Mouse,
        connection,
        has_config: true,
        limits: Limits::default(),
        reason: None,
    }
}

/// 演示数据的快照构造器：字段一一并列比拆成嵌套结构更接近 Snapshot 本身的
/// 形状，参数多是有意的。
#[allow(clippy::too_many_arguments)]
fn snapshot(
    dpi: &[u16],
    active: u8,
    polling_ms: u8,
    lod: u8,
    motion: bool,
    line_corr: bool,
    wave_corr: bool,
    debounce: (u8, u8),
    sleep_min: u8,
) -> Snapshot {
    Snapshot {
        model: String::new(),
        caps: Caps::MOUSE_FULL,
        polling_ms,
        lod,
        motion,
        stage_count: dpi.len() as u8,
        stage_active: active,
        dpi: dpi.to_vec(),
        dpi_min: 100,
        dpi_max: 26000,
        debounce_press: debounce.0,
        debounce_release: debounce.1,
        sleep_min,
        line_corr,
        wave_corr,
    }
}

pub fn devices() -> Vec<DemoDevice> {
    vec![
        DemoDevice {
            info: info(1, "Rapoo", "VT1 Pro", ConnectionType::Wireless24G),
            snap: snapshot(
                &[400, 800, 1200, 1600, 3200, 6400, 26000],
                3,
                1,
                2,
                true,
                false,
                false,
                (8, 8),
                10,
            ),
            battery: Some(86),
        },
        DemoDevice {
            info: info(2, "Rapoo", "VT3 Pro", ConnectionType::Bluetooth),
            snap: snapshot(&[800, 1600, 3200], 1, 4, 2, true, false, false, (8, 8), 15),
            battery: Some(34),
        },
        DemoDevice {
            info: info(3, "Rapoo", "VT9 Pro", ConnectionType::Wired),
            snap: snapshot(
                &[400, 800, 1600, 3200, 6400, 26000],
                2,
                1,
                1,
                true,
                false,
                false,
                (4, 4),
                30,
            ),
            battery: None,
        },
        DemoDevice {
            info: info(4, "Rapoo", "VT1 Pro Max", ConnectionType::Wireless24G),
            snap: snapshot(
                &[400, 800, 1600, 3200, 6400, 26000],
                4,
                1,
                2,
                false,
                true,
                false,
                (16, 16),
                10,
            ),
            battery: Some(12),
        },
    ]
}

pub fn apply(s: &mut Snapshot, cmd: Cmd) {
    match cmd {
        Cmd::Refresh => {}
        Cmd::Polling(ms) => s.polling_ms = ms,
        Cmd::Motion(v) => s.motion = v,
        Cmd::Line(v) => s.line_corr = v,
        Cmd::Wave(v) => s.wave_corr = v,
        Cmd::Debounce(v, press) => {
            if press {
                s.debounce_press = v;
            } else {
                s.debounce_release = v;
            }
        }
        Cmd::Sleep(m) => s.sleep_min = m,
        Cmd::Lod(v) => s.lod = v,
        Cmd::Activate(i) => s.stage_active = i.min(s.stage_count.saturating_sub(1)),
        Cmd::Dpi(stage, v) => {
            if let Some(d) = s.dpi.get_mut(stage as usize) {
                *d = v;
            }
        }
    }
}
