// 诊断工具：判定有线 F87（258A:010C）520 字节 feature 通道的性质。
//
// 三个实验：
//   A. 连续两次回读（中间不写）→ 相同 = 缓冲区；不同 = 自动推进的数据流
//   B. 用同一请求连写两次再回读 → 相同 = 地址生效；不同 = 地址被忽略
//   C. 命令通道带 subcmd（0x00/0x01/0x0A）+ 监听 4 字节输入报表
//
// 全程只发读请求，不写配置。
//   cargo run --example f87_wired_probe

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetInputReport, HidD_GetPreparsedData, HidD_SetFeature, HidP_GetCaps, HIDD_ATTRIBUTES,
    HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const BUF: usize = 520;
const PAYLOAD: usize = 378;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Feature(u16),
    Input(u16),
}

unsafe fn find(vid: u16, pid: u16, kind: Kind) -> Option<Vec<u16>> {
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
    let mut index = 0u32;
    let mut found = None;
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
        // 这块内存会被当成 `SP_DEVICE_INTERFACE_DETAIL_DATA_W` 写，`Vec<u8>`
        // 只保证 1 字节对齐；用 u64 承载保证对齐（同 src/hid.rs）。
        let mut buf = vec![0u64; needed as usize / 8 + 1];
        let base = buf.as_mut_ptr() as *mut u8;
        let detail = base as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        if SetupDiGetDeviceInterfaceDetailW(set, &did, detail, needed, &mut needed, &mut info) == 0
        {
            continue;
        }
        let off = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        let p = base.add(off) as *const u16;
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        let path: Vec<u16> = std::slice::from_raw_parts(p, n + 1).to_vec();

        let h = CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
        attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
        let mut report_len = 0u16;
        let mut pp: isize = 0;
        if HidD_GetAttributes(h, &mut attr) != 0 && HidD_GetPreparsedData(h, &mut pp) != 0 {
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            if HidP_GetCaps(pp, &mut caps) >= 0 {
                report_len = match kind {
                    Kind::Feature(_) => caps.FeatureReportByteLength,
                    Kind::Input(_) => caps.InputReportByteLength,
                };
            }
            HidD_FreePreparsedData(pp);
        }
        CloseHandle(h);
        let want = match kind {
            Kind::Feature(w) | Kind::Input(w) => w,
        };
        if attr.VendorID == vid && attr.ProductID == pid && report_len == want {
            found = Some(path);
            break;
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    found
}

fn open(path: &[u16]) -> Option<HANDLE> {
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    (h != INVALID_HANDLE_VALUE).then_some(h)
}

fn read520(h: HANDLE) -> Option<[u8; BUF]> {
    let mut buf = [0u8; BUF];
    buf[0] = 0x06;
    let ok = unsafe { HidD_GetFeature(h, buf.as_mut_ptr() as *mut c_void, BUF as u32) };
    (ok != 0).then_some(buf)
}

/// 写一条"读"请求：表头 06 08 <addr LE16> <flag> <len LE16>。
fn request(h: HANDLE, addr: u16, flag: u8) -> bool {
    let mut req = [0u8; BUF];
    req[0] = 0x06;
    req[1] = 0x08;
    req[2..4].copy_from_slice(&addr.to_le_bytes());
    req[4] = flag;
    req[6..8].copy_from_slice(&(PAYLOAD as u16).to_le_bytes());
    unsafe { HidD_SetFeature(h, req.as_mut_ptr() as *mut c_void, BUF as u32) != 0 }
}

fn head(r: &[u8; BUF], n: usize) -> String {
    r[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let Some(data_path) = (unsafe { find(0x258A, 0x010C, Kind::Feature(BUF as u16)) }) else {
        println!("没找到 520 字节 feature 集合");
        return;
    };
    let Some(cmd_path) = (unsafe { find(0x258A, 0x010C, Kind::Feature(6)) }) else {
        println!("没找到 6 字节命令集合");
        return;
    };
    let Some(data) = open(&data_path) else {
        println!("打开数据集合失败");
        return;
    };
    let Some(cmd) = open(&cmd_path) else {
        println!("打开命令集合失败");
        return;
    };
    let status = (unsafe { find(0x258A, 0x010C, Kind::Input(4)) }).and_then(|p| open(&p));

    println!("== A. 连续回读（中间不写）==");
    let a1 = read520(data).unwrap_or([0; BUF]);
    let a2 = read520(data).unwrap_or([0; BUF]);
    println!("  第一次 {}", head(&a1, 16));
    println!("  第二次 {}", head(&a2, 16));
    println!(
        "  结论：{}",
        if a1 == a2 {
            "缓冲区（内容稳定）"
        } else {
            "自动推进的数据流"
        }
    );

    println!("\n== B. 同一请求连写两次 ==");
    request(data, 0, 0x01);
    std::thread::sleep(Duration::from_millis(20));
    let b1 = read520(data).unwrap_or([0; BUF]);
    request(data, 0, 0x01);
    std::thread::sleep(Duration::from_millis(20));
    let b2 = read520(data).unwrap_or([0; BUF]);
    println!("  第一次 {}", head(&b1, 16));
    println!("  第二次 {}", head(&b2, 16));
    println!(
        "  结论：{}",
        if b1 == b2 {
            "地址生效（可寻址）"
        } else {
            "地址被忽略"
        }
    );

    println!("\n== C. 命令通道 + 输入报表监听 ==");
    let cases: [(u8, u8, &str); 12] = [
        (0x44, 0x01, "2.4G 读配置 / subcmd 01"),
        (0x44, 0x0A, "2.4G 读配置 / subcmd 0A"),
        (0x44, 0x00, "2.4G 读配置 / subcmd 00"),
        (0x41, 0x01, "2.4G 读块 41"),
        (0x42, 0x01, "2.4G 读按键"),
        (0x43, 0x01, "2.4G 读调色板"),
        (0x11, 0x01, "Sinowealth 读配置"),
        (0x12, 0x01, "Sinowealth 读按键"),
        (0x01, 0x01, "固件版本"),
        (0x02, 0x01, "配置档"),
        (0x0A, 0x01, "状态"),
        (0x1A, 0x01, "防抖"),
    ];
    for (code, sub, name) in cases {
        let mut req = [0u8; 6];
        req[0] = 0x05;
        req[1] = code;
        req[2] = sub;
        let ok = unsafe { HidD_SetFeature(cmd, req.as_mut_ptr() as *mut c_void, 6) };
        if ok == 0 {
            println!("  {name}: SetFeature 失败");
            continue;
        }
        // 先听 4 字节输入报表
        let mut frames: Vec<[u8; 4]> = Vec::new();
        if let Some(sh) = status {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(250) {
                let mut f = [0u8; 4];
                f[0] = 0x03;
                if unsafe { HidD_GetInputReport(sh, f.as_mut_ptr() as *mut c_void, 4) } != 0
                    && !frames.contains(&f)
                {
                    frames.push(f);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        std::thread::sleep(Duration::from_millis(30));
        let r = read520(data).unwrap_or([0; BUF]);
        // 命令通道自己的 6 字节 feature 回读：响应也许就放在这里。
        let mut back = [0u8; 6];
        back[0] = 0x05;
        let ok6 = unsafe { HidD_GetFeature(cmd, back.as_mut_ptr() as *mut c_void, 6) };
        println!(
            "  {name}: 520 前 16 = [{}]  6B 回读 = {}{}",
            head(&r, 16),
            if ok6 != 0 {
                format!("{:02x?}", &back)
            } else {
                "失败".to_string()
            },
            if frames.is_empty() {
                String::new()
            } else {
                format!("  输入报表 {:02x?}", frames)
            }
        );
    }

    unsafe {
        CloseHandle(data);
        CloseHandle(cmd);
        if let Some(sh) = status {
            CloseHandle(sh);
        }
    }
}
