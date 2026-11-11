//! Rapoo VT1 Pro (24AE:1214).
//!
//! 32-byte `0xBA` reports over the vendor collection `0xFF00/0x0E`:
//! `A5 A4` = read, `A5 A5` = write, `A5 A3` = commit, `A5 AA` = battery.
//! Addresses are EEPROM offsets; see `docs/protocol.md`.

use std::cell::Cell;
use std::ffi::c_void;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetAttributes, HidD_GetInputReport, HidD_SetOutputReport, HIDD_ATTRIBUTES,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use super::{Caps, Cmd, Driver, Limits, Snapshot};

const RID: u8 = 0xBA;

const A_POLL: u32 = 0x0880;
const A_LOD: u32 = 0x0884;
const A_DPI_X: u32 = 0x0888;
const A_STAGE_ACTIVE: u32 = 0x0898;
const A_DPI_Y: u32 = 0x08C8;
const A_DEB_PRESS: u32 = 0x08C0;
const A_FLAGS: u32 = 0x08C3;

/// 档位上限。`0x0888`/`0x08C8` 各是 14 字节 = 7 档，档位数本身存在紧邻的
/// `0x0896`（`0x0888 + 14`）。以前这里写 10：真读到大于 8 的档位数时，第 8/9
/// 档的"DPI"会取自档位数与当前档位这两个寄存器，`Cmd::Dpi` 还会反过来把 DPI
/// 写进它们。设备现在是 7 档，但这条约束必须是结构性的。
const MAX_STAGES: u8 = 7;

/// `0x0896` 的原始值（档位数 − 1）→ 实际档位数。
///
/// 用 `saturating_add`：坏帧里这个字节是 0xFF 时，`+ 1` 在 debug 下会 panic，
/// 直接把采集线程打死（虽然能被自愈逻辑重建，但没必要）。
fn stages_from_raw(raw: u8) -> u8 {
    raw.saturating_add(1).min(MAX_STAGES)
}

/// `0xAA` 应答 `[2]` 是百分比；不是百分比的字节一律当成"这一轮没有读数"。
///
/// 开机后第一帧实测回 `0x7F`（127%）——接收器此时还没从鼠标那里拿到电量，
/// 或者干脆是缓冲区里上一条应答被当成了电量。两种情况下把 127 画到界面上
/// 都是假的，宁可暂时显示"电量未知"。
fn battery_percent(raw: u8) -> Option<u8> {
    (raw <= 100).then_some(raw)
}

/// 一条应答算不算有效的电量读数：`[1]` 必须是已知的连接 flag（2.4G = 1、
/// 插线 = 2），`[2]` 必须是百分比。
///
/// 只校验 `[2]` 挡不住旧应答回放：提交应答 `01 7d 00…` 的 `[2]` 是 0，
/// 被当电量就是 0%；`[2] ≤ 100` 的读应答回放同样会放行。flag 不是 1/2
/// 的帧一律不算电量。
fn battery_reply(resp: &[u8; 32]) -> Option<u8> {
    if !matches!(resp[1], 1 | 2) {
        return None;
    }
    battery_percent(resp[2])
}

/// 32 字节帧的公共头：`BA A5 操作码`。
fn frame(op: u8) -> [u8; 32] {
    let mut f = [0u8; 32];
    f[0] = RID;
    f[1] = 0xA5;
    f[2] = op;
    f
}

/// 读帧：`A5 A4 len addr(LE)`，数据由设备填在 `[4..32]`。
fn read_frame(addr: u32, len: usize) -> Result<[u8; 32], String> {
    // 数据区是 r[4..32]，越界的长度只会变成一次 panic。
    if len > 28 {
        return Err(format!("读取长度超出 32 字节帧（{len}）"));
    }
    let mut f = frame(0xA4);
    f[3] = len as u8;
    f[4..8].copy_from_slice(&addr.to_le_bytes());
    Ok(f)
}

