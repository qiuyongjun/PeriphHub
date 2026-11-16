use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::demo;
use crate::driver::{self, DeviceType, Driver, DriverSpec, Limits};

pub use crate::driver::{Cmd, Snapshot};
use crate::hid::{self, ConnectionType};

pub const FULL_EVERY: Duration = Duration::from_secs(4);
pub const BATTERY_EVERY: Duration = Duration::from_secs(20);
const RETRY_EVERY: Duration = Duration::from_millis(1500);
/// 插拔通知的合并窗口。一台复合设备插上会连着报好几条接口通知，每条都扫一
/// 遍等于把枚举开销翻几倍；等这一小会儿，最后一次事件之后的状态才是终态。
const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(400);
/// 退出时等采集线程收尾的总时长上限（见 `DeviceManager::shutdown`）。
const SHUTDOWN_WAIT: Duration = Duration::from_millis(1500);
/// 采集线程正在执行可能写 EEPROM 的调用时，退出多等一会儿；仍不是无限
/// 等待，避免驱动卡死时“退出”点了没反应。
const WRITE_SHUTDOWN_WAIT: Duration = Duration::from_secs(6);
/// 关通道后采样 busy 前的调度窗口：被唤醒的采集线程要取走队列里最后一批
/// 命令并进入写序列，立刻采样会漏掉这段刚启动的写入。
const SHUTDOWN_SETTLE: Duration = Duration::from_millis(20);
/// 采集线程异常退出后的自动重建次数上限（60 秒窗口内）。设备返回一帧畸形
/// 数据就会 panic，无限重建会变成打转，超过次数就把错误摊给用户。
const MAX_RESTARTS: u8 = 3;

/// 全量配置读取的策略。主页只需摘要，配置页才需要秒级刷新；浮窗只读电量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ReadMode {
    BatteryOnly = 0,
    Summary = 1,
    Full = 2,
}

impl ReadMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => ReadMode::Summary,
            2 => ReadMode::Full,
            _ => ReadMode::BatteryOnly,
        }
    }

    /// 采集线程的空闲轮询间隔。只有浮窗会长时间静止，其他模式都要跟手。
    fn wait_ms(self) -> u64 {
        if self == ReadMode::BatteryOnly {
            900
        } else {
            200
        }
    }

    /// 周期全量读的间隔；`None` 表示只保留首次读取和电量轮询。
    fn full_every(self) -> Option<Duration> {
        match self {
            ReadMode::BatteryOnly => None,
            ReadMode::Summary => Some(Duration::from_secs(20)),
            ReadMode::Full => Some(FULL_EVERY),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(pub Arc<str>);

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub brand: String,
    pub model: String,
    /// 设备形态：首页卡片和配置页的图标据此区分鼠标、键盘、接收器。
    pub kind: DeviceType,
    pub connection: ConnectionType,
    pub has_config: bool,
    /// 该型号的取值范围，配置页的步进/下拉都从这里取。
    pub limits: Limits,
    /// Why a device without a driver can only be listed, shown in its page.
    pub reason: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Online,
    Offline(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditorState {
    pub lod: u8,
    pub deb_press: u8,
    pub deb_release: u8,
    pub sleep_min: u8,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            lod: 2,
            deb_press: 8,
            deb_release: 8,
            sleep_min: 10,
        }
    }
}

impl EditorState {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        Self {
            lod: s.lod,
            deb_press: s.debounce_press,
            deb_release: s.debounce_release,
            sleep_min: s.sleep_min,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceState {
    pub link: Link,
    pub battery: Option<u8>,
    /// 快照按帧共享：配置页每帧都会读它，整份深拷贝（含 DPI 表）没必要。
    /// `Arc` 也让同一帧内所有控件看到的是同一份数据。
    pub snap: Option<Arc<Snapshot>>,
    pub editor: EditorState,
    pub synced: Option<Instant>,
    pub last_error: Option<String>,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            link: Link::Connecting,
            battery: None,
            snap: None,
            editor: EditorState::default(),
            synced: None,
            last_error: None,
        }
    }
}

/// Keep the last value per field, at the slot of its *first* occurrence.
///
/// 契约：同名命令只留最后一个值，但它落在**第一次出现**的位置上，所以不同
/// 字段之间的先后顺序可能因此重排（`Activate(2), Dpi(2,1600), Activate(3)`
/// 会变成 `Activate(3), Dpi(2,1600)`）。这是安全的，前提是每条命令都实现为
/// "对独立字段的绝对写入"——执行合批结果与逐条执行整个队列，最终状态一致。
/// 驱动若要引入有先后依赖的命令序列，必须先改这里（或改为不在队列里合并）。
fn coalesce(cmds: &mut Vec<Cmd>) {
    let mut out: Vec<Cmd> = Vec::with_capacity(cmds.len());
    for cmd in cmds.drain(..) {
        match out.iter_mut().find(|queued| queued.same_field(&cmd)) {
            Some(slot) => *slot = cmd,
            None => out.push(cmd),
        }
    }
    *cmds = out;
}

struct DeviceMsg {
    snap: Option<Snapshot>,
    battery: Option<u8>,
    link: Link,
    error: Option<String>,
    /// How many of our commands the worker had taken off the queue when this
    /// snapshot was read. See `Entry::sent`.
    consumed: u64,
}

pub struct Entry {
    pub info: DeviceInfo,
    pub state: DeviceState,
    /// 打开设备用的 HID path。重插后 container id 不变但 path 通常变化，
    /// 需要靠它发现旧 worker 已失效。
    path: Option<Vec<u16>>,
    /// 协议适配器规格。采集线程崩掉后要原样重建，所以和 path 一起存着。
    spec: Option<DriverSpec>,
    /// Commands handed to the worker so far. A snapshot read before the newest
    /// of them describes a state the user has already moved past, so applying
    /// it would undo the edit on screen — and, worse, the next +/− step would
    /// be computed from the reverted value, which is what made rapid edits look
    /// like they were being overwritten by the read-back.
    sent: u64,
    /// 自动重建次数与最后一次重建时间，见 `restart_worker`。
    restarts: u8,
    last_restart: Option<Instant>,
    /// 采集线程正在执行一次可能写 EEPROM 的调用；退出时据此决定多等多久。
    busy: Option<Arc<AtomicBool>>,
    tx: Option<Sender<Cmd>>,
    rx: Option<Receiver<DeviceMsg>>,
    /// 采集线程句柄。退出时先关通道再 join，避免把正在写的 EEPROM 扇区
    /// 截断在 commit 之前。
    join: Option<std::thread::JoinHandle<()>>,
}

/// Take everything that is already queued, so a burst of edits is applied as
/// one batch instead of being interleaved with read-backs of older state.
///
/// `Disconnected` 只表示队列已取空且发送端不在了；已经取出的命令仍要执行
/// 完再退出，否则"调完参数立刻退出"会丢掉最后一批修改。退出由下一轮
/// `recv_timeout` 返回 `Disconnected` 时完成。
fn drain_queue(rx: &Receiver<Cmd>, first: Option<Cmd>) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    if let Some(c) = first {
        cmds.push(c);
    }
    while let Ok(c) = rx.try_recv() {
        cmds.push(c);
    }
    cmds
}

