// 诊断工具：雷柏 VT1 Pro（有线 24AE:4414 / 接收器 24AE:1214）功能自检。
//
// 按 `src/driver/rapoo.rs` 完全相同的帧序列逐项验证：
//   1) 只读快照：回报率 / LOD / 移动同步 / 档位数 / 当前档位 / DPI 表 / 防抖 / 休眠 / 修正位 / 电量
//   2) 写入自检：每个字段"原值写回"（read-modify-write + 0xA3 提交），验证应答与回读一致
//   3) DPI 往返：当前档位 +100 → 回读 → 还原 → 回读，验证改动真的落到设备
//
// 只做原值写回和一次可还原的 DPI 改动；还原失败会明确告警，正常不会留下持久改动。
// 运行时请先退出 PeriphHub：两个程序共享应答缓冲，会互相读到对方的应答。
//
//   cargo run --example rapoo_selftest

use std::ffi::c_void;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
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

const RID: u8 = 0xBA;
const A_POLL: u32 = 0x0880;
const A_LOD: u32 = 0x0884;
const A_DPI_X: u32 = 0x0888;
const A_STAGE_ACTIVE: u32 = 0x0898;
const A_DPI_Y: u32 = 0x08C8;
const A_DEB_PRESS: u32 = 0x08C0;
const MAX_STAGES: u8 = 7;
const DPI_MAX: u16 = 26000;

struct Dev {
    handle: HANDLE,
    last: [u8; 32],
    /// 上一次被接受的读命令（地址、长度），与驱动 `rapoo.rs` 的判据一致。
    last_read: Option<(u32, usize)>,
}

