//! Logitech HID++ 2.0 over a Lightspeed receiver (046D:C53F).
//!
//! Frames are 20 bytes on the `0xFF00/0x0002` collection:
//! `11 devIdx featureIdx (func<<4 | swid) params…`. The receiver answers with
//! the same header; `featureIdx == 0xFF` marks an error frame.
//!
//! Only the *long* channel works here: on the 7-byte `0xFF00/0x0001`
//! collection the receiver just replies with HID++ 1.0 `0x8F` error frames.
//!
//! Settings come from two places:
//! * `0x1000` battery, `0x2201` the DPI the sensor is running *right now*;
//! * `0x8100` onboard profiles — the flash sector that holds the five DPI
//!   stages and the report rate, i.e. everything that survives a power cycle.
//!   Writing it means recomputing the sector's CRC16, see `write_sector`.
//!
//! The verified G304 sector layout lives in [`G304`]; other models use `None`
//! until their layout has been checked on real hardware. Raw bytes, CRCs and
//! the experiments behind them: `docs/logitech-g304.md`.

use std::time::{Duration, Instant};

use super::transport::{HidTransport, OverlappedHid};
use super::{Caps, Cmd, ConfirmResult, Driver, Limits, Snapshot, WriteError, WriteOutcome};

const REPORT_LEN: usize = 20;
const SW_ID: u8 = 0x0A;
const TIMEOUT_MS: u32 = 600;

/// Feature ids this driver knows. The index is resolved at open time: a device
/// reports a feature as "not present" by answering with index 0.
const F_DEVICE_NAME: u16 = 0x0005;
const F_BATTERY_LEVEL_STATUS: u16 = 0x1000;
const F_ADJUSTABLE_DPI: u16 = 0x2201;
const F_ONBOARD_PROFILES: u16 = 0x8100;

/// `G304`'s HERO sensor: 200–12000 DPI, the whole range the firmware accepts.
pub const DPI_MIN: u16 = 200;
pub const DPI_MAX: u16 = 12000;

/// 未验证型号的当前 DPI 写入范围。传感器上限未知，这里只给一个宽范围；
/// 写超范围时设备会拒绝/回读，不会碰板载配置。
const GENERIC_DPI_MIN: u16 = 100;
const GENERIC_DPI_MAX: u16 = 26000;

/// 一个经过真机验证的 HID++ 板载 profile 布局。
///
/// HID++ 的 feature 发现、报文、CRC 和分块传输是协议层；这里只放某型号
/// 扇区的结构：长度、尾读规则、profile 字段偏移、档位数、名称匹配和能力。
/// 未验证型号用 `None`，只能改当前 DPI，不碰扇区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidppLayout {
    /// 验证时的型号名，用于日志和"布局不匹配"的提示。
    pub model: &'static str,
    /// 设备自报名字里必须包含的片段（不区分大小写）。
    pub name_contains: &'static str,
    pub sector_len: usize,
    /// 尾部那次读取的起点。G304 拒绝从 240 起读，所以从 239 开始。
    pub tail_read_at: usize,
    /// 尾读开头有多少字节与上一块重复，读取时丢掉。
    pub tail_overlap: usize,
    /// 扇区读取的块大小（每次 `0x50` 读这么多载荷）。
    pub read_chunk: usize,
    /// 扇区写入的块大小（每次 `0x70` 写这么多载荷）。
    pub write_chunk: usize,
    /// Profile sector offsets (little endian), verified against the live sector.
    pub report_rate_at: usize,
    pub default_index_at: usize,
    pub dpi_at: usize,
    pub stages: usize,
    /// Blocks this model actually exposes with this layout.
    pub caps: Caps,
    /// 该固件是否把应答里的 `swid` 清零。G304 接收器实测返回 `0x00` 而不是
    /// 请求里的 `0x0A`，此时标准 HID++ 的 swid 校验必须关掉，只按设备索引
    /// 和功能索引匹配；其他型号保持严格校验，挡住接收器上别的软件的应答。
    pub zeroed_reply_swid: bool,
}

impl HidppLayout {
    pub fn supports_name(&self, name: &str) -> bool {
        name.to_ascii_lowercase()
            .contains(&self.name_contains.to_ascii_lowercase())
    }
}

/// G304 的板载布局。2026-10-31 在真机上验证：扇区 255 字节（`info` 报
/// `0x00FF`），末尾两字节 CRC16；设备拒绝从 240 起读，尾部从 239 读并
/// 丢掉重复的第一个字节。见 `docs/logitech-g304.md`。
pub const G304: HidppLayout = HidppLayout {
    model: "G304",
    name_contains: "g304",
    sector_len: 255,
    tail_read_at: 239,
    tail_overlap: 1,
    read_chunk: 16,
    write_chunk: 16,
    report_rate_at: 0,
    default_index_at: 1,
    dpi_at: 3,
    stages: 5,
    caps: Caps::MOUSE_STAGES,
    zeroed_reply_swid: true,
};

/// 把布局声明的能力和这次 feature discovery 的结果合起来。
///
/// 布局描述"这个型号理论上有什么"，feature 描述"这台设备实际应答了什么"；
/// 两者都成立才在界面上开放对应控件。未验证布局按 DPI-only 处理，电池始终
/// 由 `0x1000` 是否出现决定。
fn caps_for(layout: Option<&HidppLayout>, has_dpi: bool, has_battery: bool) -> Caps {
    let base = layout.map(|l| l.caps).unwrap_or(Caps::MOUSE_DPI);
    Caps {
        dpi: base.dpi && has_dpi,
        dpi_stages: base.dpi_stages && has_dpi,
        polling: base.polling,
        sensor: base.sensor,
        battery: has_battery,
    }
}

pub struct Hidpp {
    io: Box<dyn HidTransport>,
    dev_idx: u8,
    name: String,
    /// Feature indexes, resolved once. `None` = the device does not have it.
    f_name: Option<u8>,
    f_battery: Option<u8>,
    f_dpi: Option<u8>,
    f_onboard: Option<u8>,
    /// Sector holding the active profile, from the control sector's first entry.
    profile_sector: Option<u16>,
    /// 已验证的板载布局。接收器可以配对其他鼠标，而扇区偏移/CRC 是型号
    /// 专有的；`None` 表示未验证，只能改当前 DPI，不碰扇区。
    layout: Option<&'static HidppLayout>,
    limits: Limits,
    /// Last profile read: DPI stages, report rate, default index. Kept so a
    /// burst of edits does not re-read the whole sector per command.
    stages: Vec<u16>,
    /// 设备扇区里**实际存着**的默认档位。只有写成功了才会变。
    default_index: u8,
    /// 用户刚切到、还没落盘的档位。切档本身立刻作用于传感器，落盘攒到下一次
    /// 周期读取时做一次（见 `flush_default_stage`）。
    pending_default: Option<u8>,
    /// 扇区已写成功、但提交后的实时 DPI 没恢复回去时的目标值。
    ///
    /// 留到下一次周期轮询补偿，不在失败路径里重试：设备休眠或超时时，反复
    /// 读写只会把采集线程卡死在这一个错误上。
    pending_restore: Option<u16>,
    polling_ms: u8,
    /// 是否校验应答里的 swid。由型号的板载布局决定；未知型号沿用标准行为。
    match_swid: bool,
    /// 周期落盘失败后的重试状态：保留待保存档位，按指数退避再试，避免一次
    /// 休眠超时就把用户的选择丢掉，也避免对永久失败反复写 EEPROM。
    save_failures: u32,
    save_retry_at: Option<Instant>,
    save_error: Option<String>,
}

impl Hidpp {
    pub fn open(
        path: &[u16],
        layout: Option<&'static HidppLayout>,
        limits: Limits,
    ) -> Result<Hidpp, String> {
        Self::with_transport(
            Box::new(OverlappedHid::open(path, REPORT_LEN)?),
            layout,
            limits,
        )
    }

    /// 用任意传输实现构造驱动：生产走 `open`，测试注入假实现。
    fn with_transport(
        io: Box<dyn HidTransport>,
        layout: Option<&'static HidppLayout>,
        limits: Limits,
    ) -> Result<Hidpp, String> {
        let mut dev = Hidpp {
            io,
            dev_idx: 0,
            name: String::new(),
            f_name: None,
            f_battery: None,
            f_dpi: None,
            f_onboard: None,
            profile_sector: None,
            layout: None,
            limits,
            stages: Vec::new(),
            default_index: 0,
            pending_default: None,
            pending_restore: None,
            polling_ms: 1,
            match_swid: layout.map(|l| !l.zeroed_reply_swid).unwrap_or(true),
            save_failures: 0,
            save_retry_at: None,
            save_error: None,
        };
        dev.dev_idx = dev
            .find_device()
            .ok_or_else(|| "接收器上没有设备应答（鼠标可能已关机或休眠）".to_string())?;
        // 功能发现失败（超时/错误帧）不能当成“没有这个功能”：那是暂时性
        // 通信问题，交给上层重连；只有设备明确回索引 0 才算不支持。
        dev.f_name = dev.feature(F_DEVICE_NAME)?;
        dev.f_battery = dev.feature(F_BATTERY_LEVEL_STATUS)?;
        dev.f_dpi = dev.feature(F_ADJUSTABLE_DPI)?;
        dev.f_onboard = dev.feature(F_ONBOARD_PROFILES)?;
        if let Some(fi) = dev.f_name {
            dev.name = dev.read_name(fi)?;
        }
        // 只有布局存在、型号名匹配、扇区大小一致，并且真的找到了一个
        // 启用的 profile 扇区，才允许写板载配置。`None`（未验证布局）
        // 仍可改当前 DPI，但不碰扇区。
        let layout_ok = match layout {
            Some(l)
                if l.supports_name(&dev.name)
                    && dev.onboard_sector_size()? == Some(l.sector_len as u16) =>
            {
                Some(l)
            }
            _ => None,
        };
        let profile_sector = match layout_ok {
            Some(l) => dev.find_profile_sector(l)?,
            None => None,
        };
        let verified = layout_ok.filter(|_| profile_sector.is_some());
        dev.layout = verified;
        dev.profile_sector = profile_sector;
        if let Some(l) = verified {
            dev.load_profile(l).ok();
        } else {
            // 未验证型号没有可信的传感器范围；当前 DPI 不是持久配置，
            // 给一个宽范围，让读取到的值仍能在编辑器里显示和微调。
            dev.limits.dpi = (GENERIC_DPI_MIN, GENERIC_DPI_MAX);
            if dev.f_onboard.is_some() {
                crate::log::line(
                    "WARN",
                    &format!(
                        "{} 的板载配置布局未验证，已限制为仅修改当前 DPI",
                        if dev.name.is_empty() {
                            "未知型号"
                        } else {
                            &dev.name
                        }
                    ),
                );
            }
        }
        Ok(dev)
    }