/// 相当于"d 之前"的时刻；系统刚开机单调钟不足 d 时退化为现在，只会推迟
/// 一个轮询周期，避免直接相减在理论上 panic。
fn ago(d: Duration) -> Instant {
    Instant::now().checked_sub(d).unwrap_or_else(Instant::now)
}

/// 同一条后台错误只记一次日志。
///
/// 鼠标休眠时 `battery()` 返回 Err 是常态，而电量是 20 秒一轮的后台轮询：
/// 照直记一晚就是几千条，1 MB 的滚动日志会被这些噪音冲掉，真正的现场反而
/// 没了。返回值表示"这条内容和上次不同，需要写日志"。
fn is_new_error(last: &mut Option<String>, msg: &str) -> bool {
    if last.as_deref() == Some(msg) {
        return false;
    }
    *last = Some(msg.to_string());
    true
}

/// 采集线程"正在做可能写 EEPROM 的调用"的标志，作用域结束时自动复位。
///
/// 手工配对 `store(true)` / `store(false)` 在驱动 panic 时只执行前半段，
/// `busy` 会永远留在 true 上，退出时 `shutdown` 白等满 6 秒。Drop 在
/// unwind 时同样会跑，所以用它替换手工复位。
struct BusyGuard(Arc<AtomicBool>);