/// 写帧：`A5 A5 len addr(LE) data…`，数据区是 `[8..32]`。
fn write_frame(addr: u32, data: &[u8]) -> Result<[u8; 32], String> {
    if data.len() > 24 {
        return Err(format!("写入长度超出 32 字节帧（{}）", data.len()));
    }
    let mut f = frame(0xA5);
    f[3] = data.len() as u8;
    f[4..8].copy_from_slice(&addr.to_le_bytes());
    f[8..8 + data.len()].copy_from_slice(data);
    Ok(f)
}

fn commit_frame() -> [u8; 32] {
    frame(0xA3)
}

fn battery_frame() -> [u8; 32] {
    frame(0xAA)
}

/// 判断一条读应答是否算新应答：内容与上一条不同，或请求本身是上一次
/// 同一地址、同一长度的读取。后者是必要的：寄存器没变时合法应答与
/// 过期回放逐字节相同，只能靠请求区分（见 docs/protocol.md §3.2）。
///
/// 因此同一地址的读取长度必须固定：应答不保证回显长度，`0x08C0` 的 4 字节读
/// 紧跟 8 字节读时，多出的 4 字节若全零，两帧就逐字节相同，合法应答会被
/// 误判成过期。
fn is_fresh(read: bool, resp: &[u8; 32], prev: &[u8; 32], repeated_read: bool) -> bool {
    !read || resp[1..] != prev[1..] || repeated_read
}

/// 把一次任意起点、任意长度的写入切成 4 字节对齐块：返回
/// `(块地址, 块内偏移, 本次写入长度)`。
///
/// 驱动的读-改-写边界完全由它决定，所以单独抽出来离线钉死：非对齐起点、
/// 跨块、末尾不足一块这三种情况在真机上才会踩到，真机又不常在手边。
fn aligned_blocks(addr: u32, len: usize) -> impl Iterator<Item = (u32, usize, usize)> {
    let mut addr = addr;
    let mut left = len;
    std::iter::from_fn(move || {
        if left == 0 {
            return None;
        }
        let block = addr & !3;
        let off = (addr - block) as usize;
        let n = (4 - off).min(left);
        addr += n as u32;
        left -= n;
        Some((block, off, n))
    })
}

pub struct Rapoo {
    handle: HANDLE,
    /// Last response the device handed us; its repeat is what a stale read
    /// returns after a command the device swallowed.
    last: Cell<[u8; 32]>,
    /// 上一次被接受的是哪条读命令（地址、长度）。同一地址的重复读取在内
    /// 容不变时与过期回放逐字节相同，只有这个信息能把两种情况分开。
    last_read: Cell<Option<(u32, usize)>>,
    /// 上一次记进日志的异常电量字节，用来把重复的 WARN 压成一条。
    bad_battery: Cell<Option<u8>>,
    limits: Limits,
}