    /// Sends one request and waits for the answer with the same header.
    fn xfer(
        &mut self,
        frame: &[u8; REPORT_LEN],
        want_feature: u8,
        ms: u32,
    ) -> Result<Option<Vec<u8>>, String> {
        self.io.send(frame)?;
        let budget = Duration::from_millis(ms as u64);
        let start = Instant::now();
        loop {
            // 预算按"整个请求"算，而不是每次读都从头等一遍。只有预算真的用光
            // 才取消这次读：取消会让驱动丢掉已经到达的那一帧，能不用就不用。
            let left = budget.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            let Some(buf) = self.io.recv(left)? else {
                break;
            };
            // Errors carry feature index 0xFF; everything else echoes the
            // feature and function we asked for. 同一 feature 下不同功能的
            // 迟到应答（例如上一次读扇区）不能被当成当前请求的结果。
            //
            // SW_ID 是请求方自己的编号：接收器上可能同时有别家软件在说话，
            // 用它再把别人的应答挡掉一层（错误帧不比对：它只用来报错，
            // 宁可多接一条也不要漏掉真正的失败）。
            if buf.len() >= 4 && buf[1] == frame[1] {
                let is_error = buf[2] == 0xFF;
                let same_func = (buf[3] & 0xF0) == (frame[3] & 0xF0);
                let same_sw = !self.match_swid || (buf[3] & 0x0F) == (frame[3] & 0x0F);
                if is_error || (buf[2] == want_feature && same_func && same_sw) {
                    return Ok(Some(buf));
                }
            }
        }
        Ok(None)
    }

    fn call_within(
        &mut self,
        feature: u8,
        func: u8,
        params: &[u8],
        ms: u32,
    ) -> Result<Option<Vec<u8>>, String> {
        let f = build_frame(self.dev_idx, feature, func, params);
        let r = self.xfer(&f, feature, ms)?;
        match r {
            Some(r) if r.get(2) == Some(&0xFF) => Err(format!(
                "设备返回错误 0x{:02X}",
                r.get(5).copied().unwrap_or_default()
            )),
            other => Ok(other),
        }
    }

    fn call(&mut self, feature: u8, func: u8, params: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.call_within(feature, func, params, TIMEOUT_MS)
    }

    /// Finds the device index the mouse answers on (1 for a single paired
    /// device, but a receiver can hold several).
    fn find_device(&mut self) -> Option<u8> {
        for idx in 1..=6u8 {
            self.dev_idx = idx;
            // A sleeping mouse simply never answers, so pings stay short: six
            // silent indices must not stall the reconnect for seconds.
            if let Ok(Some(r)) = self.call_within(0x00, 0, &[], 250) {
                if r[2] != 0xFF {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Feature index of `id`, or `None` when the device explicitly says it does
    /// not have it. 通信失败返回 `Err`：超时和错误帧是暂时性问题，不能让它
    /// 变成“本次会话永远没有 DPI/回报率/电量”。
    fn feature(&mut self, id: u16) -> Result<Option<u8>, String> {
        let Some(r) = self.call(0x00, 0, &id.to_be_bytes())? else {
            return Err(format!("查询功能 0x{id:04X} 超时"));
        };
        match r.get(4).copied() {
            // 索引 0 = 设备明确表示没有这个功能。
            Some(0) => Ok(None),
            Some(idx) => Ok(Some(idx)),
            // 短帧是协议异常，不能当成“不支持”缓存一整个会话。
            None => Err(format!("查询功能 0x{id:04X} 的应答过短")),
        }
    }

    /// 板载配置信息页里的扇区大小（G304 为 0x00FF）。读不到或不是 255 时
    /// 不能套用本驱动的扇区布局。超时向上传播，避免一次休眠把布局降级。
    fn onboard_sector_size(&mut self) -> Result<Option<u16>, String> {
        let Some(fi) = self.f_onboard else {
            return Ok(None);
        };
        let Some(r) = self.call(fi, 0, &[])? else {
            return Err("读取板载配置信息超时".into());
        };
        if r.len() < 13 {
            return Ok(None);
        }
        Ok(Some(u16::from_be_bytes([r[11], r[12]])))
    }

    /// The name arrives three characters at a time. This firmware treats the
    /// offset as a *character* index, so chunks are 3 apart (see docs).
    ///
    /// 短帧在这里是**协议异常**，不是“名字到此为止”：设备偶尔会在上一帧
    /// 还没发完时插进来一帧。静默当作结束会返回空名或截断名，而名字是布局
    /// 验证的唯一依据——验证不过就悄悄降级成“只能改当前 DPI”，一次通信
    /// 抖动换来整个会话不能写扇区。所以先立刻重试一次（偶发情况很便宜地
    /// 就能救回来），仍然短就按 `feature()` 的口径向上报错。
    fn read_name(&mut self, feature: u8) -> Result<String, String> {
        let mut head = self.call(feature, 0, &[])?;
        if head.as_ref().is_some_and(|r| r.get(4).is_none()) {
            head = self.call(feature, 0, &[])?;
        }
        let Some(head) = head else {
            return Err("读取设备名超时".into());
        };
        // 长度 0 是设备明确说“没有名字”（和 `feature()` 里的索引 0 同理），
        // 属于有效答复；帧短到没有这个字节才是异常。
        let Some(&len) = head.get(4) else {
            return Err("读取设备名的应答过短".into());
        };
        let len = len as usize;
        let mut bytes = Vec::with_capacity(len);
        let mut off = 0usize;
        while off < len {
            let mut r = self.call(feature, 1, &[off as u8])?;
            if r.as_ref().is_some_and(|r| r.get(4..7).is_none()) {
                r = self.call(feature, 1, &[off as u8])?;
            }
            let Some(r) = r else {
                return Err("读取设备名超时".into());
            };
            // 截断的分片同样过不了 `supports_name`，结果一样是静默降级，
            // 只是更难查，所以不 break，直接报错。
            let Some(chunk) = r.get(4..7) else {
                return Err("读取设备名的分片应答过短".into());
            };
            bytes.extend_from_slice(chunk);
            off += 3;
        }
        let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..end]).trim().to_string())
    }

    fn read_dpi(&mut self, feature: u8) -> Result<u16, String> {
        let r = self
            .call(feature, 2, &[0])?
            .ok_or_else(|| "读取 DPI 超时".to_string())?;
        if r.len() < 7 {
            return Err("DPI 应答过短".into());
        }
        Ok(u16::from_be_bytes([r[5], r[6]]))
    }

    // ---- onboard profiles -------------------------------------------------

    /// Reads the control sector and returns the sector of the first enabled
    /// profile. The table is `sector(2, BE) + enabled + pad`, ended by 0xFFFF.
    /// 读失败向上传播：暂时性通信错误不能让布局在整个会话里降级。
    fn find_profile_sector(&mut self, layout: &HidppLayout) -> Result<Option<u16>, String> {
        let Some(fi) = self.f_onboard else {
            return Ok(None);
        };
        let ctrl = self.read_sector(layout, fi, 0)?;
        for entry in ctrl.chunks(4) {
            if entry.len() < 4 || entry[0] == 0xFF {
                break;
            }
            if entry[2] != 0 {
                return Ok(Some(u16::from_be_bytes([entry[0], entry[1]])));
            }
        }
        Ok(None)
    }

    /// One sector: `read_chunk`-byte reads, then a tail read at
    /// `tail_read_at` whose first `tail_overlap` bytes repeat the previous
    /// block. The G304 refuses to start a read at 240, hence 239 + overlap 1.
    fn read_sector(
        &mut self,
        layout: &HidppLayout,
        fi: u8,
        sector: u16,
    ) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(layout.sector_len);
        for off in sector_read_offsets(layout) {
            let tail = off == layout.tail_read_at;
            let timeout = if tail {
                "读取配置扇区尾部超时"
            } else {
                "读取配置扇区超时"
            };
            let r = self
                .call(
                    fi,
                    5,
                    &[hi(sector), lo(sector), hi(off as u16), lo(off as u16)],
                )?
                .ok_or_else(|| timeout.to_string())?;
            // 尾读开头与上一块重叠的字节已经在 `out` 里了。
            let from = if tail { 4 + layout.tail_overlap } else { 4 };
            let short = if tail {
                "配置扇区尾部应答过短"
            } else {
                "配置扇区应答过短"
            };
            out.extend_from_slice(r.get(from..REPORT_LEN).ok_or(short)?);
        }
        debug_assert_eq!(out.len(), layout.sector_len);
        Ok(out)
    }

    /// startWrite → `write_chunk`-byte blocks → commit. The sector is only
    /// accepted when its CRC16 (big endian, last two bytes) covers everything
    /// before it.
    fn write_sector(
        &mut self,
        layout: &HidppLayout,
        fi: u8,
        sector: u16,
        data: &[u8],
    ) -> Result<(), String> {
        let n = data.len();
        if n == 0 {
            return Err("扇区数据为空".into());
        }
        let start = [hi(sector), lo(sector), 0, 0, hi(n as u16), lo(n as u16)];
        self.call(fi, 6, &start)?
            .ok_or_else(|| "开始写扇区失败".to_string())?;
        // 末块可以短于块大小（255 = 15×16 + 15，实测被设备接受）。切块
        // 规则在 `block_ranges` 里单独钉死：以前的循环条件写成 `o < n - 1`，
        // 在 n % 16 == 1 时会把最后一个字节整个漏掉。
        for chunk in block_ranges(n, layout.write_chunk) {
            self.call(fi, 7, &data[chunk])?
                .ok_or_else(|| "写入配置块失败".to_string())?;
        }
        self.call(fi, 8, &[])?
            .ok_or_else(|| "提交配置扇区失败".to_string())?;
        Ok(())
    }

    fn load_profile(&mut self, layout: &HidppLayout) -> Result<(), String> {
        let (fi, sector) = match (self.f_onboard, self.profile_sector) {
            (Some(fi), Some(s)) => (fi, s),
            _ => return Err("设备没有板载配置".into()),
        };
        let data = self.read_sector(layout, fi, sector)?;
        let profile = parse_profile(&data, layout);
        self.default_index = profile.default_index;
        self.polling_ms = profile.polling_ms;
        self.stages = profile.stages;
        Ok(())
    }