impl BusyGuard {
    fn new(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag.clone())
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// 起一个采集线程并返回它与主线程之间的两条通道（外加句柄，退出时好收尾）。
fn spawn_worker(
    spec: DriverSpec,
    path: Vec<u16>,
    read_mode: Arc<AtomicU8>,
    ctx: egui::Context,
) -> (
    Sender<Cmd>,
    Receiver<DeviceMsg>,
    std::thread::JoinHandle<()>,
    Arc<AtomicBool>,
) {
    let (tx, rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let busy = Arc::new(AtomicBool::new(false));
    let worker_busy = busy.clone();
    let handle = std::thread::Builder::new()
        .name(format!("{:?}-worker", spec.kind))
        .spawn(move || real_worker(path, spec, rx, msg_tx, ctx, read_mode, worker_busy))
        .expect("启动设备采集线程");
    (tx, msg_rx, handle, busy)
}

/// 给条目起一个新的采集线程，并把界面状态切回"连接中"。只读设备没有
/// 驱动规格，直接跳过。
fn start_worker(entry: &mut Entry, ctx: &egui::Context, read_mode: &Arc<AtomicU8>) {
    let (Some(spec), Some(path)) = (entry.spec, entry.path.clone()) else {
        return;
    };
    let (tx, rx, join, busy) = spawn_worker(spec, path, read_mode.clone(), ctx.clone());
    entry.tx = Some(tx);
    entry.rx = Some(rx);
    entry.join = Some(join);
    entry.busy = Some(busy);
    entry.sent = 0;
    entry.state.snap = None;
    entry.state.editor = EditorState::default();
    entry.state.link = Link::Connecting;
}

/// 采集线程崩溃后的自愈：先把状态摊给用户，再重建一个（有限次）。
///
/// 之前这里只是 `break`，于是线程 panic 之后条目永远停在 `Connecting`：
/// 没有错误、不会重连、`scan()` 又因为 path 没变而保留它，只能靠重插设备。
fn restart_worker(entry: &mut Entry, ctx: &egui::Context, read_mode: &Arc<AtomicU8>) {
    entry.tx = None;
    entry.rx = None;
    if entry.spec.is_none() || entry.path.is_none() {
        return;
    }
    // 同一帧坏数据会让新线程立刻再崩一次，所以按 60 秒窗口计数，别打转。
    let recent = entry
        .last_restart
        .is_some_and(|t| t.elapsed() < Duration::from_secs(60));
    entry.restarts = if recent {
        entry.restarts.saturating_add(1)
    } else {
        1
    };
    entry.last_restart = Some(Instant::now());
    crate::log::line(
        "WARN",
        &format!(
            "{} {} 的采集线程异常退出（60 秒内第 {} 次）",
            entry.info.brand, entry.info.model, entry.restarts
        ),
    );
    if entry.restarts > MAX_RESTARTS {
        entry.state.link =
            Link::Offline("设备读取线程反复异常退出，请重新扫描或重插设备".to_string());
        entry.state.snap = None;
        return;
    }
    start_worker(entry, ctx, read_mode);
    ctx.request_repaint();
}

fn real_worker(
    path: Vec<u16>,
    spec: DriverSpec,
    rx: Receiver<Cmd>,
    tx: Sender<DeviceMsg>,
    ctx: egui::Context,
    read_mode: Arc<AtomicU8>,
    busy: Arc<AtomicBool>,
) {
    let mut dev: Option<Box<dyn Driver>> = None;
    let mut snap: Option<Snapshot> = None;
    let mut last_open = Instant::now();
    let mut last_full = ago(FULL_EVERY);
    let mut last_batt = ago(BATTERY_EVERY);
    let mut last_error: Option<String> = None;
    let mut last_batt_error: Option<String> = None;
    let mut consumed: u64 = 0;

    loop {
        // 浮窗模式下没有人会发设备命令，把唤醒间隔放长；主页摘要模式只
        // 需要低频全量读，也按 200ms 醒来接收命令。
        let mode = ReadMode::from_u8(read_mode.load(Ordering::Relaxed));
        // Commands first: whichever batch is waiting has to be counted even if
        // the device is gone, or the UI would wait forever for a confirmation.
        let wait = mode.wait_ms();
        let first = match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut cmds = drain_queue(&rx, first);
        consumed += cmds.len() as u64;
        coalesce(&mut cmds);
        let force_full = cmds.iter().any(|c| matches!(c, Cmd::Refresh));

        if dev.is_none() && last_open.elapsed() >= RETRY_EVERY {
            last_open = Instant::now();
            match driver::open(spec, &path) {
                Ok(d) => {
                    dev = Some(d);
                    last_full = ago(FULL_EVERY);
                    last_batt = ago(BATTERY_EVERY);
                    last_error = None;
                    // 换了新连接（或设备刚醒），同样错误要重新留痕。
                    last_batt_error = None;
                }
                Err(e) => {
                    if last_error.as_deref() != Some(e.as_str()) {
                        last_error = Some(e.clone());
                        let _ = tx.send(DeviceMsg {
                            snap: None,
                            battery: None,
                            link: Link::Offline(e),
                            error: None,
                            consumed,
                        });
                        ctx.request_repaint();
                    }
                }
            }
        }

        let Some(d) = dev.as_mut() else {
            continue;
        };

        let mut error = None;
        let mut changed = false;
        // 命令被设备拒绝时，界面上那份乐观更新过的新值就是错的，必须马上
        // 重读全量把它纠正回来，不能让用户对着假值再点下一步。
        let mut rejected = false;

        if !cmds.is_empty() {
            // The driver owns the "write then re-read just this field" step,
            // so one burst costs one read-back per block, not per command.
            let _busy = BusyGuard::new(&busy);
            let mut scratch = Snapshot::default();
            for cmd in &cmds {
                if matches!(cmd, Cmd::Refresh) {
                    continue;
                }
                let target = snap.as_mut().unwrap_or(&mut scratch);
                match d.apply(*cmd, target) {
                    Ok(()) => {
                        if snap.is_some() {
                            changed = true;
                        }
                    }
                    Err(e) => {
                        error = Some(e);
                        rejected = true;
                    }
                }
            }
        }

        if force_full || rejected {
            // 回绕计时器 = 下一步一定全量重读：既用于"刷新配置"，也用于把
            // 被设备拒绝的乐观更新纠正回来。
            last_full = ago(FULL_EVERY);
        }

        // 全量读只服务于"当前页面真的要看配置"。浮窗和主页只留首次读
        // （型号、能力得知道一次）；主页摘要 20 秒一次，配置页才 4 秒一次。
        let due_full = force_full
            || rejected
            || snap.is_none()
            || mode
                .full_every()
                .is_some_and(|every| last_full.elapsed() >= every);
        if due_full {
            let _busy = BusyGuard::new(&busy);
            let full = d.snapshot();
            match full {
                Ok(s) => {
                    snap = Some(s);
                    last_full = Instant::now();
                    changed = true;
                }
                Err(e) => {
                    dev = None;
                    snap = None;
                    last_open = Instant::now();
                    let _ = tx.send(DeviceMsg {
                        snap: None,
                        battery: None,
                        link: Link::Offline(e),
                        error: None,
                        consumed,
                    });
                    ctx.request_repaint();
                    continue;
                }
            }
        }

        let mut battery = None;
        if last_batt.elapsed() >= BATTERY_EVERY {
            let _busy = BusyGuard::new(&busy);
            let batt = d.battery();
            match batt {
                Ok(b) => {
                    battery = b;
                    last_batt = Instant::now();
                    last_batt_error = None;
                }
                Err(e) => {
                    // 浮窗模式停掉了周期全量读，接收器被拔这类断连只能靠
                    // 电量轮询发现。句柄还活着说明只是设备休眠，保留在线
                    // 状态；句柄已失效就丢掉驱动，交给重连逻辑并更新浮窗。
                    if !d.link_alive() {
                        dev = None;
                        snap = None;
                        last_open = Instant::now();
                        let _ = tx.send(DeviceMsg {
                            snap: None,
                            battery: None,
                            link: Link::Offline(e),
                            error: None,
                            consumed,
                        });
                        ctx.request_repaint();
                        continue;
                    }
                    // 电量是后台轮询，无线鼠标休眠时失败很常见；它不是用户
                    // 刚做的操作，不能拿去触发“操作失败”提示。
                    //
                    // 失败也要把计时器推到下一轮：否则 Full 模式下每个 200 ms
                    // 周期都会重试一次，日志去重只是把洪水藏起来，设备照样被
                    // 反复吵醒。相同错误只在内容变化时记一条。
                    if is_new_error(&mut last_batt_error, &e) {
                        crate::log::line("WARN", &format!("读取电量失败：{e}"));
                    }
                    last_batt = Instant::now();
                }
            }
        }

        if changed || battery.is_some() || error.is_some() {
            let _ = tx.send(DeviceMsg {
                snap: if changed { snap.clone() } else { None },
                battery,
                link: Link::Online,
                error,
                consumed,
            });
            ctx.request_repaint();
        }
    }
}

fn demo_worker(
    mut snap: Snapshot,
    battery: Option<u8>,
    rx: Receiver<Cmd>,
    tx: Sender<DeviceMsg>,
    ctx: egui::Context,
) {
    let _ = tx.send(DeviceMsg {
        snap: Some(snap.clone()),
        battery,
        link: Link::Online,
        error: None,
        consumed: 0,
    });
    let mut consumed: u64 = 0;
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut cmds = drain_queue(&rx, first);
        consumed += cmds.len() as u64;
        // Same batching rule as the real worker: one write per field per burst,
        // so the demo shell shows the same behaviour the device does.
        coalesce(&mut cmds);
        std::thread::sleep(Duration::from_millis(40));
        for cmd in cmds {
            match cmd {
                Cmd::Refresh => {}
                other => demo::apply(&mut snap, other),
            }
        }
        let _ = tx.send(DeviceMsg {
            snap: Some(snap.clone()),
            battery,
            link: Link::Online,
            error: None,
            consumed,
        });
        ctx.request_repaint();
    }
}

/// 把最新枚举到的身份写回一个已经在列表里的条目。
///
/// 同一个 container id 不等于同一个身份：三模键盘插线是 `258A:010C`（有线），
/// 走接收器是 `3554:FA09`（2.4G），落在同一个 container 里但 VID/PID 不同。
/// 连接方式是型号表加实例路径算出来的，不随快照更新，所以不在这里刷新就会
/// 永远停在第一次枚举时的值——线拔了卡片还写着"有线"。
fn refresh_info(entry: &mut Entry, dev: &hid::HidDevice) {
    entry.info.connection = dev.connection;
    entry.info.brand = dev.brand.clone();
    entry.info.limits = dev.limits;
    entry.info.reason = dev.reason;
    // 设备自己报的名字（HID++ 读回来的真实型号）比型号表里的占位名准：已经
    // 有快照时不要用表里的通用名把它盖回去。
    if entry.state.snap.is_none() {
        entry.info.model = dev.model.clone();
        entry.info.kind = dev.kind;
    }
}

pub struct DeviceManager {
    entries: Vec<Entry>,
    demo: bool,
    ctx: egui::Context,
    /// 当前页面需要多频繁的全量读取（见 `ReadMode`）。用原子量而不是
    /// 通道：这是单向的"状态广播"。
    read_mode: Arc<AtomicU8>,
    /// 后台枚举线程的结果通道。
    ///
    /// `hid::enumerate` 会对系统里每个 HID 接口打开句柄、读产品串和
    /// preparsed data；几十个接口时是几百毫秒的量级，蓝牙设备更慢。
    /// 放在 UI 线程上，点一次"重新扫描"界面就僵一下，所以扫描整体挪到
    /// 后台，结果回到 `poll` 里再合并进设备表。
    scan_rx: Option<Receiver<Vec<hid::HidDevice>>>,
    /// 扫描进行中又点了"重新扫描"：等这一轮结果应用完再补一轮。
    rescan_queued: bool,
    /// 系统插拔通知的"有新事件"位。回调线程只置位，枚举仍由 UI 线程发起。
    dirty: Arc<AtomicBool>,
    /// 这一波通知里第一条的时刻。到 `HOTPLUG_DEBOUNCE` 之后才真正扫描。
    dirty_since: Option<Instant>,
    /// 最近一次真正把枚举结果合并进设备表的时刻；手动与自动扫描共用。
    last_scan_completed: Option<Instant>,
    /// 最近一次枚举线程中断且结果没有应用的时刻，用于在界面上承认失败。
    last_scan_failed: Option<Instant>,
    /// 插拔订阅的句柄。只为了生存期而拿着它：Drop 时注销，回调不会打到已经
    /// 销毁的管理器上。
    _watch: Option<hid::DeviceWatch>,
}

impl DeviceManager {
    pub fn new(ctx: &egui::Context) -> Self {
        let demo = std::env::var("PERIPHHUB_DEMO")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        // 订阅插拔通知：回调只置位，扫描还是回到 `poll` 里发起。订阅失败就
        // 退回手动扫描，功能不受影响。
        let dirty = Arc::new(AtomicBool::new(false));
        let watch = if demo {
            None
        } else {
            hid::watch(ctx, dirty.clone())
        };
        if !demo && watch.is_none() {
            crate::log::line("WARN", "订阅设备插拔通知失败，插拔后只能手动重新扫描");
        }
        let mut manager = Self {
            entries: Vec::new(),
            demo,
            ctx: ctx.clone(),
            read_mode: Arc::new(AtomicU8::new(ReadMode::Full as u8)),
            scan_rx: None,
            rescan_queued: false,
            dirty,
            dirty_since: None,
            last_scan_completed: None,
            last_scan_failed: None,
            _watch: watch,
        };
        manager.start_scan();
        manager
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    /// 枚举线程还在跑，或插拔事件正在等合并窗口。界面据此显示"正在扫描"，
    /// 而不是"没有发现设备"。
    pub fn is_scanning(&self) -> bool {
        self.scan_rx.is_some() || self.dirty_since.is_some()
    }

    /// 自动插拔检测是否可用。订阅失败时界面必须明示手动扫描兜底，而不是
    /// 继续宣称"自动检测已开启"。
    pub fn auto_detect_enabled(&self) -> bool {
        self.demo || self._watch.is_some()
    }

    /// 最近一次真正合并了枚举结果的时间。自动扫描完成后也要刷新它。
    pub fn last_scan_completed(&self) -> Option<Instant> {
        self.last_scan_completed
    }

    /// 最近一次结果没有合并的时间；用于让"扫描失败"覆盖旧的"已更新"。
    pub fn last_scan_failed(&self) -> Option<Instant> {
        self.last_scan_failed
    }

    /// 插拔合并窗口还有多久关闭。界面据此把重绘提前到窗口关闭那一刻，否则
    /// 要等下一次常规重绘（最长 1 秒）才会真正发起扫描。
    pub fn pending_scan_in(&self) -> Option<Duration> {
        self.dirty_since
            .map(|t| HOTPLUG_DEBOUNCE.saturating_sub(t.elapsed()))
    }

    /// 页面切换时广播新的读取策略。
    pub fn set_read_mode(&self, mode: ReadMode) {
        self.read_mode.store(mode as u8, Ordering::Relaxed);
    }

    /// 退出前收尾：关掉命令通道让采集线程从 `recv_timeout` 退出，再等它把手上
    /// 那一帧做完。HID++ 写扇区是 startWrite → 16 字节块 → commit 的多步序列，
    /// 进程被直接杀掉可能留下 CRC 不匹配的扇区。
    ///
    /// 等待有总时长上限：真卡在超时轮询里的线程不该让"退出"点了没反应。
    pub fn shutdown(&mut self) {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let busy_flags: Vec<Arc<AtomicBool>> =
            self.entries.iter().filter_map(|e| e.busy.clone()).collect();
        let mut pending = 0usize;
        for mut entry in self.entries.drain(..) {
            // 先丢发送端：`recv_timeout` 才会立刻返回 Disconnected 而不是等超时。
            entry.tx = None;
            entry.rx = None;
            if let Some(handle) = entry.join.take() {
                let done = done_tx.clone();
                std::thread::spawn(move || {
                    let _ = handle.join();
                    let _ = done.send(());
                });
                pending += 1;
            }
        }
        // 通道关闭会唤醒采集线程，但它可能刚取走队列里最后一批写命令；
        // 等它进入 `BusyGuard` 后再采样，才能拿到正确的宽限期。
        std::thread::sleep(SHUTDOWN_SETTLE);
        let busy = busy_flags.iter().any(|b| b.load(Ordering::Relaxed));
        let deadline = Instant::now()
            + if busy {
                WRITE_SHUTDOWN_WAIT
            } else {
                SHUTDOWN_WAIT
            };
        while pending > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || done_rx.recv_timeout(left).is_err() {
                break;
            }
            pending -= 1;
        }
        if pending > 0 {
            crate::log::line(
                "WARN",
                &format!("退出时仍有 {pending} 个采集线程没结束，只能直接终止进程"),
            );
        }
    }

