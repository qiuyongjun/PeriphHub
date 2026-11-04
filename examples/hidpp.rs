// 诊断工具：Logitech HID++ 2.0 只读探测。
// cargo run --example hidpp              # 扫描接收器上的设备，列出全部 feature
// cargo run --example hidpp -- 1         # 只探测设备索引 1
//
// 通过 0xFF00/0x0002 长报文通道（report 0x11）通信：G304 的接收器在 7 字节
// short 通道上只回 HID++ 1.0 的 0x8F 错误帧，所有功能都走长通道。
// 只发送读取类功能，不写配置。

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

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
    fn HidD_SetOutputReport(h: HANDLE, buf: *const c_void, len: u32) -> i32;
}

const ERROR_IO_PENDING: u32 = 997;
const SW_ID: u8 = 0x0A;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: HANDLE,
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

unsafe fn wide_path(p: *const u16) -> Vec<u16> {
    let mut n = 0usize;
    while *p.add(n) != 0 {
        n += 1;
    }
    std::slice::from_raw_parts(p, n + 1).to_vec()
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

struct Iface {
    instance: String,
    path: Vec<u16>,
    usage_page: u16,
    usage: u16,
    out_len: u16,
    in_len: u16,
}

fn interfaces() -> Vec<Iface> {
    let mut out = Vec::new();
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
            return out;
        }
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
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, detail, needed, &mut needed, &mut info)
                == 0
            {
                continue;
            }
            let path = wide_path(buf.as_ptr().add(4) as *const u16);
            let mut idbuf = [0u16; 512];
            let instance = if SetupDiGetDeviceInstanceIdW(
                set,
                &info,
                idbuf.as_mut_ptr(),
                idbuf.len() as u32,
                std::ptr::null_mut(),
            ) != 0
            {
                wide_to_string(&idbuf)
            } else {
                String::new()
            };
            if !instance.to_ascii_uppercase().contains("VID_046D") {
                continue;
            }
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
                continue;
            }
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            let mut pp: isize = 0;
            if HidD_GetPreparsedData(h, &mut pp) != 0 {
                HidP_GetCaps(pp, &mut caps);
                HidD_FreePreparsedData(pp);
            }
            CloseHandle(h);
            out.push(Iface {
                instance,
                path,
                usage_page: caps.UsagePage,
                usage: caps.Usage,
                out_len: caps.OutputReportByteLength,
                in_len: caps.InputReportByteLength,
            });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

struct Dev {
    h: HANDLE,
    ev: HANDLE,
    out_len: usize,
    in_len: usize,
    buf: Vec<u8>,
}

impl Dev {
    fn open(path: &[u16], out_len: usize, in_len: usize) -> Option<Dev> {
        unsafe {
            let h = CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let ev = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
            Some(Dev {
                h,
                ev,
                out_len,
                in_len,
                buf: vec![0u8; in_len.max(out_len)],
            })
        }
    }

    /// One request/response round trip. `None` means "no answer in time".
    fn xfer(&mut self, frame: &[u8], ms: u32) -> Option<Vec<u8>> {
        unsafe {
            if HidD_SetOutputReport(self.h, frame.as_ptr() as *const c_void, self.out_len as u32)
                == 0
            {
                return None;
            }
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(ms as u64) {
                ResetEvent(self.ev);
                let mut ov = Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    h_event: self.ev,
                };
                let mut read = 0u32;
                let ok = ReadFile(
                    self.h,
                    self.buf.as_mut_ptr() as *mut c_void,
                    self.in_len as u32,
                    &mut read,
                    &mut ov,
                );
                if ok == 0 {
                    if GetLastError() != ERROR_IO_PENDING {
                        return None;
                    }
                    if WaitForSingleObject(self.ev, ms) != 0 {
                        CancelIo(self.h);
                        return None;
                    }
                    GetOverlappedResult(self.h, &mut ov, &mut read, 0);
                }
                if read >= 4 {
                    let b = self.buf[..read as usize].to_vec();
                    if b[1] == frame[1] && (b[2] == frame[2] || b[2] == 0xFF) {
                        return Some(b);
                    }
                }
            }
            None
        }
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.ev);
            CloseHandle(self.h);
        }
    }
}

/// One HID++ 2.0 call on the long channel.
fn call(dev: &mut Dev, dev_idx: u8, idx: u8, func: u8, params: &[u8], ms: u32) -> Option<Vec<u8>> {
    let mut f = vec![0u8; dev.out_len];
    f[0] = 0x11;
    f[1] = dev_idx;
    f[2] = idx;
    f[3] = (func << 4) | SW_ID;
    for (i, p) in params.iter().enumerate() {
        f[4 + i] = *p;
    }
    dev.xfer(&f, ms)
}