impl Rapoo {
    pub fn open(path: &[u16], limits: Limits) -> Result<Rapoo, String> {
        let mut path = path.to_vec();
        if path.last() != Some(&0) {
            path.push(0);
        }
        unsafe {
            let h = CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "打开设备失败（错误码 {}）",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Rapoo {
                handle: h,
                last: Cell::new([0u8; 32]),
                last_read: Cell::new(None),
                bad_battery: Cell::new(None),
                limits,
            })
        }
    }

    fn xfer(&self, frame: &[u8; 32]) -> Result<[u8; 32], String> {
        let read = frame[2] == 0xA4;
        let addr = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let len = frame[3] as usize;
        // The dongle occasionally drops a command, so unanswered commands are
        // resent once.
        let budget = if read { 400 } else { 200 };
        for _ in 0..2 {
            unsafe {
                if HidD_SetOutputReport(self.handle, frame.as_ptr() as *const c_void, 32) == 0 {
                    return Err("发送命令失败".into());
                }
            }
            let prev = self.last.get();
            // 上一次成功应答也是同地址同长度的读：此时"内容没变"是正常
            // 结果，不是过期回放。
            let repeated_read = self.last_read.get() == Some((addr, len));
            let start = Instant::now();
            loop {
                let elapsed = start.elapsed();
                if elapsed > Duration::from_millis(budget) {
                    break;
                }
                let mut resp = [0u8; 32];
                resp[0] = RID;
                if unsafe { HidD_GetInputReport(self.handle, resp.as_mut_ptr() as *mut c_void, 32) }
                    != 0
                {
                    match resp[0] {
                        1 => {
                            let settled = elapsed >= Duration::from_millis(40);
                            // A stale answer repeats the previous response byte
                            // for byte. 主判据仍是"与上次不同"；同一地址的
                            // 重复读取允许内容不变，见 `is_fresh`。
                            let fresh = is_fresh(read, &resp, &prev, repeated_read);
                            if settled && fresh {
                                self.last.set(resp);
                                // 写应答也会进 last，它不能作为读的基线。
                                self.last_read.set(read.then_some((addr, len)));
                                sleep(Duration::from_millis(15));
                                return Ok(resp);
                            }
                        }
                        2 => {}
                        other => return Err(format!("设备返回状态 0x{other:02x}")),
                    }
                }
                sleep(Duration::from_millis(2));
            }
        }
        Err("设备无响应（超时）".into())
    }

    fn read(&self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        let f = read_frame(addr, len)?;
        let r = self.xfer(&f)?;
        Ok(r[4..4 + len].to_vec())
    }

    fn write_raw(&self, addr: u32, data: &[u8]) -> Result<(), String> {
        self.xfer(&write_frame(addr, data)?)?;
        Ok(())
    }

    fn commit(&self) -> Result<(), String> {
        self.xfer(&commit_frame())?;
        Ok(())
    }

    /// Read-modify-write an arbitrary byte range using aligned 4-byte blocks.
    fn write_field(&self, addr: u32, data: &[u8]) -> Result<(), String> {
        let mut done = 0usize;
        for (block, off, n) in aligned_blocks(addr, data.len()) {
            let mut arr = [0u8; 4];
            if off == 0 && n == 4 {
                arr.copy_from_slice(&data[done..done + 4]);
            } else {
                let cur = self.read(block, 4)?;
                arr.copy_from_slice(&cur[..4]);
                arr[off..off + n].copy_from_slice(&data[done..done + n]);
            }
            self.write_raw(block, &arr)?;
            done += n;
        }
        Ok(())
    }

    fn write_commit(&self, addr: u32, data: &[u8]) -> Result<(), String> {
        self.write_field(addr, data)?;
        self.commit()
    }

    /// 电量查询。返回 `None` 表示这一轮拿到的不是有效百分比，调用方保留上
    /// 一次的值即可。
    ///
    /// 与读命令不同，`0xAA` 的应答里没有能判定新旧的字段，`xfer` 在 40 ms
    /// 就可能把缓冲区里**上一条**应答交出来（开机后第一条命令尤其如此：
    /// §3.6 说空闲后第一条要 150–250 ms 才回，40 ms 时缓冲区里还是上一轮
    /// 快照的读应答）。所以这里不能只靠 `xfer` 的接受条件，还要对内容做一
    /// 次校验（`battery_reply`）：连接 flag 或百分比不对就不算数，等接收器
    /// 把真正的应答放进来再问一次。
    fn battery_level(&self) -> Result<Option<u8>, String> {
        let f = battery_frame();
        let mut r = self.xfer(&f)?;
        if battery_reply(&r).is_none() {
            sleep(Duration::from_millis(200));
            r = self.xfer(&f)?;
        }
        match battery_reply(&r) {
            Some(p) => {
                self.bad_battery.set(None);
                Ok(Some(p))
            }
            None => {
                // 只在异常值变化时记一条：否则每 20 秒一条，把日志刷满。
                if self.bad_battery.replace(Some(r[2])) != Some(r[2]) {
                    crate::log::line(
                        "WARN",
                        &format!(
                            "电量应答异常：[1]=0x{:02x} [2]=0x{:02x}，帧头 {:02x?}",
                            r[1],
                            r[2],
                            &r[..8]
                        ),
                    );
                }
                Ok(None)
            }
        }
    }

    fn set_flag(&self, bit: u8, disabled: bool) -> Result<(), String> {
        let mut b = self.read(A_FLAGS - 3, 4)?;
        if disabled {
            b[3] |= bit;
        } else {
            b[3] &= !bit;
        }
        self.write_commit(A_FLAGS - 3, &b)
    }

    /// One re-read per changed block after a burst, never a full snapshot:
    /// every extra round trip is time the user's next click waits for.
    fn patch(&self, change: Change, s: &mut Snapshot) -> Result<(), String> {
        match change {
            Change::Poll => {
                let b = self.read(A_POLL, 4)?;
                s.polling_ms = b[0];
            }
            Change::Lm => {
                let b = self.read(A_LOD, 4)?;
                s.lod = b[0];
                s.motion = b[1] != 0;
            }
            Change::Deb => {
                // 与 snapshot 一样读 4 字节：同一地址的读取长度必须固定，
                // 否则内容不变的合法应答会被判成过期（见 `is_fresh`）。
                let b = self.read(A_DEB_PRESS, 4)?;
                s.debounce_press = b[0];
                s.debounce_release = b[1];
                s.sleep_min = b[2];
                s.line_corr = b[3] & 0x01 == 0;
                s.wave_corr = b[3] & 0x02 == 0;
            }
            Change::Active => {
                let b = self.read(A_STAGE_ACTIVE, 4)?;
                s.stage_active = b[0].min(s.stage_count.saturating_sub(1));
            }
            Change::Dpi(i) => {
                // 读整块而不是 2 字节：`write_field` 的读-改-写也读这个块，
                // 同址同长（4）重复读取才能被 `is_fresh` 当成合法应答。
                let addr = A_DPI_X + i as u32 * 2;
                let block = addr & !3;
                let off = (addr - block) as usize;
                let b = self.read(block, 4)?;
                if let Some(d) = s.dpi.get_mut(i as usize) {
                    *d = u16::from_le_bytes([b[off], b[off + 1]]);
                }
            }
        }
        Ok(())
    }
}