    pub fn list(&self) -> &[Entry] {
        &self.entries
    }

    pub fn rescan(&mut self) {
        self.start_scan();
    }

    /// 起一轮后台枚举。已有扫描在跑时不叠加：记一个标记，等结果回来再补
    /// 一轮，按钮连点也不会同时开一堆线程。
    fn start_scan(&mut self) {
        if self.demo {
            self.seed_demo_devices();
            // 演示数据不会走枚举通道，但“重新扫描”确实完成了：刷新完成时间，
            // 让按钮的“已更新”反馈保持一致。
            self.last_scan_completed = Some(Instant::now());
            return;
        }
        if self.scan_rx.is_some() {
            self.rescan_queued = true;
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("hid-scan".into())
            .spawn(move || {
                let _ = tx.send(hid::enumerate());
                // 线程自己叫醒界面，否则结果要等下一次交互才被看到。
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            // 开不出线程就只能回到同步枚举：慢一点，但列表不能空着。
            crate::log::line("WARN", &format!("启动设备枚举线程失败：{e}，改为同步扫描"));
            let found = hid::enumerate();
            self.reconcile(found);
            self.last_scan_completed = Some(Instant::now());
            return;
        }
        self.scan_rx = Some(rx);
    }

    /// 后台扫描的结果到了就并进设备表；返回 true 表示列表发生了变化。
    fn apply_scan_result(&mut self) -> bool {
        let result = self.scan_rx.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(found)) => {
                self.scan_rx = None;
                // 只有结果真的应用了才算“已更新”；发起时间不算完成时间。
                self.last_scan_completed = Some(Instant::now());
                self.reconcile(found);
                if self.rescan_queued {
                    self.rescan_queued = false;
                    self.start_scan();
                }
                true
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                // 枚举线程没能把结果送回来（panic 或进程在退）；别让界面
                // 永远停在"正在扫描"。
                self.scan_rx = None;
                // 这不是完成：不能刷新“已更新”，让界面显示失败态。
                self.last_scan_failed = Some(Instant::now());
                crate::log::line("WARN", "设备枚举线程中断，本轮扫描结果没有应用");
                if self.rescan_queued {
                    self.rescan_queued = false;
                    self.start_scan();
                }
                true
            }
            _ => false,
        }
    }

