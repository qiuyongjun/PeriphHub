//! Protocol drivers.
//!
//! Every supported family speaks a different vendor protocol, but everything
//! above this module only ever sees [`Cmd`] and [`Snapshot`]. A driver is owned
//! by one worker thread (see `devices::real_worker`), so it can hold raw HID
//! handles and cached registers without any locking.
//!
//! The shared model is the **mouse** configuration domain: [`Cmd`],
//! [`Snapshot`], [`Caps`] and [`Limits`] describe DPI, polling and sensor
//! tuning. Keyboard profiles are read-only and never get a driver; adding
//! keyboard lighting/keymap settings requires a separate settings model, not
//! more fields here.

pub mod hidpp;
pub mod rapoo;
mod transport;

use std::time::Instant;

pub use hidpp::{Hidpp, HidppLayout};
pub use rapoo::{Rapoo, RapooLayout};

/// 写入在协议层的阶段结果。调用方据此决定是否还要做只读确认。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// 设备已经是目标值，一个字节都没写。
    Unchanged,
    /// 驱动已完成本批写入并更新 `snap`；协议允许时已做回读校验。
    ///
    /// 若驱动还有异步落盘的部分（例如 HID++ 攒着默认档位一起写扇区），
    /// 落盘结果由 `Snapshot::save_error` 单独表达，不在这里假装已经持久化；
    /// 代价是不会再走 `confirm`。这类驱动必须让 `confirm` 返回 `ReadOnly`。
    Confirmed,
    /// 写入与提交已完成，但按协议要等通信保护间隔后再只读确认。
    Committed,
}

/// 写入失败的结构化原因。调用方不能把所有失败都当成"重试一次写入"：
/// 已经可能落盘的失败重放会多写一次 EEPROM，而未开始的失败又必须纠正界面。
///
/// “提交已确认、回读暂时失败”不在这里：那是 [`WriteOutcome::Committed`]
/// 之后的异步确认路径，由采集线程只重试读取。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// 尚未开始写入：参数/能力校验失败，或准备阶段的读取失败。设备状态未变。
    Rejected(String),
    /// 写入或提交途中失败/应答超时，是否落盘未知。
    Unconfirmed(String),
}

/// 只读确认的结果。`snap` 不论哪种成功结果都应按设备实际值更新。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmResult {
    /// 回读值与本次写入目标一致。
    Verified,
    /// 回读成功，但设备值不等于写入目标（设备没有持久化）。
    Mismatch(String),
    /// 没有待确认的写入目标，只刷新了设备实际配置。
    ///
    /// 驱动若不做提交后比对（写入路径已自校验，或落盘是异步的），必须返回
    /// 这个值而不是 `Verified`：采集线程据此不会把本批命令标成已确认。
    ReadOnly,
}

/// Which **mouse** blocks a device really has. The UI hides everything that is
/// `false` instead of rendering switches that would do nothing on this model.
/// Keyboards are read-only and never reach this model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// A single DPI value that can be read and written.
    pub dpi: bool,
    /// Several switchable stages ([`Cmd::Activate`] has an effect).
    pub dpi_stages: bool,
    pub polling: bool,
    /// Motion sync, line/wave correction, debounce, sleep, lift-off distance.
    pub sensor: bool,
    /// The device reports a battery percentage.
    pub battery: bool,
}

impl Caps {
    /// A full gaming-mouse block: stages, polling rate and sensor tuning.
    pub const MOUSE_FULL: Caps = Caps {
        dpi: true,
        dpi_stages: true,
        polling: true,
        sensor: true,
        battery: true,
    };

    /// DPI only — the fallback when a mouse's onboard profile cannot be read.
    /// Battery is feature-derived for HID++, so the base value stays false.
    pub const MOUSE_DPI: Caps = Caps {
        dpi: true,
        dpi_stages: false,
        polling: false,
        sensor: false,
        battery: false,
    };

    /// Stages and report rate, both stored in the onboard profile; no
    /// lift-off/debounce knobs, because that firmware does not expose them.
    pub const MOUSE_STAGES: Caps = Caps {
        dpi: true,
        dpi_stages: true,
        polling: true,
        sensor: false,
        battery: true,
    };

