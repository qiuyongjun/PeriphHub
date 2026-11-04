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
//! Raw bytes, CRCs and the experiments behind them: `docs/logitech-g304.md`.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_SetOutputReport;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use super::{Caps, Cmd, Driver, HidppLayout, Limits, Snapshot};

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn ReadFile(h: HANDLE, buf: *mut c_void, n: u32, read: *mut u32, ov: *mut Overlapped) -> i32;
    fn CreateEventW(attr: *mut c_void, manual: i32, initial: i32, name: *const u16) -> HANDLE;
    fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    fn GetOverlappedResult(h: HANDLE, ov: *mut Overlapped, transferred: *mut u32, wait: i32)
        -> i32;
    fn CancelIo(h: HANDLE) -> i32;
    fn ResetEvent(h: HANDLE) -> i32;
    fn GetLastError() -> u32;
}

const ERROR_IO_PENDING: u32 = 997;
const ERROR_OPERATION_ABORTED: u32 = 995;
const REPORT_LEN: usize = 20;
const SW_ID: u8 = 0x0A;
const TIMEOUT_MS: u32 = 600;
/// 取消一次挂起的读取后，最多再等这么久让它真正完成。等不到就放弃这次
/// 读取并泄漏那一小块堆内存，避免内核稍后写回已释放的栈变量。
const CANCEL_WAIT_MS: u32 = 250;

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

/// Onboard sectors are 255 bytes here (`info` reports 0x00FF), with a CRC16 in
/// the last two bytes. The device rejects a 16-byte read starting at 240, so
/// the tail has to be read at 239 and shifted.
const SECTOR_LEN: usize = 255;
const CRC_AT: usize = SECTOR_LEN - 2;
/// Profile sector offsets (little endian), verified against the live sector.
const P_REPORT_RATE: usize = 0;
const P_DEFAULT_INDEX: usize = 1;
const P_DPI: usize = 3;
const STAGES: usize = 5;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: HANDLE,
}

/// 一次 overlapped 读取需要的两个缓冲区。放在堆上，超时取消后即使内核
/// 稍后才完成，也不会写到已经退栈的内存；最坏情况下故意泄漏这一块。
struct PendingRead {
    ov: Overlapped,
    buf: [u8; REPORT_LEN],
}

pub struct Hidpp {
    handle: HANDLE,
    event: HANDLE,
    dev_idx: u8,
    name: String,
    /// Feature indexes, resolved once. `None` = the device does not have it.
    f_name: Option<u8>,
    f_battery: Option<u8>,
    f_dpi: Option<u8>,
    f_onboard: Option<u8>,
    /// Sector holding the active profile, from the control sector's first entry.
    profile_sector: Option<u16>,
    /// 板载扇区布局是否与 G304 实测一致。接收器可以配对其他鼠标，而
    /// 扇区偏移/CRC 是 G304 专有的，未验证的型号只能改当前 DPI。
    profile_ok: bool,
    limits: Limits,
    /// Last profile read: DPI stages, report rate, default index. Kept so a
    /// burst of edits does not re-read the whole sector per command.
    stages: Vec<u16>,
    /// 设备扇区里**实际存着**的默认档位。只有写成功了才会变。
    default_index: u8,
    /// 用户刚切到、还没落盘的档位。切档本身立刻作用于传感器，落盘攒到下一次
    /// 周期读取时做一次（见 `flush_default_stage`）。
    pending_default: Option<u8>,
    polling_ms: u8,
}