    /// 插拔通知的状态机：只回答"现在该扫了吗"，真正的扫描由 `poll` 发起。
    ///
    /// 拆出来是为了能单独测——直接调 `start_scan` 会真的去枚举系统设备。
    fn hotplug_due(&mut self) -> bool {
        if self.dirty.swap(false, Ordering::Relaxed) {
            // 只认这一波里第一条的时间：持续插拔不应该把窗口一直往后推，
            // 否则界面上永远等不到更新。
            self.dirty_since.get_or_insert_with(Instant::now);
        }
        if self
            .dirty_since
            .is_some_and(|t| t.elapsed() >= HOTPLUG_DEBOUNCE)
        {
            self.dirty_since = None;
            return true;
        }
        false
    }

    fn seed_demo_devices(&mut self) {
        if !self.entries.is_empty() {
            return;
        }
        for d in demo::devices() {
            let (tx, rx) = mpsc::channel();
            let (msg_tx, msg_rx) = mpsc::channel();
            let ctx = self.ctx.clone();
            std::thread::Builder::new()
                .name(format!("demo-{}", d.info.id.0))
                .spawn(move || demo_worker(d.snap, d.battery, rx, msg_tx, ctx))
                .expect("启动演示线程");
            self.entries.push(Entry {
                info: d.info,
                state: DeviceState::default(),
                sent: 0,
                path: None,
                spec: None,
                restarts: 0,
                last_restart: None,
                busy: None,
                tx: Some(tx),
                rx: Some(msg_rx),
                join: None,
            });
        }
    }