    /// Whether this capability set allows a command. The UI already hides
    /// unsupported controls; drivers use this at the write boundary so a stale
    /// page or a programmatic caller cannot write a field the model lacks.
    pub fn supports(self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Refresh => true,
            Cmd::Polling(_) => self.polling,
            Cmd::Motion(_)
            | Cmd::Line(_)
            | Cmd::Wave(_)
            | Cmd::Debounce(..)
            | Cmd::Sleep(_)
            | Cmd::Lod(_) => self.sensor,
            Cmd::Activate(_) => self.dpi_stages,
            Cmd::Dpi(..) => self.dpi,
        }
    }
}

/// 鼠标配置快照。键盘/只读设备没有驱动，不会产生快照；新增键盘设置应另建
/// 设置模型，而不是往这里加字段。
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
    /// 周期落盘失败的原因。成功后清空；界面据此提示“设置仍在运行态生效，
    /// 但断电后会回到旧值”，而不是把失败只写进日志。
    pub save_error: Option<String>,
}

/// 鼠标配置命令。每条命令都携带绝对值，所以连续微调只需要每字段的最后一条。
/// 键盘只读，不接受这里的命令。
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

/// 打开一个协议适配器。`spec` 里同时带着该型号的协议、布局和取值范围，
/// 驱动不再把型号差异写死在代码里。
pub fn open(spec: DriverSpec, path: &[u16]) -> Result<Box<dyn Driver>, String> {
    spec.protocol.open(path, spec.limits)
}

pub trait Driver: Send {
    /// Reads the whole configuration. Called on open and every few seconds.
    fn snapshot(&mut self) -> Result<Snapshot, String>;

    /// Battery percentage, or `None` when the device does not report one.
    fn battery(&mut self) -> Result<Option<u8>, String>;

    /// 写入一个字段，并按协议能做到的程度更新 `snap`。
    ///
    /// 返回 [`WriteOutcome::Committed`] 表示已经提交、但还需要在
    /// [`Self::next_io_at`] 之后调用 [`Self::confirm`] 做只读校验。
    fn apply(&mut self, cmd: Cmd, snap: &mut Snapshot) -> Result<WriteOutcome, WriteError>;

    /// 执行一次合批。默认逐条执行，协议允许时由驱动覆盖成“读一次、写一次、
    /// 提交一次”，减少一轮输入里对同一寄存器块/扇区的重复读写。
    ///
    /// 默认实现必须保留 [`WriteOutcome::Committed`]：只实现 `apply` 的驱动
    /// 若返回 `Committed`，合批后仍需采集线程调用 `confirm`。遇到第一个错误
    /// 立刻停止——`Unconfirmed` 之后继续写可能重复擦写 EEPROM。
    ///
    /// 注意默认实现无法预校验：它逐条调用 `apply`，所以**可能已经写入前几条**
    /// 才返回 [`WriteError::Rejected`]。这不会让设备状态错乱（调用方收到
    /// `Rejected` 会重新读取实际配置），但与 `Rejected`“设备状态未变”的字面
    /// 约定不符。要保住这条约定，驱动得像 `hidpp`/`rapoo` 那样先把整批校验
    /// 完再动手。
    fn apply_batch(
        &mut self,
        cmds: &[Cmd],
        snap: &mut Snapshot,
    ) -> Result<WriteOutcome, WriteError> {
        let mut committed = false;
        let mut written = false;
        for cmd in cmds {
            match self.apply(*cmd, snap)? {
                WriteOutcome::Committed => {
                    committed = true;
                    written = true;
                }
                WriteOutcome::Confirmed => written = true,
                WriteOutcome::Unchanged => {}
            }
        }
        Ok(if committed {
            WriteOutcome::Committed
        } else if written {
            WriteOutcome::Confirmed
        } else {
            WriteOutcome::Unchanged
        })
    }

    /// 只读确认：读回设备当前配置，更新 `snap`，并比对上一次提交的目标值。
    ///
    /// 这条路径**不得**写入、补偿或落盘，否则一次后台恢复就可能改掉用户
    /// 没有编辑过的设置。HID++ 的 `snapshot()`/`battery()` 会补写待落档位，
    /// 因而不能拿来当确认。
    fn confirm(&mut self, snap: &mut Snapshot) -> Result<ConfirmResult, String>;

