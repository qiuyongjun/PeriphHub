// 诊断工具：枚举 HID 接口，打印 caps / 报表描述符，并对厂商集合做只读探测。
// cargo run --example probe -- [vid_hex]

use std::ffi::c_void;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetInputReport, HidD_GetManufacturerString, HidD_GetPreparsedData, HidD_GetProductString,
    HidD_GetSerialNumberString, HidD_SetFeature, HidP_GetCaps, HidP_GetLinkCollectionNodes,
    HidP_GetValueCaps, HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_LINK_COLLECTION_NODE, HIDP_VALUE_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn DeviceIoControl(
        h: HANDLE,
        code: u32,
        inbuf: *mut c_void,
        inlen: u32,
        outbuf: *mut c_void,
        outlen: u32,
        ret: *mut u32,
        ov: *mut c_void,
    ) -> i32;
}

/// CTL_CODE(FILE_DEVICE_KEYBOARD, 0, METHOD_NEITHER=3, FILE_ANY_ACCESS)
const IOCTL_HID_GET_REPORT_DESCRIPTOR: u32 = 0x000B_C000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const ERROR_IO_PENDING: i32 = 997;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: HANDLE,
}

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn ReadFile(h: HANDLE, buf: *mut c_void, n: u32, read: *mut u32, ov: *mut Overlapped) -> i32;
    fn CreateEventW(attr: *mut c_void, manual: i32, initial: i32, name: *const u16) -> HANDLE;
    fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    fn GetOverlappedResult(h: HANDLE, ov: *mut Overlapped, transferred: *mut u32, wait: i32)
        -> i32;
    fn CancelIo(h: HANDLE) -> i32;
    fn GetLastError() -> u32;
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

fn open(path: &[u16]) -> HANDLE {
    for access in [GENERIC_READ | GENERIC_WRITE, GENERIC_READ, 0] {
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return h;
        }
    }
    INVALID_HANDLE_VALUE
}

fn get_string(path: &[u16], which: &str) -> String {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        return String::new();
    }
    let mut buf = [0u16; 256];
    let ok = unsafe {
        match which {
            "product" => {
                HidD_GetProductString(h, buf.as_mut_ptr() as *mut c_void, (buf.len() * 2) as u32)
            }
            "manufacturer" => HidD_GetManufacturerString(
                h,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as u32,
            ),
            _ => HidD_GetSerialNumberString(
                h,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as u32,
            ),
        }
    };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return String::new();
    }
    wide_to_string(&buf)
}

