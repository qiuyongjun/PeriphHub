//! Rapoo vendor protocol: 32-byte `0xBA` reports over the vendor collection
//! `0xFF00/0x0E`: `A5 A4` = read, `A5 A5` = write, `A5 A3` = commit,
//! `A5 AA` = battery.
//!
//! Frame format, timing and 4-byte-aligned read-modify-write are protocol-level
//! behavior. EEPROM addresses, flag bits, stage limits, battery reply flags and
//! capabilities are model data and live in [`RapooLayout`]; only layouts
//! verified on real hardware belong in `PROFILES`. See `docs/protocol.md`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use super::transport::{BlockingHid, HidTransport};
use super::{Caps, Cmd, ConfirmResult, Driver, Limits, Snapshot, WriteError, WriteOutcome};

const RID: u8 = 0xBA;
const REPORT_LEN: usize = 32;
/// 提交后设备需要的时间。回读、下一条写、全量读和电量查询都要等过它。
const COMMIT_GUARD: Duration = Duration::from_millis(150);

/// 一个经过真机验证的 Rapoo 型号的 EEPROM 布局与能力。
///
/// 帧构造、时序和 4 字节对齐读-改-写属于协议层，留在这个模块里；这里只放
/// "哪个地址、哪个字节、这台设备有哪些能力"这类型号数据。新增型号必须先在
/// 真机上验证，再新建一个 layout；复用别的型号的 layout，就是往未知的 EEPROM
/// 单元里写数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RapooLayout {
    /// 验证时的型号名，用于日志和"布局不匹配"的错误信息。
    pub model: &'static str,
    /// 允许使用这套布局的 VID/PID。驱动打开句柄后会先校验，再读写任何数据。
    pub supported_ids: &'static [(u16, u16)],
    pub poll_addr: u32,
    pub lod_addr: u32,
    pub dpi_x_addr: u32,
    pub dpi_y_addr: u32,
    pub active_stage_addr: u32,
    /// 防抖、休眠与修正标志共用的 4 字节块。
    pub debounce_addr: u32,
    /// 标志字节在 [`Self::debounce_addr`] 块内的偏移。
    pub flags_offset: usize,
    /// 档位数在 `dpi_x` 读块中的偏移。
    pub stage_count_at: usize,
    /// 当前档位在 `dpi_x` 读块中的偏移。
    pub active_stage_at: usize,
    /// `dpi_x` / `dpi_y` 各读多少字节（含档位数与当前档位）。
    pub dpi_read_len: usize,
    pub max_stages: u8,
    /// `0xAA` 电量应答里已知的连接 flag。
    pub battery_link_flags: &'static [u8],
    /// 写应答 `[1]` 的期望值。接收器吞命令时会回放上一条应答，形状对不上就
    /// 不能算成功；不同型号的这一字节实测过不一样，所以放进布局而不是写死在
    /// 协议层——否则换型号后所有写入都会静默超时。
    pub write_ack: u8,
    /// 提交应答 `[1]` 的期望值（VT1 Pro 实测 `0x7d`）。
    pub commit_ack: u8,
    pub debounce_press_at: usize,
    pub debounce_release_at: usize,
    pub sleep_at: usize,
    /// 修正标志位；置位表示关闭。
    pub line_flag: u8,
    pub wave_flag: u8,
    /// 该型号真正开放的能力块。
    pub caps: Caps,
}

impl RapooLayout {
    pub fn supports(&self, vid: u16, pid: u16) -> bool {
        self.supported_ids.contains(&(vid, pid))
    }
}

/// VT1 Pro 的布局。2026-11-09 在有线（`24AE:4414`）和接收器（`24AE:1214`）
/// 两种接法下实测，两者寄存器布局一致，见 `docs/protocol.md`。
pub const VT1_PRO: RapooLayout = RapooLayout {
    model: "VT1 Pro",
    supported_ids: &[(0x24AE, 0x1214), (0x24AE, 0x4414)],
    poll_addr: 0x0880,
    lod_addr: 0x0884,
    dpi_x_addr: 0x0888,
    dpi_y_addr: 0x08C8,
    active_stage_addr: 0x0898,
    debounce_addr: 0x08C0,
    flags_offset: 3,
    stage_count_at: 14,
    active_stage_at: 16,
    dpi_read_len: 24,
    max_stages: 7,
    battery_link_flags: &[1, 2],
    write_ack: 0x00,
    commit_ack: 0x7D,
    debounce_press_at: 0,
    debounce_release_at: 1,
    sleep_at: 2,
    line_flag: 0x01,
    wave_flag: 0x02,
    caps: Caps::MOUSE_FULL,
};