    /// Reads the sector, lets `edit` change it, and writes it back only when
    /// something actually differs — every write costs an erase cycle.
    fn update_profile<F>(&mut self, layout: &HidppLayout, edit: F) -> Result<(), String>
    where
        F: FnOnce(&mut [u8]) -> bool,
    {
        let (fi, sector) = match (self.f_onboard, self.profile_sector) {
            (Some(fi), Some(s)) => (fi, s),
            _ => return Err("设备没有板载配置".into()),
        };
        let mut data = self.read_sector(layout, fi, sector)?;
        // 写入是整扇区覆盖：读回的数据一旦损坏，重算 CRC 只会把它“洗白”，
        // 再把坏数据连同其他字段一起写进设备。所以先验证原 CRC，不匹配就
        // 拒绝这次写入。
        if !sector_crc_valid(&data) {
            return Err("配置扇区 CRC 校验失败，已拒绝写入".into());
        }
        // 提交后回读要拿它做差异定位：只重读真正改动过的那几块。
        let before = data.clone();
        let mut changed = edit(&mut data);
        // 提交整扇区会让固件按扇区里的默认档位重新加载传感器；用户刚切到、
        // 还没落盘的档位必须一起写进去，否则这次提交（例如改回报率）会把
        // 实时 DPI 顶回旧档位，界面却还显示新档位。
        let carried = carry_pending_stage(&mut data, self.pending_default, layout.default_index_at);
        if carried.is_some() {
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        // 上一次提交只写成功、DPI 没恢复时，先补完再叠加新的扇区写入；
        // 否则这次会以错误的实时值为基准，把错误状态固化下来。
        self.settle_pending_restore();
        if self.pending_restore.is_some() {
            return Err("上一次写入后的 DPI 尚未恢复，请稍后重试".into());
        }
        // 物理按键切档不经过 pending_default；提交整扇区会让固件按旧默认
        // 档位重载传感器。先记下实时 DPI，写完放回去，否则改回报率这类
        // 无关修改会顺带把用户刚切的档位顶掉。
        //
        // 读不到实时 DPI 就不知道这次提交会不会改掉传感器：宁可拒绝写入，
        // 也不能在运行态未知的情况下提交扇区还报成功。
        let live = match self.f_dpi {
            Some(fi) => Some(self.read_dpi(fi)?),
            None => None,
        };
        stamp_crc(&mut data);
        // 提交的应答可能就丢在这一步：扇区也许已经写进去了，固件也按扇区里的
        // 默认档位重载了传感器。这个提前返回既不能假装成功，也不能把恢复目标
        // 一起丢掉——否则设备停在默认档位的 DPI 上，界面却还显示用户刚选的
        // 那一档，后面的轮询也没有可补偿的目标。
        if let Err(e) = self.write_sector(layout, fi, sector, &data) {
            if let Some(live) = live {
                if let Err(restore) = self.defer_live_dpi(live) {
                    return Err(format!("{e}；恢复实时 DPI 失败：{restore}"));
                }
            }
            return Err(e);
        }
        // 每一步都拿到应答只说明命令到了接收器，不说明扇区真被改写：被吞掉
        // 的 commit 会收到上一条应答（`zeroed_reply_swid` 的固件连应答归属都
        // 分不清）。这里回读改动块，用设备自己的数据判定落盘。
        let readback = self.verify_sector(layout, fi, sector, &before, &data);
        if readback.is_ok() {
            if let Some(idx) = carried {
                self.default_index = idx;
                self.pending_default = None;
                // 档位已随本次扇区写入落盘，之前的“尚未保存”失败状态一并清掉。
                self.clear_save_failure();
            }
        }
        // 扇区没落盘也照样要补偿传感器：commit 可能已经让固件按扇区里的
        // 默认档位重载过，放着不管会把用户当前的档位顶掉。
        let mut error = readback.err();
        if let Some(live) = live {
            if let Err(e) = self.defer_live_dpi(live) {
                // 扇区已经写进去了，不能回报“完全成功”；目标值留在
                // `pending_restore` 里交给下一次轮询补偿，同时把部分成功
                // 告诉调用方纠正界面。
                error.get_or_insert(format!("配置已写入，但恢复 DPI 失败：{e}"));
            }
        }
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 提交后回读本次改动覆盖的读块，逐字节比对。
    ///
    /// 只读改动块而不是整扇区：整扇区要 15~16 次往返，而改一个档位通常只动
    /// 一块（改 DPI 会同时动 X/Y 各一块）。读块与 `read_sector` 完全同一套
    /// 对齐——非尾块是 `read_chunk` 的整数倍，尾块从 `tail_read_at` 读、丢掉
    /// 与前一块重叠的 `tail_overlap` 字节。
    fn verify_sector(
        &mut self,
        layout: &HidppLayout,
        fi: u8,
        sector: u16,
        before: &[u8],
        after: &[u8],
    ) -> Result<(), String> {
        let tail_start = layout.tail_read_at + layout.tail_overlap;
        // 末尾两字节是本函数自己重算的 CRC，内容一变它必然跟着变。校验它只会
        // 每次都多读一个尾块，而内容块能对上就说明这次提交整体生效了。
        let content_len = after.len().saturating_sub(2);
        let mut checked: Vec<usize> = Vec::new();
        for (i, (&old, &new)) in before.iter().zip(after).take(content_len).enumerate() {
            if old == new {
                continue;
            }
            // (读起点, 应答里的载荷起点, 载荷覆盖的扇区起点, 覆盖长度)
            let (read_at, from, start, len) = if i < tail_start {
                let at = i / layout.read_chunk * layout.read_chunk;
                (at, 4, at, (REPORT_LEN - 4).min(layout.sector_len - at))
            } else {
                (
                    layout.tail_read_at,
                    4 + layout.tail_overlap,
                    tail_start,
                    layout.sector_len - tail_start,
                )
            };
            if checked.contains(&read_at) {
                continue;
            }
            checked.push(read_at);
            let r = self
                .call(
                    fi,
                    5,
                    &[
                        hi(sector),
                        lo(sector),
                        hi(read_at as u16),
                        lo(read_at as u16),
                    ],
                )?
                .ok_or_else(|| format!("写入后回读扇区 0x{read_at:04X} 超时"))?;
            let got = r
                .get(from..from + len)
                .ok_or_else(|| "写入后回读扇区应答过短".to_string())?;
            let want = &after[start..start + len];
            if got != want {
                let at = got
                    .iter()
                    .zip(want)
                    .position(|(a, b)| a != b)
                    .unwrap_or_default();
                return Err(format!(
                    "写入未落盘：回读 0x{:04X} 得到 {:02X}，目标是 {:02X}",
                    start + at,
                    got[at],
                    want[at]
                ));
            }
        }
        Ok(())
    }

    /// 补偿上一次“扇区已写、DPI 未恢复”的提交。
    ///
    /// 只在周期轮询里重试，不在失败路径里循环。设备休眠时读不到就保留目标，
    /// 等下一次唤醒；恢复成功或确认已经是目标值就清掉。
    fn settle_pending_restore(&mut self) {
        let Some(target) = self.pending_restore else {
            return;
        };
        let Some(fi) = self.f_dpi else {
            self.pending_restore = None;
            return;
        };
        match self.read_dpi(fi) {
            // 上次其实恢复成功了，只是回读失败。
            Ok(now) if now == target => self.pending_restore = None,
            Ok(_) => {
                // `set_current_dpi` 自己会回读比对，成功就等于已经恢复到位。
                if self.set_current_dpi(target).is_ok() {
                    self.pending_restore = None;
                }
            }
            // 设备暂不可达：保留目标，下一次轮询再试。
            Err(_) => {}
        }
    }

    /// 记下恢复目标并当场尽力恢复。
    ///
    /// 目标先落进 `pending_restore` 再动手：提交已经发生、设备可能已经换了
    /// 档位，这一步失败也不能把"该恢复成多少"忘掉，否则只能等用户自己发现。
    /// 当场恢复成功才清掉它。
    fn defer_live_dpi(&mut self, live: u16) -> Result<(), String> {
        self.pending_restore = Some(live);
        match self.restore_live_dpi(live) {
            Ok(()) => {
                self.pending_restore = None;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 用户明确下的一条实时 DPI 指令。
    ///
    /// 旧补偿目标先作废再写：写成功自不必说；写成功但回读超时（结果未知）
    /// 时更不能留——那条旧目标会在下一次轮询把设备改回去，等于吞掉刚发出、
    /// 很可能已经生效的指令。真实值交给只读确认去读。
    fn set_dpi_from_user(&mut self, value: u16) -> Result<(), String> {
        self.pending_restore = None;
        self.set_current_dpi(value)
    }

    /// 提交扇区后把传感器放回提交前的实时值，并回读确认。
    fn restore_live_dpi(&mut self, live: u16) -> Result<(), String> {
        let Some(fi) = self.f_dpi else {
            return Ok(());
        };
        if self.read_dpi(fi)? == live {
            return Ok(());
        }
        self.set_current_dpi(live)
    }

    /// 把用户最后选中的档位写回设备扇区。
    ///
    /// 只在"下一次周期读取"时调用，而不是每次点档位都调：写一次意味着读回
    /// 255 字节 + 擦写 + 重算 CRC + commit，而连续切档是查看行为的常态。
    /// 代价是断电前最后几次切换可能没落盘，设备会回到旧档位——运行态不受影响。
    fn flush_default_stage(&mut self, layout: &HidppLayout) -> Result<(), String> {
        let Some(idx) = self.pending_default else {
            // 没有待保存内容：上一次的失败提示也一并清掉。
            self.clear_save_failure();
            return Ok(());
        };
        let (Some(_), Some(_)) = (self.f_onboard, self.profile_sector) else {
            // 没有可写的板载配置：档位只存在于固件运行态。
            self.pending_default = None;
            self.clear_save_failure();
            return Ok(());
        };
        if idx == self.default_index {
            self.pending_default = None;
            self.clear_save_failure();
            return Ok(());
        }
        self.update_profile(layout, |data| {
            if data[layout.default_index_at] == idx {
                return false;
            }
            data[layout.default_index_at] = idx;
            true
        })?;
        self.default_index = idx;
        self.pending_default = None;
        self.clear_save_failure();
        Ok(())
    }

    fn clear_save_failure(&mut self) {
        self.save_failures = 0;
        self.save_retry_at = None;
        self.save_error = None;
    }

    /// 落盘失败保留待保存档位并退避重试：成功确认后才清空，否则退出时
    /// 还能补存；失败原因随快照回传，让界面能说明“只是还没落盘”。
    fn settle_default_stage(&mut self) {
        let Some(layout) = self.layout else {
            self.pending_default = None;
            self.clear_save_failure();
            return;
        };
        if self.pending_default.is_none() {
            return;
        }
        if self.save_retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if let Err(e) = self.flush_default_stage(layout) {
            self.save_failures = self.save_failures.saturating_add(1);
            // 5s、10s、20s、40s，之后每分钟一次；设备休眠时不要每轮都写 EEPROM。
            let shift = self.save_failures.saturating_sub(1).min(4);
            let delay = Duration::from_secs((5u64 << shift).min(60));
            self.save_retry_at = Some(Instant::now() + delay);
            if self.save_error.as_deref() != Some(e.as_str()) {
                crate::log::line("WARN", &format!("保存默认档位失败：{e}"));
            }
            self.save_error = Some(format!("默认档位尚未保存：{e}"));
        }
    }

    /// 退出路径的严格版本：错误交给调用方记录，不在这里吞掉。
    fn flush_default_stage_checked(&mut self) -> Result<(), String> {
        let Some(layout) = self.layout else {
            self.pending_default = None;
            self.clear_save_failure();
            return Ok(());
        };
        self.flush_default_stage(layout)
    }

    fn current_caps(&self) -> Caps {
        caps_for(self.layout, self.f_dpi.is_some(), self.f_battery.is_some())
    }

    fn set_current_dpi(&mut self, value: u16) -> Result<(), String> {
        let fi = self
            .f_dpi
            .ok_or_else(|| "该设备不支持调整 DPI".to_string())?;
        let b = value.to_be_bytes();
        // X and Y are separate fields; write both so a device that really
        // splits the axes cannot end up with mismatched halves.
        //
        // 没有应答就是没写进去：鼠标休眠时这是常态，不能当作成功——界面上
        // 会留下一个设备其实没有的值，用户再点下一步就是基于假值算的。
        self.call(fi, 3, &[0, b[0], b[1], b[0], b[1]])?
            .ok_or_else(|| "设置 DPI 无应答".to_string())?;
        // 有应答也只说明命令到了接收器：接收器会吞掉写命令、照样回上一条
        // 应答。传感器到底改没改只有回读能答，否则界面显示 1600、设备还是
        // 800，下一次点档位就是基于假值算的。
        let now = self.read_dpi(fi)?;
        if now != value {
            return Err(format!("回读 DPI 为 {now}，期望 {value}"));
        }
        Ok(())
    }
}

/// 一帧长报文：`11 devIdx feature (func<<4|swid) params…`。
///
/// 参数超过 16 字节时截断，而不是越界 panic：长帧只有这么多载荷。
fn build_frame(dev_idx: u8, feature: u8, func: u8, params: &[u8]) -> [u8; REPORT_LEN] {
    let mut f = [0u8; REPORT_LEN];
    f[0] = 0x11;
    f[1] = dev_idx;
    f[2] = feature;
    f[3] = (func << 4) | SW_ID;
    for (i, b) in params.iter().take(REPORT_LEN - 4).enumerate() {
        f[4 + i] = *b;
    }
    f
}

/// 扇区读取的起始偏移：从 0 开始每 `read_chunk` 字节一次，再补一次尾读。
/// G304 拒绝从 240 起读，尾部 15 字节靠与上一块重叠一个字节取回。
fn sector_read_offsets(layout: &HidppLayout) -> impl Iterator<Item = usize> {
    (0..=layout.tail_read_at)
        .step_by(layout.read_chunk)
        .chain(std::iter::once(layout.tail_read_at))
}

/// 写扇区时的分块（末块可短）。
///
/// 单独抽出来是因为边界条件曾经写错过：`o < n - 1` 在 `n % 16 == 1` 时会
/// 漏掉最后一个字节。现在有测试直接钉住 1 / 16 / 17 / 255 这几种长度。
fn block_ranges(n: usize, chunk: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    (0..n).step_by(chunk).map(move |o| o..(o + chunk).min(n))
}

/// 重算并写入扇区尾部的 CRC16（大端）。
fn stamp_crc(data: &mut [u8]) {
    let crc_at = data.len() - 2;
    let crc = crc16(&data[..crc_at]);
    data[crc_at..].copy_from_slice(&crc.to_be_bytes());
}

/// 整扇区提交会让固件按扇区里的默认档位重载传感器。把待落盘的档位并进
/// 这次要提交的数据，避免无关修改（如回报率）把它顶回旧档位。
/// 返回实际并入扇区的档位；扇区里已经是该档位时返回 `None`。
fn carry_pending_stage(
    data: &mut [u8],
    pending: Option<u8>,
    default_index_at: usize,
) -> Option<u8> {
    let idx = pending?;
    if data[default_index_at] == idx {
        return None;
    }
    data[default_index_at] = idx;
    Some(idx)
}

/// 一条合批里对 profile 扇区的修改。扇区只读一次、写一次、提交一次。
enum SectorEdit {
    Byte(usize, u8),
    Le16(usize, u16),
}

/// 从 profile 扇区里解出可编辑字段。
struct Profile {
    stages: Vec<u16>,
    default_index: u8,
    polling_ms: u8,
}

fn parse_profile(data: &[u8], layout: &HidppLayout) -> Profile {
    debug_assert_eq!(data.len(), layout.sector_len);
    let mut stages = Vec::with_capacity(layout.stages);
    for i in 0..layout.stages {
        stages.push(u16::from_le_bytes([
            data[layout.dpi_at + i * 2],
            data[layout.dpi_at + i * 2 + 1],
        ]));
    }
    // A trailing 0 means "that stage is not configured".
    while stages.len() > 1 && stages.last() == Some(&0) {
        stages.pop();
    }
    Profile {
        stages,
        default_index: data[layout.default_index_at],
        // 0 会让 snapshot 里 `1000 / polling_ms` 除零。
        polling_ms: data[layout.report_rate_at].max(1),
    }
}

fn hi(v: u16) -> u8 {
    (v >> 8) as u8
}

fn lo(v: u16) -> u8 {
    (v & 0xFF) as u8
}

/// 扇区最后两字节（大端）必须覆盖前面的全部内容。读回的数据用这个判定，
/// 写回前不通过就不碰设备。
fn sector_crc_valid(data: &[u8]) -> bool {
    let crc_at = data.len() - 2;
    data[crc_at..] == crc16(&data[..crc_at]).to_be_bytes()
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF), the one Logitech's sectors
/// use. Verified against the live sector: stored bytes equal this value.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// 只读地读出一份配置快照。
///
/// 不能用于写后确认以外的“顺手补写”场景：这个函数本身不补 DPI、不落档位。
/// `prefer_pending` 只影响“当前档位”的推断，不会向设备发写命令。
fn read_snapshot(dev: &mut Hidpp, prefer_pending: bool) -> Result<Snapshot, String> {
    let caps = dev.current_caps();
    if let Some(layout) = dev.layout {
        dev.load_profile(layout)?;
    }
    let current = match dev.f_dpi {
        Some(fi) => dev.read_dpi(fi)?,
        None => *dev.stages.first().unwrap_or(&0),
    };
    let stages = if dev.stages.is_empty() {
        vec![current]
    } else {
        dev.stages.clone()
    };
    // The mouse's own DPI button moves the sensor without touching the
    // profile, so the active stage is whichever one matches right now.
    // Two stages can hold the same DPI, in which case the profile's own
    // default index is the better guess. 用户刚切、还没落盘的那一档优先：
    // 扇区里读回的还是旧值。
    let pending = if prefer_pending {
        dev.pending_default
    } else {
        None
    };
    let default = pending
        .map(|i| i as usize)
        .unwrap_or(dev.default_index as usize)
        .min(stages.len() - 1);
    let active = if stages.get(default) == Some(&current) {
        default
    } else {
        stages.iter().position(|&v| v == current).unwrap_or(default)
    };

    Ok(Snapshot {
        model: dev.name.clone(),
        caps,
        polling_ms: dev.polling_ms,
        lod: 0,
        motion: false,
        stage_count: stages.len() as u8,
        stage_active: active as u8,
        dpi: stages,
        dpi_min: dev.limits.dpi.0,
        dpi_max: dev.limits.dpi.1,
        debounce_press: 0,
        debounce_release: 0,
        sleep_min: 0,
        line_corr: false,
        wave_corr: false,
        save_error: dev.save_error.clone(),
    })
}

impl Driver for Hidpp {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        // 先补完上一次没恢复的 DPI，再把待落默认档位写掉：这样下面读到的
        // index、当前 DPI 都是同一次写入之后的状态，不会一会儿新一会儿旧。
        self.settle_pending_restore();
        self.settle_default_stage();
        read_snapshot(self, true)
    }

    fn battery(&mut self) -> Result<Option<u8>, String> {
        // 浮窗模式下全量读是停掉的，电量这条 20 秒的心跳就成了唯一会定期
        // 经过驱动的地方——待落的档位和没恢复的 DPI 都在这里补。
        self.settle_pending_restore();
        self.settle_default_stage();
        let Some(fi) = self.f_battery else {
            return Ok(None);
        };
        let Some(r) = self.call(fi, 0, &[])? else {
            return Ok(None);
        };
        // [level%, levelStatus, chargingStatus, externalPower]
        Ok(r.get(4).copied().map(|v| v.min(100)))
    }

    fn apply(&mut self, cmd: Cmd, snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
        self.apply_batch(&[cmd], snap)
    }

    /// 一轮输入里可能同时改多个档位/回报率。逐条执行会让每条命令各做一次
    /// 整扇区读 + 写 + 提交；这里合并成一次，顺序语义仍按命令先后：
    /// 切档立刻作用传感器，扇区提交后最后再把当前档位的 DPI 放回去。
    fn apply_batch(
        &mut self,
        cmds: &[Cmd],
        snap: &mut Snapshot,
    ) -> Result<WriteOutcome, WriteError> {
        let Some(layout) = self.layout else {
            // 未验证布局只能改当前 DPI，没有扇区可合批。先把整批校验完再动
            // 设备：`Rejected` 的契约是“设备状态未变”，先写了半批再报拒绝
            // 就变成了误导。
            for cmd in cmds {
                match *cmd {
                    Cmd::Refresh => {}
                    Cmd::Dpi(_, _) if self.f_dpi.is_some() => {}
                    Cmd::Dpi(_, _) => {
                        return Err(WriteError::Rejected("该设备不支持调整 DPI".into()))
                    }
                    Cmd::Polling(_) => {
                        return Err(WriteError::Rejected("该设备不支持调整回报率".into()))
                    }
                    _ => return Err(WriteError::Rejected("该设备不支持这个设置".into())),
                }
            }
            let mut applied = None;
            for cmd in cmds {
                if let Cmd::Dpi(_, v) = *cmd {
                    let v = v.clamp(self.limits.dpi.0, self.limits.dpi.1);
                    self.set_dpi_from_user(v).map_err(WriteError::Unconfirmed)?;
                    applied = Some(v);
                }
            }
            // 设备调用都成功后才更新快照：失败时 `snap` 必须还是上一份可信
            // 基准，否则下一次读改写会拿被污染的乐观值去算。
            if let Some(v) = applied {
                snap.dpi = vec![v];
                snap.stage_count = 1;
                snap.stage_active = 0;
                return Ok(WriteOutcome::Confirmed);
            }
            return Ok(WriteOutcome::Unchanged);
        };

        // 校验与执行分开：任何一条命令非法都要在写设备之前拒绝。
        for cmd in cmds {
            match *cmd {
                Cmd::Refresh | Cmd::Activate(_) => {}
                Cmd::Dpi(stage, _) => {
                    if self.f_dpi.is_none() {
                        return Err(WriteError::Rejected("该设备不支持调整 DPI".into()));
                    }
                    // `stage` comes from outside the driver; this is the
                    // protocol boundary, so reject an out-of-range index
                    // here instead of indexing past the sector.
                    if stage as usize >= layout.stages {
                        return Err(WriteError::Rejected(format!(
                            "dpi stage {stage} out of range (0..{})",
                            layout.stages
                        )));
                    }
                }
                Cmd::Polling(ms) => {
                    if !self.limits.polling.contains(&ms) {
                        return Err(WriteError::Rejected("该型号不支持这个回报率".into()));
                    }
                }
                // Everything else is hidden by the UI for this device.
                _ => return Err(WriteError::Rejected("该设备不支持这个设置".into())),
            }
        }

        let mut edits: Vec<SectorEdit> = Vec::new();
        let mut stages = self.stages.clone();
        // 快照先在副本上改，写入全部成功后再提交；中途 `Unconfirmed` 时
        // `snap` 保持上一份可信值。
        let mut dpi = snap.dpi.clone();
        let mut active = snap.stage_active;
        let mut polling = None;
        let mut post_commit_dpi = None;
        let mut sector_touched = false;

        for cmd in cmds {
            match *cmd {
                Cmd::Refresh => {}
                Cmd::Dpi(stage, v) => {
                    let v = v.clamp(self.limits.dpi.0, self.limits.dpi.1);
                    let idx = stage as usize;
                    edits.push(SectorEdit::Le16(layout.dpi_at + idx * 2, v));
                    if let Some(slot) = stages.get_mut(idx) {
                        *slot = v;
                    }
                    if let Some(slot) = dpi.get_mut(idx) {
                        *slot = v;
                    }
                    // Editing the stage in use should be felt immediately, not
                    // after the next power cycle. 提交会让固件重载默认档位，
                    // 所以这条实时值排在提交之后执行。
                    if idx == active as usize {
                        post_commit_dpi = Some(v);
                    }
                    sector_touched = true;
                }
                Cmd::Activate(i) => {
                    let idx = (i as usize).min(dpi.len().saturating_sub(1));
                    let Some(v) = dpi.get(idx).copied() else {
                        continue;
                    };
                    // 切档只动传感器，默认档位攒着一起落盘（见 flush_default_stage）。
                    // 这是一条新的用户指令：上一次没恢复成功的旧 DPI 目标必须
                    // 先作废，否则下一轮轮询会把它改回去，等于吞掉这次选择。
                    self.set_dpi_from_user(v).map_err(WriteError::Unconfirmed)?;
                    self.pending_default = Some(idx as u8);
                    active = idx as u8;
                    // 后面的提交会以这个实时值为基准恢复；除非还有更晚的
                    // Dpi 改当前档位，否则不要在提交后再改一次。
                    post_commit_dpi = None;
                }
                Cmd::Polling(ms) => {
                    edits.push(SectorEdit::Byte(layout.report_rate_at, ms));
                    polling = Some(ms);
                    sector_touched = true;
                }
                // 校验阶段已经挡掉了其余命令。
                _ => {}
            }
        }

        if sector_touched {
            self.update_profile(layout, |data| {
                let mut changed = false;
                for edit in &edits {
                    match *edit {
                        SectorEdit::Byte(at, v) => {
                            if data[at] != v {
                                data[at] = v;
                                changed = true;
                            }
                        }
                        SectorEdit::Le16(at, v) => {
                            let b = v.to_le_bytes();
                            if data[at..at + 2] != b {
                                data[at..at + 2].copy_from_slice(&b);
                                changed = true;
                            }
                        }
                    }
                }
                changed
            })
            .map_err(WriteError::Unconfirmed)?;
        }
        self.stages = stages;
        if let Some(ms) = polling {
            self.polling_ms = ms;
        }
        if let Some(v) = post_commit_dpi {
            self.set_dpi_from_user(v).map_err(WriteError::Unconfirmed)?;
        }
        // 设备侧到这里全部成功：现在才让快照反映新值。
        snap.dpi = dpi;
        snap.stage_active = active;
        if let Some(ms) = polling {
            snap.polling_ms = ms;
        }
        Ok(WriteOutcome::Confirmed)
    }

    /// HID++ 的写路径自己会回读扇区并恢复实时 DPI，因此平时不会走到这里。
    /// 真正需要只读确认时（例如采集线程要求重新读取实际配置），必须走
    /// `read_snapshot`，不能调用 `snapshot()`：后者会补写待落档位。
    ///
    /// HID++ 没有「提交后待比对的目标块」：扇区写入在 `update_profile` 里
    /// 就已完成，延迟落盘的默认档位由 `settle_default_stage` 异步补写，并
    /// 通过 `Snapshot::save_error` 单独上报。所以这里只能返回
    /// [`ConfirmResult::ReadOnly`]——它是只读刷新，绝不能冒称已确认持久化。
    fn confirm(&mut self, snap: &mut Snapshot) -> Result<ConfirmResult, String> {
        *snap = read_snapshot(self, false)?;
        Ok(ConfirmResult::ReadOnly)
    }

    fn link_alive(&self) -> bool {
        self.io.alive()
    }

    fn flush(&mut self) -> Result<(), String> {
        // 退出前能补的先补：没恢复的 DPI 会挡住后续扇区提交，必须排在
        // 待落档位之前。
        self.settle_pending_restore();
        if self.pending_restore.is_some() {
            return Err("DPI 尚未恢复，默认档位未提交".into());
        }
        self.flush_default_stage_checked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Control sector read from the G304: profile 1 at sector 1, then fill,
    /// then the CRC the device stored.
    fn shipped_sector() -> Vec<u8> {
        let mut sector = vec![0x00, 0x01, 0x01, 0x00, 0xFF, 0xFF, 0x00, 0x00];
        sector.resize(G304.sector_len - 2, 0xFF);
        sector.extend_from_slice(&[0x58, 0x7C]);
        sector
    }

    #[test]
    fn crc_matches_the_sector_the_device_ships_with() {
        // Reproducing the stored value proves the polynomial, the init value
        // and the byte order used by `write_sector`.
        let sector = shipped_sector();
        let crc = crc16(&sector[..sector.len() - 2]);
        assert_eq!(crc, 0x587C);
        assert_eq!(crc.to_be_bytes(), [0x58, 0x7C]);
    }

    /// 写回前先验原 CRC：坏数据不能被重算的校验和“洗白”后再覆盖设备。
    #[test]
    fn rejects_sectors_whose_stored_crc_does_not_match() {
        let good = shipped_sector();
        assert!(sector_crc_valid(&good));

        let mut flipped = good.clone();
        flipped[G304.dpi_at] ^= 0x01;
        assert!(!sector_crc_valid(&flipped));

        let crc_at = good.len() - 2;
        let mut swapped = good;
        swapped.swap(crc_at, crc_at + 1);
        assert!(!sector_crc_valid(&swapped));
    }

    #[test]
    fn dpi_round_trips_through_little_endian() {
        // Profile stores 1600 as 40 06; the sensor feature uses big endian.
        assert_eq!(1600u16.to_le_bytes(), [0x40, 0x06]);
        assert_eq!(1600u16.to_be_bytes(), [0x06, 0x40]);
    }

    #[test]
    fn clamp_keeps_hero_sensor_range() {
        assert_eq!(100u16.clamp(DPI_MIN, DPI_MAX), DPI_MIN);
        assert_eq!(20000u16.clamp(DPI_MIN, DPI_MAX), DPI_MAX);
        assert_eq!(3200u16.clamp(DPI_MIN, DPI_MAX), 3200);
    }

    /// 能力位必须同时受布局和 feature discovery 约束，不能因为“是鼠标”
    /// 就假设有 DPI/电量。没有 0x2201 时 `dpi=false`，UI 不会再放一个
    /// 写了也没效果的编辑框。
    #[test]
    fn caps_follow_feature_discovery() {
        let none = caps_for(None, false, false);
        assert!(!none.dpi);
        assert!(!none.dpi_stages);
        assert!(!none.polling);
        assert!(!none.sensor);
        assert!(!none.battery);

        let dpi_only = caps_for(None, true, true);
        assert!(dpi_only.dpi);
        assert!(!dpi_only.dpi_stages);
        assert!(!dpi_only.polling);
        assert!(dpi_only.battery);

        let full = caps_for(Some(&G304), true, true);
        assert_eq!(full, G304.caps);
        assert!(full.battery);

        let no_battery = caps_for(Some(&G304), true, false);
        assert!(no_battery.dpi_stages);
        assert!(!no_battery.battery);

        let no_dpi = caps_for(Some(&G304), false, true);
        assert!(!no_dpi.dpi);
        assert!(!no_dpi.dpi_stages);
        assert!(no_dpi.polling);
    }

    /// 接收器是通用的：只有 G304 的扇区布局验证过，其他型号必须降级。
    /// 同时钉住布局内部自洽，避免偏移/长度改错后测试仍然通过。
    #[test]
    fn g304_layout_is_internally_consistent() {
        let l = &G304;
        assert_eq!(l.model, "G304");
        assert!(l.supports_name("G304 Lightspeed Wireless Gaming Mouse"));
        assert!(l.supports_name("g304 lightspeed"));
        assert!(!l.supports_name("G502 HERO"));
        assert!(!l.supports_name(""));
        // 名称匹配对两边都不区分大小写，避免布局作者写成大写后静默失效。
        let mut upper = *l;
        upper.name_contains = "G304";
        assert!(upper.supports_name("g304 lightspeed"));

        assert!(l.sector_len > 2);
        assert!(l.tail_read_at < l.sector_len);
        // 尾读起点不能落在整块边界上：否则块序列与补尾重复读同一段，
        // 扇区长度会错，而 release 下没有 debug_assert 兜底。
        assert!(!l.tail_read_at.is_multiple_of(l.read_chunk));
        assert!(l.report_rate_at < l.sector_len);
        assert!(l.default_index_at < l.sector_len);
        assert!(l.dpi_at + l.stages * 2 <= l.sector_len);
        assert!(l.tail_overlap < REPORT_LEN - 4);
        assert!(l.read_chunk > 0);
        assert!(l.write_chunk > 0);
        assert_eq!(l.caps, Caps::MOUSE_STAGES);
        // 这台固件把应答的 swid 清零；匹配策略必须跟着型号走。
        assert!(l.zeroed_reply_swid);
        // 与真机扇区一致的边界：255 = 15×16 + 15。
        assert_eq!(l.sector_len, 255);
        assert_eq!(l.tail_read_at, 239);
        assert_eq!(l.stages, 5);
    }

    /// 报文头：`11 devIdx feature (func<<4 | swid)`，参数写在 `[4..20]`，
    /// 超长截断而不是越界。
    #[test]
    fn build_frame_encodes_header_and_truncates_params() {
        let f = build_frame(2, 0x21, 3, &[0xAA, 0xBB]);
        assert_eq!(&f[..4], &[0x11, 2, 0x21, (3 << 4) | SW_ID]);
        assert_eq!(&f[4..6], &[0xAA, 0xBB]);
        assert!(f[6..].iter().all(|&b| b == 0));

        let f = build_frame(1, 0x81, 7, &[0x7F; 20]);
        assert_eq!(f.len(), REPORT_LEN);
        assert!(f[4..].iter().all(|&b| b == 0x7F));
    }

    /// 255 = 15×16 + 15，末块短一点；1 字节的扇区也必须写出去——历史 bug
    /// 的循环条件会在 n % 16 == 1 时漏掉最后一个字节。
    #[test]
    fn block_ranges_never_drop_the_tail() {
        let lens = |n: usize| {
            block_ranges(n, G304.write_chunk)
                .map(|r| r.len())
                .collect::<Vec<_>>()
        };
        assert_eq!(lens(0), Vec::<usize>::new());
        assert_eq!(lens(1), vec![1]);
        assert_eq!(lens(16), vec![16]);
        assert_eq!(lens(17), vec![16, 1]);
        let mut full = vec![16; 15];
        full.push(15);
        assert_eq!(lens(255), full);
        assert_eq!(
            block_ranges(255, G304.write_chunk).map(|r| r.end).max(),
            Some(G304.sector_len)
        );
    }

    /// 扇区读取必须完整覆盖 255 字节：0..240 每 16 字节一次，再在 239 补
    /// 一次尾读（设备拒绝从 240 起读）。
    #[test]
    fn sector_reads_cover_the_whole_sector() {
        let head: Vec<usize> = (0..=G304.tail_read_at).step_by(G304.read_chunk).collect();
        let offsets: Vec<usize> = sector_read_offsets(&G304).collect();
        assert_eq!(offsets.len(), head.len() + 1);
        assert_eq!(&offsets[..head.len()], head.as_slice());
        assert_eq!(offsets[head.len()], G304.tail_read_at);
        // 前 15 次各 16 字节，尾读从 239 起 15 字节（239 与上一块重复）。
        assert_eq!(
            head.len() * G304.read_chunk + (REPORT_LEN - 4 - G304.tail_overlap),
            G304.sector_len
        );
    }

    /// profile 扇区的字段偏移：回报率、默认档位、五个小端 DPI 槽。
    #[test]
    fn parses_profile_fields_from_the_sector() {
        let mut sector = vec![0u8; G304.sector_len];
        sector[G304.report_rate_at] = 4;
        sector[G304.default_index_at] = 2;
        for (i, v) in [400u16, 800, 1600, 0, 0].into_iter().enumerate() {
            let at = G304.dpi_at + i * 2;
            sector[at..at + 2].copy_from_slice(&v.to_le_bytes());
        }
        let p = parse_profile(&sector, &G304);
        assert_eq!(p.stages, vec![400, 800, 1600]);
        assert_eq!(p.default_index, 2);
        assert_eq!(p.polling_ms, 4);

        // 回报率为 0 时退化到 1 ms，UI 才不会除零。
        sector[G304.report_rate_at] = 0;
        assert_eq!(parse_profile(&sector, &G304).polling_ms, 1);
    }

    /// 写完 CRC 之后，改过的扇区必须重新通过校验。
    #[test]
    fn stamping_the_crc_makes_a_modified_sector_valid() {
        let mut sector = shipped_sector();
        sector[G304.dpi_at] ^= 0x11;
        assert!(!sector_crc_valid(&sector));
        stamp_crc(&mut sector);
        assert!(sector_crc_valid(&sector));
    }

    /// 改回报率会整扇区提交并让固件重载当前档位；待落盘的档位必须一起
    /// 写进去，否则刚切到的 DPI 会被顶回旧档位。
    #[test]
    fn pending_stage_travels_with_any_sector_write() {
        let mut sector = shipped_sector();
        assert_eq!(sector[G304.default_index_at], 1);
        assert_eq!(
            carry_pending_stage(&mut sector, Some(2), G304.default_index_at),
            Some(2)
        );
        assert_eq!(sector[G304.default_index_at], 2);
        // 扇区里已经是这一档：不算改动，也不重复记账。
        assert_eq!(
            carry_pending_stage(&mut sector, Some(2), G304.default_index_at),
            None
        );
        assert_eq!(
            carry_pending_stage(&mut sector, None, G304.default_index_at),
            None
        );
    }

    // ---- 可靠写入的假设备 -------------------------------------------------

    use super::super::transport::HidTransport;
    use crate::driver::G304_LIMITS;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct FakeDevice {
        dev_idx: u8,
        f_dpi: u8,
        f_onboard: u8,
        sector: Vec<u8>,
        dpi: u16,
        dpi_read_timeout: bool,
        dpi_set_fails: bool,
        fail_start_write: bool,
        reload_on_commit: bool,
        /// 模拟 G304 固件：应答里的 swid 固定为 0。
        zero_swid: bool,
        /// 模拟功能查询超时（不回帧）。
        feature_timeout: bool,
        /// 模拟接收器吞掉 commit：每一步都回 OK，但扇区一个字节都没改。
        swallow_commit: bool,
        /// 模拟 commit 已经生效、只是应答丢了。
        lose_commit_reply: bool,
        /// 模拟 setDpi 被吞：照样回应答，传感器值不变。
        swallow_dpi_set: bool,
        write_buf: Vec<u8>,
        sector_writes: usize,
        sent: Vec<Vec<u8>>,
        replies: VecDeque<Vec<u8>>,
    }

    #[derive(Clone)]
    struct FakeIo(Arc<Mutex<FakeDevice>>);

    impl FakeIo {
        fn new(sector: Vec<u8>, dpi: u16) -> (Self, Arc<Mutex<FakeDevice>>) {
            let state = Arc::new(Mutex::new(FakeDevice {
                dev_idx: 1,
                f_dpi: 3,
                f_onboard: 7,
                sector,
                dpi,
                dpi_read_timeout: false,
                dpi_set_fails: false,
                fail_start_write: false,
                reload_on_commit: true,
                zero_swid: false,
                feature_timeout: false,
                swallow_commit: false,
                lose_commit_reply: false,
                swallow_dpi_set: false,
                write_buf: Vec::new(),
                sector_writes: 0,
                sent: Vec::new(),
                replies: VecDeque::new(),
            }));
            (Self(state.clone()), state)
        }
    }

    impl HidTransport for FakeIo {
        fn send(&mut self, report: &[u8]) -> Result<(), String> {
            let mut dev = self.0.lock().unwrap();
            dev.sent.push(report.to_vec());
            let feature = report[2];
            let func = report[3] >> 4;
            let mut reply = vec![0u8; REPORT_LEN];
            reply[0] = 0x11;
            reply[1] = dev.dev_idx;
            reply[2] = feature;
            reply[3] = if dev.zero_swid {
                report[3] & 0xF0
            } else {
                report[3]
            };

            // 功能发现（root.getFeature）超时：用来验证“暂时失败不能当成
            // 功能不存在”。
            if feature == 0 && func == 0 && dev.feature_timeout {
                return Ok(());
            }

            if feature == dev.f_onboard {
                match func {
                    // 读扇区块
                    5 => {
                        let off = u16::from_be_bytes([report[6], report[7]]) as usize;
                        if off < dev.sector.len() {
                            let end = (off + G304.read_chunk).min(dev.sector.len());
                            reply[4..4 + (end - off)].copy_from_slice(&dev.sector[off..end]);
                        }
                        dev.replies.push_back(reply);
                    }
                    // startWrite
                    6 => {
                        if !dev.fail_start_write {
                            dev.write_buf.clear();
                            dev.replies.push_back(reply);
                        }
                    }
                    // writeChunk
                    7 => {
                        dev.write_buf.extend_from_slice(&report[4..]);
                        dev.replies.push_back(reply);
                    }
                    // commit：固件按扇区里的默认档位重载传感器
                    8 => {
                        let n = dev.sector.len().min(dev.write_buf.len());
                        if !dev.swallow_commit {
                            let written = dev.write_buf[..n].to_vec();
                            dev.sector[..n].copy_from_slice(&written);
                        }
                        dev.sector_writes += 1;
                        if dev.reload_on_commit && !dev.swallow_commit {
                            let idx = dev.sector[G304.default_index_at] as usize;
                            let at = G304.dpi_at + idx * 2;
                            dev.dpi = u16::from_le_bytes([dev.sector[at], dev.sector[at + 1]]);
                        }
                        // 扇区已经改完、传感器也重载了，只是这一帧应答丢了。
                        if !dev.lose_commit_reply {
                            dev.replies.push_back(reply);
                        }
                    }
                    _ => dev.replies.push_back(reply),
                }
            } else if feature == dev.f_dpi {
                match func {
                    2 => {
                        if !dev.dpi_read_timeout {
                            let b = dev.dpi.to_be_bytes();
                            reply[5..7].copy_from_slice(&b);
                            dev.replies.push_back(reply);
                        }
                    }
                    3 => {
                        if !dev.dpi_set_fails {
                            // 参数布局是 [axis, hi, lo, hi, lo]。
                            if !dev.swallow_dpi_set {
                                dev.dpi = u16::from_be_bytes([report[5], report[6]]);
                            }
                            dev.replies.push_back(reply);
                        }
                    }
                    _ => dev.replies.push_back(reply),
                }
            } else {
                dev.replies.push_back(reply);
            }
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            Ok(self.0.lock().unwrap().replies.pop_front())
        }

        fn alive(&self) -> bool {
            true
        }
    }

    fn fake_sector() -> Vec<u8> {
        let mut sector = vec![0u8; G304.sector_len];
        sector[G304.report_rate_at] = 1;
        sector[G304.default_index_at] = 0;
        for (i, dpi) in [400u16, 800, 1600, 3200, 6400].into_iter().enumerate() {
            let at = G304.dpi_at + i * 2;
            sector[at..at + 2].copy_from_slice(&dpi.to_le_bytes());
        }
        for b in &mut sector[G304.dpi_at + 10..G304.sector_len - 2] {
            *b = 0xFF;
        }
        stamp_crc(&mut sector);
        sector
    }

    fn test_hidpp(io: FakeIo) -> Hidpp {
        hidpp_with(Box::new(io))
    }

    fn hidpp_with(io: Box<dyn HidTransport>) -> Hidpp {
        Hidpp {
            io,
            dev_idx: 1,
            name: "G304".into(),
            f_name: None,
            f_battery: None,
            f_dpi: Some(3),
            f_onboard: Some(7),
            profile_sector: Some(1),
            layout: Some(&G304),
            limits: G304_LIMITS,
            stages: vec![400, 800, 1600, 3200, 6400],
            default_index: 0,
            pending_default: None,
            pending_restore: None,
            polling_ms: 1,
            match_swid: false,
            save_failures: 0,
            save_retry_at: None,
            save_error: None,
        }
    }

    /// 按队列逐帧回放的传输：用来精确模拟“这一帧是短帧”这类时序异常，
    /// FakeIo 那种自动应答做不到。
    struct ScriptIo {
        frames: VecDeque<Option<Vec<u8>>>,
        sends: Arc<Mutex<usize>>,
    }

    impl ScriptIo {
        fn script(frames: Vec<Option<Vec<u8>>>) -> (Box<dyn HidTransport>, Arc<Mutex<usize>>) {
            let sends = Arc::new(Mutex::new(0));
            (
                Box::new(Self {
                    frames: frames.into(),
                    sends: sends.clone(),
                }),
                sends,
            )
        }
    }

    impl HidTransport for ScriptIo {
        fn send(&mut self, _report: &[u8]) -> Result<(), String> {
            *self.sends.lock().unwrap() += 1;
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            // 队列里是 None 就当作没回帧（超时）。
            Ok(self.frames.pop_front().flatten())
        }

        fn alive(&self) -> bool {
            true
        }
    }

    const F_NAME: u8 = 0x0A;

    /// 组装一帧设备名应答；`payload` 从第 4 字节开始铺。
    fn name_frame(func: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; REPORT_LEN];
        f[0] = 0x11;
        f[1] = 1;
        f[2] = F_NAME;
        f[3] = func << 4;
        f[4..4 + payload.len()].copy_from_slice(payload);
        f
    }

    /// 只有 4 字节头部的短帧：连长度字节都没有。
    fn stub_frame() -> Vec<u8> {
        vec![0x11, 1, F_NAME, 0]
    }

    /// 头帧缺失长度字节时先重试一次：偶发短帧能被救回来，不能让会话降级。
    #[test]
    fn short_name_head_is_retried_once() {
        let (io, sends) = ScriptIo::script(vec![
            Some(stub_frame()),
            Some(name_frame(0, &[4])),
            Some(name_frame(1, b"G30")),
            Some(name_frame(1, b"4\0\0")),
        ]);
        let mut dev = hidpp_with(io);
        assert_eq!(dev.read_name(F_NAME).unwrap(), "G304");
        assert_eq!(*sends.lock().unwrap(), 4, "短帧后应重发一次头帧");
    }

    /// 短帧持续出现必须报错。返回空名会让布局验证静默失败，整个会话退化成
    /// “只能改当前 DPI”，用户看不到任何提示。
    #[test]
    fn persistent_short_name_head_is_an_error() {
        let (io, _) = ScriptIo::script(vec![Some(stub_frame()), Some(stub_frame())]);
        let mut dev = hidpp_with(io);
        let err = dev.read_name(F_NAME).unwrap_err();
        assert!(err.contains("过短"), "{err}");
    }

    /// 分片短帧也不能当成“名字到此结束”：截断名（这里是 "G30"）过不了
    /// `supports_name`，后果和空名一样。
    #[test]
    fn short_name_chunk_is_retried_then_errors() {
        let short_chunk = || vec![0x11, 1, F_NAME, 0x10];

        // 重试一次成功：名字完整
        let (io, _) = ScriptIo::script(vec![
            Some(name_frame(0, &[4])),
            Some(short_chunk()),
            Some(name_frame(1, b"G30")),
            Some(name_frame(1, b"4\0\0")),
        ]);
        let mut dev = hidpp_with(io);
        assert_eq!(dev.read_name(F_NAME).unwrap(), "G304");

        // 重试后仍短：报错，而不是返回 "G30"
        let (io, _) = ScriptIo::script(vec![
            Some(name_frame(0, &[4])),
            Some(short_chunk()),
            Some(short_chunk()),
        ]);
        let mut dev = hidpp_with(io);
        let err = dev.read_name(F_NAME).unwrap_err();
        assert!(err.contains("分片"), "{err}");
    }

    /// 长度 0 是设备明确答复“没有名字”（等同于没有该功能），是有效结果，
    /// 不能因为“名字为空会导致降级”就把它变成错误。
    #[test]
    fn empty_name_is_a_valid_answer() {
        let (io, _) = ScriptIo::script(vec![Some(name_frame(0, &[0]))]);
        let mut dev = hidpp_with(io);
        assert_eq!(dev.read_name(F_NAME).unwrap(), "");
    }

    /// 读不到实时 DPI 时不能提交扇区：否则运行态未知，提交后 DPI 可能被
    /// 顶回旧档位，调用方却收到成功。
    #[test]
    fn live_dpi_read_failure_aborts_the_sector_write() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().dpi_read_timeout = true;
        let mut dev = test_hidpp(io);
        let err = dev
            .update_profile(&G304, |data| {
                data[G304.report_rate_at] = 4;
                true
            })
            .unwrap_err();
        assert!(err.contains("读取 DPI"), "{err}");
        let s = state.lock().unwrap();
        assert_eq!(s.sector_writes, 0, "读不到实时 DPI 时不能提交扇区");
        assert!(
            !s.sent.iter().any(|f| f[2] == 7 && f[3] >> 4 >= 6),
            "不应发送扇区写命令"
        );
    }

    /// 写成功但 DPI 恢复失败必须回报部分失败，并在下一次轮询补偿。
    #[test]
    fn dpi_restore_failure_is_partial_success_and_gets_compensated() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().dpi_set_fails = true;
        let mut dev = test_hidpp(io);

        let err = dev
            .update_profile(&G304, |data| {
                data[G304.report_rate_at] = 4;
                true
            })
            .unwrap_err();
        assert!(err.contains("配置已写入"), "{err}");
        assert_eq!(dev.pending_restore, Some(800));

        {
            let mut s = state.lock().unwrap();
            assert_eq!(s.sector_writes, 1);
            assert_eq!(s.sector[G304.report_rate_at], 4);
            assert_eq!(s.dpi, 400, "提交应按默认档位重载传感器");
            s.dpi_set_fails = false;
        }

        dev.settle_pending_restore();
        assert_eq!(dev.pending_restore, None);
        assert_eq!(state.lock().unwrap().dpi, 800);
    }

    /// 提交已经生效、应答却丢在半路：错误照报，但传感器必须放回提交前的
    /// 值。以前这里的 `?` 直接返回，DPI 停在扇区默认档位上，连补偿目标都
    /// 没留下，后面的轮询再也恢复不了。
    #[test]
    fn a_lost_commit_reply_still_restores_the_live_dpi() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().lose_commit_reply = true;
        let mut dev = test_hidpp(io);

        let err = dev
            .update_profile(&G304, |data| {
                data[G304.report_rate_at] = 4;
                true
            })
            .unwrap_err();
        assert!(err.contains("提交配置扇区失败"), "{err}");

        assert_eq!(dev.pending_restore, None, "当场恢复成功就不该留尾巴");
        let s = state.lock().unwrap();
        assert_eq!(s.sector_writes, 1, "提交确实送到了设备");
        assert_eq!(s.sector[G304.report_rate_at], 4, "扇区已经落盘");
        assert_eq!(s.dpi, 800, "传感器要回到提交前的值，而不是扇区默认档位");
    }

    /// 同上，但当场恢复也失败：错误要说清楚恢复失败，目标值留下来等轮询。
    #[test]
    fn a_lost_commit_reply_keeps_the_restore_target() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        {
            let mut s = state.lock().unwrap();
            s.lose_commit_reply = true;
            s.dpi_set_fails = true;
        }
        let mut dev = test_hidpp(io);

        let err = dev
            .update_profile(&G304, |data| {
                data[G304.report_rate_at] = 4;
                true
            })
            .unwrap_err();
        assert!(err.contains("恢复实时 DPI 失败"), "{err}");
        assert_eq!(dev.pending_restore, Some(800));

        state.lock().unwrap().dpi_set_fails = false;
        dev.settle_pending_restore();
        assert_eq!(dev.pending_restore, None);
        assert_eq!(state.lock().unwrap().dpi, 800);
    }

