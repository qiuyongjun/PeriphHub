//! Protocol drivers.
//!
//! Every supported family speaks a different vendor protocol, but everything
//! above this module only ever sees [`Cmd`] and [`Snapshot`]. A driver is owned
//! by one worker thread (see `devices::real_worker`), so it can hold raw HID
//! handles and cached registers without any locking.

pub mod hidpp;
pub mod rapoo;

pub use hidpp::Hidpp;
pub use rapoo::Rapoo;

/// Which blocks a device really has. The UI hides everything that is `false`
/// instead of rendering switches that would do nothing on this model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// A single DPI value that can be read and written.
    pub dpi: bool,
    /// Several switchable stages ([`Cmd::Activate`] has an effect).
    pub dpi_stages: bool,
    pub polling: bool,
    /// Motion sync, line/wave correction, debounce, sleep, lift-off distance.
    pub sensor: bool,
}

impl Caps {
    /// A full gaming-mouse block: stages, polling rate and sensor tuning.
    pub const MOUSE_FULL: Caps = Caps {
        dpi: true,
        dpi_stages: true,
        polling: true,
        sensor: true,
    };

    /// DPI only — the fallback when a mouse's onboard profile cannot be read.
    pub const MOUSE_DPI: Caps = Caps {
        dpi: true,
        dpi_stages: false,
        polling: false,
        sensor: false,
    };

    /// Stages and report rate, both stored in the onboard profile; no
    /// lift-off/debounce knobs, because that firmware does not expose them.
    pub const MOUSE_STAGES: Caps = Caps {
        dpi: true,
        dpi_stages: true,
        polling: true,
        sensor: false,
    };
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Filled in by drivers that can ask the device for its own name; empty
    /// means "keep the name from the device table".
    pub model: String,
    pub caps: Caps,
    pub polling_ms: u8,
    pub lod: u8,
    pub motion: bool,
    pub stage_count: u8,
    pub stage_active: u8,
    pub dpi: Vec<u16>,
    /// What the firmware accepts, so the editor can clamp instead of writing
    /// a value the sensor silently rounds off.
    pub dpi_min: u16,
    pub dpi_max: u16,
    pub debounce_press: u8,
    pub debounce_release: u8,
    pub sleep_min: u8,
    pub line_corr: bool,
    pub wave_corr: bool,
}

/// Every command carries an absolute value, so a queued burst only needs its
/// last one per field.
#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    Refresh,
    Polling(u8),
    Motion(bool),
    Line(bool),
    Wave(bool),
    Debounce(u8, bool),
    Sleep(u8),
    Lod(u8),
    Activate(u8),
    Dpi(u8, u16),
}

impl Cmd {
    /// Whether two commands write the same field. Holding "+" for a second
    /// becomes a single write, not ten.
    pub fn same_field(&self, other: &Cmd) -> bool {
        match (self, other) {
            (Cmd::Refresh, Cmd::Refresh) => true,
            (Cmd::Polling(_), Cmd::Polling(_)) => true,
            (Cmd::Motion(_), Cmd::Motion(_)) => true,
            (Cmd::Line(_), Cmd::Line(_)) => true,
            (Cmd::Wave(_), Cmd::Wave(_)) => true,
            (Cmd::Debounce(_, a), Cmd::Debounce(_, b)) => a == b,
            (Cmd::Sleep(_), Cmd::Sleep(_)) => true,
            (Cmd::Lod(_), Cmd::Lod(_)) => true,
            (Cmd::Activate(_), Cmd::Activate(_)) => true,
            (Cmd::Dpi(a, _), Cmd::Dpi(b, _)) => a == b,
            _ => false,
        }
    }
}

/// 打开一个协议适配器。`spec` 里同时带着该型号的布局和取值范围，驱动
/// 不再把型号差异写死在代码里。
pub fn open(spec: DriverSpec, path: &[u16]) -> Result<Box<dyn Driver>, String> {
    spec.kind.open(spec, path)
}

pub trait Driver: Send {
    /// Reads the whole configuration. Called on open and every few seconds.
    fn snapshot(&mut self) -> Result<Snapshot, String>;

    /// Battery percentage, or `None` when the device does not report one.
    fn battery(&mut self) -> Result<Option<u8>, String>;

    /// Writes one field and patches `snap` with the value the device reports
    /// back, so the caller never has to re-read the whole configuration.
    fn apply(&mut self, cmd: Cmd, snap: &mut Snapshot) -> Result<(), String>;