// The raw HANDLE is only ever touched by the worker thread that owns this
// driver, so moving it to that thread is sound.
unsafe impl Send for Rapoo {}

impl Drop for Rapoo {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Which block needs re-reading after a command, so we avoid a full snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Poll,
    Lm,
    Deb,
    Active,
    Dpi(u8),
}

impl Driver for Rapoo {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        let p = self.read(A_POLL, 4)?;
        let lm = self.read(A_LOD, 4)?;
        let d = self.read(A_DEB_PRESS, 4)?;
        let x = self.read(A_DPI_X, 24)?;
        let y = self.read(A_DPI_Y, 24)?;

        let stage_count = stages_from_raw(x[14]);
        let mut dpi = Vec::with_capacity(stage_count as usize);
        for i in 0..stage_count as usize {
            dpi.push(u16::from_le_bytes([x[i * 2], x[i * 2 + 1]]));
        }
        let mut dpi_y = Vec::with_capacity(stage_count as usize);
        for i in 0..stage_count as usize {
            dpi_y.push(u16::from_le_bytes([y[i * 2], y[i * 2 + 1]]));
        }
        for (a, b) in dpi.iter_mut().zip(dpi_y.iter()) {
            if *a == 0 {
                *a = *b;
            }
        }

        Ok(Snapshot {
            model: String::new(),
            caps: Caps::MOUSE_FULL,
            polling_ms: p[0],
            lod: lm[0],
            motion: lm[1] != 0,
            stage_count,
            stage_active: x[16].min(stage_count.saturating_sub(1)),
            dpi,
            dpi_min: self.limits.dpi.0,
            dpi_max: self.limits.dpi.1,
            debounce_press: d[0],
            debounce_release: d[1],
            sleep_min: d[2],
            line_corr: d[3] & 0x01 == 0,
            wave_corr: d[3] & 0x02 == 0,
        })
    }

    fn battery(&mut self) -> Result<Option<u8>, String> {
        self.battery_level()
    }

    fn apply(&mut self, cmd: Cmd, snap: &mut Snapshot) -> Result<(), String> {
        let change = match cmd {
            Cmd::Refresh => return Ok(()),
            Cmd::Polling(ms) => {
                if !self.limits.polling.contains(&ms) {
                    return Err(format!("不支持的回报率档位：{ms} ms"));
                }
                let mut b = self.read(A_POLL, 4)?;
                b[0] = ms;
                self.write_commit(A_POLL, &b)?;
                Change::Poll
            }
            Cmd::Motion(on) => {
                let mut b = self.read(A_LOD, 4)?;
                b[1] = on as u8;
                self.write_commit(A_LOD, &b)?;
                Change::Lm
            }
            Cmd::Lod(v) => {
                let v = v.clamp(self.limits.lod.0, self.limits.lod.1);
                let mut b = self.read(A_LOD, 4)?;
                b[0] = v;
                self.write_commit(A_LOD, &b)?;
                Change::Lm
            }
            Cmd::Line(on) => {
                self.set_flag(0x01, !on)?;
                Change::Deb
            }
            Cmd::Wave(on) => {
                self.set_flag(0x02, !on)?;
                Change::Deb
            }
            Cmd::Debounce(v, press) => {
                let v = v.clamp(self.limits.debounce.0, self.limits.debounce.1);
                let mut b = self.read(A_DEB_PRESS, 4)?;
                b[if press { 0 } else { 1 }] = v;
                self.write_commit(A_DEB_PRESS, &b)?;
                Change::Deb
            }
            Cmd::Sleep(m) => {
                let m = m.clamp(self.limits.sleep.0, self.limits.sleep.1);
                let mut b = self.read(A_DEB_PRESS, 4)?;
                b[2] = m;
                self.write_commit(A_DEB_PRESS, &b)?;
                Change::Deb
            }
            Cmd::Activate(i) => {
                let i = i.min(snap.stage_count.saturating_sub(1));
                let mut b = self.read(A_STAGE_ACTIVE, 4)?;
                b[0] = i;
                self.write_commit(A_STAGE_ACTIVE, &b)?;
                Change::Active
            }
            Cmd::Dpi(stage, value) => {
                // `stage` selects a fixed 2-byte EEPROM slot; an out-of-range
                // index would read-modify-write unrelated cells. Reject it
                // here at the protocol boundary rather than trusting the
                // caller to pass a stage the device actually has.
                let count = snap.stage_count.clamp(1, MAX_STAGES) as u32;
                if stage as u32 >= count {
                    return Err(format!("dpi stage {stage} out of range (0..{count})"));
                }
                let value = value.clamp(self.limits.dpi.0, self.limits.dpi.1);
                let bytes = value.to_le_bytes();
                self.write_field(A_DPI_X + stage as u32 * 2, &bytes)?;
                self.write_field(A_DPI_Y + stage as u32 * 2, &bytes)?;
                self.commit()?;
                Change::Dpi(stage)
            }
        };
        self.patch(change, snap)
    }

    fn link_alive(&self) -> bool {
        unsafe {
            let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
            attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
            HidD_GetAttributes(self.handle, &mut attr) != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7 档是协议上限（DPI X/Y 各 14 字节）。再大就意味着"档位数"那个字节
    /// 其实是第 8 档的 DPI 低字节，读会读错、写会写坏别的寄存器。
    #[test]
    fn stage_count_never_exceeds_the_dpi_table() {
        assert_eq!(stages_from_raw(0), 1);
        assert_eq!(stages_from_raw(6), 7);
        assert_eq!(stages_from_raw(7), 7);
        // 坏帧：debug 下也不能 panic。
        assert_eq!(stages_from_raw(0xFF), 7);
    }

    /// 开机后第一帧 0xAA 应答的 `[2]` 是 0x7F，界面曾经显示成 127%。
    #[test]
    fn battery_byte_out_of_range_is_not_a_percentage() {
        assert_eq!(battery_percent(0), Some(0));
        assert_eq!(battery_percent(93), Some(93));
        assert_eq!(battery_percent(100), Some(100));
        assert_eq!(battery_percent(101), None);
        assert_eq!(battery_percent(0x7F), None);
        assert_eq!(battery_percent(0xFF), None);
    }

    /// 旧应答回放不只会给出 `> 100` 的值：提交应答 `01 7d 00…` 或读应答的
    /// `[2]` 可能是 0，`[1]` 没有连接 flag，这种帧不能当成电量。
    #[test]
    fn battery_reply_needs_a_known_link_flag() {
        let mut r = [0u8; 32];
        r[0] = 1;
        r[1] = 2;
        r[2] = 71;
        assert_eq!(battery_reply(&r), Some(71));
        r[1] = 1;
        r[2] = 0;
        assert_eq!(battery_reply(&r), Some(0));
        // 提交应答 01 7d 00… 被回放。
        r[1] = 0x7d;
        r[2] = 0;
        assert_eq!(battery_reply(&r), None);
        // flag 对但百分比超范围。
        r[1] = 2;
        r[2] = 0x7F;
        assert_eq!(battery_reply(&r), None);
    }

    /// 帧头是协议契约：`BA A5 op`，地址小端写在 `[4..8]`，长度写在 `[3]`。
    /// 这些字节以前散在 `read`/`write_raw`/`commit` 里，只有真机能验。
    #[test]
    fn frames_encode_the_wire_format() {
        let f = read_frame(0x0880, 4).expect("4 bytes fit");
        assert_eq!(&f[..4], &[RID, 0xA5, 0xA4, 4]);
        assert_eq!(&f[4..8], &0x0880u32.to_le_bytes());
        assert!(read_frame(0x0880, 29).is_err());

        let f = write_frame(0x08C3, &[1, 2, 3]).expect("3 bytes fit");
        assert_eq!(&f[..4], &[RID, 0xA5, 0xA5, 3]);
        assert_eq!(&f[4..8], &0x08C3u32.to_le_bytes());
        assert_eq!(&f[8..11], &[1, 2, 3]);
        assert!(f[11..].iter().all(|&b| b == 0));
        assert!(write_frame(0, &[0u8; 25]).is_err());

        assert_eq!(&commit_frame()[..3], &[RID, 0xA5, 0xA3]);
        assert_eq!(&battery_frame()[..3], &[RID, 0xA5, 0xAA]);
    }

    /// 连续两次读同一寄存器且值没变时，应答与上一条逐字节相同；这必须
    /// 当成合法应答，否则第二次修改（或写后回读）会被判成"设备无响应"。
    #[test]
    fn identical_reply_is_only_stale_for_a_different_read() {
        let prev = [0x11u8; 32];
        let same = prev;
        assert!(is_fresh(false, &same, &prev, false));
        assert!(!is_fresh(true, &same, &prev, false));
        assert!(is_fresh(true, &same, &prev, true));

        let mut changed = prev;
        changed[4] = 0x22;
        assert!(is_fresh(true, &changed, &prev, false));
    }

    /// 4 字节对齐切分：起点不在块首时先补齐，末尾不足一块按实际长度切。
    /// `write_field` 的每一次读-改-写都由它决定，偏移算错会写到相邻字段。
    #[test]
    fn aligned_blocks_covers_any_range_without_gaps() {
        assert_eq!(aligned_blocks(0x0888, 0).collect::<Vec<_>>(), vec![]);
        assert_eq!(
            aligned_blocks(0x0888, 2).collect::<Vec<_>>(),
            vec![(0x0888, 0, 2)]
        );
        assert_eq!(
            aligned_blocks(0x0881, 7).collect::<Vec<_>>(),
            vec![(0x0880, 1, 3), (0x0884, 0, 4)]
        );
        assert_eq!(
            aligned_blocks(0x0896, 1).collect::<Vec<_>>(),
            vec![(0x0894, 2, 1)]
        );

        // 任意起点/长度：每个块都从块首 + off 开始，拼起来正好覆盖整段。
        let (addr, len) = (0x087F, 11);
        let mut at = addr;
        for (block, off, n) in aligned_blocks(addr, len) {
            assert_eq!(block + off as u32, at);
            assert_eq!(block % 4, 0);
            assert!(off + n <= 4);
            at += n as u32;
        }
        assert_eq!(at, addr + len as u32);
    }
}
