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
    HidD_GetInputReport, HidD_SetOutputReport,
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

pub struct Rapoo {
    handle: HANDLE,
    /// Last response the device handed us; its repeat is what a stale read
    /// returns after a command the device swallowed.
    last: Cell<[u8; 32]>,
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
                limits,
            })
        }
    }

    fn xfer(&self, frame: &[u8; 32]) -> Result<[u8; 32], String> {
        let read = frame[2] == 0xA4;
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
                            // for byte. 判据的前提——连续两次读应答的 [1..]
                            // 必然不同——写在 docs/protocol.md §3.2：若某次读
                            // 返回与上次逐字节相同的数据（寄存器没变且应答里
                            // 无变化字段），会被误判成过期应答，重发两次后
                            // 报"设备无响应"。新增读命令前先确认应答带变化
                            // 字段，否则要换判据（如应答序号）。
                            let fresh = !read || resp[1..] != prev[1..];
                            if settled && fresh {
                                self.last.set(resp);
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
        // 数据区是 r[4..32]，越界的长度只会变成一次 panic。
        if len > 28 {
            return Err(format!("读取长度超出 32 字节帧（{len}）"));
        }
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA4;
        f[3] = len as u8;
        f[4..8].copy_from_slice(&addr.to_le_bytes());
        let r = self.xfer(&f)?;
        Ok(r[4..4 + len].to_vec())
    }

    fn write_raw(&self, addr: u32, data: &[u8]) -> Result<(), String> {
        // 数据区是 f[8..32]。
        if data.len() > 24 {
            return Err(format!("写入长度超出 32 字节帧（{}）", data.len()));
        }
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA5;
        f[3] = data.len() as u8;
        f[4..8].copy_from_slice(&addr.to_le_bytes());
        f[8..8 + data.len()].copy_from_slice(data);
        self.xfer(&f)?;
        Ok(())
    }

    fn commit(&self) -> Result<(), String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA3;
        self.xfer(&f)?;
        Ok(())
    }

    /// Read-modify-write an arbitrary byte range using aligned 4-byte blocks.
    fn write_field(&self, mut addr: u32, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            let block = addr & !3;
            let off = (addr - block) as usize;
            let n = (4 - off).min(data.len());
            let mut arr = [0u8; 4];
            if off == 0 && n == 4 {
                arr.copy_from_slice(&data[..4]);
            } else {
                let cur = self.read(block, 4)?;
                arr.copy_from_slice(&cur[..4]);
                arr[off..off + n].copy_from_slice(&data[..n]);
            }
            self.write_raw(block, &arr)?;
            addr += n as u32;
            data = &data[n..];
        }
        Ok(())
    }

    fn write_commit(&self, addr: u32, data: &[u8]) -> Result<(), String> {
        self.write_field(addr, data)?;
        self.commit()
    }

    fn battery_level(&self) -> Result<u8, String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xAA;
        let r = self.xfer(&f)?;
        Ok(r[2])
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
                let b = self.read(A_DEB_PRESS, 8)?;
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
                let b = self.read(A_DPI_X + i as u32 * 2, 2)?;
                if let Some(d) = s.dpi.get_mut(i as usize) {
                    *d = u16::from_le_bytes([b[0], b[1]]);
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
        let d = self.read(A_DEB_PRESS, 8)?;
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
        self.battery_level().map(Some)
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
}