    /// 后台轮询失败后，判断 HID 句柄本身是否还有效（设备被拔掉时会失效）。
    /// 只查句柄，不向设备发命令，用来区分休眠超时与真正断连。
    fn link_alive(&self) -> bool;
}

/// Which protocol family a device speaks. Matched on VID/PID because the
/// protocol is a property of the receiver firmware, not of the HID layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Rapoo,
    Hidpp,
}

/// HID++ 的板载配置布局。只有经过真机验证的型号才能写扇区；其他型号
/// 仍可通过 feature discovery 改当前 DPI、读电量，但不碰板载配置。
///
/// 只列真正验证过的布局。`DriverSpec::layout` 为 `None` 时驱动按"未验证"
/// 处理——这不是一条死路径：同一个接收器配对其他鼠标时，运行时也会因为
/// 型号对不上而降级到同一状态（见 `hidpp::profile_layout_supported`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidppLayout {
    G304,
}

/// 一个协议适配器需要的全部型号差异。
#[derive(Clone, Copy, Debug)]
pub struct DriverSpec {
    pub kind: Kind,
    /// 板载扇区布局；只有 `Kind::Hidpp` 会用到，Rapoo 留 `None`。
    pub layout: Option<HidppLayout>,
    pub limits: Limits,
}

/// 界面允许写入的取值范围。字段是值编码，不是物理单位；驱动和 UI
/// 都以它为唯一边界，避免页面里再出现一套写死的 1..32 / 5..120。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub dpi: (u16, u16),
    pub polling: &'static [u8],
    pub debounce: (u8, u8),
    pub sleep: (u8, u8),
    pub lod: (u8, u8),
}

impl Default for Limits {
    fn default() -> Self {
        RAPOO_LIMITS
    }
}

pub const RAPOO_LIMITS: Limits = Limits {
    dpi: (100, 26000),
    polling: &[8, 4, 2, 1],
    debounce: (1, 32),
    sleep: (5, 120),
    lod: (1, 11),
};

pub const G304_LIMITS: Limits = Limits {
    dpi: (hidpp::DPI_MIN, hidpp::DPI_MAX),
    polling: &[8, 4, 2, 1],
    // G304 的固件不开放这三项；数值保留，能力位会隐藏对应 UI。
    debounce: (1, 32),
    sleep: (5, 120),
    lod: (1, 11),
};

/// 设备形态。界面据此选图标；与协议族（`Kind`）无关，一把键盘和一个
/// 接收器都可能走同一条厂商通道。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceType {
    Mouse,
    Keyboard,
    Receiver,
    Unknown,
}

impl DeviceType {
    /// 接收器在配对后才报出真正的设备名，用它把“接收器”图标换成鼠标/键盘。
    /// 只认明确的词，认不出来就保留原形态。
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.contains("mouse") || lower.contains("鼠标") {
            Some(Self::Mouse)
        } else if lower.contains("keyboard") || lower.contains("键盘") {
            Some(Self::Keyboard)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mouse => "鼠标",
            Self::Keyboard => "键盘",
            Self::Receiver => "接收器",
            Self::Unknown => "设备",
        }
    }
}

/// 支持的设备总表。识别（`hid::enumerate`）、驱动选择、首页"支持范围"
/// 文案都从这里派生；新增同协议型号只需要加一条 profile。
pub struct DeviceProfile {
    pub vid: u16,
    pub pid: u16,
    pub brand: &'static str,
    pub model: &'static str,
    /// 物理形态：首页卡片与配置页头部的图标都取自这里。
    pub kind: DeviceType,
    /// 这条 profile 是否只在插线时出现。接收器和鼠标都枚举成 USB 设备，
    /// 光看实例路径分不出"无线接收器"和"有线直连"，所以由型号表声明：
    /// 同一个鼠标插线和走接收器是两个不同的 PID。
    pub wired: bool,
    /// `None` = 只读设备：没有可写的厂商通道，只识别不配置。
    pub driver: Option<DriverSpec>,
    /// 只读设备在配置页显示的原因（写给用户看的，不是给日志看的）。
    pub reason: Option<&'static str>,
}

