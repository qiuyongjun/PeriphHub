// 诊断工具：顺序读取有线 F87（258A:010C）的固件数据流并分析。
//
// 已确认 520 字节 feature 通道是自动推进的数据流（地址字段被忽略），
// 这里连续读 N 次，拼接后找 ASCII 字符串和 5A A5 配置签名。
//
//   cargo run --example f87_wired_dump            # 默认读 400 段（约 150 KB）
//   cargo run --example f87_wired_dump -- 800     # 指定段数

use std::ffi::c_void;
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetPreparsedData, HidD_SetFeature, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const BUF: usize = 520;
const PAYLOAD: usize = 378;

unsafe fn find_feature(vid: u16, pid: u16, len: u16) -> Option<Vec<u16>> {
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
        let mut feature = 0u16;
        let mut pp: isize = 0;
        if HidD_GetAttributes(h, &mut attr) != 0 && HidD_GetPreparsedData(h, &mut pp) != 0 {
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            if HidP_GetCaps(pp, &mut caps) >= 0 {
                feature = caps.FeatureReportByteLength;
            }
            HidD_FreePreparsedData(pp);
        }
        CloseHandle(h);
        if attr.VendorID == vid && attr.ProductID == pid && feature == len {
            found = Some(path);
            break;
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    found
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);

    let Some(path) = (unsafe { find_feature(0x258A, 0x010C, BUF as u16) }) else {
        println!("没找到 520 字节 feature 集合");
        return;
    };
    let h: HANDLE = unsafe {
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
    if h == INVALID_HANDLE_VALUE {
        println!("打开失败：{}", std::io::Error::last_os_error());
        return;
    }

    // 起一次流：写一条读请求，之后只 GetFeature，让设备自己往下推。
    let mut req = [0u8; BUF];
    req[0] = 0x06;
    req[1] = 0x08;
    req[4] = 0x01;
    req[6] = 0x7A;
    req[7] = 0x01;
    unsafe { HidD_SetFeature(h, req.as_mut_ptr() as *mut c_void, BUF as u32) };

    let mut image = Vec::with_capacity(rounds * PAYLOAD);
    for i in 0..rounds {
        std::thread::sleep(Duration::from_millis(6));
        let mut buf = [0u8; BUF];
        buf[0] = 0x06;
        let ok = unsafe { HidD_GetFeature(h, buf.as_mut_ptr() as *mut c_void, BUF as u32) };
        if ok == 0 {
            println!("第 {i} 段读取失败，停止");
            break;
        }
        image.extend_from_slice(&buf[8..8 + PAYLOAD]);
    }
    println!("共读取 {} 字节", image.len());

    let out = std::path::Path::new("target/f87_wired_stream.bin");
    if std::fs::write(out, &image).is_ok() {
        println!("已写入 {}", out.display());
    }

    // ASCII 字符串（>=6 个可打印字符），带偏移
    println!("\n== ASCII 字符串（前 60 条）==");
    let mut printed = 0;
    let mut i = 0usize;
    while i < image.len() && printed < 60 {
        if (0x20..0x7f).contains(&image[i]) {
            let mut j = i;
            while j < image.len() && (0x20..0x7f).contains(&image[j]) {
                j += 1;
            }
            if j - i >= 6 {
                println!("  @0x{:05x}  {}", i, String::from_utf8_lossy(&image[i..j]));
                printed += 1;
            }
            i = j;
            continue;
        }
        i += 1;
    }

    let hits: Vec<usize> = (0..image.len().saturating_sub(1))
        .filter(|&k| image[k] == 0x5A && image[k + 1] == 0xA5)
        .collect();
    println!("\n== 5A A5 命中 {} 处 ==", hits.len());
    for (n, &k) in hits.iter().enumerate().take(10) {
        let s = k.saturating_sub(24);
        let e = (k + 12).min(image.len());
        let row: Vec<String> = image[s..e].iter().map(|b| format!("{b:02x}")).collect();
        println!("  @0x{k:05x}  ...{}", row.join(" "));
        let _ = n;
    }

    unsafe { CloseHandle(h) };
}