#[allow(clippy::type_complexity)]
fn caps_of(path: &[u16]) -> Option<(u16, u16, HIDP_CAPS, Vec<HIDP_LINK_COLLECTION_NODE>, Vec<u8>)> {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut attr: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    if unsafe { HidD_GetAttributes(h, &mut attr) } == 0 {
        unsafe { CloseHandle(h) };
        return None;
    }
    let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
    let mut nodes = Vec::new();
    let mut desc = Vec::new();
    let mut pp: isize = 0;
    if unsafe { HidD_GetPreparsedData(h, &mut pp) } != 0 {
        unsafe { HidP_GetCaps(pp, &mut caps) };
        let n = caps.NumberLinkCollectionNodes.max(1) as usize;
        let mut v: Vec<HIDP_LINK_COLLECTION_NODE> = vec![unsafe { std::mem::zeroed() }; n];
        let mut len = n as u32;
        if unsafe { HidP_GetLinkCollectionNodes(v.as_mut_ptr(), &mut len, pp) } >= 0 {
            v.truncate(len as usize);
            nodes = v;
        }
        unsafe { HidD_FreePreparsedData(pp) };
    }
    let mut buf = vec![0u8; 4096];
    let mut got = 0u32;
    if unsafe {
        DeviceIoControl(
            h,
            IOCTL_HID_GET_REPORT_DESCRIPTOR,
            std::ptr::null_mut(),
            0,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    } != 0
        && got > 0
    {
        desc = buf[..got as usize].to_vec();
    }
    unsafe { CloseHandle(h) };
    Some((attr.VendorID, attr.ProductID, caps, nodes, desc))
}

fn value_caps(path: &[u16], kind: i32, label: &str) {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    let mut pp: isize = 0;
    if unsafe { HidD_GetPreparsedData(h, &mut pp) } != 0 {
        let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
        unsafe { HidP_GetCaps(pp, &mut caps) };
        let n = match kind {
            0 => caps.NumberInputValueCaps,
            1 => caps.NumberOutputValueCaps,
            _ => caps.NumberFeatureValueCaps,
        };
        if n > 0 {
            let mut v: Vec<HIDP_VALUE_CAPS> = vec![unsafe { std::mem::zeroed() }; n as usize];
            let mut len = n;
            if unsafe { HidP_GetValueCaps(kind, v.as_mut_ptr(), &mut len, pp) } >= 0 {
                for c in &v[..len as usize] {
                    let usage = if c.IsRange != 0 {
                        format!(
                            "0x{:04X}..0x{:04X}",
                            unsafe { c.Anonymous.Range.UsageMin },
                            unsafe { c.Anonymous.Range.UsageMax }
                        )
                    } else {
                        format!("0x{:04X}", unsafe { c.Anonymous.NotRange.Usage })
                    };
                    println!(
                        "    {label} valcap: rid=0x{:02X} up=0x{:04X} usage={usage} bits={} count={} logical={}..{}",
                        c.ReportID, c.UsagePage, c.BitSize, c.ReportCount, c.LogicalMin, c.LogicalMax
                    );
                }
            }
        }
        unsafe { HidD_FreePreparsedData(pp) };
    }
    unsafe { CloseHandle(h) };
}

fn poll_feature(path: &[u16], len: usize, rid: u8, times: usize) {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    println!("    -- 连续读取 feature rid 0x{rid:02x}（{times} 次，间隔 400ms）--");
    let mut prev = Vec::new();
    for i in 0..times {
        let mut b = vec![0u8; len];
        b[0] = rid;
        let ok = unsafe { HidD_GetFeature(h, b.as_mut_ptr() as *mut c_void, len as u32) };
        let text = if ok != 0 {
            hex(&b)
        } else {
            "<读取失败>".into()
        };
        let mark = if i > 0 && b != prev {
            "  <= 变化"
        } else {
            ""
        };
        println!("      #{i}: {text}{mark}");
        prev = b;
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    unsafe { CloseHandle(h) };
}

fn capture_input(path: &[u16], len: usize, secs: u64) {
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
            println!("    [overlapped 打开失败]");
            return;
        }
        let ev = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
        println!("    -- 监听 input 报表 {len}B，{secs} 秒（期间请敲按键/转旋钮）--");
        let start = std::time::Instant::now();
        let mut seen = 0;
        while start.elapsed().as_secs() < secs {
            let mut buf = vec![0u8; len];
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: ev,
            };
            let mut read = 0u32;
            let ok = ReadFile(
                h,
                buf.as_mut_ptr() as *mut c_void,
                len as u32,
                &mut read,
                &mut ov,
            );
            if ok == 0 && GetLastError() != ERROR_IO_PENDING as u32 {
                println!("    [ReadFile 失败 {}]", GetLastError());
                break;
            }
            if ok == 0 {
                let wait = WaitForSingleObject(ev, 200);
                if wait != 0 {
                    CancelIo(h);
                    continue;
                }
                GetOverlappedResult(h, &mut ov, &mut read, 0);
            }
            if read as usize >= len {
                println!("      {:>6} ms  {}", start.elapsed().as_millis(), hex(&buf));
                seen += 1;
            }
        }
        println!("    (共收到 {seen} 帧)");
        CloseHandle(ev);
        CloseHandle(h);
    }
}

fn probe_reads(path: &[u16], caps: &HIDP_CAPS) {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        println!("    [打不开，无法探测]");
        return;
    }
    let feat = caps.FeatureReportByteLength as usize;
    if feat > 0 {
        println!("    -- GetFeature 探测 (len={feat}) --");
        for rid in 0u8..=0x10 {
            let mut b = vec![0u8; feat];
            b[0] = rid;
            let ok = unsafe { HidD_GetFeature(h, b.as_mut_ptr() as *mut c_void, feat as u32) };
            if ok != 0 {
                let all_zero = b.iter().all(|&x| x == 0);
                if !all_zero {
                    println!("      rid 0x{rid:02x} -> {}", hex(&b));
                }
            }
        }
    }
    let inp = caps.InputReportByteLength as usize;
    if inp > 0 {
        println!("    -- GetInputReport 探测 (len={inp}) --");
        for rid in 0u8..=0x10 {
            let mut b = vec![0u8; inp];
            b[0] = rid;
            let ok = unsafe { HidD_GetInputReport(h, b.as_mut_ptr() as *mut c_void, inp as u32) };
            if ok != 0 {
                let all_zero = b.iter().all(|&x| x == 0);
                if !all_zero {
                    println!("      rid 0x{rid:02x} -> {}", hex(&b));
                }
            }
        }
    }
    unsafe { CloseHandle(h) };
}

