use std::cell::Cell;
use std::ffi::c_void;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetInputReport,
    HidD_GetPreparsedData, HidD_SetOutputReport, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const VID: u16 = 0x24AE;
const PID: u16 = 0x1214;
const USAGE_PAGE: u16 = 0xFF00;
const USAGE: u16 = 0x000E;
const RID: u8 = 0xBA;

pub const A_POLL: u32 = 0x0880;
pub const A_LOD: u32 = 0x0884;
pub const A_DPI_X: u32 = 0x0888;
pub const A_STAGE_ACTIVE: u32 = 0x0898;
pub const A_DPI_Y: u32 = 0x08C8;
pub const A_DEB_PRESS: u32 = 0x08C0;
pub const A_FLAGS: u32 = 0x08C3;

pub struct Device {
    handle: HANDLE,
    /// Last response the device handed us; its repeat is what a stale read returns.
    last: Cell<[u8; 32]>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub polling_ms: u8,
    pub lod: u8,
    pub motion: bool,
    pub stage_count: u8,
    pub stage_active: u8,
    pub dpi: Vec<u16>,
    pub debounce_press: u8,
    pub debounce_release: u8,
    pub sleep_min: u8,
    pub line_corr: bool,
    pub wave_corr: bool,
}

unsafe fn wide_path(p: *const u16) -> Vec<u16> {
    let mut n = 0usize;
    while *p.add(n) != 0 {
        n += 1;
    }
    std::slice::from_raw_parts(p, n + 1).to_vec()
}

unsafe fn device_matches(path: *const u16) -> bool {
    let h = CreateFileW(
        path,
        GENERIC_READ | GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        std::ptr::null_mut(),
    );
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut ok = false;
    let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
    attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    if HidD_GetAttributes(h, &mut attr) != 0
        && attr.VendorID == VID
        && attr.ProductID == PID
    {
        let mut pp: isize = 0;
        if HidD_GetPreparsedData(h, &mut pp) != 0 {
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            if HidP_GetCaps(pp, &mut caps) >= 0
                && caps.UsagePage == USAGE_PAGE
                && caps.Usage == USAGE
            {
                ok = true;
            }
            HidD_FreePreparsedData(pp);
        }
    }
    CloseHandle(h);
    ok
}

fn find_path() -> Option<Vec<u16>> {
    unsafe {
        let mut guid: windows_sys::core::GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == -1 {
            return None;
        }
        let mut found = None;
        let mut index = 0u32;
        loop {
            let mut did: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut did) == 0 {
                break;
            }
            index += 1;
            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &did,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            );
            if needed == 0 {
                continue;
            }
            let mut buf = vec![0u8; needed as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize =
                std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &did,
                detail,
                needed,
                &mut needed,
                std::ptr::null_mut(),
            ) == 0
            {
                continue;
            }
            let path_ptr = buf.as_ptr().add(4) as *const u16;
            if device_matches(path_ptr) {
                found = Some(wide_path(path_ptr));
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        found
    }
}

impl Device {
    pub fn open() -> Result<Device, String> {
        let path = find_path().ok_or_else(|| "未检测到 VT1 Pro 接收器".to_string())?;
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
                return Err(format!("打开设备失败（错误码 {}）", std::io::Error::last_os_error()));
            }
            Ok(Device {
                handle: h,
                last: Cell::new([0u8; 32]),
            })
        }
    }

    fn xfer(&self, frame: &[u8; 32]) -> Result<[u8; 32], String> {
        let read = frame[2] == 0xA4;
        // The dongle occasionally drops a command, so unanswered commands are resent.
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
                            // A stale answer repeats the previous response byte for byte.
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

    pub fn read(&self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
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

    pub fn commit(&self) -> Result<(), String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA3;
        self.xfer(&f)?;
        Ok(())
    }

    /// Read-modify-write an arbitrary byte range using aligned 4-byte blocks.
    pub fn write_field(&self, mut addr: u32, mut data: &[u8]) -> Result<(), String> {
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

    pub fn write_commit(&self, addr: u32, data: &[u8]) -> Result<(), String> {
        self.write_field(addr, data)?;
        self.commit()
    }

    pub fn battery(&self) -> Result<u8, String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xAA;
        let r = self.xfer(&f)?;
        Ok(r[2])
    }

    pub fn snapshot(&self) -> Result<Snapshot, String> {
        let p = self.read(A_POLL, 4)?;
        let lm = self.read(A_LOD, 4)?;
        let d = self.read(A_DEB_PRESS, 8)?;
        let x = self.read(A_DPI_X, 24)?;
        let y = self.read(A_DPI_Y, 24)?;

        let stage_count = (x[14] + 1).min(10);
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
            polling_ms: p[0],
            lod: lm[0],
            motion: lm[1] != 0,
            stage_count,
            stage_active: x[16].min(stage_count.saturating_sub(1)),
            dpi,
            debounce_press: d[0],
            debounce_release: d[1],
            sleep_min: d[2],
            line_corr: d[3] & 0x01 == 0,
            wave_corr: d[3] & 0x02 == 0,
        })
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