unsafe fn find_device() -> Option<(Vec<u16>, u16, u16)> {
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
        // 与驱动一致：这块内存会被当成 `SP_DEVICE_INTERFACE_DETAIL_DATA_W`
        // 写，`Vec<u8>` 只保证 1 字节对齐，写结构体是未定义行为。用 u64
        // 承载保证对齐，路径偏移交给 offset_of 算。
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
        let mut page = 0u16;
        let mut usage = 0u16;
        let mut pp: isize = 0;
        if HidD_GetAttributes(h, &mut attr) != 0 && HidD_GetPreparsedData(h, &mut pp) != 0 {
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            if HidP_GetCaps(pp, &mut caps) >= 0 {
                page = caps.UsagePage;
                usage = caps.Usage;
            }
            HidD_FreePreparsedData(pp);
        }
        CloseHandle(h);
        if attr.VendorID == 0x24AE
            && (attr.ProductID == 0x4414 || attr.ProductID == 0x1214)
            && page == 0xFF00
            && usage == 0x000E
        {
            found = Some((path, attr.ProductID, usage));
            break;
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    found
}

impl Dev {
    fn open(path: &[u16]) -> Option<Dev> {
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
        (h != INVALID_HANDLE_VALUE).then_some(Dev {
            handle: h,
            last: [0u8; 32],
            last_read: None,
        })
    }

    fn xfer(&mut self, frame: &[u8; 32]) -> Result<[u8; 32], String> {
        let is_read = frame[2] == 0xA4;
        let addr = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let len = frame[3] as usize;
        let budget = if is_read { 400 } else { 200 };
        for _ in 0..2 {
            unsafe {
                if HidD_SetOutputReport(self.handle, frame.as_ptr() as *const c_void, 32) == 0 {
                    return Err("发送命令失败".into());
                }
            }
            let prev = self.last;
            // 与驱动一致：同一地址、同一长度的重复读取允许应答不变，
            // 否则写后回读和连续两次修改会被自己判成过期应答。
            let repeated_read = self.last_read == Some((addr, len));
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
                            let fresh = !is_read || resp[1..] != prev[1..] || repeated_read;
                            if elapsed >= Duration::from_millis(40) && fresh {
                                self.last = resp;
                                self.last_read = is_read.then_some((addr, len));
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

    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA4;
        f[3] = len as u8;
        f[4..8].copy_from_slice(&addr.to_le_bytes());
        let r = self.xfer(&f)?;
        Ok(r[4..4 + len].to_vec())
    }

    fn write_block(&mut self, addr: u32, data: &[u8; 4]) -> Result<[u8; 32], String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA5;
        f[3] = 4;
        f[4..8].copy_from_slice(&addr.to_le_bytes());
        f[8..12].copy_from_slice(data);
        self.xfer(&f)
    }

    fn commit(&mut self) -> Result<[u8; 32], String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xA3;
        self.xfer(&f)
    }

    /// 与驱动 `battery_level` 相同：`[1]` 必须是连接 flag（2.4G = 1、
    /// 插线 = 2），`[2]` 必须是百分比；不符就等 200 ms 再问一次。
    fn battery(&mut self) -> Result<[u8; 8], String> {
        let mut f = [0u8; 32];
        f[0] = RID;
        f[1] = 0xA5;
        f[2] = 0xAA;
        let mut r = self.xfer(&f)?;
        if !valid_battery(&r) {
            sleep(Duration::from_millis(200));
            r = self.xfer(&f)?;
        }
        if !valid_battery(&r) {
            return Err(format!("应答不像电量帧：{:02x?}", &r[..8]));
        }
        let mut out = [0u8; 8];
        out.copy_from_slice(&r[..8]);
        Ok(out)
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// 每项自检的结果
struct Row {
    name: &'static str,
    read: String,
    write: String,
}

/// 与驱动 `battery_reply` 相同：连接 flag 1/2 且百分比 ≤ 100。
fn valid_battery(r: &[u8; 32]) -> bool {
    matches!(r[1], 1 | 2) && r[2] <= 100
}

fn ok(b: bool) -> &'static str {
    if b {
        "ok"
    } else {
        "FAIL"
    }
}

fn main() {
    let Some((path, pid, _)) = (unsafe { find_device() }) else {
        println!("没找到雷柏设备（24AE:4414 有线 / 24AE:1214 接收器）");
        return;
    };
    let Some(mut d) = Dev::open(&path) else {
        println!("打开设备失败：{}", std::io::Error::last_os_error());
        return;
    };
    println!(
        "设备 24AE:{pid:04X}（{}），命令通道 0xFF00/0x0E\n",
        if pid == 0x4414 {
            "有线"
        } else {
            "2.4G 接收器"
        }
    );

    // ---- 只读快照（顺序与驱动一致）----
    let p = match d.read(A_POLL, 4) {
        Ok(v) => v,
        Err(e) => {
            println!("读回报率块失败：{e}");
            return;
        }
    };
    let lm = d.read(A_LOD, 4).unwrap_or_default();
    let deb = d.read(A_DEB_PRESS, 4).unwrap_or_default();
    let x = d.read(A_DPI_X, 24).unwrap_or_default();
    let y = d.read(A_DPI_Y, 24).unwrap_or_default();
    if lm.len() < 2 || deb.len() < 4 || x.len() < 18 || y.len() < 18 {
        println!("快照读取不完整（LOD / 防抖 / DPI 表），请重试");
        return;
    }
    let stages = (x[14].saturating_add(1)).min(MAX_STAGES);
    let active = x[16].min(stages.saturating_sub(1));
    let dpi_x: Vec<u16> = (0..stages as usize)
        .map(|i| u16::from_le_bytes([x[i * 2], x[i * 2 + 1]]))
        .collect();
    let dpi_y: Vec<u16> = (0..stages as usize)
        .map(|i| u16::from_le_bytes([y[i * 2], y[i * 2 + 1]]))
        .collect();
    let batt = d.battery();

    println!("== 只读快照 ==");
    println!(
        "  回报率      {} Hz（0x0880 = {:02x}）",
        1000 / p[0].max(1) as u32,
        p[0]
    );
    println!("  静默高度    raw {}（1.0 + 0.1×raw mm）", lm[0]);
    println!("  移动同步    {}", if lm[1] != 0 { "开" } else { "关" });
    println!("  档位数      {stages}");
    println!("  当前档位    第 {} 档（索引 {active}）", active + 1);
    println!("  DPI X       {dpi_x:?}");
    println!("  DPI Y       {dpi_y:?}");
    println!("  防抖        按下 {} ms / 抬起 {} ms", deb[0], deb[1]);
    println!("  休眠        {} 分钟", deb[2]);
    println!(
        "  修正位      直线 {} / 波纹 {}（0x08C3 = {:02x}）",
        if deb[3] & 1 == 0 { "开" } else { "关" },
        if deb[3] & 2 == 0 { "开" } else { "关" },
        deb[3]
    );
    match batt {
        Ok(b) => println!("  电量        {}%（flag {}）", b[2], b[1]),
        Err(e) => println!("  电量        读取失败（{e}）"),
    }

    // ---- 写入自检：原值写回 ----
    // 应答判据与驱动一致：同一地址、同一长度的重复读取允许内容不变。
    // 这里仍按块组织：读一次 → 原样写回 + 提交 → 再读一次 → 逐字段比对。
    struct Field {
        name: &'static str,
        byte: usize,
        mask: u8,
    }
    fn block_check(d: &mut Dev, block: u32, fields: &[Field], out: &mut Vec<Row>) {
        // 读失败时绝不能写：原值未知，"写回"只会把整块清零并 commit。
        let before = match d.read(block, 4) {
            Ok(v) => v,
            Err(e) => {
                for f in fields {
                    out.push(Row {
                        name: f.name,
                        read: "—".to_string(),
                        write: format!("ERR 读取失败（未写入）：{e}"),
                    });
                }
                return;
            }
        };
        let w = d
            .write_block(block, &[before[0], before[1], before[2], before[3]])
            .and_then(|_| d.commit());
        // 回读失败同样按 ERR 记，不能拿空 Vec 当"和原值一样"。
        let after = d.read(block, 4);
        for f in fields {
            let b = before[f.byte] & f.mask;
            let (verdict, detail) = match &w {
                Err(e) => ("ERR".to_string(), e.clone()),
                Ok(_) => match &after {
                    Err(e) => ("ERR".to_string(), format!("回读失败：{e}")),
                    Ok(a) => {
                        let a = a[f.byte] & f.mask;
                        if a == b {
                            ("ok".to_string(), String::new())
                        } else {
                            ("FAIL".to_string(), format!("回读 {:02x} → {:02x}", b, a))
                        }
                    }
                },
            };
            out.push(Row {
                name: f.name,
                read: format!("{:02x}", b),
                write: if detail.is_empty() {
                    verdict
                } else {
                    format!("{verdict} {detail}")
                },
            });
        }
    }

    let mut rows: Vec<Row> = Vec::new();
    block_check(
        &mut d,
        A_POLL,
        &[Field {
            name: "回报率",
            byte: 0,
            mask: 0xff,
        }],
        &mut rows,
    );
    block_check(
        &mut d,
        A_LOD,
        &[
            Field {
                name: "静默高度",
                byte: 0,
                mask: 0xff,
            },
            Field {
                name: "移动同步",
                byte: 1,
                mask: 0xff,
            },
        ],
        &mut rows,
    );
    block_check(
        &mut d,
        A_DEB_PRESS,
        &[
            Field {
                name: "防抖（按下）",
                byte: 0,
                mask: 0xff,
            },
            Field {
                name: "防抖（抬起）",
                byte: 1,
                mask: 0xff,
            },
            Field {
                name: "休眠时间",
                byte: 2,
                mask: 0xff,
            },
            Field {
                name: "直线修正",
                byte: 3,
                mask: 0x01,
            },
            Field {
                name: "波纹修正",
                byte: 3,
                mask: 0x02,
            },
        ],
        &mut rows,
    );
    block_check(
        &mut d,
        A_STAGE_ACTIVE,
        &[Field {
            name: "当前档位",
            byte: 0,
            mask: 0xff,
        }],
        &mut rows,
    );

    println!("\n== 写入自检（读块 → 原样写回 → 0xA3 提交 → 回读）==");
    for r in &rows {
        println!("  {:<12} 原值 {}  结果 {}", r.name, r.read, r.write);
    }

    // ---- DPI 往返：当前档 +100 → 回读 → 还原 ----
    println!("\n== DPI 往返（当前档 +100，再还原）==");
    let idx = active as u32;
    let orig = dpi_x[active as usize];
    let moved = (orig + 100).min(DPI_MAX);
    let write_dpi = |d: &mut Dev, v: u16| -> Result<(), String> {
        let bytes = v.to_le_bytes();
        for addr in [A_DPI_X + idx * 2, A_DPI_Y + idx * 2] {
            let block = addr & !3;
            let off = (addr - block) as usize;
            let cur = d.read(block, 4)?;
            let mut arr = [cur[0], cur[1], cur[2], cur[3]];
            arr[off] = bytes[0];
            arr[off + 1] = bytes[1];
            d.write_block(block, &arr)?;
        }
        d.commit()?;
        Ok(())
    };
    let read_dpi = |d: &mut Dev, slot: u32| -> u16 {
        let addr = A_DPI_X + slot * 2;
        let block = addr & !3;
        let off = (addr - block) as usize;
        d.read(block, 4)
            .map(|b| u16::from_le_bytes([b[off], b[off + 1]]))
            .unwrap_or(0)
    };
    let t0 = Instant::now();
    let w1 = write_dpi(&mut d, moved).is_ok();
    let back1 = read_dpi(&mut d, idx);
    // 还原失败不能让 +100 留在设备里：有限重试并校验回读值。
    let mut back2 = 0u16;
    let mut w2 = false;
    for attempt in 0..3 {
        if attempt > 0 {
            sleep(Duration::from_millis(100));
        }
        w2 = write_dpi(&mut d, orig).is_ok();
        back2 = read_dpi(&mut d, idx);
        if w2 && back2 == orig {
            break;
        }
    }
    println!(
        "  {orig} → 写 {moved} {} → 回读 {back1} {} → 还原 {orig} {} → 回读 {back2} {}   用时 {} ms",
        ok(w1),
        ok(back1 == moved),
        ok(w2),
        ok(back2 == orig),
        t0.elapsed().as_millis()
    );
    if !(w2 && back2 == orig) {
        println!(
            "  警告：DPI 还原失败，设备里的当前档可能仍是 {moved}。请打开 PeriphHub 确认并手动改回 {orig}。"
        );
    }

    // ---- 结论 ----
    let failed = rows.iter().filter(|r| !r.write.contains("ok")).count()
        + usize::from(back1 != moved)
        + usize::from(back2 != orig);
    println!(
        "\n结论：{}",
        if failed == 0 {
            "读、写、提交、回读全部正常".to_string()
        } else {
            format!("有 {failed} 项异常，见上表")
        }
    );
}