pub const PROFILES: &[DeviceProfile] = &[
    DeviceProfile {
        vid: 0x24AE,
        pid: 0x1214,
        brand: "Rapoo",
        model: "VT1 Pro",
        kind: DeviceType::Mouse,
        wired: false,
        driver: Some(DriverSpec {
            kind: Kind::Rapoo,
            layout: None,
            limits: RAPOO_LIMITS,
        }),
        reason: None,
    },
    // 同一个鼠标插线后的身份：PID 换成 0x4414，命令通道仍是 0xFF00/0x0E，
    // 帧格式与接收器完全一致（读 0x0880/0x0884/0x08C0/DPI 表、写块 + 0xA3
    // 提交都在有线模式下实测通过）。不认这个 PID，插着线时设备会整个消失。
    DeviceProfile {
        vid: 0x24AE,
        pid: 0x4414,
        brand: "Rapoo",
        model: "VT1 Pro",
        kind: DeviceType::Mouse,
        wired: true,
        driver: Some(DriverSpec {
            kind: Kind::Rapoo,
            layout: None,
            limits: RAPOO_LIMITS,
        }),
        reason: None,
    },
    DeviceProfile {
        vid: 0x046D,
        pid: 0xC53F,
        brand: "Logitech",
        model: "Lightspeed 接收器",
        // 刚插上时只知道是接收器；配对后设备会报出自己的名字，图标随之换成
        // 真实形态（见 `devices::poll`）。
        kind: DeviceType::Receiver,
        wired: false,
        // 接收器是通用的，布局按 G304 验证；驱动会用设备名再做一次门禁。
        driver: Some(DriverSpec {
            kind: Kind::Hidpp,
            layout: Some(HidppLayout::G304),
            limits: G304_LIMITS,
        }),
        reason: None,
    },
    DeviceProfile {
        vid: 0x3554,
        pid: 0xFA09,
        brand: "AULA",
        model: "F87",
        kind: DeviceType::Keyboard,
        wired: false,
        // Speaks for the keyboard itself, in the user's terms: the old text
        // compared it to another vendor's protocol, which told nobody anything
        // about the device in their hand.
        driver: None,
        reason: Some("能识别到这把键盘，但灯效、按键映射要按厂商私有帧读写，本版本还没接入。"),
    },
    // 键盘插线后的身份：接收器是 3554:FA09，线连走的是键盘自己的控制器
    // （258A:010C，BY Tech）。厂商集合是 0xFF00 的 feature 报表，与接收器那套
    // 20 字节帧不同，所以仍然只读；登记它至少能让有线状态下的键盘出现在列表里。
    // 排在具体型号之后：首页"支持范围"是按表顺序念的。
    DeviceProfile {
        vid: 0x25A7,
        pid: 0xFA70,
        brand: "RK",
        model: "RK98",
        kind: DeviceType::Keyboard,
        wired: false,
        driver: None,
        reason: Some(
            "这个 2.4G 接收器只暴露一个可写的命令字节，没有任何应答通道，也没有输出端点，\
             因此只能识别设备，读不到也无法修改键盘参数。",
        ),
    },
    DeviceProfile {
        vid: 0x258A,
        pid: 0x010C,
        brand: "BY Tech",
        model: "Gaming Keyboard",
        kind: DeviceType::Keyboard,
        wired: true,
        driver: None,
        reason: Some(
            "有线模式下键盘换成 BY Tech 自己的控制器，只有 feature 报表、没有可用的应答\
             通道，协议还没破解。想改灯效或按键，把接收器插上、键盘切到 2.4G 即可。",
        ),
    },
];

/// Lookup by VID/PID; `None` for devices outside the table.
pub fn profile(vid: u16, pid: u16) -> Option<&'static DeviceProfile> {
    PROFILES.iter().find(|d| d.vid == vid && d.pid == pid)
}

impl Kind {
    /// The HID collection the driver has to open.
    pub fn channel(self) -> (u16, u16) {
        match self {
            Kind::Rapoo => (0xFF00, 0x000E),
            Kind::Hidpp => (0xFF00, 0x0002),
        }
    }