/// `0x0896` 的原始值（档位数 − 1）→ 实际档位数。
///
/// `max` 是该型号 DPI 表的容量：`0x0888`/`0x08C8` 各 14 字节 = 7 档，档位数
/// 本身存在紧邻的 `0x0896`。以前这里写死 10：真读到大于 8 的档位数时，第 8/9
/// 档的"DPI"会取自档位数与当前档位这两个寄存器，`Cmd::Dpi` 还会反过来把 DPI
/// 写进它们。设备现在是 7 档，但这条约束必须是结构性的。
///
/// 用 `saturating_add`：坏帧里这个字节是 0xFF 时，`+ 1` 在 debug 下会 panic，
/// 直接把采集线程打死（虽然能被自愈逻辑重建，但没必要）。
fn stages_from_raw(raw: u8, max: u8) -> u8 {
    raw.saturating_add(1).min(max)
}

/// `0xAA` 应答 `[2]` 是百分比；不是百分比的字节一律当成"这一轮没有读数"。
///
/// 开机后第一帧实测回 `0x7F`（127%）——接收器此时还没从鼠标那里拿到电量，
/// 或者干脆是缓冲区里上一条应答被当成了电量。两种情况下把 127 画到界面上
/// 都是假的，宁可暂时显示"电量未知"。
fn battery_percent(raw: u8) -> Option<u8> {
    (raw <= 100).then_some(raw)
}