    /// 把一次枚举结果并进设备表。
    fn reconcile(&mut self, found: Vec<hid::HidDevice>) {
        // 同 id（container id）但 path 变了 = 设备重插过，旧 worker 一直重试
        // 失效 path 会永远离线；丢弃条目让下面的循环重建 worker（旧 worker
        // 随 channel 关闭自然退出）。
        //
        // 驱动规格变了同样要重建：三模键盘插线和走接收器是两个 PID，落在同
        // 一个 container 里但通道和能力都不同，沿用旧 worker 会一直读不到。
        // 只读设备（无驱动、无 worker）本来就不存 path，重插也无需重建。
        self.entries.retain(|e| {
            let Some(d) = found.iter().find(|d| d.id == e.info.id.0.as_ref()) else {
                return false;
            };
            e.spec == d.driver && (e.spec.is_none() || e.path.as_deref() == Some(d.path.as_slice()))
        });
        // 手动重扫是用户明确要求"再试一次"，把自愈计数清零。自愈次数用尽
        // 后 worker 已经被丢掉（tx/rx 为空），只清零不会恢复，必须真的
        // 重建线程；否则同 id/path 的设备会一直停在离线状态。
        for e in &mut self.entries {
            // 同一个 container 里身份可能已经变了（插线 ↔ 接收器），连接方式
            // 和型号都得跟着换，否则线拔了卡片还挂着"有线"。
            if let Some(d) = found.iter().find(|d| d.id == e.info.id.0.as_ref()) {
                refresh_info(e, d);
            }
            e.restarts = 0;
            e.last_restart = None;
            if e.tx.is_none() {
                start_worker(e, &self.ctx, &self.read_mode);
            }
        }
        for dev in found {
            if self.entries.iter().any(|e| e.info.id.0.as_ref() == dev.id) {
                continue;
            }
            let spec = dev.driver;
            let has_worker = spec.is_some();
            let path = dev.path.clone();
            let (tx, rx, join, busy) = match spec {
                Some(spec) => {
                    let (tx, rx, join, busy) =
                        spawn_worker(spec, path.clone(), self.read_mode.clone(), self.ctx.clone());
                    (Some(tx), Some(rx), Some(join), Some(busy))
                }
                None => (None, None, None, None),
            };
            self.entries.push(Entry {
                info: DeviceInfo {
                    id: DeviceId(Arc::from(dev.id)),
                    brand: dev.brand,
                    model: dev.model,
                    kind: dev.kind,
                    connection: dev.connection,
                    has_config: has_worker,
                    limits: dev.limits,
                    reason: dev.reason,
                },
                state: DeviceState {
                    link: if has_worker {
                        Link::Connecting
                    } else {
                        Link::Online
                    },
                    ..DeviceState::default()
                },
                path: has_worker.then_some(path),
                spec,
                sent: 0,
                restarts: 0,
                last_restart: None,
                busy,
                tx,
                rx,
                join,
            });
        }
    }

