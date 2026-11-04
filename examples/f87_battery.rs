// 验证 AULA F87 / 3554:FA09 接收器的 20 字节报文。
//
// 帧格式（来源：aula-f87-control docs/PROTOCOL.md，F87 Pro 已验）
//   13  cmd  sub  seq  len  data[14]  sum      sum = (byte0..byte18) 累加 mod 256
// 电量：发 13 4A 00 …     收 13 4A 01 00 02 <level> <state> …
//       level 1..100，state 0x01 = 电池供电，0x10 = 插线供电
// 读配置：发 13 44 01 00 … 收 10 帧 13 44 0A seq len data…
//
// 用法：
//   cargo run --example f87_battery                  # 发 3 次电量查询
//   cargo run --example f87_battery -- --watch 120   # 持续查询 120 秒
//   cargo run --example f87_battery -- --send "13 44 01 00"   # 自定义帧

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
    HidD_SetOutputReport, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const USAGE_PAGE: u16 = 0xFF02;
const USAGE: u16 = 0x0002;
const REPORT_ID: u8 = 0x13;
const FRAME_LEN: usize = 20;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const ERROR_IO_PENDING: u32 = 997;

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
    fn WriteFile(
        h: HANDLE,
        buf: *const c_void,
        n: u32,
        written: *mut u32,
        ov: *mut Overlapped,
    ) -> i32;
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

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_hex(s: &str) -> Vec<u8> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    cleaned
        .as_bytes()
        .chunks(2)
        .filter_map(|c| {
            std::str::from_utf8(c)
                .ok()
                .and_then(|x| u8::from_str_radix(x, 16).ok())
        })
        .collect()
}