    pub fn open(self, spec: DriverSpec, path: &[u16]) -> Result<Box<dyn Driver>, String> {
        match self {
            Kind::Rapoo => Ok(Box::new(Rapoo::open(path, spec.limits)?)),
            Kind::Hidpp => Ok(Box::new(Hidpp::open(path, spec.layout, spec.limits)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stages are separate fields: editing stage 2 must not swallow a pending
    /// edit of stage 5.
    #[test]
    fn dpi_stages_are_separate_fields() {
        assert!(!Cmd::Dpi(2, 800).same_field(&Cmd::Dpi(5, 800)));
        assert!(Cmd::Dpi(2, 800).same_field(&Cmd::Dpi(2, 1600)));
    }

    /// Press and release debounce share no field even though they live in the
    /// same EEPROM block.
    #[test]
    fn debounce_edges_are_separate_fields() {
        assert!(!Cmd::Debounce(8, true).same_field(&Cmd::Debounce(8, false)));
    }

    /// 型号差异全部落在 profile 里：协议、布局、范围都能从同一处查表。
    #[test]
    fn profiles_carry_driver_spec_and_limits() {
        let rapoo = profile(0x24AE, 0x1214).expect("VT1 Pro profile");
        let spec = rapoo.driver.expect("writable");
        assert_eq!(spec.kind, Kind::Rapoo);
        assert_eq!(spec.limits.dpi, (100, 26000));
        assert_eq!(spec.limits.polling, &[8, 4, 2, 1]);

        let logitech = profile(0x046D, 0xC53F).expect("Lightspeed profile");
        let spec = logitech.driver.expect("writable");
        assert_eq!(spec.kind, Kind::Hidpp);
        assert_eq!(spec.layout, Some(HidppLayout::G304));
        assert_eq!(spec.limits.dpi, (200, 12000));

        assert!(profile(0x25A7, 0xFA70).is_some_and(|p| p.driver.is_none()));
    }

    /// 每台设备都要有明确的形态，首页图标不能退化成“未知设备”。
    #[test]
    fn profiles_declare_device_type() {
        assert_eq!(
            profile(0x24AE, 0x1214).map(|p| p.kind),
            Some(DeviceType::Mouse)
        );
        assert_eq!(
            profile(0x24AE, 0x4414).map(|p| p.kind),
            Some(DeviceType::Mouse)
        );
        assert_eq!(
            profile(0x046D, 0xC53F).map(|p| p.kind),
            Some(DeviceType::Receiver)
        );
        for (vid, pid) in [(0x3554u16, 0xFA09u16), (0x25A7, 0xFA70), (0x258A, 0x010C)] {
            assert_eq!(
                profile(vid, pid).map(|p| p.kind),
                Some(DeviceType::Keyboard)
            );
        }
    }

    /// 插线是另一个 PID：不登记它，用户拔掉接收器改用线连时设备会整个从
    /// 列表里消失（`hid::enumerate` 只保留表里的设备）。
    #[test]
    fn wired_variants_are_registered_next_to_the_receiver() {
        let wireless = profile(0x24AE, 0x1214).expect("2.4G profile");
        let wired = profile(0x24AE, 0x4414).expect("wired profile");
        assert!(!wireless.wired);
        assert!(wired.wired);
        assert_eq!((wireless.brand, wireless.model), (wired.brand, wired.model));
        assert!(wired.driver.is_some_and(|s| s.kind == Kind::Rapoo));
        assert_eq!(wired.driver.map(|s| s.limits), Some(RAPOO_LIMITS));
    }

    /// 只有在名字里明确写了形态时才改判，接收器自己报的名字保持原样。
    #[test]
    fn announced_name_refines_the_device_type() {
        assert_eq!(
            DeviceType::from_name("G304 Lightspeed Wireless Gaming Mouse"),
            Some(DeviceType::Mouse)
        );
        assert_eq!(
            DeviceType::from_name("MX Keys Keyboard"),
            Some(DeviceType::Keyboard)
        );
        assert_eq!(DeviceType::from_name("Lightspeed 接收器"), None);
        assert_eq!(DeviceType::from_name("USB Receiver"), None);
    }

    /// A mouse whose firmware only exposes DPI gets the narrower capability
    /// set; the UI relies on this to hide the rest. These are compile-time
    /// constants, so they are checked at compile time (runtime asserts would
    /// just trip clippy's `assertions_on_constants`).
    const _: () = {
        assert!(Caps::MOUSE_DPI.dpi);
        assert!(!Caps::MOUSE_DPI.dpi_stages);
        assert!(!Caps::MOUSE_DPI.polling);
        assert!(!Caps::MOUSE_DPI.sensor);
        assert!(Caps::MOUSE_FULL.sensor);
    };
}
