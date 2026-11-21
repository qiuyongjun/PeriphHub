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
use super::{Caps, Cmd, Driver, Limits, Snapshot};

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
        };
        dev.dev_idx = dev
            .find_device()
            .ok_or_else(|| "接收器上没有设备应答（鼠标可能已关机或休眠）".to_string())?;
        dev.f_name = dev.feature(F_DEVICE_NAME);
        dev.f_battery = dev.feature(F_BATTERY_LEVEL_STATUS);
        dev.f_dpi = dev.feature(F_ADJUSTABLE_DPI);
        dev.f_onboard = dev.feature(F_ONBOARD_PROFILES);
        if let Some(fi) = dev.f_name {
            dev.name = dev.read_name(fi);
        }
        // 只有布局存在、型号名匹配、扇区大小一致，并且真的找到了一个
        // 启用的 profile 扇区，才允许写板载配置。`None`（未验证布局）
        // 仍可改当前 DPI，但不碰扇区。
        let verified = layout.filter(|l| {
            l.supports_name(&dev.name) && dev.onboard_sector_size() == Some(l.sector_len as u16)
        });
        let profile_sector = verified.and_then(|l| dev.find_profile_sector(l));
        let verified = verified.filter(|_| profile_sector.is_some());
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
                let same_sw = (buf[3] & 0x0F) == (frame[3] & 0x0F);
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

    /// Feature index of `id`, or `None` when the device does not have it.
    fn feature(&mut self, id: u16) -> Option<u8> {
        let r = self.call(0x00, 0, &id.to_be_bytes()).ok()??;
        match r.get(4).copied() {
            // index 0 表示"没有这个功能"；短帧同样按没有处理。
            None | Some(0) => None,
            Some(idx) => Some(idx),
        }
    }

    /// 板载配置信息页里的扇区大小（G304 为 0x00FF）。读不到或不是 255 时
    /// 不能套用本驱动的扇区布局。
    fn onboard_sector_size(&mut self) -> Option<u16> {
        let fi = self.f_onboard?;
        let r = self.call(fi, 0, &[]).ok()??;
        if r.len() < 13 {
            return None;
        }
        Some(u16::from_be_bytes([r[11], r[12]]))
    }

    /// The name arrives three characters at a time. This firmware treats the
    /// offset as a *character* index, so chunks are 3 apart (see docs).
    fn read_name(&mut self, feature: u8) -> String {
        let Ok(Some(head)) = self.call(feature, 0, &[]) else {
            return String::new();
        };
        let Some(&len) = head.get(4) else {
            return String::new();
        };
        let len = len as usize;
        let mut bytes = Vec::with_capacity(len);
        let mut off = 0usize;
        while off < len {
            match self.call(feature, 1, &[off as u8]) {
                // 设备偶尔回一帧短应答。缺字节就当作名字结束，不要用越界切片
                // 把整个采集线程 panic 掉。
                Ok(Some(r)) => match r.get(4..7) {
                    Some(chunk) => bytes.extend_from_slice(chunk),
                    None => break,
                },
                _ => break,
            }
            off += 3;
        }
        let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).trim().to_string()
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
    fn find_profile_sector(&mut self, layout: &HidppLayout) -> Option<u16> {
        let fi = self.f_onboard?;
        let ctrl = self.read_sector(layout, fi, 0).ok()?;
        for entry in ctrl.chunks(4) {
            if entry.len() < 4 || entry[0] == 0xFF {
                break;
            }
            if entry[2] != 0 {
                return Some(u16::from_be_bytes([entry[0], entry[1]]));
            }
        }
        None
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
        self.write_sector(layout, fi, sector, &data)?;
        if let Some(idx) = carried {
            self.default_index = idx;
            self.pending_default = None;
        }
        if let Some(live) = live {
            if let Err(e) = self.restore_live_dpi(live) {
                // 扇区已经写进去了，不能回报“完全成功”；留下目标值交给
                // 下一次轮询补偿，同时把部分成功告诉调用方纠正界面。
                self.pending_restore = Some(live);
                return Err(format!("配置已写入，但恢复 DPI 失败：{e}"));
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
                if self.set_current_dpi(target).is_ok() && self.read_dpi(fi).ok() == Some(target) {
                    self.pending_restore = None;
                }
            }
            // 设备暂不可达：保留目标，下一次轮询再试。
            Err(_) => {}
        }
    }

    /// 提交扇区后把传感器放回提交前的实时值，并回读确认。
    fn restore_live_dpi(&mut self, live: u16) -> Result<(), String> {
        let Some(fi) = self.f_dpi else {
            return Ok(());
        };
        if self.read_dpi(fi)? == live {
            return Ok(());
        }
        self.set_current_dpi(live)?;
        let now = self.read_dpi(fi)?;
        if now != live {
            return Err(format!("回读 DPI 为 {now}，期望 {live}"));
        }
        Ok(())
    }

    /// 把用户最后选中的档位写回设备扇区。
    ///
    /// 只在"下一次周期读取"时调用，而不是每次点档位都调：写一次意味着读回
    /// 255 字节 + 擦写 + 重算 CRC + commit，而连续切档是查看行为的常态。
    /// 代价是断电前最后几次切换可能没落盘，设备会回到旧档位——运行态不受影响。
    fn flush_default_stage(&mut self, layout: &HidppLayout) -> Result<(), String> {
        let Some(idx) = self.pending_default else {
            return Ok(());
        };
        let (Some(_), Some(_)) = (self.f_onboard, self.profile_sector) else {
            // 没有可写的板载配置：档位只存在于固件运行态。
            self.pending_default = None;
            return Ok(());
        };
        if idx == self.default_index {
            self.pending_default = None;
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
        Ok(())
    }

    /// 落盘失败只记一笔日志：设备本身仍然是好的，这次会话也照常用，只是
    /// 重启后档位回到旧值——这比把设备判成离线温和得多。
    fn settle_default_stage(&mut self) {
        let Some(layout) = self.layout else {
            self.pending_default = None;
            return;
        };
        if let Err(e) = self.flush_default_stage(layout) {
            crate::log::line("WARN", &format!("保存默认档位失败：{e}"));
            self.pending_default = None;
        }
    }

    /// 退出路径的严格版本：错误交给调用方记录，不在这里吞掉。
    fn flush_default_stage_checked(&mut self) -> Result<(), String> {
        let Some(layout) = self.layout else {
            self.pending_default = None;
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

impl Driver for Hidpp {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        // 先补完上一次没恢复的 DPI，再把待落默认档位写掉：这样下面读到的
        // index、当前 DPI 都是同一次写入之后的状态，不会一会儿新一会儿旧。
        self.settle_pending_restore();
        self.settle_default_stage();
        let caps = self.current_caps();
        if let Some(layout) = self.layout {
            self.load_profile(layout)?;
        }
        let current = match self.f_dpi {
            Some(fi) => self.read_dpi(fi)?,
            None => *self.stages.first().unwrap_or(&0),
        };
        let stages = if self.stages.is_empty() {
            vec![current]
        } else {
            self.stages.clone()
        };
        // The mouse's own DPI button moves the sensor without touching the
        // profile, so the active stage is whichever one matches right now.
        // Two stages can hold the same DPI, in which case the profile's own
        // default index is the better guess. 用户刚切、还没落盘的那一档优先：
        // 扇区里读回的还是旧值。
        let default = self
            .pending_default
            .map(|i| i as usize)
            .unwrap_or(self.default_index as usize)
            .min(stages.len() - 1);
        let active = if stages.get(default) == Some(&current) {
            default
        } else {
            stages.iter().position(|&v| v == current).unwrap_or(default)
        };

        Ok(Snapshot {
            model: self.name.clone(),
            caps,
            polling_ms: self.polling_ms,
            lod: 0,
            motion: false,
            stage_count: stages.len() as u8,
            stage_active: active as u8,
            dpi: stages,
            dpi_min: self.limits.dpi.0,
            dpi_max: self.limits.dpi.1,
            debounce_press: 0,
            debounce_release: 0,
            sleep_min: 0,
            line_corr: false,
            wave_corr: false,
        })
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

    fn apply(&mut self, cmd: Cmd, snap: &mut Snapshot) -> Result<(), String> {
        match cmd {
            Cmd::Refresh => Ok(()),
            Cmd::Dpi(stage, v) => {
                if self.f_dpi.is_none() {
                    return Err("该设备不支持调整 DPI".into());
                }
                let v = v.clamp(self.limits.dpi.0, self.limits.dpi.1);
                let idx = stage as usize;
                if let Some(layout) = self.layout {
                    // `stage` comes from outside the driver; this is the
                    // protocol boundary, so reject an out-of-range index
                    // here instead of indexing past the sector (which
                    // would panic the collection thread).
                    if idx >= layout.stages {
                        return Err(format!(
                            "dpi stage {stage} out of range (0..{})",
                            layout.stages
                        ));
                    }
                    let b = v.to_le_bytes();
                    let at = layout.dpi_at + idx * 2;
                    self.update_profile(layout, |data| {
                        if data[at] == b[0] && data[at + 1] == b[1] {
                            return false;
                        }
                        data[at] = b[0];
                        data[at + 1] = b[1];
                        true
                    })?;
                    if let Some(slot) = self.stages.get_mut(idx) {
                        *slot = v;
                    }
                    if let Some(slot) = snap.dpi.get_mut(idx) {
                        *slot = v;
                    }
                    // Editing the stage in use should be felt immediately, not
                    // after the next power cycle.
                    if idx == snap.stage_active as usize {
                        self.set_current_dpi(v)?;
                    }
                } else {
                    self.set_current_dpi(v)?;
                    snap.dpi = vec![v];
                    snap.stage_count = 1;
                    snap.stage_active = 0;
                }
                Ok(())
            }
            Cmd::Activate(i) => {
                let idx = (i as usize).min(snap.dpi.len().saturating_sub(1));
                let Some(v) = snap.dpi.get(idx).copied() else {
                    return Ok(());
                };
                // 切档只动传感器，默认档位攒着一起落盘（见 flush_default_stage）。
                //
                // 以前这里先写扇区再切传感器，因为 commit 会让固件重载当前
                // 档位、丢掉先写的 DPI；代价是每次点档位都要读回 255 字节再
                // 擦写一次——切档恰恰是查看配置时最常做的动作。现在顺序反过来：
                // 传感器立刻跟手，扇区留到下一次周期读取时写一次，那之后读到
                // 的当前 DPI 就是刚写进去的这一档。
                self.set_current_dpi(v)?;
                // 用户明确切档后，新的传感器值优先：上一次没恢复成功的旧
                // DPI 目标若还留着，下一轮轮询会把它改回去，等于吞掉这次选择。
                self.pending_restore = None;
                self.pending_default = Some(idx as u8);
                snap.stage_active = idx as u8;
                Ok(())
            }
            Cmd::Polling(ms) => {
                let Some(layout) = self.layout else {
                    return Err("该设备不支持调整回报率".into());
                };
                if !self.limits.polling.contains(&ms) {
                    return Err("该型号不支持这个回报率".into());
                }
                self.update_profile(layout, |data| {
                    if data[layout.report_rate_at] == ms {
                        return false;
                    }
                    data[layout.report_rate_at] = ms;
                    true
                })?;
                self.polling_ms = ms;
                snap.polling_ms = ms;
                Ok(())
            }
            // Everything else is hidden by the UI for this device.
            _ => Err("该设备不支持这个设置".into()),
        }
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
            reply[3] = report[3];

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
                        let written = dev.write_buf[..n].to_vec();
                        dev.sector[..n].copy_from_slice(&written);
                        dev.sector_writes += 1;
                        if dev.reload_on_commit {
                            let idx = dev.sector[G304.default_index_at] as usize;
                            let at = G304.dpi_at + idx * 2;
                            dev.dpi = u16::from_le_bytes([dev.sector[at], dev.sector[at + 1]]);
                        }
                        dev.replies.push_back(reply);
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
                            dev.dpi = u16::from_be_bytes([report[5], report[6]]);
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
        Hidpp {
            io: Box::new(io),
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
        }
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
    }
}