fn feat_write(path: &[u16], len: usize, rid: u8) {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    println!("    -- feature 通道写测试 --");
    let mut b = vec![0u8; len];
    b[0] = rid;
    unsafe { HidD_GetFeature(h, b.as_mut_ptr() as *mut c_void, len as u32) };
    println!("    原始读: {}", hex(&b));

    let r = unsafe { HidD_SetFeature(h, b.as_ptr() as *const c_void, len as u32) };
    println!("    原样写回: {}", if r != 0 { "ok" } else { "失败" });
    let mut after = vec![0u8; len];
    after[0] = rid;
    unsafe { HidD_GetFeature(h, after.as_mut_ptr() as *mut c_void, len as u32) };
    println!("    写后读: {}", hex(&after));

    let mut q = vec![0u8; len];
    q[0] = rid;
    q[1] = 0x4A;
    let r = unsafe { HidD_SetFeature(h, q.as_ptr() as *const c_void, len as u32) };
    println!("    发 06 4A …: {}", if r != 0 { "ok" } else { "失败" });
    for i in 0..3 {
        let mut b2 = vec![0u8; len];
        b2[0] = rid;
        unsafe { HidD_GetFeature(h, b2.as_mut_ptr() as *mut c_void, len as u32) };
        println!("    #{i} 读: {}", hex(&b2));
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    unsafe { CloseHandle(h) };
}

fn feat_cmds(path: &[u16], len: usize, rid: u8) {
    let h = open(path);
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    println!("    -- feature 命令扫描 --");
    for cmd in [0xF1u8, 0xF0, 0x01, 0x02, 0x03, 0x05, 0x44, 0x4A, 0x00] {
        let mut q = vec![0u8; len];
        q[0] = rid;
        q[1] = cmd;
        let w = unsafe { HidD_SetFeature(h, q.as_ptr() as *const c_void, len as u32) };
        std::thread::sleep(std::time::Duration::from_millis(150));
        let mut b = vec![0u8; len];
        b[0] = rid;
        let r = unsafe { HidD_GetFeature(h, b.as_mut_ptr() as *mut c_void, len as u32) };
        println!(
            "    cmd 0x{cmd:02x}: write={} read={} -> {}",
            if w != 0 { "ok" } else { "fail" },
            if r != 0 { "ok" } else { "fail" },
            hex(&b)
        );
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    unsafe { CloseHandle(h) };
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn uage(page: u16) -> String {
    match page {
        0x01 => "Generic Desktop".into(),
        0x06 => "Generic Device Controls(电池)".into(),
        0x07 => "Keyboard".into(),
        0x08 => "LED".into(),
        0x09 => "Button".into(),
        0x0C => "Consumer".into(),
        0x0B => "Telephony".into(),
        p if p >= 0xFF00 => format!("Vendor 0x{p:04X}"),
        p => format!("0x{p:04X}"),
    }
}

fn main() {
    let want: Option<u16> = std::env::args()
        .nth(1)
        .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok());

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
            println!("SetupDiGetClassDevs 失败");
            return;
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
            let Some((vid, pid, caps, nodes, desc)) = caps_of(&path) else {
                continue;
            };
            if let Some(w) = want {
                if vid != w {
                    continue;
                }
            }
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

            println!("\n==================================================");
            println!("{instance}");
            println!("  VID:{vid:04X} PID:{pid:04X}");
            println!(
                "  product={:?} mfr={:?} serial={:?}",
                get_string(&path, "product"),
                get_string(&path, "manufacturer"),
                get_string(&path, "serial")
            );
            println!(
                "  usagePage=0x{:04X}({}) usage=0x{:04X}",
                caps.UsagePage,
                uage(caps.UsagePage),
                caps.Usage
            );
            println!(
                "  in={} out={} feature={} nodes={} btnCaps in/out={}/{} valCaps in/out/feat={}/{}/{}",
                caps.InputReportByteLength,
                caps.OutputReportByteLength,
                caps.FeatureReportByteLength,
                caps.NumberLinkCollectionNodes,
                caps.NumberInputButtonCaps,
                caps.NumberOutputButtonCaps,
                caps.NumberInputValueCaps,
                caps.NumberOutputValueCaps,
                caps.NumberFeatureValueCaps
            );
            for (i, n) in nodes.iter().enumerate() {
                println!(
                    "    node[{i}] up=0x{:04X} u=0x{:04X} parent={} first={} next={} children={}",
                    n.LinkUsagePage,
                    n.LinkUsage,
                    n.Parent,
                    n.NumberOfChildren,
                    n.NextSibling,
                    n.FirstChild
                );
            }
            if !desc.is_empty() {
                println!("  reportDescriptor ({} B):", desc.len());
                for chunk in desc.chunks(24) {
                    println!("    {}", hex(chunk));
                }
            }
            if caps.UsagePage >= 0xFF00 || caps.FeatureReportByteLength > 0 {
                probe_reads(&path, &caps);
            }
            if caps.UsagePage >= 0xFF00 {
                value_caps(&path, 2, "feat");
                value_caps(&path, 0, "input");
            }
            if caps.UsagePage == 0xFF04 && caps.FeatureReportByteLength > 0 {
                poll_feature(&path, caps.FeatureReportByteLength as usize, 6, 3);
                if std::env::args().any(|a| a == "setfeat") {
                    feat_write(&path, caps.FeatureReportByteLength as usize, 6);
                }
                if std::env::args().any(|a| a == "cmds") {
                    feat_cmds(&path, caps.FeatureReportByteLength as usize, 6);
                }
            }
            // Any vendor collection that pushes input reports can carry status
            // (battery, layer, lighting); listening is read-only and cheap.
            if caps.UsagePage >= 0xFF00 && caps.InputReportByteLength > 0 {
                capture_input(&path, caps.InputReportByteLength as usize, 6);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
}