/// 一条应答算不算有效的电量读数：`[1]` 必须是该型号已知的连接 flag，
/// `[2]` 必须是百分比。
///
/// 只校验 `[2]` 挡不住旧应答回放：提交应答 `01 7d 00…` 的 `[2]` 是 0，
/// 被当电量就是 0%；`[2] ≤ 100` 的读应答回放同样会放行。flag 不在型号
/// 白名单里的帧一律不算电量。
fn battery_reply(resp: &[u8; 32], layout: &RapooLayout) -> Option<u8> {
    if !layout.battery_link_flags.contains(&resp[1]) {
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

/// 期望的应答种类。非读请求不能只看“来了一帧 status=1”：接收器被吞命令
/// 时会回放上一条应答，统一放行会让提交假成功。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Read,
    Write,
    Commit,
    Battery,
}

impl Reply {
    fn accepts(self, resp: &[u8; 32], layout: &RapooLayout) -> bool {
        match self {
            // 读的新旧由 `is_fresh` 判定，这里不再加内容约束。
            Reply::Read => true,
            // 实测写应答 `01 00 00 00`。读应答的数据从 [4] 开始，[1] 同样
            // 是 0，所以这条只能挡住形状明显不对的帧，挡不住「回放上一条
            // 读应答」；真正判定写入有没有落盘的是 confirm() 的整块回读。
            Reply::Write => resp[1] == layout.write_ack,
            // 实测提交应答 `01 7d`；回放的读/写应答不会长这样。
            Reply::Commit => resp[1] == layout.commit_ack,
            // 电量帧没有可判定新旧的字段，交给 `battery_reply` 校验内容。
            Reply::Battery => true,
        }
    }

    /// 超时后能不能原样重发。
    ///
    /// 只有读取可以：设备吞掉一条读命令，重发只是再读一次，没有副作用。
    /// 写入和提交不行——它们可能已经生效，只是应答丢了；重发就是再写一次
    /// 寄存器、再提交一次 EEPROM，把一个"结果未知"变成"写过两遍"。
    fn retriable(self) -> bool {
        matches!(self, Reply::Read | Reply::Battery)
    }
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
    io: Box<dyn HidTransport>,
    layout: &'static RapooLayout,
    /// Last response the device handed us; its repeat is what a stale read
    /// returns after a command the device swallowed.
    last: Cell<[u8; 32]>,
    /// 上一次被接受的是哪条读命令（地址、长度）。同一地址的重复读取在内
    /// 容不变时与过期回放逐字节相同，只有这个信息能把两种情况分开。
    last_read: Cell<Option<(u32, usize)>>,
    /// 上一次记进日志的异常电量字节，用来把重复的 WARN 压成一条。
    bad_battery: Cell<Option<u8>>,
    /// 提交后的通信保护期限，见 [`COMMIT_GUARD`]。
    guard_until: Option<Instant>,
    /// 已提交、等待只读确认的寄存器块（地址与写入值）。
    pending: Vec<(u32, [u8; 4])>,
    limits: Limits,
}

impl Rapoo {
    pub fn open(
        path: &[u16],
        layout: &'static RapooLayout,
        limits: Limits,
    ) -> Result<Rapoo, String> {
        let io = BlockingHid::open(path, RID, REPORT_LEN)?;
        // 布局是按型号验证的。VID/PID 不在白名单里就拒绝打开，避免把
        // VT1 Pro 的地址写到另一款鼠标的 EEPROM 上。这只能挡住"未验证的
        // PID 复用了 VT1 布局"；同一个接收器换鼠标时协议还没有可靠的
        // 型号名可查（0xB0 接收器信息未细究），那一层仍要靠真机确认。
        let (vid, pid) = io.ids().ok_or_else(|| "读取设备标识失败".to_string())?;
        if !layout.supports(vid, pid) {
            return Err(format!(
                "{} 的寄存器布局未覆盖该设备（{:04X}:{:04X}）",
                layout.model, vid, pid
            ));
        }
        Ok(Rapoo {
            io: Box::new(io),
            layout,
            last: Cell::new([0u8; 32]),
            last_read: Cell::new(None),
            bad_battery: Cell::new(None),
            guard_until: None,
            pending: Vec::new(),
            limits,
        })
    }

    fn xfer(&mut self, frame: &[u8; 32], reply: Reply) -> Result<[u8; 32], String> {
        let read = reply == Reply::Read;
        let addr = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let len = frame[3] as usize;
        // 只重试读取：写入/提交可能已经生效，只是应答丢了，重发会把一次
        // 结果未知的写入变成两次。所以非读命令只发一次，但把原来两次 200ms
        // 尝试的时间预算并进这一次（共 400ms），免得"慢一点的提交"被误判。
        let attempts = if reply.retriable() { 2 } else { 1 };
        let budget = 400;
        for _ in 0..attempts {
            self.io.send(frame)?;
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
                if let Ok(Some(resp)) = self.io.recv(Duration::ZERO) {
                    let Ok(resp) = <[u8; 32]>::try_from(resp) else {
                        continue;
                    };
                    match resp[0] {
                        1 => {
                            let settled = elapsed >= Duration::from_millis(40);
                            // A stale answer repeats the previous response byte
                            // for byte. 主判据仍是"与上次不同"；同一地址的
                            // 重复读取允许内容不变，见 `is_fresh`。
                            let fresh = is_fresh(read, &resp, &prev, repeated_read);
                            // 非读请求还要看应答形状：被吞的提交可能收到
                            // 上一条读/写应答，不能算成功。
                            if settled && fresh && reply.accepts(&resp, self.layout) {
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

    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        let f = read_frame(addr, len)?;
        let r = self.xfer(&f, Reply::Read)?;
        Ok(r[4..4 + len].to_vec())
    }

    fn write_raw(&mut self, addr: u32, data: &[u8]) -> Result<(), String> {
        self.xfer(&write_frame(addr, data)?, Reply::Write)?;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), String> {
        self.xfer(&commit_frame(), Reply::Commit)?;
        Ok(())
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
    fn battery_level(&mut self) -> Result<Option<u8>, String> {
        let f = battery_frame();
        let mut r = self.xfer(&f, Reply::Battery)?;
        if battery_reply(&r, self.layout).is_none() {
            sleep(Duration::from_millis(200));
            r = self.xfer(&f, Reply::Battery)?;
        }
        match battery_reply(&r, self.layout) {
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

    /// 读一个 4 字节对齐块；同一轮合批里只读一次。
    fn load_block<'a>(
        &mut self,
        blocks: &'a mut BTreeMap<u32, ([u8; 4], bool)>,
        addr: u32,
    ) -> Result<&'a mut ([u8; 4], bool), String> {
        use std::collections::btree_map::Entry;
        match blocks.entry(addr) {
            Entry::Occupied(slot) => Ok(slot.into_mut()),
            Entry::Vacant(slot) => {
                let b = self.read(addr, 4)?;
                Ok(slot.insert(([b[0], b[1], b[2], b[3]], false)))
            }
        }
    }

    /// 把一段连续读取切成 4 字节块。DPI 表按整表读、整表回读，避免相邻块
    /// 内容相同时被 `is_fresh` 误判成过期回放。
    fn read_blocks(&mut self, base: u32, len: usize) -> Result<Vec<(u32, [u8; 4])>, String> {
        let data = self.read(base, len)?;
        let mut out = Vec::new();
        for (i, chunk) in data.chunks(4).enumerate() {
            let mut arr = [0u8; 4];
            arr[..chunk.len()].copy_from_slice(chunk);
            out.push((base + i as u32 * 4, arr));
        }
        Ok(out)
    }

    /// 只读地读一遍完整配置，并返回按 4 字节块索引的原始字节。
    ///
    /// `snapshot()` 与 `confirm()` 共用它：确认路径必须只读，不能触发任何
    /// 补偿写入或落盘。
    fn read_config(&mut self) -> Result<(Snapshot, BTreeMap<u32, [u8; 4]>), String> {
        let p = self.read(self.layout.poll_addr, 4)?;
        let lm = self.read(self.layout.lod_addr, 4)?;
        let d = self.read(self.layout.debounce_addr, 4)?;
        let x = self.read(self.layout.dpi_x_addr, self.layout.dpi_read_len)?;
        let y = self.read(self.layout.dpi_y_addr, self.layout.dpi_read_len)?;

        let stage_count = stages_from_raw(x[self.layout.stage_count_at], self.layout.max_stages);
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

        let block = |b: &[u8]| [b[0], b[1], b[2], b[3]];
        let mut blocks = BTreeMap::new();
        blocks.insert(self.layout.poll_addr, block(&p));
        blocks.insert(self.layout.lod_addr, block(&lm));
        blocks.insert(self.layout.debounce_addr, block(&d));
        // X/Y 两段各 24 字节 = 6 个块；DPI 校验必须同时覆盖两个写入目标。
        for (base, raw) in [(self.layout.dpi_x_addr, &x), (self.layout.dpi_y_addr, &y)] {
            for (i, chunk) in raw.chunks_exact(4).enumerate() {
                blocks.insert(base + i as u32 * 4, block(chunk));
            }
        }

        Ok((
            Snapshot {
                model: String::new(),
                caps: self.layout.caps,
                polling_ms: p[0],
                lod: lm[0],
                motion: lm[1] != 0,
                stage_count,
                stage_active: x[self.layout.active_stage_at].min(stage_count.saturating_sub(1)),
                dpi,
                dpi_min: self.limits.dpi.0,
                dpi_max: self.limits.dpi.1,
                debounce_press: d[self.layout.debounce_press_at],
                debounce_release: d[self.layout.debounce_release_at],
                sleep_min: d[self.layout.sleep_at],
                line_corr: d[self.layout.flags_offset] & self.layout.line_flag == 0,
                wave_corr: d[self.layout.flags_offset] & self.layout.wave_flag == 0,
                save_error: None,
            },
            blocks,
        ))
    }
}

impl Driver for Rapoo {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.read_config().map(|(snapshot, _)| snapshot)
    }

    fn battery(&mut self) -> Result<Option<u8>, String> {
        if !self.layout.caps.battery {
            return Ok(None);
        }
        self.battery_level()
    }

    fn apply(&mut self, cmd: Cmd, snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
        self.apply_batch(&[cmd], snap)
    }

    /// 一轮输入里把同一 4 字节块的读-改-写合并，最后统一提交一次。
    ///
    /// 这里只到“提交”为止：回读要等 150 ms 通信保护间隔，由采集线程在
    /// 等待期间继续接收命令，然后调用 [`Self::confirm`]。DPI 的 X/Y 仍
    /// 成对写入；任何一条命令非法都不会留下半批写入。
    fn apply_batch(
        &mut self,
        cmds: &[Cmd],
        snap: &mut Snapshot,
    ) -> Result<WriteOutcome, WriteError> {
        let mut blocks: BTreeMap<u32, ([u8; 4], bool)> = BTreeMap::new();
        let mut dpi_loaded = false;

        for cmd in cmds {
            if !self.layout.caps.supports(*cmd) {
                return Err(WriteError::Rejected("该设备不支持这个设置".into()));
            }
            match *cmd {
                Cmd::Refresh => {}
                Cmd::Polling(ms) => {
                    if !self.limits.polling.contains(&ms) {
                        return Err(WriteError::Rejected(format!("不支持的回报率档位：{ms} ms")));
                    }
                    let e = self
                        .load_block(&mut blocks, self.layout.poll_addr)
                        .map_err(WriteError::Rejected)?;
                    if e.0[0] != ms {
                        e.0[0] = ms;
                        e.1 = true;
                    }
                }
                Cmd::Motion(on) => {
                    let v = on as u8;
                    let e = self
                        .load_block(&mut blocks, self.layout.lod_addr)
                        .map_err(WriteError::Rejected)?;
                    if e.0[1] != v {
                        e.0[1] = v;
                        e.1 = true;
                    }
                }
                Cmd::Lod(v) => {
                    let v = v.clamp(self.limits.lod.0, self.limits.lod.1);
                    let e = self
                        .load_block(&mut blocks, self.layout.lod_addr)
                        .map_err(WriteError::Rejected)?;
                    if e.0[0] != v {
                        e.0[0] = v;
                        e.1 = true;
                    }
                }
                Cmd::Line(on) => {
                    let idx = self.layout.flags_offset;
                    let bit = self.layout.line_flag;
                    let e = self
                        .load_block(&mut blocks, self.layout.debounce_addr)
                        .map_err(WriteError::Rejected)?;
                    let v = if on { e.0[idx] & !bit } else { e.0[idx] | bit };
                    if e.0[idx] != v {
                        e.0[idx] = v;
                        e.1 = true;
                    }
                }
                Cmd::Wave(on) => {
                    let idx = self.layout.flags_offset;
                    let bit = self.layout.wave_flag;
                    let e = self
                        .load_block(&mut blocks, self.layout.debounce_addr)
                        .map_err(WriteError::Rejected)?;
                    let v = if on { e.0[idx] & !bit } else { e.0[idx] | bit };
                    if e.0[idx] != v {
                        e.0[idx] = v;
                        e.1 = true;
                    }
                }
                Cmd::Debounce(v, press) => {
                    let v = v.clamp(self.limits.debounce.0, self.limits.debounce.1);
                    let idx = if press {
                        self.layout.debounce_press_at
                    } else {
                        self.layout.debounce_release_at
                    };
                    let e = self
                        .load_block(&mut blocks, self.layout.debounce_addr)
                        .map_err(WriteError::Rejected)?;
                    if e.0[idx] != v {
                        e.0[idx] = v;
                        e.1 = true;
                    }
                }
                Cmd::Sleep(m) => {
                    let m = m.clamp(self.limits.sleep.0, self.limits.sleep.1);
                    let idx = self.layout.sleep_at;
                    let e = self
                        .load_block(&mut blocks, self.layout.debounce_addr)
                        .map_err(WriteError::Rejected)?;
                    if e.0[idx] != m {
                        e.0[idx] = m;
                        e.1 = true;
                    }
                }
                Cmd::Activate(i) => {
                    let i = i.min(snap.stage_count.saturating_sub(1));
                    let e = self
                        .load_block(&mut blocks, self.layout.active_stage_addr)
                        .map_err(WriteError::Rejected)?;
                    if e.0[0] != i {
                        e.0[0] = i;
                        e.1 = true;
                    }
                }
                Cmd::Dpi(stage, value) => {
                    // `stage` selects a fixed 2-byte EEPROM slot; an out-of-range
                    // index would read-modify-write unrelated cells. Reject it
                    // here at the protocol boundary rather than trusting the
                    // caller to pass a stage the device actually has.
                    let count = snap.stage_count.clamp(1, self.layout.max_stages) as u32;
                    if stage as u32 >= count {
                        return Err(WriteError::Rejected(format!(
                            "dpi stage {stage} out of range (0..{count})"
                        )));
                    }
                    let value = value.clamp(self.limits.dpi.0, self.limits.dpi.1);
                    let bytes = value.to_le_bytes();
                    // DPI 表按整表读：相邻 4 字节块可能装着相同的 DPI，
                    // 逐块读会被“与上一条应答不同”的判据误杀。
                    if !dpi_loaded {
                        dpi_loaded = true;
                        for base in [self.layout.dpi_x_addr, self.layout.dpi_y_addr] {
                            let len = self.layout.dpi_read_len;
                            for (addr, arr) in
                                self.read_blocks(base, len).map_err(WriteError::Rejected)?
                            {
                                blocks.entry(addr).or_insert((arr, false));
                            }
                        }
                    }
                    // X/Y 两个块都要写，避免拆轴型号只改了一半。
                    for base in [self.layout.dpi_x_addr, self.layout.dpi_y_addr] {
                        let addr = base + stage as u32 * 2;
                        let mut done = 0usize;
                        for (block, off, n) in aligned_blocks(addr, 2) {
                            let e = self
                                .load_block(&mut blocks, block)
                                .map_err(WriteError::Rejected)?;
                            if e.0[off..off + n] != bytes[done..done + n] {
                                e.0[off..off + n].copy_from_slice(&bytes[done..done + n]);
                                e.1 = true;
                            }
                            done += n;
                        }
                    }
                }
            }
        }

        let dirty_blocks: Vec<(u32, [u8; 4])> = blocks
            .iter()
            .filter(|(_, (_, dirty))| *dirty)
            .map(|(&addr, (bytes, _))| (addr, *bytes))
            .collect();
        if dirty_blocks.is_empty() {
            return Ok(WriteOutcome::Unchanged);
        }

        let started = Instant::now();
        for (addr, bytes) in &dirty_blocks {
            if let Err(e) = self.write_raw(*addr, bytes) {
                // 已经写了一部分：结果不可假定，交给采集线程标记未确认并
                // 重读实际配置，而不是自动重放整个事务。
                self.guard_until = Some(Instant::now() + COMMIT_GUARD);
                self.pending.clear();
                crate::log::line(
                    "WARN",
                    &format!(
                        "{} 写入 0x{addr:04X} 失败（{e}），提交结果未确认",
                        self.layout.model
                    ),
                );
                return Err(WriteError::Unconfirmed(e));
            }
        }
        if let Err(e) = self.commit() {
            self.guard_until = Some(Instant::now() + COMMIT_GUARD);
            self.pending.clear();
            crate::log::line(
                "WARN",
                &format!(
                    "{} 提交应答超时（{e}），已等待 {} ms，结果未确认",
                    self.layout.model,
                    started.elapsed().as_millis()
                ),
            );
            return Err(WriteError::Unconfirmed(e));
        }

        self.pending = dirty_blocks;
        self.guard_until = Some(Instant::now() + COMMIT_GUARD);
        Ok(WriteOutcome::Committed)
    }

    /// 提交后的只读校验。读回完整配置，和本次提交的每个目标块逐一比对。
    ///
    /// 读取失败时保留目标块，采集线程可以只重试读取；一旦读成功，即使值
    /// 不一致也不再保留目标——设备实际值已经知道，不能靠重放写入去“修复”。
    fn confirm(&mut self, snap: &mut Snapshot) -> Result<ConfirmResult, String> {
        let (fresh, blocks) = self.read_config()?;
        let pending = std::mem::take(&mut self.pending);

        let mut mismatch = None;
        for (addr, expected) in &pending {
            match blocks.get(addr) {
                Some(actual) if actual == expected => {}
                Some(actual) => {
                    mismatch = Some(format!(
                        "0x{addr:04X} 回读 {} != 写入 {}",
                        hex(actual),
                        hex(expected)
                    ));
                }
                None => mismatch = Some(format!("0x{addr:04X} 不在本次读取范围")),
            }
        }

        *snap = fresh;
        match mismatch {
            Some(m) => {
                crate::log::line("WARN", &format!("{} 写入确认失败：{m}", self.layout.model));
                Ok(ConfirmResult::Mismatch(m))
            }
            None if pending.is_empty() => Ok(ConfirmResult::ReadOnly),
            None => Ok(ConfirmResult::Verified),
        }
    }

    fn next_io_at(&self) -> Option<Instant> {
        self.guard_until
    }

    fn link_alive(&self) -> bool {
        self.io.alive()
    }
}

/// 4 字节块的可读十六进制形式，只用于确认失败的日志/错误信息。
fn hex(b: &[u8; 4]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7 档是 VT1 Pro 的协议上限（DPI X/Y 各 14 字节）。再大就意味着
    /// "档位数"那个字节其实是第 8 档的 DPI 低字节，读会读错、写会写坏
    /// 别的寄存器；上限来自 layout，而不是驱动里的常量。
    #[test]
    fn stage_count_never_exceeds_the_dpi_table() {
        let max = VT1_PRO.max_stages;
        assert_eq!(stages_from_raw(0, max), 1);
        assert_eq!(stages_from_raw(6, max), 7);
        assert_eq!(stages_from_raw(7, max), 7);
        // 坏帧：debug 下也不能 panic。
        assert_eq!(stages_from_raw(0xFF, max), 7);
    }

    /// 布局里的地址/偏移必须自洽：索引越界会让采集线程 panic，地址错位会
    /// 把无关的 EEPROM 单元当成 DPI/防抖写入。新增型号时先让这条测试通过。
    #[test]
    fn vt1_layout_is_internally_consistent() {
        let l = &VT1_PRO;
        assert_eq!(l.model, "VT1 Pro");
        assert!(l.supports(0x24AE, 0x1214));
        assert!(l.supports(0x24AE, 0x4414));
        assert!(
            !l.supports(0x24AE, 0x1230),
            "未验证的 PID 不能复用 VT1 布局"
        );

        assert!(l.stage_count_at < l.dpi_read_len);
        assert!(l.active_stage_at < l.dpi_read_len);
        assert!(l.max_stages >= 1);
        assert!((l.max_stages as usize) * 2 <= l.dpi_read_len);
        assert!(l.flags_offset < 4);
        assert!(l.debounce_press_at < 4);
        assert!(l.debounce_release_at < 4);
        assert!(l.sleep_at < 4);
        assert_eq!(l.debounce_addr + l.flags_offset as u32, 0x08C3);
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
        assert_eq!(battery_reply(&r, &VT1_PRO), Some(71));
        r[1] = 1;
        r[2] = 0;
        assert_eq!(battery_reply(&r, &VT1_PRO), Some(0));
        // 提交应答 01 7d 00… 被回放。
        r[1] = 0x7d;
        r[2] = 0;
        assert_eq!(battery_reply(&r, &VT1_PRO), None);
        // flag 对但百分比超范围。
        r[1] = 2;
        r[2] = 0x7F;
        assert_eq!(battery_reply(&r, &VT1_PRO), None);
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

    /// 提交必须认实测的 `01 7d`；回放的读/写应答不能被当成提交成功。
    #[test]
    fn commit_reply_must_be_the_measured_confirmation() {
        let mut r = [0u8; 32];
        r[0] = 1;
        r[1] = 0x7D;
        assert!(Reply::Commit.accepts(&r, &VT1_PRO));
        r[1] = 0x00;
        assert!(
            !Reply::Commit.accepts(&r, &VT1_PRO),
            "写/读应答不能冒充提交"
        );
        assert!(Reply::Write.accepts(&r, &VT1_PRO));
        r[1] = 0x02;
        assert!(!Reply::Write.accepts(&r, &VT1_PRO));
    }

    // ---- 合批与写入校验的假设备 -------------------------------------------

    use crate::driver::VT1_PRO_LIMITS;
    use std::sync::{Arc, Mutex};

    struct FakeState {
        mem: BTreeMap<u32, [u8; 4]>,
        /// 真机应答缓冲区会一直重复上一条应答，直到下一条命令覆盖它。
        reply: Vec<u8>,
        commits: usize,
        writes: usize,
        reads: usize,
        /// 写命令仍回 OK，但不落到寄存器：模拟接收器吞命令。
        drop_writes: bool,
        /// 提交已经生效、只是应答丢了：缓冲区保持上一条应答。
        lose_commit_reply: bool,
        /// 只吞掉下一条读命令的应答，用来确认读仍在自动重试。
        drop_read_once: bool,
    }

    #[derive(Clone)]
    struct FakeIo(Arc<Mutex<FakeState>>);

    impl FakeIo {
        fn new() -> (Self, Arc<Mutex<FakeState>>) {
            let mut mem = BTreeMap::new();
            for (addr, bytes) in [
                (VT1_PRO.poll_addr, [1u8, 0, 1, 0xFF]),
                (VT1_PRO.lod_addr, [2, 1, 1, 0]),
                (VT1_PRO.debounce_addr, [8, 8, 10, 0]),
                (VT1_PRO.active_stage_addr, [0, 0, 0, 0]),
            ] {
                mem.insert(addr, bytes);
            }
            for addr in 0..4u32 {
                mem.insert(VT1_PRO.dpi_x_addr + addr * 4, [0x90, 0x01, 0x20, 0x03]);
                // Y 表尾部与 X 不同，模拟真机上那几字节是档位/档数而非 DPI，
                // 避免整表回读跟 X 逐字节相同被当成过期应答。
                let y = if addr == 3 {
                    [0x90, 0x01, 0x00, 0x00]
                } else {
                    [0x90, 0x01, 0x20, 0x03]
                };
                mem.insert(VT1_PRO.dpi_y_addr + addr * 4, y);
            }
            let state = Arc::new(Mutex::new(FakeState {
                mem,
                reply: Vec::new(),
                commits: 0,
                writes: 0,
                reads: 0,
                drop_writes: false,
                lose_commit_reply: false,
                drop_read_once: false,
            }));
            (Self(state.clone()), state)
        }
    }

    impl HidTransport for FakeIo {
        fn send(&mut self, report: &[u8]) -> Result<(), String> {
            let mut s = self.0.lock().unwrap();
            let addr = u32::from_le_bytes([report[4], report[5], report[6], report[7]]);
            let len = report[3] as usize;
            let mut reply = vec![0u8; REPORT_LEN];
            reply[0] = 1;
            // 应答丢失：缓冲区里仍是上一条应答，接收器什么都不回。
            let mut keep_stale = false;
            match report[2] {
                0xA4 => {
                    s.reads += 1;
                    if s.drop_read_once {
                        s.drop_read_once = false;
                        keep_stale = true;
                    }
                    for i in 0..len {
                        let a = addr + i as u32;
                        let block = a & !3;
                        let off = (a - block) as usize;
                        let bytes = s.mem.get(&block).copied().unwrap_or([0; 4]);
                        reply[4 + i] = bytes[off];
                    }
                }
                0xA5 => {
                    if !s.drop_writes {
                        let block = addr & !3;
                        let off = (addr - block) as usize;
                        let mut bytes = s.mem.get(&block).copied().unwrap_or([0; 4]);
                        bytes[off..off + len].copy_from_slice(&report[8..8 + len]);
                        s.mem.insert(block, bytes);
                    }
                    s.writes += 1;
                }
                0xA3 => {
                    s.commits += 1;
                    if s.lose_commit_reply {
                        keep_stale = true;
                    }
                    reply[1] = 0x7D;
                }
                0xAA => {
                    reply[1] = 1;
                    reply[2] = 50;
                }
                _ => {}
            }
            if !keep_stale {
                s.reply = reply;
            }
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            let s = self.0.lock().unwrap();
            Ok((!s.reply.is_empty()).then(|| s.reply.clone()))
        }

        fn alive(&self) -> bool {
            true
        }
    }

    fn test_rapoo(io: FakeIo) -> Rapoo {
        Rapoo {
            io: Box::new(io),
            layout: &VT1_PRO,
            last: Cell::new([0u8; 32]),
            last_read: Cell::new(None),
            bad_battery: Cell::new(None),
            guard_until: None,
            pending: Vec::new(),
            limits: VT1_PRO_LIMITS,
        }
    }

    fn seven_stages() -> Snapshot {
        Snapshot {
            stage_count: 7,
            dpi: vec![400, 800, 1600, 3200, 6400, 26000, 1000],
            ..Snapshot::default()
        }
    }

    /// 同一 4 字节块里的两个字段只读一次、写一次、提交一次。
    #[test]
    fn batch_merges_fields_in_the_same_block() {
        let (io, state) = FakeIo::new();
        let mut dev = test_rapoo(io);
        let mut snap = seven_stages();
        let outcome = dev
            .apply_batch(&[Cmd::Motion(true), Cmd::Lod(3)], &mut snap)
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Committed, "提交后等只读确认");
        assert_eq!(
            dev.confirm(&mut snap).unwrap(),
            ConfirmResult::Verified,
            "回读一致才算确认"
        );

        let s = state.lock().unwrap();
        assert_eq!(s.writes, 1, "Motion/Lod 共用同一块");
        assert_eq!(s.commits, 1, "整批只提交一次");
        assert_eq!(s.mem[&VT1_PRO.lod_addr], [3, 1, 1, 0]);
        assert_eq!(snap.lod, 3);
        assert!(snap.motion);
    }

    /// 一次改两个档位：X/Y 仍成对写入，但只提交一次。
    #[test]
    fn batch_writes_all_dpi_blocks_with_one_commit() {
        let (io, state) = FakeIo::new();
        let mut dev = test_rapoo(io);
        let mut snap = seven_stages();
        let outcome = dev
            .apply_batch(&[Cmd::Dpi(1, 1200), Cmd::Dpi(2, 2400)], &mut snap)
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Committed);
        assert_eq!(
            dev.confirm(&mut snap).unwrap(),
            ConfirmResult::Verified,
            "X/Y 两个写入目标都要校验"
        );

        let s = state.lock().unwrap();
        assert_eq!(s.writes, 4, "两档 × X/Y 各一个块");
        assert_eq!(s.commits, 1);
        // 第 1 档的 2 字节从 0x088A 开始，落在 0x0888 块内偏移 2。
        let x1 = s.mem[&VT1_PRO.dpi_x_addr];
        assert_eq!(u16::from_le_bytes([x1[2], x1[3]]), 1200);
        let y2 = s.mem[&(VT1_PRO.dpi_y_addr + 4)];
        assert_eq!(u16::from_le_bytes([y2[0], y2[1]]), 2400);
        assert_eq!(snap.dpi[1], 1200);
        assert_eq!(snap.dpi[2], 2400);
    }

    /// 写命令回 OK 但没落盘时，提交后的只读确认必须把这次写入判为失败；
    /// 确认只读，不会自动重放写入。
    #[test]
    fn swallowed_write_is_caught_by_read_back() {
        let (io, state) = FakeIo::new();
        state.lock().unwrap().drop_writes = true;
        let mut dev = test_rapoo(io);
        let mut snap = seven_stages();
        let outcome = dev.apply(Cmd::Polling(2), &mut snap).unwrap();
        assert_eq!(outcome, WriteOutcome::Committed);
        let writes_after_apply = state.lock().unwrap().writes;

        match dev.confirm(&mut snap).unwrap() {
            ConfirmResult::Mismatch(m) => assert!(m.contains("回读"), "{m}"),
            other => panic!("未落盘必须判为 Mismatch，实际 {other:?}"),
        }
        assert_eq!(
            state.lock().unwrap().writes,
            writes_after_apply,
            "确认失败不能重放写入"
        );
    }

    /// 提交已经生效、应答却在半路丢了：结果未知，但**不能**再提交一次。
    /// 重发会把一个"不知道有没有写进去"变成"EEPROM 被擦写两遍"。
    #[test]
    fn a_lost_commit_reply_is_not_resent() {
        let (io, state) = FakeIo::new();
        state.lock().unwrap().lose_commit_reply = true;
        let mut dev = test_rapoo(io);
        let mut snap = seven_stages();

        match dev.apply(Cmd::Lod(3), &mut snap).unwrap_err() {
            WriteError::Unconfirmed(m) => assert!(m.contains("超时"), "{m}"),
            other => panic!("提交结果未知必须报未确认，实际 {other:?}"),
        }
        assert_eq!(
            state.lock().unwrap().commits,
            1,
            "提交结果未知时不能再提交一次"
        );
        assert!(dev.pending.is_empty(), "未确认时不保留待校验目标");
    }

    /// 读超时仍是自动重发一次：读取没有副作用，重发只是再读一遍。
    #[test]
    fn a_dropped_read_is_still_retried() {
        let (io, state) = FakeIo::new();
        state.lock().unwrap().drop_read_once = true;
        let mut dev = test_rapoo(io);
        let mut snap = seven_stages();

        let outcome = dev.apply(Cmd::Lod(3), &mut snap).unwrap();
        assert_eq!(outcome, WriteOutcome::Committed);
        assert_eq!(state.lock().unwrap().reads, 2, "读超时后应重发一次");
    }
}