    /// 驱动要求的下一次可通信时刻（提交后的保护间隔）。
    ///
    /// 采集线程在等待期间继续接收、合并命令，而不是把 HID 调用卡在驱动里。
    fn next_io_at(&self) -> Option<Instant> {
        None
    }

    /// 关闭驱动前调用：把攒着、还没落盘的持久化状态提交掉。
    ///
    /// 默认实现没有待写内容。返回错误让退出路径能记录“这次没保存成功”，
    /// 而不是像周期落盘那样只写日志继续。
    fn flush(&mut self) -> Result<(), String> {
        Ok(())
    }

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

/// 具体协议适配器与它的型号布局。
///
/// `Kind` 只描述协议族；把"用哪个驱动"和"该型号的布局"绑在同一个值里，
/// 避免出现 `Kind::Rapoo` 配上未知布局这种运行时才发现的组合。Rapoo 必须
/// 带经过真机验证的 [`RapooLayout`]；HID++ 的 `None` 表示板载布局未验证，
/// 此时仍可通过 feature discovery 改当前 DPI、读电量，但不碰板载配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Rapoo(&'static RapooLayout),
    Hidpp(Option<&'static HidppLayout>),
}

impl Protocol {
    pub fn kind(self) -> Kind {
        match self {
            Protocol::Rapoo(_) => Kind::Rapoo,
            Protocol::Hidpp(_) => Kind::Hidpp,
        }
    }

    pub fn channel(self) -> (u16, u16) {
        self.kind().channel()
    }

    pub fn open(self, path: &[u16], limits: Limits) -> Result<Box<dyn Driver>, String> {
        match self {
            Protocol::Rapoo(layout) => Ok(Box::new(Rapoo::open(path, layout, limits)?)),
            Protocol::Hidpp(layout) => Ok(Box::new(Hidpp::open(path, layout, limits)?)),
        }
    }
}

/// 一个鼠标协议适配器需要的全部型号差异。键盘没有 `DriverSpec`；要加键盘
/// 配置，先建立独立的设置模型，再重新定义这条边界。
///
/// `PartialEq` 用于热插拔后的对比：同一个 container id 换了身份（插线 ↔ 接收
/// 器）时规格会变，旧 worker 必须重建而不是继续用着。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriverSpec {
    pub protocol: Protocol,
    pub limits: Limits,
}

/// 原始 LOD 编码 → 物理毫米的显示换算。
///
/// 写入仍然用原始编码；这里只回答"这个编码在界面上该显示几毫米"。G304 这
/// 类型号不开放 LOD，对应字段不会被用到；VT1 Pro 的编码 1..11 对应
/// 1.0..2.0 mm（0.9 + code / 10）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LodScale {
    /// 编码 0 对应的毫米数（十分之一毫米）。
    pub base_tenths: u16,
    /// 每增加一个编码增加的十分之一毫米数。
    pub step_tenths: u16,
}

impl LodScale {
    pub const UNKNOWN: LodScale = LodScale {
        base_tenths: 0,
        step_tenths: 0,
    };

    pub fn tenths(self, code: u8) -> u16 {
        self.base_tenths + code as u16 * self.step_tenths
    }
}

/// 界面允许写入的取值范围。字段是值编码，不是物理单位；驱动和 UI
/// 都以它为唯一边界，避免页面里再出现一套写死的 1..32 / 5..120。
/// `lod_scale` 只负责把编码显示成物理单位，不参与写入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub dpi: (u16, u16),
    pub polling: &'static [u8],
    pub debounce: (u8, u8),
    pub sleep: (u8, u8),
    pub lod: (u8, u8),
    pub lod_scale: LodScale,
    /// LOD 的推荐编码范围（含端点），只用于 UI 的建议标签；`None` 表示
    /// 这个型号不给建议。
    pub lod_recommended: Option<(u8, u8)>,
}

/// 只读/未知设备的占位范围；不会用于可编辑页面。
pub const UNKNOWN_LIMITS: Limits = Limits {
    dpi: (0, 0),
    polling: &[],
    debounce: (0, 0),
    sleep: (0, 0),
    lod: (0, 0),
    lod_scale: LodScale::UNKNOWN,
    lod_recommended: None,
};

impl Default for Limits {
    fn default() -> Self {
        UNKNOWN_LIMITS
    }
}