/// 找到 vendor 通道（usage page 0xFF02 / usage 0x02）的接口路径。
fn find_vendor_path() -> Option<Vec<u16>> {
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
            let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
            attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
            let mut hit = false;
            if HidD_GetAttributes(h, &mut attr) != 0
                && attr.VendorID == 0x3554
                && attr.ProductID == 0xFA09
            {
                let mut pp: isize = 0;
                if HidD_GetPreparsedData(h, &mut pp) != 0 {
                    let mut caps: HIDP_CAPS = std::mem::zeroed();
                    if HidP_GetCaps(pp, &mut caps) >= 0
                        && caps.UsagePage == USAGE_PAGE
                        && caps.Usage == USAGE
                        && caps.OutputReportByteLength as usize >= FRAME_LEN
                    {
                        hit = true;
                    }
                    HidD_FreePreparsedData(pp);
                }
            }
            CloseHandle(h);
            if hit {
                found = Some(path);
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// 0x13 cmd sub seq len data[14] sum
fn frame(cmd: u8, sub: u8, data: &[u8]) -> [u8; FRAME_LEN] {
    let mut f = [0u8; FRAME_LEN];
    f[0] = REPORT_ID;
    f[1] = cmd;
    f[2] = sub;
    f[3] = 0;
    f[4] = data.len() as u8;
    f[5..5 + data.len()].copy_from_slice(data);
    f[FRAME_LEN - 1] = checksum(&f);
    f
}

fn checksum(f: &[u8]) -> u8 {
    f[..FRAME_LEN - 1]
        .iter()
        .fold(0u8, |a, b| a.wrapping_add(*b))
}

struct Conn {
    h: HANDLE,
    ev_read: HANDLE,
    ev_write: HANDLE,
    start: Instant,
}

impl Conn {
    unsafe fn open(path: &[u16]) -> Option<Conn> {
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
        Some(Conn {
            h,
            ev_read: CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()),
            ev_write: CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()),
            start: Instant::now(),
        })
    }

    fn ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }

    fn send(&self, f: &[u8; FRAME_LEN]) -> bool {
        unsafe {
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: self.ev_write,
            };
            let mut written = 0u32;
            let ok = WriteFile(
                self.h,
                f.as_ptr() as *const c_void,
                FRAME_LEN as u32,
                &mut written,
                &mut ov,
            );
            // `&&` 短路：只有 ok == 0 时才去读 last-error。
            if ok == 0 && GetLastError() == ERROR_IO_PENDING {
                WaitForSingleObject(self.ev_write, 500);
                GetOverlappedResult(self.h, &mut ov, &mut written, 0);
            }
            if written == 0 {
                // 退回控制传输
                let r = HidD_SetOutputReport(self.h, f.as_ptr() as *const c_void, FRAME_LEN as u32);
                return r != 0;
            }
            true
        }
    }

    /// 监听 ms 毫秒，收集收到的每一帧（不打印）。
    fn collect(&self, ms: u64) -> Vec<[u8; FRAME_LEN]> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut out = Vec::new();
        unsafe {
            while Instant::now() < deadline {
                let mut buf = [0u8; FRAME_LEN];
                let mut ov = Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    h_event: self.ev_read,
                };
                let mut read = 0u32;
                let ok = ReadFile(
                    self.h,
                    buf.as_mut_ptr() as *mut c_void,
                    FRAME_LEN as u32,
                    &mut read,
                    &mut ov,
                );
                if ok == 0 && GetLastError() != ERROR_IO_PENDING {
                    break;
                }
                if ok == 0 {
                    let left = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u32;
                    if WaitForSingleObject(self.ev_read, left.max(1)) != 0 {
                        CancelIo(self.h);
                        continue;
                    }
                    GetOverlappedResult(self.h, &mut ov, &mut read, 0);
                }
                if read as usize == FRAME_LEN {
                    out.push(buf);
                }
            }
        }
        out
    }

    /// 监听 ms 毫秒，打印收到的每一帧，返回收到帧数。
    fn listen(&self, ms: u64) -> usize {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut n = 0;
        unsafe {
            while Instant::now() < deadline {
                let mut buf = [0u8; FRAME_LEN];
                let mut ov = Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    h_event: self.ev_read,
                };
                let mut read = 0u32;
                let ok = ReadFile(
                    self.h,
                    buf.as_mut_ptr() as *mut c_void,
                    FRAME_LEN as u32,
                    &mut read,
                    &mut ov,
                );
                if ok == 0 && GetLastError() != ERROR_IO_PENDING {
                    break;
                }
                if ok == 0 {
                    let left = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u32;
                    if WaitForSingleObject(self.ev_read, left.max(1)) != 0 {
                        CancelIo(self.h);
                        continue;
                    }
                    GetOverlappedResult(self.h, &mut ov, &mut read, 0);
                }
                if read as usize == FRAME_LEN {
                    n += 1;
                    let tag = describe(&buf);
                    println!("  [{:>6}ms] 收到 {}  {tag}", self.ms(), hex(&buf));
                }
            }
        }
        n
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.ev_read);
            CloseHandle(self.ev_write);
            CloseHandle(self.h);
        }
    }
}

fn describe(f: &[u8]) -> String {
    let sum_ok = checksum(f) == f[FRAME_LEN - 1];
    let base = if sum_ok {
        String::new()
    } else {
        " [校验错]".to_string()
    };
    match (f[1], f[2]) {
        (0x4A, 0x01) => format!(
            "<== 电量应答 {}% ({})",
            f[5],
            if f[6] & 0x10 != 0 { "插线" } else { "电池" }
        ),
        (0x4A, 0x00) => format!("<== 请求回声{base}"),
        (0x44, _) => format!("<== 配置应答 seq={} len={}{base}", f[3], f[4]),
        (0x0A, _) => format!("<== 保存/状态帧{base}"),
        _ => format!("<== 其它帧{base}"),
    }
}

fn read_config(conn: &Conn) -> Option<Vec<u8>> {
    let f = frame(0x44, 0x01, &[]);
    if !conn.send(&f) {
        return None;
    }
    let frames = conn.collect(900);
    let mut block = vec![0u8; 128];
    let mut seen = 0;
    for r in frames {
        if r[1] == 0x44 && r[2] == 0x0A {
            let seq = r[3] as usize;
            let len = r[4] as usize;
            if seq < 10 && len <= 14 {
                block[seq * 14..seq * 14 + len].copy_from_slice(&r[5..5 + len]);
                seen += 1;
            }
        }
    }
    if seen >= 9 {
        Some(block)
    } else {
        None
    }
}