fn get_feature(dev: &mut Dev, dev_idx: u8, feature_id: u16) -> Option<(u8, u8, u8)> {
    let r = call(dev, dev_idx, 0x00, 0, &feature_id.to_be_bytes(), 500)?;
    if r[2] == 0xFF {
        return None;
    }
    Some((r[4], r[5], r[6]))
}

fn main() {
    let want_dev: Option<u8> = std::env::args().nth(1).and_then(|s| s.parse().ok());
    let ifaces = interfaces();
    if ifaces.is_empty() {
        println!("没找到 Logitech 接收器（VID 046D）");
        return;
    }
    for i in &ifaces {
        println!(
            "{}  up=0x{:04X} u=0x{:02X} in={} out={}",
            i.instance, i.usage_page, i.usage, i.in_len, i.out_len
        );
    }

    let Some(long) = ifaces
        .iter()
        .find(|i| i.usage_page == 0xFF00 && i.usage == 0x0002)
    else {
        println!("没有 HID++ 长报文通道（0xFF00/0x0002）");
        return;
    };
    let mut dev = match Dev::open(&long.path, long.out_len as usize, long.in_len as usize) {
        Some(d) => d,
        None => {
            println!("打开长报文通道失败");
            return;
        }
    };

    let mut device: Option<u8> = None;
    for idx in 0..=6u8 {
        match call(&mut dev, idx, 0x00, 0, &[], 400) {
            Some(r) => println!(
                "设备索引 {idx}: root.getProtocolVersion -> {} (raw {})",
                if r[2] == 0xFF {
                    "错误".to_string()
                } else {
                    format!("{}.{}", r[4], r[5])
                },
                hex(&r)
            ),
            None => println!("设备索引 {idx}: 无应答"),
        }
        if device.is_none() && want_dev.is_none_or(|w| w == idx) {
            if let Some(r) = call(&mut dev, idx, 0x00, 0, &[], 400) {
                if r[2] != 0xFF {
                    device = Some(idx);
                }
            }
        }
    }
    let Some(di) = device else {
        println!("\n没有任何设备索引响应 HID++ 2.0 请求");
        return;
    };
    println!("\n=== 使用设备索引 {di} ===\n");

    const FEATURES: &[(u16, &str)] = &[
        (0x0003, "DEVICE_FW_VERSION"),
        (0x0005, "DEVICE_NAME"),
        (0x0020, "CONFIG_CHANGE"),
        (0x0021, "CRYPTO_ID"),
        (0x1000, "BATTERY_LEVEL_STATUS"),
        (0x1001, "BATTERY_VOLTAGE"),
        (0x1004, "UNIFIED_BATTERY"),
        (0x1802, "LED_CONTROL"),
        (0x1814, "CHANGE_HOST"),
        (0x1B04, "REPROG_CONTROLS_V4"),
        (0x2110, "BT_HOST_INFO"),
        (0x2150, "ADJUSTABLE_REPORT_RATE"),
        (0x2201, "ADJUSTABLE_DPI"),
        (0x2202, "POINTER_SPEED"),
        (0x2205, "POINTER_MOTION"),
        (0x8100, "ONBOARD_PROFILES"),
        (0x8110, "PER_KEY_LIGHTING"),
        (0x8060, "MODE_STATUS"),
        (0x8061, "MODE_STATUS2"),
    ];
    let mut found: Vec<(u16, u8)> = Vec::new();
    for (id, name) in FEATURES {
        if let Some((idx, ty, ver)) = get_feature(&mut dev, di, *id) {
            println!("feature 0x{id:04X} {name:<22} -> idx 0x{idx:02X} type {ty} ver {ver}");
            found.push((*id, idx));
        }
    }
    let idx_of = |id: u16| found.iter().find(|f| f.0 == id).map(|f| f.1);

    if let Some(fi) = idx_of(0x0005) {
        if let Some(r) = call(&mut dev, di, fi, 0, &[], 500) {
            let count = r[4];
            let mut s = Vec::new();
            for off in 0..count {
                if let Some(r) = call(&mut dev, di, fi, 1, &[off], 500) {
                    s.extend_from_slice(&r[4..4 + 3.min(r.len() - 4)]);
                }
            }
            let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
            println!("\n设备名: {:?}", String::from_utf8_lossy(&s[..end]));
        }
    }

    if let Some(fi) = idx_of(0x0003) {
        if let Some(r) = call(&mut dev, di, fi, 0, &[], 500) {
            println!("固件实体数 {}: {}", r[4], hex(&r[4..r.len().min(14)]));
        }
    }

    if let Some(fi) = idx_of(0x1004) {
        println!(
            "UNIFIED_BATTERY caps   = {:?}",
            call(&mut dev, di, fi, 0, &[], 500)
        );
        println!(
            "UNIFIED_BATTERY status = {:?}",
            call(&mut dev, di, fi, 1, &[], 500)
        );
    }
    if let Some(fi) = idx_of(0x1000) {
        println!(
            "BATTERY_LEVEL_STATUS   = {:?}",
            call(&mut dev, di, fi, 0, &[], 500)
        );
    }
    if let Some(fi) = idx_of(0x1001) {
        println!(
            "BATTERY_VOLTAGE        = {:?}",
            call(&mut dev, di, fi, 0, &[], 500)
        );
    }

    if let Some(fi) = idx_of(0x2201) {
        let count = call(&mut dev, di, fi, 0, &[], 500);
        println!("\nADJUSTABLE_DPI sensorCount = {:?}", count);
        if let Some(c) = &count {
            for s in 0..c[4] as usize {
                println!(
                    "  sensor {s} dpiList = {:?}",
                    call(&mut dev, di, fi, 1, &[s as u8], 500)
                );
                println!(
                    "  sensor {s} current = {:?}",
                    call(&mut dev, di, fi, 2, &[s as u8], 500)
                );
            }
        }
    }

    if let Some(fi) = idx_of(0x2202) {
        println!(
            "\nPOINTER_SPEED = {:?}",
            call(&mut dev, di, fi, 0, &[], 500)
        );
    }

    if let Some(fi) = idx_of(0x8100) {
        dump_profiles(&mut dev, di, fi);
    }
}