    /// 接收器吞掉 DPI 写、却照样回应答：只有回读能识破，写成功必须由
    /// 传感器值证明。
    #[test]
    fn an_acknowledged_but_swallowed_dpi_write_is_not_confirmed() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().swallow_dpi_set = true;
        let mut dev = test_hidpp(io);

        let err = dev.set_current_dpi(1600).unwrap_err();
        assert!(err.contains("1600"), "{err}");
        assert_eq!(state.lock().unwrap().dpi, 800);
    }

    /// 切档同样不能被假应答骗过去：未确认时快照保持上一份可信值。
    #[test]
    fn activating_a_stage_needs_the_sensor_to_agree() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().swallow_dpi_set = true;
        let mut dev = test_hidpp(io);
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            stage_active: 1,
            ..Snapshot::default()
        };

        let err = dev.apply(Cmd::Activate(2), &mut snap).unwrap_err();
        match err {
            WriteError::Unconfirmed(m) => assert!(m.contains("1600"), "{m}"),
            other => panic!("吞掉的 DPI 写必须报未确认，实际 {other:?}"),
        }
        assert_eq!(snap.stage_active, 1, "未确认时快照不能改");
        assert_eq!(dev.pending_default, None, "没写成功就不该记待落盘档位");
    }

    /// 新切档已经写进去、只是回读超时（结果未知）时，不能留着上一条旧补偿
    /// 目标：下一轮轮询会拿它把设备改回去，把刚生效的指令吞掉。
    #[test]
    fn an_unconfirmed_new_stage_does_not_reinstate_the_old_restore() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        // 先制造一个旧的补偿目标（提交后恢复失败会留下它）。
        state.lock().unwrap().dpi_set_fails = true;
        let mut dev = test_hidpp(io);
        let _ = dev
            .update_profile(&G304, |data| {
                data[G304.report_rate_at] = 4;
                true
            })
            .unwrap_err();
        assert_eq!(dev.pending_restore, Some(800));

        // 用户改切到 1600：设备收到并生效，但这次回读超时。
        {
            let mut s = state.lock().unwrap();
            s.dpi_set_fails = false;
            s.dpi_read_timeout = true;
        }
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            stage_active: 0,
            ..Snapshot::default()
        };
        match dev.apply(Cmd::Activate(2), &mut snap).unwrap_err() {
            WriteError::Unconfirmed(m) => assert!(m.contains("超时"), "{m}"),
            other => panic!("回读超时必须是未确认，实际 {other:?}"),
        }
        assert_eq!(dev.pending_restore, None, "旧目标不能跨过新指令");

        // 之后只读确认成功，轮询也不能再把传感器拽回 800。
        state.lock().unwrap().dpi_read_timeout = false;
        dev.settle_pending_restore();
        assert_eq!(state.lock().unwrap().dpi, 1600);
    }

    /// 用户切档必须覆盖上一次没恢复成功的 DPI 目标，否则下一轮轮询会把
    /// 刚切到的档位改回去。
    #[test]
    fn activating_a_stage_supersedes_a_pending_restore() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().dpi_set_fails = true;
        let mut dev = test_hidpp(io);

        let _ = dev
            .update_profile(&G304, |data| {
                data[G304.report_rate_at] = 4;
                true
            })
            .unwrap_err();
        assert_eq!(dev.pending_restore, Some(800));

        state.lock().unwrap().dpi_set_fails = false;
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            ..Snapshot::default()
        };
        dev.apply(Cmd::Activate(2), &mut snap).unwrap();

        assert_eq!(dev.pending_restore, None);
        assert_eq!(state.lock().unwrap().dpi, 1600);
        // 下一轮补偿不能再动传感器。
        dev.settle_pending_restore();
        assert_eq!(state.lock().unwrap().dpi, 1600);
    }

    /// 退出落盘要把待保存的档位写进扇区，同时保持传感器当前的 DPI。
    #[test]
    fn flush_commits_the_pending_stage_and_keeps_live_dpi() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        let mut dev = test_hidpp(io);
        dev.pending_default = Some(2);
        dev.flush().unwrap();

        let s = state.lock().unwrap();
        assert_eq!(s.sector_writes, 1);
        assert_eq!(s.sector[G304.default_index_at], 2);
        assert_eq!(s.dpi, 800);
    }

    /// 落盘失败必须从 flush 返回错误，供退出路径记录，而不是静默成功。
    #[test]
    fn flush_reports_a_deferred_write_failure() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().fail_start_write = true;
        let mut dev = test_hidpp(io);
        dev.pending_default = Some(2);
        let err = dev.flush().unwrap_err();
        assert!(err.contains("开始写扇区失败"), "{err}");
        assert_eq!(state.lock().unwrap().sector_writes, 0);
        // 失败后待保存状态不能丢，退出时还有机会补存。
        assert_eq!(dev.pending_default, Some(2));
    }

    /// G304 固件把应答 swid 清零：型号声明允许后读 DPI 必须成功；严格
    /// 模式（未知型号）仍拒绝，避免接收器上别的软件的应答被当成自己的。
    #[test]
    fn zeroed_swid_replies_follow_the_model_policy() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().zero_swid = true;
        let mut dev = test_hidpp(io);
        assert_eq!(dev.read_dpi(3).unwrap(), 800);

        dev.match_swid = true;
        state.lock().unwrap().replies.clear();
        assert!(dev.call_within(3, 2, &[0], 80).unwrap().is_none());
    }

    /// 功能查询超时是暂时失败，不能和“设备明确回索引 0”混为一谈。
    #[test]
    fn feature_probe_separates_timeout_from_absent() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        let mut dev = test_hidpp(io);
        state.lock().unwrap().feature_timeout = true;
        assert!(dev.feature(F_ADJUSTABLE_DPI).is_err());

        state.lock().unwrap().feature_timeout = false;
        assert_eq!(dev.feature(F_ADJUSTABLE_DPI).unwrap(), None);
    }

    /// 一轮输入里的多个字段只读/写/提交一次扇区。
    #[test]
    fn batch_edits_commit_the_sector_once() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        let mut dev = test_hidpp(io);
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            stage_active: 0,
            ..Snapshot::default()
        };
        dev.apply_batch(
            &[Cmd::Dpi(1, 900), Cmd::Dpi(2, 1700), Cmd::Polling(2)],
            &mut snap,
        )
        .unwrap();

        let s = state.lock().unwrap();
        assert_eq!(s.sector_writes, 1, "整轮只提交一次");
        assert_eq!(
            u16::from_le_bytes([s.sector[G304.dpi_at + 2], s.sector[G304.dpi_at + 3]]),
            900
        );
        assert_eq!(
            u16::from_le_bytes([s.sector[G304.dpi_at + 4], s.sector[G304.dpi_at + 5]]),
            1700
        );
        assert_eq!(s.sector[G304.report_rate_at], 2);
        assert_eq!(dev.polling_ms, 2);
        assert_eq!(snap.polling_ms, 2);
    }

    /// 整批校验必须在写设备之前完成：一条非法命令既不能留下半批写入，也
    /// 不能把乐观值写进快照——下一次读改写会拿它当基准算。
    #[test]
    fn invalid_command_in_a_batch_writes_nothing() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        let mut dev = test_hidpp(io);
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            stage_active: 1,
            ..Snapshot::default()
        };
        let before = snap.dpi.clone();
        let stages_before = dev.stages.clone();

        // 第一条合法、第二条越界：`Rejected` 的契约是“设备状态未变”。
        let err = dev
            .apply_batch(&[Cmd::Dpi(0, 900), Cmd::Dpi(9, 900)], &mut snap)
            .unwrap_err();

        assert!(matches!(err, WriteError::Rejected(_)));
        assert_eq!(snap.dpi, before, "拒绝后快照不能留下乐观值");
        assert_eq!(dev.stages, stages_before, "驱动缓存也不能提前改");
        let s = state.lock().unwrap();
        assert_eq!(s.sector_writes, 0, "拒绝前不能碰扇区");
        assert!(s.write_buf.is_empty(), "拒绝前不能开始写扇区");
    }

    /// 每一步都应答 OK、但扇区根本没被改写时，提交后的回读必须抓住它：
    /// 这正是接收器吞命令的现场，只看应答会当成功。
    #[test]
    fn swallowed_commit_is_caught_by_read_back() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().swallow_commit = true;
        let mut dev = test_hidpp(io);
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            stage_active: 0,
            ..Snapshot::default()
        };
        let dpi_before = snap.dpi.clone();
        snap.polling_ms = 1;

        let err = dev.apply_batch(&[Cmd::Polling(2)], &mut snap).unwrap_err();

        match err {
            WriteError::Unconfirmed(m) => assert!(m.contains("未落盘"), "{m}"),
            other => panic!("扇区没改必须报未确认，实际 {other:?}"),
        }
        assert_eq!(snap.polling_ms, 1, "快照不能留下没落盘的乐观值");
        assert_eq!(snap.dpi, dpi_before);
        assert_eq!(dev.polling_ms, 1, "驱动缓存也不能提前改");
    }

    /// 回读只覆盖改动的块：改一个字段不该为了校验再读整扇区。
    #[test]
    fn read_back_touches_only_the_changed_block() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        let mut dev = test_hidpp(io);
        let mut snap = Snapshot {
            dpi: vec![400, 800, 1600, 3200, 6400],
            stage_count: 5,
            stage_active: 0,
            ..Snapshot::default()
        };
        dev.apply_batch(&[Cmd::Polling(2)], &mut snap).unwrap();

        let s = state.lock().unwrap();
        let reads = s
            .sent
            .iter()
            .filter(|f| f[2] == s.f_onboard && f[3] >> 4 == 5)
            .count();
        // 1 次整扇区读（15 块 + 尾读 = 16 帧）+ 1 次校验回读。
        assert_eq!(reads, 17, "回读不该把整扇区再读一遍");
    }

    /// HID++ 没有「提交后待比对的目标块」：confirm 只能刷新实际配置，
    /// 既不能冒称 Verified，也不能顺手补写待落档位。
    #[test]
    fn confirm_only_refreshes_without_claiming_success() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        let mut dev = test_hidpp(io);
        dev.pending_default = Some(2);
        let mut snap = Snapshot::default();
        assert_eq!(dev.confirm(&mut snap).unwrap(), ConfirmResult::ReadOnly);
        assert_eq!(dev.pending_default, Some(2), "确认路径不能落盘");
        assert_eq!(state.lock().unwrap().sector_writes, 0, "确认不能写扇区");
        assert!(!snap.dpi.is_empty(), "确认仍要刷新实际配置");
    }

    /// 保存失败必须保留待保存档位并进入退避；成功确认后才能清空。
    #[test]
    fn failed_deferred_save_keeps_the_pending_stage() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().fail_start_write = true;
        let mut dev = test_hidpp(io);
        dev.pending_default = Some(2);
        dev.settle_default_stage();
        assert_eq!(dev.pending_default, Some(2));
        assert!(dev.save_error.is_some());
        let retry_at = dev.save_retry_at.expect("失败后要排重试");
        let remaining = retry_at.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::from_secs(4) && remaining <= Duration::from_secs(5),
            "首轮退避应为 5s，实际 {remaining:?}"
        );

        let sent = state.lock().unwrap().sent.len();
        dev.settle_default_stage();
        assert_eq!(
            state.lock().unwrap().sent.len(),
            sent,
            "退避期内不能反复写 EEPROM"
        );

        state.lock().unwrap().fail_start_write = false;
        dev.save_retry_at = None;
        dev.settle_default_stage();
        assert_eq!(dev.pending_default, None);
        assert!(dev.save_error.is_none());
        assert_eq!(state.lock().unwrap().sector[G304.default_index_at], 2);
    }

    /// 默认档位保存失败后，改回报率等修改会顺带把档位写进扇区；这次合并
    /// 保存成功后旧的“尚未保存”错误必须清掉，不能永久留在快照里。
    #[test]
    fn carried_save_clears_a_previous_save_failure() {
        let (io, state) = FakeIo::new(fake_sector(), 800);
        state.lock().unwrap().fail_start_write = true;
        let mut dev = test_hidpp(io);
        dev.pending_default = Some(2);
        dev.settle_default_stage();
        assert!(dev.save_error.is_some());

        state.lock().unwrap().fail_start_write = false;
        dev.update_profile(&G304, |data| {
            data[G304.report_rate_at] = 4;
            true
        })
        .unwrap();

        assert_eq!(dev.pending_default, None);
        assert!(dev.save_error.is_none(), "合并保存成功后不能残留旧错误");
        assert_eq!(dev.save_failures, 0);
        assert_eq!(dev.save_retry_at, None);
        assert_eq!(state.lock().unwrap().sector[G304.default_index_at], 2);
    }
}