    /// Drain worker messages; returns true if any visible state changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = self.apply_scan_result();
        if self.hotplug_due() {
            self.start_scan();
            // 只有真的开始枚举才要求立即重绘；扫描进行中重复触发只会排队，
            // 真正的状态变化会随结果合并出现。
            changed |= self.scan_rx.is_some();
        }
        for entry in &mut self.entries {
            let mut died = false;
            if let Some(rx) = &entry.rx {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => {
                            changed = true;
                            if let Some(b) = msg.battery {
                                entry.state.battery = Some(b);
                            }
                            // Battery readings are independent of our edits, but a
                            // snapshot is not: one read before the newest command
                            // still holds the old value, and taking it would undo
                            // the edit on screen.
                            if let Some(s) = msg.snap {
                                if msg.consumed >= entry.sent {
                                    // Logitech receivers all enumerate as "USB
                                    // Receiver"; the mouse tells us its own name.
                                    // 名字里带形态时图标也一起纠正：配对的 G304
                                    // 不该继续挂着接收器的图标。
                                    if !s.model.is_empty() && s.model != entry.info.model {
                                        if let Some(kind) = DeviceType::from_name(&s.model) {
                                            entry.info.kind = kind;
                                        }
                                        entry.info.model = s.model.clone();
                                    }
                                    entry.state.editor = EditorState::from_snapshot(&s);
                                    entry.state.snap = Some(Arc::new(s));
                                    entry.state.synced = Some(Instant::now());
                                }
                            }
                            entry.state.link = msg.link;
                            entry.state.last_error = msg.error;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        // 采集线程只会在 panic 之后让消息通道断开（正常退出
                        // 只在条目被丢弃、发送端一起消失时发生）。
                        Err(mpsc::TryRecvError::Disconnected) => {
                            died = true;
                            break;
                        }
                    }
                }
            }
            if died {
                restart_worker(entry, &self.ctx, &self.read_mode);
                changed = true;
            }
        }
        changed
    }

    pub fn send(&mut self, id: &DeviceId, cmd: Cmd) {
        let Some(entry) = self.entries.iter_mut().find(|e| &e.info.id == id) else {
            return;
        };
        if let Some(s) = entry.state.snap.as_mut() {
            // 界面上可能还持有本帧开始时的 Arc（配置页会 clone 一份用来
            // 渲染），所以这里必须 make_mut：有别的读者时复制一份再改，
            // 不能把已经画出去的数据就地改掉。
            let s = Arc::make_mut(s);
            match cmd {
                Cmd::Polling(ms) => s.polling_ms = ms,
                Cmd::Motion(v) => s.motion = v,
                Cmd::Line(v) => s.line_corr = v,
                Cmd::Wave(v) => s.wave_corr = v,
                Cmd::Debounce(v, press) => {
                    if press {
                        s.debounce_press = v;
                    } else {
                        s.debounce_release = v;
                    }
                }
                Cmd::Sleep(m) => s.sleep_min = m,
                Cmd::Lod(v) => s.lod = v,
                Cmd::Activate(i) => s.stage_active = i.min(s.stage_count.saturating_sub(1)),
                Cmd::Dpi(stage, v) => {
                    if let Some(d) = s.dpi.get_mut(stage as usize) {
                        *d = v;
                    }
                }
                Cmd::Refresh => {}
            }
            entry.state.editor = EditorState::from_snapshot(s);
        }
        if let Some(tx) = &entry.tx {
            // Only a command the worker will actually see counts as pending;
            // otherwise a dead channel would block snapshots for good.
            if tx.send(cmd).is_ok() {
                entry.sent += 1;
            }
        }
    }

    pub fn entry(&self, id: &DeviceId) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.info.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holding "+" queues one command per repeat; only the last value has to
    /// reach the device.
    #[test]
    fn burst_keeps_last_value_per_field() {
        let mut cmds = vec![
            Cmd::Debounce(9, true),
            Cmd::Debounce(10, true),
            Cmd::Debounce(11, true),
        ];
        coalesce(&mut cmds);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Debounce(11, true)));
    }

    /// Press and release debounce are two different fields, even though they
    /// live in the same EEPROM block.
    #[test]
    fn press_and_release_stay_separate() {
        let mut cmds = vec![Cmd::Debounce(9, true), Cmd::Debounce(4, false)];
        coalesce(&mut cmds);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::Debounce(9, true)));
        assert!(matches!(cmds[1], Cmd::Debounce(4, false)));
    }

    /// Each DPI stage is its own field: editing stage 2 must not swallow a
    /// pending edit of stage 5.
    #[test]
    fn dpi_stages_stay_separate() {
        let mut cmds = vec![Cmd::Dpi(2, 800), Cmd::Dpi(5, 3200), Cmd::Dpi(2, 900)];
        coalesce(&mut cmds);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::Dpi(2, 900)));
        assert!(matches!(cmds[1], Cmd::Dpi(5, 3200)));
    }

    /// 合批会把同名命令的值挪到首次出现的位置，不同字段的顺序可能重排。
    /// 最终状态仍然正确：每条命令都是对独立字段的绝对写入（见 `coalesce`
    /// 的契约注释）。这个测试把该行为钉死，防止有人无意中改成别的语义。
    #[test]
    fn coalesce_keeps_last_value_at_the_first_slot() {
        let mut cmds = vec![Cmd::Activate(2), Cmd::Dpi(2, 1600), Cmd::Activate(3)];
        coalesce(&mut cmds);
        assert_eq!(cmds.len(), 2);
        // Activate(3) 占据了 Activate(2) 的位置：顺序重排是预期行为。
        assert!(matches!(cmds[0], Cmd::Activate(3)));
        assert!(matches!(cmds[1], Cmd::Dpi(2, 1600)));
    }

    #[test]
    fn toggles_of_the_same_switch_collapse_to_the_last_state() {
        let mut cmds = vec![Cmd::Motion(false), Cmd::Motion(true), Cmd::Line(false)];
        coalesce(&mut cmds);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::Motion(true)));
        assert!(matches!(cmds[1], Cmd::Line(false)));
    }

    /// 主页只做摘要，配置页才需要秒级刷新；浮窗不应周期读配置。
    #[test]
    fn read_mode_controls_full_read_cadence() {
        assert_eq!(ReadMode::Full.full_every(), Some(FULL_EVERY));
        assert_eq!(
            ReadMode::Summary.full_every(),
            Some(Duration::from_secs(20))
        );
        assert_eq!(ReadMode::BatteryOnly.full_every(), None);
        assert_eq!(ReadMode::BatteryOnly.wait_ms(), 900);
        assert_eq!(ReadMode::Full.wait_ms(), 200);
    }

    /// 鼠标休眠时电量轮询会一遍遍返回同一个错误；日志只在内容变化时写一条，
    /// 否则 1 MB 的滚动日志会被噪音冲掉。恢复正常后要能重新记录。
    #[test]
    fn repeated_background_errors_are_logged_once() {
        let mut last = None;
        assert!(is_new_error(&mut last, "设备无响应（超时）"));
        assert!(!is_new_error(&mut last, "设备无响应（超时）"));
        assert!(is_new_error(&mut last, "读取应答失败"));
        assert!(!is_new_error(&mut last, "读取应答失败"));
        // 成功一轮后清空，下一次失败仍然要留下痕迹。
        last = None;
        assert!(is_new_error(&mut last, "设备无响应（超时）"));
    }

    /// 驱动 panic 时也要复位：否则退出会白等满 6 秒的写等待。
    #[test]
    fn busy_guard_resets_the_flag_when_dropped() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = BusyGuard::new(&flag);
            assert!(flag.load(Ordering::Relaxed));
        }
        assert!(!flag.load(Ordering::Relaxed));
    }

    /// 退出时发送端先被丢弃；已经排队的命令仍要取出来执行，不能因为通道
    /// 断开就连同已取出的这一批一起丢掉。
    #[test]
    fn queued_commands_survive_a_closed_sender() {
        let (tx, rx) = mpsc::channel();
        tx.send(Cmd::Polling(2)).expect("queue polling");
        tx.send(Cmd::Activate(1)).expect("queue activate");
        let first = rx.recv().expect("first command");
        drop(tx);
        let cmds = drain_queue(&rx, Some(first));
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::Polling(2)));
        assert!(matches!(cmds[1], Cmd::Activate(1)));
    }

    /// 枚举结果走通道回到 UI 线程：结果没到之前保持"扫描中"，到了以后结束
    /// 扫描并报告列表变化。异步枚举的关键就是这个状态机。
    #[test]
    fn scan_results_are_applied_when_they_arrive() {
        let (tx, rx) = mpsc::channel();
        let mut manager = test_manager();
        manager.scan_rx = Some(rx);
        assert!(manager.is_scanning());
        assert!(!manager.poll(), "结果还没到，不该报告列表变化");
        tx.send(Vec::new()).expect("send scan result");
        assert!(manager.poll());
        assert!(!manager.is_scanning());
        assert!(manager.list().is_empty());
        assert!(
            manager.last_scan_completed().is_some(),
            "结果合并后才算扫描完成"
        );
        assert!(manager.last_scan_failed().is_none());
    }

    /// 枚举线程崩溃/断开不是扫描完成：不能刷新“已更新”，界面要能看到失败。
    #[test]
    fn failed_scan_is_not_marked_completed() {
        let (tx, rx) = mpsc::channel();
        let mut manager = test_manager();
        manager.scan_rx = Some(rx);
        drop(tx);
        assert!(manager.poll());
        assert!(!manager.is_scanning());
        assert!(manager.last_scan_failed().is_some());
        assert!(manager.last_scan_completed().is_none());
        assert!(!manager.auto_detect_enabled(), "测试管理器没有订阅");
    }

    /// 插拔事件等待合并窗口的 400ms 也是“检测中”，首页/浮窗要给出反馈。
    #[test]
    fn pending_hotplug_window_counts_as_scanning() {
        let mut manager = test_manager();
        manager.dirty.store(true, Ordering::Relaxed);
        assert!(!manager.poll());
        assert!(manager.dirty_since.is_some());
        assert!(manager.is_scanning());
        assert!(manager.pending_scan_in().is_some());
    }

    fn test_manager() -> DeviceManager {
        DeviceManager {
            entries: Vec::new(),
            demo: false,
            ctx: egui::Context::default(),
            read_mode: Arc::new(AtomicU8::new(ReadMode::Summary as u8)),
            scan_rx: None,
            rescan_queued: false,
            dirty: Arc::new(AtomicBool::new(false)),
            dirty_since: None,
            last_scan_completed: None,
            last_scan_failed: None,
            _watch: None,
        }
    }

    fn found_device(id: &str, model: &str, connection: ConnectionType) -> hid::HidDevice {
        hid::HidDevice {
            id: id.to_string(),
            path: vec![0x5c, 0, 0],
            vid: 0x258A,
            pid: 0x010C,
            brand: "BY Tech".to_string(),
            model: model.to_string(),
            kind: DeviceType::Keyboard,
            connection,
            // 只读设备：reconcile 不会给它起采集线程，测试里没有副作用。
            driver: None,
            limits: Limits::default(),
            reason: None,
        }
    }

    fn read_only_entry(id: &str, model: &str, connection: ConnectionType) -> Entry {
        Entry {
            info: DeviceInfo {
                id: DeviceId(Arc::from(id)),
                brand: "BY Tech".to_string(),
                model: model.to_string(),
                kind: DeviceType::Keyboard,
                connection,
                has_config: false,
                limits: Limits::default(),
                reason: None,
            },
            state: DeviceState::default(),
            sent: 0,
            path: None,
            spec: None,
            restarts: 0,
            last_restart: None,
            busy: None,
            tx: None,
            rx: None,
            join: None,
        }
    }

    /// 一台复合设备插上会连着报好几条接口通知；它们必须合成一轮扫描，而且
    /// 要等最后一波事件过去再扫（否则扫到的是半注册状态）。
    #[test]
    fn hotplug_events_are_coalesced_into_one_scan() {
        let mut manager = test_manager();
        for _ in 0..5 {
            manager.dirty.store(true, Ordering::Relaxed);
            assert!(!manager.hotplug_due(), "窗口没关就扫，等于每条通知扫一遍");
        }
        assert!(manager.pending_scan_in().is_some());
        // 窗口已过（不想在测试里真睡 400 ms，直接把起点往前挪）。
        manager.dirty_since = Some(ago(HOTPLUG_DEBOUNCE));
        assert!(manager.hotplug_due());
        assert!(!manager.hotplug_due(), "扫过一轮之后没有新事件，不该再扫");
        assert!(manager.pending_scan_in().is_none());
    }

    /// 没有通知时不该自己扫：`pending_scan_in` 是界面用的提前重绘量，一直
    /// 有值会让界面不停重绘。
    #[test]
    fn no_hotplug_event_means_no_scan() {
        let mut manager = test_manager();
        assert!(!manager.hotplug_due());
        assert!(manager.pending_scan_in().is_none());
    }

    /// 三模键盘从有线切到 2.4G：container id 不变，但连接方式必须跟着换，
    /// 不然线拔了首页还写着"有线"。
    #[test]
    fn reconcile_refreshes_the_identity_of_a_kept_entry() {
        let mut manager = test_manager();
        manager.entries.push(read_only_entry(
            "c:abc",
            "通用有线键盘",
            ConnectionType::Wired,
        ));
        manager.reconcile(vec![found_device(
            "c:abc",
            "F87",
            ConnectionType::Wireless24G,
        )]);
        assert_eq!(manager.entries.len(), 1);
        assert_eq!(
            manager.entries[0].info.connection,
            ConnectionType::Wireless24G
        );
        assert_eq!(manager.entries[0].info.model, "F87");
    }

    /// 拔掉的设备必须从列表里消失——这就是"键盘拔了卡片还在"的根因。
    #[test]
    fn reconcile_drops_devices_that_are_gone() {
        let mut manager = test_manager();
        manager.entries.push(read_only_entry(
            "c:abc",
            "通用有线键盘",
            ConnectionType::Wired,
        ));
        manager.reconcile(Vec::new());
        assert!(manager.entries.is_empty());
    }

    /// 已经有了快照（设备自报的真实型号），就别让型号表里的占位名把它盖
    /// 回去；连接方式仍然要刷新。
    #[test]
    fn reconcile_keeps_a_snapshot_derived_model() {
        let mut manager = test_manager();
        let mut entry = read_only_entry(
            "c:abc",
            "G304 Lightspeed Wireless Gaming Mouse",
            ConnectionType::Wired,
        );
        entry.state.snap = Some(Arc::new(Snapshot::default()));
        entry.info.kind = DeviceType::Mouse;
        manager.entries.push(entry);
        manager.reconcile(vec![found_device(
            "c:abc",
            "接收器",
            ConnectionType::Wireless24G,
        )]);
        assert_eq!(
            manager.entries[0].info.model,
            "G304 Lightspeed Wireless Gaming Mouse"
        );
        assert_eq!(manager.entries[0].info.kind, DeviceType::Mouse);
        assert_eq!(
            manager.entries[0].info.connection,
            ConnectionType::Wireless24G
        );
    }
}