pub const VT1_PRO_LIMITS: Limits = Limits {
    dpi: (100, 26000),
    polling: &[8, 4, 2, 1],
    debounce: (1, 32),
    sleep: (5, 120),
    lod: (1, 11),
    lod_scale: LodScale {
        base_tenths: 9,
        step_tenths: 1,
    },
    // 编码 1..3 对应 1.0–1.2 mm，是 VT1 Pro 的常用抬鼠高度。
    lod_recommended: Some((1, 3)),
};

pub const G304_LIMITS: Limits = Limits {
    dpi: (hidpp::DPI_MIN, hidpp::DPI_MAX),
    polling: &[8, 4, 2, 1],
    // G304 的固件不开放这三项；数值保留，能力位会隐藏对应 UI。
    debounce: (1, 32),
    sleep: (5, 120),
    lod: (1, 11),
    lod_scale: LodScale::UNKNOWN,
    lod_recommended: None,
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
    /// 键盘目前一律为 `None`；配置域只覆盖鼠标（含承载鼠标的接收器）。
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
            protocol: Protocol::Rapoo(&rapoo::VT1_PRO),
            limits: VT1_PRO_LIMITS,
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
            protocol: Protocol::Rapoo(&rapoo::VT1_PRO),
            limits: VT1_PRO_LIMITS,
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
            protocol: Protocol::Hidpp(Some(&hidpp::G304)),
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
        // 文案写"这台设备"而不是分析某个具体型号：面板认不出、协议没接入
        // 就照实说，用户手里的可能只是尚未适配的型号。
        reason: Some("已识别到这台键盘，但它的配置协议尚未适配；灯效、按键等参数暂时无法读写，仅显示连接状态。"),
    },
    DeviceProfile {
        vid: 0x25A7,
        pid: 0xFA70,
        brand: "RK",
        model: "RK98",
        kind: DeviceType::Keyboard,
        wired: false,
        driver: None,
        reason: Some(
            "已识别到这台键盘，但它所在的接收器通道是单向的：读不到设备参数，也无法下发配置，仅显示连接状态。",
        ),
    },
    // 键盘插线后的身份：接收器是 3554:FA09，线连走的是键盘自己的控制器
    // （258A:010C，BY Tech）。厂商集合是 0xFF00 的 feature 报表，与接收器那套
    // 20 字节帧不同，所以仍然只读；登记它至少能让有线状态下的键盘出现在列表里。
    // 排在具体型号之后：首页"支持范围"是按表顺序念的。
    DeviceProfile {
        vid: 0x258A,
        pid: 0x010C,
        brand: "BY Tech",
        // 型号故意写通用名：只读探测（`cargo run --example probe -- 258a`）
        // 显示这个控制器的每条接口都自报 product="Gaming Keyboard"、
        // 厂商"BY Tech"、无序列号，而 F87 和 RK98 插线后都变成它。写具体
        // 型号只能是猜——猜错比不猜更糟，用户会以为面板认出了他的键盘。
        model: "通用有线键盘",
        kind: DeviceType::Keyboard,
        wired: true,
        driver: None,
        reason: Some(
            "设备以通用控制器模式接入，未上报具体型号，也没有发现可写的配置通道；仅显示连接状态。",
        ),
    },
];

/// Lookup by VID/PID; `None` for devices outside the table.
pub fn profile(vid: u16, pid: u16) -> Option<&'static DeviceProfile> {
    PROFILES.iter().find(|d| d.vid == vid && d.pid == pid)
}