impl Hidpp {
    pub fn open(path: &[u16], layout: HidppLayout, limits: Limits) -> Result<Hidpp, String> {
        let mut p = path.to_vec();
        if p.last() != Some(&0) {
            p.push(0);
        }
        unsafe {
            let handle = CreateFileW(
                p.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "打开接收器失败（错误码 {}）",
                    std::io::Error::last_os_error()
                ));
            }
            let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
            let mut dev = Hidpp {
                handle,
                event,
                dev_idx: 0,
                name: String::new(),
                f_name: None,
                f_battery: None,
                f_dpi: None,
                f_onboard: None,
                profile_sector: None,
                profile_ok: false,
                limits,
                stages: Vec::new(),
                default_index: 0,
                pending_default: None,
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
            // 只有 profile 声明已验证、型号和扇区大小都对得上才允许写板载
            // 配置。Generic 布局仍可改当前 DPI，但不碰扇区。
            dev.profile_ok = match layout {
                HidppLayout::G304 => {
                    profile_layout_supported(&dev.name)
                        && dev.onboard_sector_size() == Some(SECTOR_LEN as u16)
                }
                HidppLayout::Generic => false,
            };
            dev.profile_sector = dev.find_profile_sector();
            if dev.profile_ok {
                dev.load_profile().ok();
            } else {
                // 未验证型号没有可信的传感器范围；当前 DPI 不是持久配置，
                // 给一个宽范围，让读取到的值仍能在编辑器里显示和微调。
                dev.limits.dpi = (GENERIC_DPI_MIN, GENERIC_DPI_MAX);
                if dev.f_onboard.is_some() && dev.profile_sector.is_some() {
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
    }

    /// One overlapped read, bounded by `ms`.
    ///
    /// 超时这一路不能"取消完就返回"：`CancelIo` 只是请求取消，内核仍可能
    /// 稍后才完成这次读。读取缓冲在堆上（`PendingRead`），取消后用有界
    /// 等待确认结束；等不到就故意泄漏那块内存，驱动随后会被判离线并重建，
    /// 这样既不会 use-after-free，也不会让采集线程永久卡住。
    fn read_frame(&self, ms: u32) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            ResetEvent(self.event);
            let mut pending = Box::new(PendingRead {
                ov: Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    h_event: self.event,
                },
                buf: [0u8; REPORT_LEN],
            });
            let mut read = 0u32;
            let ok = ReadFile(
                self.handle,
                pending.buf.as_mut_ptr() as *mut c_void,
                REPORT_LEN as u32,
                &mut read,
                &mut pending.ov,
            );
            let taken =
                |read: u32, buf: &[u8; REPORT_LEN]| buf[..(read as usize).min(REPORT_LEN)].to_vec();
            if ok != 0 {
                return Ok(Some(taken(read, &pending.buf)));
            }
            if GetLastError() != ERROR_IO_PENDING {
                return Err("读取应答失败".into());
            }
            if WaitForSingleObject(self.event, ms) == 0 {
                if GetOverlappedResult(self.handle, &mut pending.ov, &mut read, 0) == 0 {
                    return Err("读取应答失败".into());
                }
                return Ok(Some(taken(read, &pending.buf)));
            }
            // 超时：请求取消，但只等有限时间。等不到就放弃这块缓冲。
            CancelIo(self.handle);
            if WaitForSingleObject(self.event, CANCEL_WAIT_MS) != 0 {
                std::mem::forget(pending);
                return Err("取消读取请求超时".into());
            }
            if GetOverlappedResult(self.handle, &mut pending.ov, &mut read, 0) != 0 {
                return Ok(Some(taken(read, &pending.buf)));
            }
            if GetLastError() != ERROR_OPERATION_ABORTED {
                return Err("读取应答失败".into());
            }
            Ok(None)
        }
    }

    /// Sends one request and waits for the answer with the same header.
    fn xfer(
        &mut self,
        frame: &[u8; REPORT_LEN],
        want_feature: u8,
        ms: u32,
    ) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            if HidD_SetOutputReport(
                self.handle,
                frame.as_ptr() as *const c_void,
                REPORT_LEN as u32,
            ) == 0
            {
                return Err("发送命令失败".into());
            }
        }
        let budget = Duration::from_millis(ms as u64);
        let start = Instant::now();
        loop {
            // 预算按"整个请求"算，而不是每次读都从头等一遍。只有预算真的用光
            // 才取消这次读：取消会让驱动丢掉已经到达的那一帧，能不用就不用。
            let left = budget.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            let Some(buf) = self.read_frame(left.as_millis().max(1) as u32)? else {
                break;
            };
            // Errors carry feature index 0xFF; everything else echoes the
            // feature and function we asked for. 同一 feature 下不同功能的
            // 迟到应答（例如上一次读扇区）不能被当成当前请求的结果。
            if buf.len() >= 4 && buf[1] == frame[1] {
                let is_error = buf[2] == 0xFF;
                let same_func = (buf[3] & 0xF0) == (frame[3] & 0xF0);
                if is_error || (buf[2] == want_feature && same_func) {
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
        let mut f = [0u8; REPORT_LEN];
        f[0] = 0x11;
        f[1] = self.dev_idx;
        f[2] = feature;
        f[3] = (func << 4) | SW_ID;
        // 超长参数截断而不是越界 panic：长帧只有 16 字节载荷。
        for (i, b) in params.iter().take(REPORT_LEN - 4).enumerate() {
            f[4 + i] = *b;
        }
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
    fn find_profile_sector(&mut self) -> Option<u16> {
        let fi = self.f_onboard?;
        let ctrl = self.read_sector(fi, 0).ok()?;
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

    /// One 255-byte sector. 16-byte reads up to 239, then a read at 239 whose
    /// first byte repeats 239 — the device refuses to start a read at 240.
    fn read_sector(&mut self, fi: u8, sector: u16) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(SECTOR_LEN);
        let mut off = 0usize;
        while off < 240 {
            let r = self
                .call(
                    fi,
                    5,
                    &[hi(sector), lo(sector), hi(off as u16), lo(off as u16)],
                )?
                .ok_or_else(|| "读取配置扇区超时".to_string())?;
            out.extend_from_slice(r.get(4..REPORT_LEN).ok_or("配置扇区应答过短")?);
            off += 16;
        }
        let r = self
            .call(fi, 5, &[hi(sector), lo(sector), 0, 239])?
            .ok_or_else(|| "读取配置扇区尾部超时".to_string())?;
        out.extend_from_slice(r.get(5..REPORT_LEN).ok_or("配置扇区尾部应答过短")?);
        Ok(out)
    }

    /// startWrite → 16-byte blocks → commit. The sector is only accepted when
    /// its CRC16 (big endian, last two bytes) covers everything before it.
    fn write_sector(&mut self, fi: u8, sector: u16, data: &[u8]) -> Result<(), String> {
        let n = data.len();
        if n == 0 {
            return Err("扇区数据为空".into());
        }
        let start = [hi(sector), lo(sector), 0, 0, hi(n as u16), lo(n as u16)];
        self.call(fi, 6, &start)?
            .ok_or_else(|| "开始写扇区失败".to_string())?;
        let mut o = 0usize;
        // 末块可以短于 16 字节（255 = 15×16 + 15，实测被设备接受），所以按
        // `min(o + 16, n)` 截尾。循环条件必须是 `o < n` 而不是 `o < n - 1`：
        // 后者在 n % 16 == 1 时会把最后一个字节整个漏掉（此前调用方固定传
        // 255 才没踩到，约束本身没有任何东西保护）。
        while o < n {
            let end = (o + 16).min(n);
            self.call(fi, 7, &data[o..end])?
                .ok_or_else(|| "写入配置块失败".to_string())?;
            o += 16;
        }
        self.call(fi, 8, &[])?
            .ok_or_else(|| "提交配置扇区失败".to_string())?;
        Ok(())
    }

    fn load_profile(&mut self) -> Result<(), String> {
        let (fi, sector) = match (self.f_onboard, self.profile_sector) {
            (Some(fi), Some(s)) => (fi, s),
            _ => return Err("设备没有板载配置".into()),
        };
        let data = self.read_sector(fi, sector)?;
        let mut stages = Vec::with_capacity(STAGES);
        for i in 0..STAGES {
            let v = u16::from_le_bytes([data[P_DPI + i * 2], data[P_DPI + i * 2 + 1]]);
            stages.push(v);
        }
        // A trailing 0 means "that stage is not configured".
        while stages.len() > 1 && stages.last() == Some(&0) {
            stages.pop();
        }
        self.default_index = data[P_DEFAULT_INDEX];
        self.polling_ms = data[P_REPORT_RATE].max(1);
        self.stages = stages;
        Ok(())
    }

    /// Reads the sector, lets `edit` change it, and writes it back only when
    /// something actually differs — every write costs an erase cycle.
    fn update_profile<F>(&mut self, edit: F) -> Result<(), String>
    where
        F: FnOnce(&mut [u8]) -> bool,
    {
        let (fi, sector) = match (self.f_onboard, self.profile_sector) {
            (Some(fi), Some(s)) => (fi, s),
            _ => return Err("设备没有板载配置".into()),
        };
        let mut data = self.read_sector(fi, sector)?;
        // 写入是整扇区覆盖：读回的数据一旦损坏，重算 CRC 只会把它“洗白”，
        // 再把坏数据连同其他字段一起写进设备。所以先验证原 CRC，不匹配就
        // 拒绝这次写入。
        if !sector_crc_valid(&data) {
            return Err("配置扇区 CRC 校验失败，已拒绝写入".into());
        }
        if !edit(&mut data) {
            return Ok(());
        }
        let crc = crc16(&data[..CRC_AT]);
        data[CRC_AT..SECTOR_LEN].copy_from_slice(&crc.to_be_bytes());
        self.write_sector(fi, sector, &data)
        // Callers patch the cached fields themselves; re-reading the sector
        // here would add ~200 ms to every click.
    }

    /// 把用户最后选中的档位写回设备扇区。
    ///
    /// 只在"下一次周期读取"时调用，而不是每次点档位都调：写一次意味着读回
    /// 255 字节 + 擦写 + 重算 CRC + commit，而连续切档是查看行为的常态。
    /// 代价是断电前最后几次切换可能没落盘，设备会回到旧档位——运行态不受影响。
    fn flush_default_stage(&mut self) -> Result<(), String> {
        let Some(idx) = self.pending_default else {
            return Ok(());
        };
        let (Some(_), true, Some(_)) = (self.f_onboard, self.profile_ok, self.profile_sector)
        else {
            // 没有可写的板载配置（或布局未验证）：档位只存在于固件运行态。
            self.pending_default = None;
            return Ok(());
        };
        if idx == self.default_index {
            self.pending_default = None;
            return Ok(());
        }
        self.update_profile(|data| {
            if data[P_DEFAULT_INDEX] == idx {
                return false;
            }
            data[P_DEFAULT_INDEX] = idx;
            true
        })?;
        self.default_index = idx;
        self.pending_default = None;
        Ok(())
    }

    /// 落盘失败只记一笔日志：设备本身仍然是好的，这次会话也照常用，只是
    /// 重启后档位回到旧值——这比把设备判成离线温和得多。
    fn settle_default_stage(&mut self) {
        if let Err(e) = self.flush_default_stage() {
            crate::log::line("WARN", &format!("保存默认档位失败：{e}"));
            self.pending_default = None;
        }
    }

    fn set_current_dpi(&mut self, value: u16) -> Result<(), String> {
        let Some(fi) = self.f_dpi else {
            return Ok(());
        };
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

/// 只有 G304 的板载扇区布局经过真机验证；同一接收器配对其他鼠标时
/// 不能按这套偏移写入。
fn profile_layout_supported(name: &str) -> bool {
    name.to_ascii_lowercase().contains("g304")
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
    data[CRC_AT..SECTOR_LEN] == crc16(&data[..CRC_AT]).to_be_bytes()
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

// Same as the Rapoo driver: one worker thread owns both handles.
unsafe impl Send for Hidpp {}

impl Drop for Hidpp {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.event);
            CloseHandle(self.handle);
        }
    }
}

impl Driver for Hidpp {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        // 先把待落的默认档位写掉，再读扇区：这样下面读到的 index、当前 DPI
        // 都是同一次写入之后的状态，不会一会儿新一会儿旧。
        self.settle_default_stage();
        if self.profile_ok && self.f_onboard.is_some() && self.profile_sector.is_some() {
            self.load_profile()?;
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
            caps: if self.profile_ok && self.profile_sector.is_some() {
                Caps::MOUSE_STAGES
            } else {
                Caps::MOUSE_DPI
            },
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
        // 经过驱动的地方——待落的档位放在这里补写，不用再拉一次全量。
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
                let v = v.clamp(self.limits.dpi.0, self.limits.dpi.1);
                let idx = stage as usize;
                if self.profile_ok && self.profile_sector.is_some() {
                    // `stage` comes from outside the driver; this is the
                    // protocol boundary, so reject an out-of-range index
                    // here instead of indexing past the sector (which
                    // would panic the collection thread).
                    if idx >= STAGES {
                        return Err(format!("dpi stage {stage} out of range (0..{STAGES})"));
                    }
                    let b = v.to_le_bytes();
                    let at = P_DPI + idx * 2;
                    self.update_profile(|data| {
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
                self.pending_default = Some(idx as u8);
                snap.stage_active = idx as u8;
                Ok(())
            }
            Cmd::Polling(ms) => {
                if !self.profile_ok || self.profile_sector.is_none() {
                    return Err("该设备不支持调整回报率".into());
                }
                if !self.limits.polling.contains(&ms) {
                    return Err("该型号不支持这个回报率".into());
                }
                self.update_profile(|data| {
                    if data[P_REPORT_RATE] == ms {
                        return false;
                    }
                    data[P_REPORT_RATE] = ms;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Control sector read from the G304: profile 1 at sector 1, then fill,
    /// then the CRC the device stored.
    fn shipped_sector() -> Vec<u8> {
        let mut sector = vec![0x00, 0x01, 0x01, 0x00, 0xFF, 0xFF, 0x00, 0x00];
        sector.resize(CRC_AT, 0xFF);
        sector.extend_from_slice(&[0x58, 0x7C]);
        sector
    }

    #[test]
    fn crc_matches_the_sector_the_device_ships_with() {
        // Reproducing the stored value proves the polynomial, the init value
        // and the byte order used by `write_sector`.
        let crc = crc16(&shipped_sector()[..CRC_AT]);
        assert_eq!(crc, 0x587C);
        assert_eq!(crc.to_be_bytes(), [0x58, 0x7C]);
    }

    /// 写回前先验原 CRC：坏数据不能被重算的校验和“洗白”后再覆盖设备。
    #[test]
    fn rejects_sectors_whose_stored_crc_does_not_match() {
        let good = shipped_sector();
        assert!(sector_crc_valid(&good));

        let mut flipped = good.clone();
        flipped[P_DPI] ^= 0x01;
        assert!(!sector_crc_valid(&flipped));

        let mut swapped = good;
        swapped.swap(CRC_AT, CRC_AT + 1);
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

    /// 接收器是通用的：只有 G304 的扇区布局验证过，其他型号必须降级。
    #[test]
    fn profile_layout_gate_accepts_only_verified_model() {
        assert!(profile_layout_supported(
            "G304 Lightspeed Wireless Gaming Mouse"
        ));
        assert!(profile_layout_supported("g304 lightspeed"));
        assert!(!profile_layout_supported("G502 HERO"));
        assert!(!profile_layout_supported(""));
    }
}