/// Reads the onboard control sector and every profile it points at, then
/// prints a hexdump plus the decoded fields. Read-only: no sector is written.
fn dump_profiles(dev: &mut Dev, di: u8, fi: u8) {
    let info = match call(dev, di, fi, 0, &[], 500) {
        Some(r) => r,
        None => return,
    };
    println!(
        "\nONBOARD_PROFILES info = {}  (memory {} / format {} / sectors {} / size {})",
        hex(&info[4..]),
        info[4],
        info[5],
        info[10],
        u16::from_be_bytes([info[11], info[12]])
    );

    let ctrl = match read_sector(dev, di, fi, 0) {
        Some(c) => c,
        None => return,
    };
    let mut profiles = Vec::new();
    for entry in ctrl.chunks(4) {
        if entry.len() < 4 || entry[0] == 0xFF {
            break;
        }
        println!(
            "  控制项: 扇区 {} 启用 {}",
            u16::from_be_bytes([entry[0], entry[1]]),
            entry[2]
        );
        if entry[2] != 0 {
            profiles.push(u16::from_be_bytes([entry[0], entry[1]]));
        }
    }

    for sector in profiles {
        let Some(data) = read_sector(dev, di, fi, sector) else {
            continue;
        };
        println!("\n== 配置扇区 {sector} ==");
        for (i, row) in data.chunks(16).enumerate() {
            println!("  {:03X}: {}", i * 16, hex(row));
        }
        let dpi: Vec<u16> = (0..5)
            .map(|k| u16::from_le_bytes([data[3 + k * 2], data[4 + k * 2]]))
            .collect();
        let stored = u16::from_be_bytes([data[253], data[254]]);
        println!(
            "  回报率 {} ms / 默认档 {} / DPI {:?} / CRC 存储 {:04X} 计算 {:04X}",
            data[0],
            data[1],
            dpi,
            stored,
            crc16(&data[..253])
        );
    }
}

fn crc16(data: &[u8]) -> u16 {
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

/// 255-byte sector: 16-byte reads up to 239, then one at 239 whose first byte
/// repeats 239 — a read starting at 240 is rejected by the firmware.
fn read_sector(dev: &mut Dev, di: u8, fi: u8, sector: u16) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(255);
    let mut off = 0usize;
    while off < 240 {
        let r = call(
            dev,
            di,
            fi,
            5,
            &[
                (sector >> 8) as u8,
                (sector & 0xFF) as u8,
                (off >> 8) as u8,
                (off & 0xFF) as u8,
            ],
            600,
        )?;
        out.extend_from_slice(&r[4..20]);
        off += 16;
    }
    let r = call(
        dev,
        di,
        fi,
        5,
        &[(sector >> 8) as u8, (sector & 0xFF) as u8, 0, 239],
        600,
    )?;
    out.extend_from_slice(&r[5..20]);
    Some(out)
}