impl Kind {
    /// The HID collection the protocol family lives on.
    pub fn channel(self) -> (u16, u16) {
        match self {
            Kind::Rapoo => (0xFF00, 0x000E),
            Kind::Hidpp => (0xFF00, 0x0002),
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
        assert_eq!(spec.protocol.kind(), Kind::Rapoo);
        let Protocol::Rapoo(layout) = spec.protocol else {
            panic!("VT1 Pro 必须带 Rapoo 布局");
        };
        assert_eq!(layout.model, "VT1 Pro");
        assert!(layout.supports(0x24AE, 0x1214));
        assert!(layout.supports(0x24AE, 0x4414));
        assert_eq!(spec.limits.dpi, (100, 26000));
        assert_eq!(spec.limits.polling, &[8, 4, 2, 1]);
        assert_eq!(
            spec.limits.lod_scale,
            LodScale {
                base_tenths: 9,
                step_tenths: 1
            }
        );
        assert_eq!(spec.limits.lod_recommended, Some((1, 3)));

        let logitech = profile(0x046D, 0xC53F).expect("Lightspeed profile");
        let spec = logitech.driver.expect("writable");
        assert_eq!(spec.protocol, Protocol::Hidpp(Some(&hidpp::G304)));
        assert_eq!(spec.limits.dpi, (200, 12000));

        assert!(profile(0x25A7, 0xFA70).is_some_and(|p| p.driver.is_none()));
    }

    /// 配置域目前只覆盖鼠标：带驱动的 profile 只能是鼠标，或以接收器身份
    /// 承载鼠标的设备；键盘必须保持只读并说明原因。要加键盘灯效/改键，先
    /// 建立独立的设置模型，再改这条不变量。
    #[test]
    fn keyboard_profiles_stay_read_only() {
        for p in PROFILES {
            if p.driver.is_some() {
                assert!(
                    matches!(p.kind, DeviceType::Mouse | DeviceType::Receiver),
                    "{} 带驱动但不是鼠标/接收器；配置域只覆盖鼠标",
                    p.model
                );
            }
            if p.kind == DeviceType::Keyboard {
                assert!(p.driver.is_none(), "{} 是键盘，必须保持只读", p.model);
                assert!(p.reason.is_some(), "{} 只读但没写原因", p.model);
            }
        }
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
        assert!(wired
            .driver
            .is_some_and(|s| s.protocol.kind() == Kind::Rapoo));
        assert_eq!(wired.driver.map(|s| s.limits), Some(VT1_PRO_LIMITS));
    }

    /// LOD 的物理单位换算是型号数据：VT1 Pro 的编码 1..11 是 1.0..2.0 mm。
    /// UI 只格式化，不再自己写 0.9 + code / 10。
    #[test]
    fn lod_scale_maps_the_model_codes() {
        let scale = VT1_PRO_LIMITS.lod_scale;
        assert_eq!(scale.tenths(0), 9);
        assert_eq!(scale.tenths(1), 10);
        assert_eq!(scale.tenths(11), 20);
        assert_eq!(UNKNOWN_LIMITS.lod_scale.tenths(5), 0);
        assert_eq!(Limits::default(), UNKNOWN_LIMITS);
    }

    /// 能力位必须和命令一一对应：UI 隐藏只是第一层，驱动在写入边界还要
    /// 挡住能力集不允许的命令。
    #[test]
    fn caps_gate_the_commands_they_claim() {
        assert!(Caps::MOUSE_FULL.supports(Cmd::Polling(1)));
        assert!(Caps::MOUSE_FULL.supports(Cmd::Motion(true)));
        assert!(Caps::MOUSE_FULL.supports(Cmd::Lod(2)));
        assert!(Caps::MOUSE_FULL.supports(Cmd::Dpi(0, 800)));
        assert!(Caps::MOUSE_FULL.supports(Cmd::Activate(1)));

        assert!(Caps::MOUSE_STAGES.supports(Cmd::Dpi(0, 800)));
        assert!(Caps::MOUSE_STAGES.supports(Cmd::Activate(1)));
        assert!(Caps::MOUSE_STAGES.supports(Cmd::Polling(1)));
        assert!(!Caps::MOUSE_STAGES.supports(Cmd::Motion(true)));
        assert!(!Caps::MOUSE_STAGES.supports(Cmd::Lod(2)));

        assert!(Caps::MOUSE_DPI.supports(Cmd::Dpi(0, 800)));
        assert!(!Caps::MOUSE_DPI.supports(Cmd::Activate(1)));
        assert!(!Caps::MOUSE_DPI.supports(Cmd::Polling(1)));
        assert!(!Caps::MOUSE_DPI.supports(Cmd::Motion(true)));

        assert!(Caps::MOUSE_DPI.supports(Cmd::Refresh));
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
        assert!(!Caps::MOUSE_DPI.battery);
        assert!(Caps::MOUSE_FULL.sensor);
        assert!(Caps::MOUSE_FULL.battery);
        assert!(Caps::MOUSE_STAGES.battery);
    };
}