fn dump_config(b: &[u8]) {
    println!("配置块 128 字节: {}", hex(b));
    println!(
        "  灯效={} 亮度={} 速度={} 色彩模式={} 侧灯效果={} 休眠={} (单位 30s，0=不睡) 逐键模式={}",
        b[10],
        b[56 + 2 * b[10].min(18) as usize],
        b[57 + 2 * b[10].min(18) as usize] >> 4,
        b[12],
        b[18],
        b[24],
        b[9]
    );
    println!("  尾部标记 {:02x} {:02x}", b[126], b[127]);
}

fn probe_battery(conn: &Conn) {
    println!("先做配置快照…");
    let before = read_config(conn);
    match &before {
        Some(b) => println!("  快照 OK（{} 字节，灯效 {}）", b.len(), b[10]),
        None => println!("  快照失败，仍继续（只读候选命令）"),
    }
    if let Some(b) = &before {
        let path = "target/f87_config_before.txt";
        let _ = std::fs::write(path, hex(b));
        println!("  已保存到 {path}");
    }

    // 候选：0x4x 段基本是“请求”指令族（0x44 读配置已证）
    let mut cands: Vec<(u8, u8, Vec<u8>)> = Vec::new();
    for cmd in [0x40u8, 0x41, 0x42, 0x43, 0x45, 0x46, 0x47, 0x48, 0x49] {
        cands.push((cmd, 0x00, vec![]));
    }
    cands.push((0x4A, 0x00, vec![]));
    cands.push((0x4A, 0x01, vec![]));
    cands.push((0x4A, 0x00, vec![0x02]));
    for cmd in [0x4Bu8, 0x4C, 0x4D, 0x4E, 0x4F] {
        cands.push((cmd, 0x00, vec![]));
    }

    let mut hits = 0;
    for (cmd, sub, data) in cands {
        let f = frame(cmd, sub, &data);
        print!("cmd 0x{cmd:02x} sub 0x{sub:02x} -> ");
        if !conn.send(&f) {
            println!("写入失败");
            continue;
        }
        let frames = conn.collect(450);
        let mine: Vec<_> = frames
            .iter()
            .filter(|r| r[1] == cmd)
            .map(|r| hex(r))
            .collect();
        if mine.is_empty() {
            println!("无应答");
        } else {
            hits += 1;
            println!("{} 帧", mine.len());
            for m in mine.iter().take(12) {
                println!("      {m}");
            }
        }
    }

    println!("\n再读一次配置，确认没被写坏…");
    match (before, read_config(conn)) {
        (Some(a), Some(b)) if a == b => println!("  一致，配置未变 ✅"),
        (Some(a), Some(b)) => {
            println!("  ⚠ 配置发生变化！");
            println!("  前: {}", hex(&a));
            println!("  后: {}", hex(&b));
        }
        _ => println!("  读取失败，无法比对（可能有其它程序占用）"),
    }
    println!("\n有应答的命令数: {hits}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = find_vendor_path() else {
        println!("没找到 3554:FA09 的 0xFF02 厂商通道（键盘在 2.4G 模式且接收器在位？）");
        return;
    };
    println!(
        "vendor 路径: {}",
        String::from_utf16_lossy(&path[..path.len() - 1])
    );
    let Some(conn) = (unsafe { Conn::open(&path) }) else {
        println!("打开设备失败");
        return;
    };

    if let Some(i) = args.iter().position(|a| a == "--scan") {
        // 用法: --scan 50-5f
        let spec = args.get(i + 1).cloned().unwrap_or_default();
        let parsed: Vec<u8> = spec
            .split('-')
            .filter_map(|s| u8::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            .collect();
        let (lo, hi) = if parsed.len() == 2 {
            (parsed[0], parsed[1])
        } else {
            (0x50, 0x5F)
        };
        for cmd in lo..=hi {
            let f = frame(cmd, 0x00, &[]);
            print!("cmd 0x{cmd:02x} -> ");
            if !conn.send(&f) {
                println!("写入失败");
                continue;
            }
            let frames = conn.collect(400);
            if frames.is_empty() {
                println!("无应答");
            } else {
                println!("{} 帧", frames.len());
                for r in frames.iter().take(4) {
                    println!(
                        "      {}{}",
                        hex(r),
                        if checksum(r) == r[FRAME_LEN - 1] {
                            ""
                        } else {
                            "  校验错"
                        }
                    );
                }
            }
        }
        return;
    }

    if let Some(i) = args.iter().position(|a| a == "--idle") {
        let secs: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
        // 先读一次配置把链路唤醒
        let _ = read_config(&conn);
        println!("静默监听 {secs} 秒，不发送任何帧…");
        let frames = conn.collect(secs * 1000);
        println!("收到 {} 帧", frames.len());
        for r in frames {
            println!("  {}", hex(&r));
        }
        return;
    }

    if args.iter().any(|a| a == "--battery") {
        // 电量应答的命令字节可能带高位（0xCA），且首帧常被吞——所以全量打印 + 重试
        for attempt in 0..6 {
            let f = frame(0x4A, 0x00, &[]);
            print!("#{attempt} 发送 {} -> ", hex(&f));
            if !conn.send(&f) {
                println!("写入失败");
                continue;
            }
            let frames = conn.collect(1200);
            println!("{} 帧", frames.len());
            let mut hit = None;
            for r in &frames {
                let is_batt = (r[1] & 0x7F) == 0x4A;
                println!(
                    "    {}  cmd=0x{:02x}{}  sum={}",
                    hex(r),
                    r[1],
                    if is_batt { " <== 电量应答" } else { "" },
                    if checksum(r) == r[FRAME_LEN - 1] {
                        "ok"
                    } else {
                        "错"
                    }
                );
                if is_batt {
                    hit = Some(*r);
                }
            }
            if let Some(r) = hit {
                let level = r[5];
                let state = r[6];
                let mode = match state {
                    0x10 => "插线（读数固定 100）",
                    0x01 => "电池供电",
                    _ => "未知状态",
                };
                if (1..=100).contains(&level) {
                    println!("  ==> 电量 {level}%（{mode}）");
                } else {
                    println!("  ==> 电平字节 0x{level:02x}，不像是有效百分比（{mode}）");
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        println!("\n6 次尝试都没有电量应答。");
        return;
    }

    if args.iter().any(|a| a == "--fuzz") {
        probe_battery(&conn);
        return;
    }

    if args.iter().any(|a| a == "--config") {
        match read_config(&conn) {
            Some(b) => dump_config(&b),
            None => println!("读配置失败（10 帧没凑齐）"),
        }
        return;
    }

    if args.iter().any(|a| a == "--test") {
        // 先读配置证明链路活着，紧接着发电量查询，排除“键盘休眠”的干扰
        let f = frame(0x44, 0x01, &[]);
        println!("\n[1] 读配置 {}", hex(&f));
        conn.send(&f);
        let a = conn.listen(600);
        println!("    -> {a} 帧");
        std::thread::sleep(Duration::from_millis(300));
        let f = frame(0x4A, 0x00, &[]);
        println!("\n[2] 电量查询 {}", hex(&f));
        conn.send(&f);
        let b = conn.listen(1500);
        println!("    -> {b} 帧");
        return;
    }

    if let Some(i) = args.iter().position(|a| a == "--send") {
        let spec = args.get(i + 1).cloned().unwrap_or_default();
        let raw = parse_hex(&spec);
        let mut f = [0u8; FRAME_LEN];
        let n = raw.len().min(FRAME_LEN - 1);
        f[..n].copy_from_slice(&raw[..n]);
        f[FRAME_LEN - 1] = checksum(&f);
        println!("发送: {}", hex(&f));
        if conn.send(&f) {
            println!("已写入，等待应答 2 秒…");
        } else {
            println!("写入失败");
        }
        let got = conn.listen(2000);
        println!("收到 {got} 帧");
        return;
    }

    let watch: u64 = args
        .iter()
        .position(|a| a == "--watch")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut round = 0;
    loop {
        if watch > 0 {
            if conn.ms() / 1000 >= watch as u128 {
                break;
            }
        } else if round >= 3 {
            break;
        }
        round += 1;
        let f = frame(0x4A, 0x00, &[]);
        let ok = conn.send(&f);
        println!(
            "#{round} [{:>6}ms] 发送 {} -> {}",
            conn.ms(),
            hex(&f),
            if ok { "ok" } else { "fail" }
        );
        conn.listen(700);
        std::thread::sleep(Duration::from_millis(if watch > 0 { 300 } else { 200 }));
    }
}
