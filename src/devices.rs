use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::demo;
use crate::driver::{
    self, ConfirmResult, DeviceType, Driver, DriverSpec, Limits, WriteError, WriteOutcome,
};

pub use crate::driver::{Cmd, Snapshot};
use crate::hid::{self, ConnectionType};

pub const FULL_EVERY: Duration = Duration::from_secs(4);
pub const BATTERY_EVERY: Duration = Duration::from_secs(20);
const RETRY_EVERY: Duration = Duration::from_millis(1500);
/// 读取失败后的快速重试间隔；预算内最多两次额外只读重试。
const READ_RETRY: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];
/// 写后只读确认的重试间隔。
const CONFIRM_RETRY: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
];
/// 快速恢复预算：预算内按短间隔重试，耗尽后降低频率但保持在线。
const RECOVERY_BUDGET: Duration = Duration::from_secs(3);
/// 预算耗尽后的全量读间隔：配置页要跟手，主页/浮窗可以更慢。
const SLOW_FULL: Duration = Duration::from_secs(5);
const SLOW_SUMMARY: Duration = Duration::from_secs(20);
/// 排队写入的最长等待时间（实际取它与当前模式慢读间隔的较大者）。
///
/// 配置读不回来时写入会一直卡在队列里：因为基准不可信，不敢写。但界面不能
/// 因此永远显示“正在写入设备”，命令必须有一个明确的结论——超时就结算成
/// “未写入”，等设备恢复后用户重点一次即可。
const WRITE_STALL: Duration = Duration::from_secs(6);
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
/// 退出排空的总预算：通道关闭后把仍卡在通信保护期里的待写命令写完并确认。
/// 必须短于 `WRITE_SHUTDOWN_WAIT`，给最后的 flush 留出时间。
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(4);
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

/// 鼠标配置页的编辑态；键盘只读，不使用。
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

/// 后台同步状态，和写入错误分开保存。
///
/// 电量轮询成功不能清掉写入失败，后台读取失败也不该触发“操作失败”提示。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    /// 最近一次后台读取失败的原因；成功后清空。
    pub error: Option<String>,
    /// 快速恢复预算耗尽：当前快照可能过期，界面显示“同步异常”。
    pub stale: bool,
}

/// 一条命令的最终/阶段结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CmdOutcome {
    Confirmed,
    Unchanged,
    /// 被同一字段的后一条命令替代。
    Superseded,
    /// 已提交，等待设备确认。
    AwaitingConfirm,
    /// 写入结果未确认；不能据此宣称已持久化。
    Unconfirmed(String),
    Failed(String),
    /// 设备离线或基准配置尚未恢复。
    Unavailable(String),
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
    /// 用户操作失败（写入被拒绝、设备不可用等）。只有新操作开始或同一批
    /// 命令全部成功时才清除，和后台同步错误互不影响。
    pub last_error: Option<String>,
    /// 后台同步错误与快照是否过期。
    pub sync: SyncState,
    /// 有一条写命令已发出、采集线程还没回报处理结果。
    pub writing: bool,
    /// 写入已提交，正在等待设备确认。
    pub pending_confirm: bool,
    /// 提交结果未确认；设备实际配置会被重新读取，但不能宣称已持久化。
    pub write_unconfirmed: bool,
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
            sync: SyncState::default(),
            writing: false,
            pending_confirm: false,
            write_unconfirmed: false,
        }
    }
}

/// 采集线程收到的命令。序号由界面分配，结果回来时据此把“旧确认”和
/// “新编辑”区分开。
#[derive(Clone, Copy, Debug)]
struct QueuedCmd {
    seq: u64,
    cmd: Cmd,
}

/// Keep the last value per field, at the slot of its *first* occurrence.
///
/// 契约：同名命令只留最后一个值，但它落在**第一次出现**的位置上，所以不同
/// 字段之间的先后顺序可能因此重排（`Activate(2), Dpi(2,1600), Activate(3)`
/// 会变成 `Activate(3), Dpi(2,1600)`）。这是安全的，前提是每条命令都实现为
/// "对独立字段的绝对写入"——执行合批结果与逐条执行整个队列，最终状态一致。
/// 驱动若要引入有先后依赖的命令序列，必须先改这里（或改为不在队列里合并）。
///
/// 返回被替代命令的结果：它们也必须有一条明确反馈，界面不能一直等。
fn coalesce_tagged(cmds: &mut Vec<QueuedCmd>) -> Vec<(u64, CmdOutcome)> {
    let mut out: Vec<QueuedCmd> = Vec::with_capacity(cmds.len());
    let mut superseded = Vec::new();
    for cmd in cmds.drain(..) {
        match out
            .iter_mut()
            .find(|queued| queued.cmd.same_field(&cmd.cmd))
        {
            Some(slot) => {
                superseded.push((slot.seq, CmdOutcome::Superseded));
                *slot = cmd;
            }
            None => out.push(cmd),
        }
    }
    *cmds = out;
    superseded
}

enum DeviceMsg {
    /// 一次可信的全量读取。`settled` 是读取时已经结算到的命令序号：
    /// 小于界面最新命令的快照不能覆盖乐观编辑。
    Snapshot {
        snap: Snapshot,
        settled: u64,
    },
    Battery(u8),
    Link(Link),
    Sync(SyncState),
    Write {
        pending_confirm: bool,
        unconfirmed: bool,
    },
    Results(Vec<(u64, CmdOutcome)>),
}

pub struct Entry {
    pub info: DeviceInfo,
    pub state: DeviceState,
    /// 打开设备用的 HID path。重插后 container id 不变但 path 通常变化，
    /// 需要靠它发现旧 worker 已失效。
    path: Option<Vec<u16>>,
    /// 协议适配器规格。采集线程崩掉后要原样重建，所以和 path 一起存着。
    spec: Option<DriverSpec>,
    /// 已发送命令的最大序号。读取快照时用它判断快照是否早于用户的编辑。
    sent: u64,
    /// 已发出、还没拿到最终结果的命令。全部结算前界面保持“写入中”。
    unacked: BTreeSet<u64>,
    /// 自动重建次数与最后一次重建时间，见 `restart_worker`。
    restarts: u8,
    last_restart: Option<Instant>,
    /// 采集线程正在执行一次可能写 EEPROM 的调用；退出时据此决定多等多久。
    busy: Option<Arc<AtomicBool>>,
    /// 该设备自己的读取策略。配置页只看当前设备，不能让别的设备跟着
    /// 4 秒一次地唤醒、反复读 EEPROM。
    read_mode: Arc<AtomicU8>,
    tx: Option<Sender<QueuedCmd>>,
    rx: Option<Receiver<DeviceMsg>>,
    /// 采集线程句柄。退出时先关通道再 join，避免把正在写的 EEPROM 扇区
    /// 截断在 commit 之前。
    join: Option<std::thread::JoinHandle<()>>,
}

/// Take everything that is already queued, so a burst of edits is applied as
/// one batch instead of being interleaved with read-backs of older state.
///
/// `Disconnected` 只表示队列已取空且发送端不在了；已经取出的命令仍要执行
/// 完再退出，否则"调完参数立刻退出"会丢掉最后一批修改。退出在 `Disconnected`
/// 之后的排空阶段完成（见 `run_worker` 的 `shutting_down` 分支）。
fn drain_queue(rx: &Receiver<QueuedCmd>, first: Option<QueuedCmd>) -> Vec<QueuedCmd> {
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
    Sender<QueuedCmd>,
    Receiver<DeviceMsg>,
    std::thread::JoinHandle<()>,
    Arc<AtomicBool>,
) {
    let (tx, rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let busy = Arc::new(AtomicBool::new(false));
    let worker_busy = busy.clone();
    let handle = std::thread::Builder::new()
        .name(format!("{:?}-worker", spec.protocol.kind()))
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
    entry.unacked.clear();
    entry.state.snap = None;
    entry.state.editor = EditorState::default();
    entry.state.link = Link::Connecting;
    entry.state.writing = false;
    entry.state.pending_confirm = false;
    entry.state.write_unconfirmed = false;
    entry.state.sync = SyncState::default();
    // 旧连接的失败提示不属于新连接：留着会让用户以为刚重连就写失败了。
    entry.state.last_error = None;
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
    // 崩溃时还挂着的命令永远不会再有结果了。新连接不能继承旧的失败提示，
    // 但也不能假装它们成功了：结算数量要如实告诉用户，尤其是写入途中崩掉
    // 的那批——设备有没有落盘是未知的。
    let dropped = entry.unacked.len();
    let dropped_note = |reconnected: bool| {
        (dropped > 0).then(|| {
            let tail = if reconnected {
                "；已重新连接并重读配置"
            } else {
                ""
            };
            format!("采集线程异常退出，{dropped} 条命令结果未知{tail}")
        })
    };
    if entry.restarts > MAX_RESTARTS {
        entry.state.link =
            Link::Offline("设备读取线程反复异常退出，请重新扫描或重插设备".to_string());
        entry.state.snap = None;
        entry.state.writing = false;
        entry.state.pending_confirm = false;
        entry.state.write_unconfirmed = false;
        entry.state.sync = SyncState::default();
        entry.state.last_error = dropped_note(false);
        entry.unacked.clear();
        return;
    }
    start_worker(entry, ctx, read_mode);
    // `start_worker` 会清掉 `last_error`（旧连接的错误不该压着新连接），
    // 所以这条提示必须放在它之后。
    entry.state.last_error = dropped_note(true);
    ctx.request_repaint();
}

fn real_worker(
    path: Vec<u16>,
    spec: DriverSpec,
    rx: Receiver<QueuedCmd>,
    tx: Sender<DeviceMsg>,
    ctx: egui::Context,
    read_mode: Arc<AtomicU8>,
    busy: Arc<AtomicBool>,
) {
    run_worker(
        move || driver::open(spec, &path),
        rx,
        tx,
        ctx,
        read_mode,
        busy,
    );
}

/// 周期读取、失败退避与写后确认的期限。
///
/// 所有时间都从“上一次请求结束”起算。快速恢复预算用完后不再密集重试，
/// 但仍保持在线：只有句柄失效或扫描确认设备移除才进入离线。
#[derive(Clone, Copy, Debug)]
struct SyncSchedule {
    last_ok: Option<Instant>,
    retries: u8,
    budget_until: Option<Instant>,
    slow: bool,
    not_before: Instant,
}

impl SyncSchedule {
    fn new(now: Instant) -> Self {
        Self {
            last_ok: None,
            retries: 0,
            budget_until: None,
            slow: false,
            not_before: now,
        }
    }

    /// 是否该发起一次全量读。刷新和“还没有快照”都不能绕过退避期限。
    fn due(&self, now: Instant, mode: ReadMode, force: bool, no_snap: bool) -> bool {
        if now < self.not_before {
            return false;
        }
        // 失败后的重试不依赖页面的周期策略：浮窗模式没有周期全量读，
        // 也要把尚未恢复的配置读回来。
        if self.retries > 0 || force || no_snap {
            return true;
        }
        match self.last_ok {
            Some(at) => mode
                .full_every()
                .is_some_and(|every| now.saturating_duration_since(at) >= every),
            None => true,
        }
    }

    fn ok(&mut self, now: Instant) {
        self.last_ok = Some(now);
        self.retries = 0;
        self.budget_until = None;
        self.slow = false;
        self.not_before = now;
    }

    /// 记录一次读取失败。返回 true 表示快速恢复预算已耗尽，配置应标记过期。
    fn failed(&mut self, now: Instant, mode: ReadMode) -> bool {
        self.retries = self.retries.saturating_add(1);
        let budget = *self.budget_until.get_or_insert(now + RECOVERY_BUDGET);
        let backoff = READ_RETRY
            .get(self.retries.saturating_sub(1) as usize)
            .copied();
        match backoff {
            Some(delay) if now + delay <= budget => self.not_before = now + delay,
            _ => {
                self.slow = true;
                self.not_before = now + slow_every(mode);
            }
        }
        self.slow
    }

    /// 写后确认成功：周期读取从此刻重新起算。
    fn defer(&mut self, now: Instant) {
        self.ok(now);
    }

    /// 故障纠正：允许尽快重读，但仍要等驱动的通信保护间隔。
    fn expedite(&mut self, now: Instant) {
        self.not_before = now;
    }

    /// 下一次读取任务的期限；`None` 表示当前模式没有周期全量读。
    ///
    /// 不能用 `not_before` 代替：成功读取后它等于“现在”，把它当唤醒期限
    /// 会让采集线程在下一个周期前空转。
    fn next_due(&self, mode: ReadMode) -> Option<Instant> {
        if self.retries > 0 || self.last_ok.is_none() {
            return Some(self.not_before);
        }
        match (self.last_ok, mode.full_every()) {
            (Some(last), Some(every)) => Some((last + every).max(self.not_before)),
            _ => None,
        }
    }
}

fn slow_every(mode: ReadMode) -> Duration {
    if mode == ReadMode::Full {
        SLOW_FULL
    } else {
        SLOW_SUMMARY
    }
}

/// 读取快照时已经结算到的命令序号。
///
/// 只要还有一条命令没结算，快照就停在它之前，避免旧值覆盖用户的乐观编辑。
fn settled_through(unresolved: &BTreeSet<u64>, max_seen: u64) -> u64 {
    unresolved
        .iter()
        .next()
        .map_or(max_seen, |seq| seq.saturating_sub(1))
}

fn emit(tx: &Sender<DeviceMsg>, ctx: &egui::Context, msg: DeviceMsg) {
    let _ = tx.send(msg);
    ctx.request_repaint();
}

/// 发送命令结果。`AwaitingConfirm` 只是阶段反馈，不算结算。
fn resolve(
    tx: &Sender<DeviceMsg>,
    ctx: &egui::Context,
    unresolved: &mut BTreeSet<u64>,
    results: Vec<(u64, CmdOutcome)>,
) {
    if results.is_empty() {
        return;
    }
    for (seq, outcome) in &results {
        if !matches!(outcome, CmdOutcome::AwaitingConfirm) {
            unresolved.remove(seq);
        }
    }
    emit(tx, ctx, DeviceMsg::Results(results));
}

/// 采集线程主体。驱动由 `open` 提供，测试可以注入假实现验证超时、写入失败
/// 和退出落盘，不必接真机。
fn run_worker<F>(
    mut open: F,
    rx: Receiver<QueuedCmd>,
    tx: Sender<DeviceMsg>,
    ctx: egui::Context,
    read_mode: Arc<AtomicU8>,
    busy: Arc<AtomicBool>,
) where
    F: FnMut() -> Result<Box<dyn Driver>, String>,
{
    let mut dev: Option<Box<dyn Driver>> = None;
    let mut snap: Option<Snapshot> = None;
    let mut link = Link::Connecting;
    let mut last_open = ago(RETRY_EVERY);
    let mut last_open_error: Option<String> = None;
    let mut last_batt_error: Option<String> = None;
    let mut sync_error: Option<String> = None;
    let mut stale = false;
    let mut force_read = false;
    let mut manual_refresh = false;

    let mut schedule = SyncSchedule::new(Instant::now());
    let mut next_batt = Instant::now();
    let mut guard = Instant::now();

    let mut pending: Vec<QueuedCmd> = Vec::new();
    // 队列里最早那条命令进队的时刻。写入被挡住（配置过期/基准不可信/还没
    // 快照）时用它判断该不该放弃等待，见 `WRITE_STALL`。
    let mut pending_since: Option<Instant> = None;
    let mut awaiting: Vec<QueuedCmd> = Vec::new();
    let mut confirm_at: Option<Instant> = None;
    let mut confirm_started = Instant::now();
    let mut confirm_retries: u8 = 0;
    let mut correction = false;
    // 读改写基准是否可信。写后未确认、确认读取又失败时必须先重新读回
    // 实际配置，之后才允许执行后续写入。
    let mut baseline_dirty = false;
    let mut write_unconfirmed = false;

    let mut unresolved: BTreeSet<u64> = BTreeSet::new();
    let mut max_seen: u64 = 0;

    let mut shutting_down = false;
    let mut shutdown_deadline = Instant::now();
    // 排空期间一直举着 busy，shutdown 才会按“正在写”给足宽限期。
    let mut _exit_busy: Option<BusyGuard> = None;

    loop {
        let mode = ReadMode::from_u8(read_mode.load(Ordering::Relaxed));
        let now = Instant::now();

        let first = if shutting_down {
            // 通道已关，不会有新命令：pending 里可能还有被通信保护期挡住的
            // 修改，直接退出会静默丢掉。这里继续遵守保护期写完并确认，再
            // flush；预算耗尽或基准不可信时结算成“未写入”，不无限拖住退出。
            let blocked = snap.is_none() || stale || baseline_dirty;
            let done = pending.is_empty() && confirm_at.is_none();
            if dev.is_none()
                || done
                || now >= shutdown_deadline
                || (blocked && confirm_at.is_none())
            {
                let reason = if stale {
                    "设备配置同步异常，命令未写入"
                } else if snap.is_none() {
                    "还没读到设备配置，命令未写入"
                } else {
                    "上一批写入尚未结算，命令未写入"
                };
                let batch = std::mem::take(&mut pending);
                if !batch.is_empty() {
                    crate::log::line(
                        "WARN",
                        &format!("退出前放弃写入：{reason}（{} 条命令）", batch.len()),
                    );
                }
                let results = batch
                    .iter()
                    .map(|q| (q.seq, CmdOutcome::Unavailable(reason.to_string())))
                    .collect();
                resolve(&tx, &ctx, &mut unresolved, results);
                let batch = std::mem::take(&mut awaiting);
                let results = batch
                    .iter()
                    .map(|q| {
                        (
                            q.seq,
                            CmdOutcome::Unconfirmed("退出前未能确认写入结果".into()),
                        )
                    })
                    .collect();
                resolve(&tx, &ctx, &mut unresolved, results);
                if let Some(d) = dev.as_mut() {
                    if let Err(e) = d.flush() {
                        crate::log::line("WARN", &format!("退出前保存设备配置失败：{e}"));
                    }
                }
                break;
            }
            let mut wait = shutdown_deadline - now;
            if let Some(at) = confirm_at {
                wait = wait.min(at.saturating_duration_since(now));
            }
            wait = wait.min(guard.saturating_duration_since(now));
            std::thread::sleep(wait.max(Duration::from_millis(1)));
            None
        } else {
            // recv_timeout 跟着最近的任务期限缩短；浮窗的 900ms 空闲等待不能
            // 拖慢写后确认。
            let mut wait = Duration::from_millis(mode.wait_ms());
            let mut ready = false;
            if dev.is_none() {
                let at = last_open + RETRY_EVERY;
                if now >= at {
                    ready = true;
                } else {
                    wait = wait.min(at - now);
                }
            } else {
                let guard_ok = now >= guard;
                if let Some(at) = confirm_at {
                    if at <= now && guard_ok {
                        ready = true;
                    } else if at > now {
                        wait = wait.min(at - now);
                    } else {
                        wait = wait.min(guard - now);
                    }
                } else {
                    if !pending.is_empty()
                        && snap.is_some()
                        && !stale
                        && !baseline_dirty
                        && guard_ok
                    {
                        ready = true;
                    }
                    // 配置过期后仍要继续尝试恢复，哪怕浮窗模式没有周期全量读。
                    if schedule.due(now, mode, force_read || stale, snap.is_none()) {
                        if guard_ok {
                            ready = true;
                        } else {
                            wait = wait.min(guard - now);
                        }
                    } else if let Some(at) = schedule.next_due(mode) {
                        wait = wait.min(at.saturating_duration_since(now));
                    }
                }
                // 确认/写入期间不再插入电量查询，否则会变成 1ms 空转。
                if confirm_at.is_none() {
                    if now >= next_batt {
                        if guard_ok {
                            ready = true;
                        } else {
                            wait = wait.min(guard - now);
                        }
                    } else {
                        wait = wait.min(next_batt - now);
                    }
                }
            }
            if ready {
                wait = Duration::from_millis(1);
            }

            match rx.recv_timeout(wait) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // 队列已取空且界面不会再发命令：进入退出排空，把保护期里
                    // 的修改写完，再提交延迟落盘的档位。
                    shutting_down = true;
                    shutdown_deadline = Instant::now() + SHUTDOWN_DRAIN;
                    _exit_busy = Some(BusyGuard::new(&busy));
                    None
                }
            }
        };

        let incoming = drain_queue(&rx, first);
        for q in &incoming {
            max_seen = max_seen.max(q.seq);
            unresolved.insert(q.seq);
        }
        let mut writes = Vec::with_capacity(incoming.len());
        for q in incoming {
            if matches!(q.cmd, Cmd::Refresh) {
                manual_refresh = true;
                force_read = true;
                resolve(
                    &tx,
                    &ctx,
                    &mut unresolved,
                    vec![(q.seq, CmdOutcome::Confirmed)],
                );
            } else {
                writes.push(q);
            }
        }
        if !writes.is_empty() {
            pending.extend(writes);
            if pending_since.is_none() {
                pending_since = Some(Instant::now());
            }
            let superseded = coalesce_tagged(&mut pending);
            resolve(&tx, &ctx, &mut unresolved, superseded);
        }

        if !shutting_down && dev.is_none() && last_open.elapsed() >= RETRY_EVERY {
            last_open = Instant::now();
            match open() {
                Ok(d) => {
                    dev = Some(d);
                    snap = None;
                    link = Link::Connecting;
                    schedule = SyncSchedule::new(Instant::now());
                    next_batt = Instant::now();
                    guard = Instant::now();
                    last_open_error = None;
                    last_batt_error = None;
                    sync_error = None;
                    stale = false;
                    force_read = false;
                    manual_refresh = false;
                    baseline_dirty = false;
                    correction = false;
                    // 断线时特意保留的“写入未确认”只属于上一条链路：新链路
                    // 会重新读实际配置，旧警告再挂着就是误报。这里和
                    // `start_worker` 保持一致（它也是重连即清零），否则休眠
                    // 唤醒后这条标志会一直显示到下一次成功写入。
                    if write_unconfirmed {
                        write_unconfirmed = false;
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Write {
                                pending_confirm: false,
                                unconfirmed: false,
                            },
                        );
                    }
                    emit(&tx, &ctx, DeviceMsg::Link(link.clone()));
                    emit(&tx, &ctx, DeviceMsg::Sync(SyncState::default()));
                }
                Err(e) => {
                    if last_open_error.as_deref() != Some(e.as_str()) {
                        last_open_error = Some(e.clone());
                        crate::log::line("WARN", &format!("打开设备失败：{e}"));
                        link = Link::Offline(e);
                        emit(&tx, &ctx, DeviceMsg::Link(link.clone()));
                    }
                }
            }
        }

        let Some(d) = dev.as_mut() else {
            // 离线时命令不能无限挂着：每条都要有明确结果。
            let reason = match &link {
                Link::Offline(e) => e.clone(),
                _ => "设备尚未连接".to_string(),
            };
            pending.clear();
            pending_since = None;
            awaiting.clear();
            confirm_at = None;
            if !unresolved.is_empty() {
                let items = unresolved
                    .iter()
                    .map(|seq| (*seq, CmdOutcome::Unavailable(reason.clone())))
                    .collect();
                unresolved.clear();
                resolve(&tx, &ctx, &mut unresolved, items);
            }
            continue;
        };

        let mut did_io = false;
        let mut drop_reason: Option<String> = None;
        let now = Instant::now();

        // 1) 写后确认优先：它决定后续写入能不能开始。
        if let Some(at) = confirm_at {
            if now >= at && now >= guard {
                confirm_at = None;
                did_io = true;
                // 正常路径下 `dev = None` 会同时清掉 `confirm_at` 和 `snap`，
                // 所以这里拿不到基准说明状态机被破坏了。也不能 panic：那会把
                // 整条采集线程连同队列里还没执行的命令一起丢掉。
                let Some(target) = snap.as_mut() else {
                    let batch = std::mem::take(&mut awaiting);
                    let results = batch
                        .iter()
                        .map(|q| {
                            (
                                q.seq,
                                CmdOutcome::Unconfirmed("确认时已没有配置基准".into()),
                            )
                        })
                        .collect();
                    resolve(&tx, &ctx, &mut unresolved, results);
                    write_unconfirmed = true;
                    baseline_dirty = false;
                    correction = false;
                    confirm_retries = 0;
                    force_read = true;
                    emit(
                        &tx,
                        &ctx,
                        DeviceMsg::Write {
                            pending_confirm: false,
                            unconfirmed: true,
                        },
                    );
                    continue;
                };
                let _busy = BusyGuard::new(&busy);
                let result = d.confirm(target);
                drop(_busy);
                if let Some(t) = d.next_io_at() {
                    guard = guard.max(t);
                }
                // 提交结果本来就未确认时，即使回读到了目标值，也只能说明
                // 当前配置是什么，不能据此宣称已经持久化。
                let result = match result {
                    Ok(ConfirmResult::Verified) if correction => Ok(ConfirmResult::ReadOnly),
                    other => other,
                };
                match result {
                    Ok(ConfirmResult::Verified) => {
                        let batch = std::mem::take(&mut awaiting);
                        let recovered = write_unconfirmed;
                        write_unconfirmed = false;
                        baseline_dirty = false;
                        correction = false;
                        // 这次读取已经刷新了 `snap`，第 3 步不必再读一遍。
                        force_read = false;
                        schedule.defer(Instant::now());
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Confirmed))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Write {
                                pending_confirm: false,
                                unconfirmed: false,
                            },
                        );
                        let snapshot = target.clone();
                        let settled = settled_through(&unresolved, max_seen);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Snapshot {
                                snap: snapshot,
                                settled,
                            },
                        );
                        if recovered {
                            crate::log::line(
                                "INFO",
                                &format!("设备写入确认恢复（{} 条命令）", batch.len()),
                            );
                        }
                    }
                    Ok(ConfirmResult::ReadOnly) => {
                        // 驱动没有待确认的提交目标：这次只刷新了设备实际值，
                        // 不能宣称持久化。若确实还有一批命令在等确认，必须
                        // 给它们一个明确结果，否则这些 seq 会永久留在
                        // `unresolved` 里，`settled_through` 从此停在它们之前，
                        // 之后所有快照都会被判成过期、配置页冻结在乐观值。
                        let batch = std::mem::take(&mut awaiting);
                        let reason = "设备未返回本次写入的确认目标";
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Unconfirmed(reason.into())))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        if !batch.is_empty() {
                            write_unconfirmed = true;
                        }
                        // 没有待确认目标时保持原值：correction 路径本来就为
                        // true，而一次普通只读刷新不该凭空标成「写入未确认」。
                        baseline_dirty = false;
                        correction = false;
                        force_read = false;
                        schedule.defer(Instant::now());
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Write {
                                pending_confirm: false,
                                unconfirmed: write_unconfirmed,
                            },
                        );
                        let snapshot = target.clone();
                        let settled = settled_through(&unresolved, max_seen);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Snapshot {
                                snap: snapshot,
                                settled,
                            },
                        );
                    }
                    Ok(ConfirmResult::Mismatch(m)) => {
                        let batch = std::mem::take(&mut awaiting);
                        write_unconfirmed = false;
                        baseline_dirty = false;
                        correction = false;
                        force_read = false;
                        schedule.defer(Instant::now());
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Failed(m.clone())))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Write {
                                pending_confirm: false,
                                unconfirmed: false,
                            },
                        );
                        let snapshot = target.clone();
                        let settled = settled_through(&unresolved, max_seen);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Snapshot {
                                snap: snapshot,
                                settled,
                            },
                        );
                    }
                    Err(e) => {
                        if !d.link_alive() {
                            drop_reason = Some(e);
                        } else if now.saturating_duration_since(confirm_started) >= RECOVERY_BUDGET
                            || confirm_retries as usize >= CONFIRM_RETRY.len()
                        {
                            // 只能继续读，不能重放写入。
                            if !correction {
                                let batch = std::mem::take(&mut awaiting);
                                let results = batch
                                    .iter()
                                    .map(|q| (q.seq, CmdOutcome::Unconfirmed(e.clone())))
                                    .collect();
                                resolve(&tx, &ctx, &mut unresolved, results);
                            }
                            write_unconfirmed = true;
                            // 只能先读回实际配置，之后才允许新的读改写。
                            baseline_dirty = true;
                            correction = false;
                            force_read = true;
                            schedule.expedite(Instant::now());
                            emit(
                                &tx,
                                &ctx,
                                DeviceMsg::Write {
                                    pending_confirm: false,
                                    unconfirmed: true,
                                },
                            );
                            crate::log::line("WARN", &format!("写入已提交但确认读取失败：{e}"));
                        } else {
                            confirm_at =
                                Some(Instant::now() + CONFIRM_RETRY[confirm_retries as usize]);
                            confirm_retries = confirm_retries.saturating_add(1);
                        }
                    }
                }
            }
        }

        // 2) 写入：基准配置没有恢复时暂停，命令继续留在队列里合并。
        if !did_io && confirm_at.is_none() && !baseline_dirty && now >= guard && !stale {
            if let Some(target) = snap.as_mut().filter(|_| !pending.is_empty()) {
                did_io = true;
                let batch = std::mem::take(&mut pending);
                pending_since = None;
                let cmds: Vec<Cmd> = batch.iter().map(|q| q.cmd).collect();
                let _busy = BusyGuard::new(&busy);
                let outcome = d.apply_batch(&cmds, target);
                drop(_busy);
                if let Some(t) = d.next_io_at() {
                    guard = guard.max(t);
                }
                match outcome {
                    Ok(WriteOutcome::Unchanged) => {
                        baseline_dirty = false;
                        // 设备本来就是这个值：写入目标已经成立，之前那条
                        // “未确认”可以收了（和 Confirmed 同一处理）。
                        if write_unconfirmed {
                            write_unconfirmed = false;
                            emit(
                                &tx,
                                &ctx,
                                DeviceMsg::Write {
                                    pending_confirm: false,
                                    unconfirmed: false,
                                },
                            );
                        }
                        schedule.defer(Instant::now());
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Unchanged))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                    }
                    Ok(WriteOutcome::Confirmed) => {
                        baseline_dirty = false;
                        // 驱动已经回读确认，之前“未确认”的提示可以清了。
                        if write_unconfirmed {
                            write_unconfirmed = false;
                            emit(
                                &tx,
                                &ctx,
                                DeviceMsg::Write {
                                    pending_confirm: false,
                                    unconfirmed: false,
                                },
                            );
                        }
                        schedule.defer(Instant::now());
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Confirmed))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        let snapshot = target.clone();
                        let settled = settled_through(&unresolved, max_seen);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Snapshot {
                                snap: snapshot,
                                settled,
                            },
                        );
                    }
                    Ok(WriteOutcome::Committed) => {
                        // 提交之后、确认之前，基准值不能再拿来做下一次读改写。
                        baseline_dirty = true;
                        let at = d.next_io_at().unwrap_or_else(Instant::now);
                        confirm_at = Some(at);
                        confirm_started = Instant::now();
                        confirm_retries = 0;
                        correction = false;
                        awaiting = batch;
                        let results = awaiting
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::AwaitingConfirm))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Write {
                                pending_confirm: true,
                                unconfirmed: write_unconfirmed,
                            },
                        );
                    }
                    Err(WriteError::Rejected(m)) => {
                        // 参数被拒绝、尚未写入：失败是明确的，立刻安排一次
                        // 只读把界面上的乐观值纠正回来。
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Failed(m.clone())))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        baseline_dirty = false;
                        force_read = true;
                        schedule.expedite(Instant::now());
                        crate::log::line("WARN", &format!("设备拒绝写入：{m}"));
                    }
                    Err(WriteError::Unconfirmed(m)) => {
                        // 是否落盘未知：不重放，只读实际配置。命令结果明确
                        // 标记为“未确认”，不能宣称持久化成功。
                        let results = batch
                            .iter()
                            .map(|q| (q.seq, CmdOutcome::Unconfirmed(m.clone())))
                            .collect();
                        resolve(&tx, &ctx, &mut unresolved, results);
                        write_unconfirmed = true;
                        baseline_dirty = true;
                        correction = true;
                        confirm_at = Some(Instant::now() + CONFIRM_RETRY[0]);
                        confirm_started = Instant::now();
                        confirm_retries = 1;
                        force_read = true;
                        emit(
                            &tx,
                            &ctx,
                            DeviceMsg::Write {
                                pending_confirm: false,
                                unconfirmed: true,
                            },
                        );
                        crate::log::line("WARN", &format!("写入结果未确认：{m}"));
                    }
                }
            }
        }

        // 2b) 被挡住的写入不能无限期挂着。
        //
        // 配置过期、基准不可信或还没读到快照时，第 2 步会一直跳过写入，命令
        // 留在队列里等恢复。设备彻底不响应时这个“等待”没有尽头：界面会显示
        // “正在写入设备”而实际上一个字节都没写。给队列一个期限，超时就结算
        // 成“未写入”，让用户知道要等设备恢复后重来。
        if !pending.is_empty() {
            let limit = WRITE_STALL.max(slow_every(mode));
            if pending_since.is_some_and(|at| now.saturating_duration_since(at) >= limit) {
                let reason = if stale {
                    "设备配置同步异常，命令未写入"
                } else if snap.is_none() {
                    "还没读到设备配置，命令未写入"
                } else {
                    "上一批写入尚未结算，命令未写入"
                };
                let batch = std::mem::take(&mut pending);
                pending_since = None;
                let results = batch
                    .iter()
                    .map(|q| (q.seq, CmdOutcome::Unavailable(reason.to_string())))
                    .collect();
                resolve(&tx, &ctx, &mut unresolved, results);
                crate::log::line(
                    "WARN",
                    &format!("{reason}（{} 条命令，已放弃等待）", batch.len()),
                );
            }
        }

        // 3) 全量读：退避与通信保护间隔都不能绕过。退出排空只写不读。
        if !did_io && confirm_at.is_none() && now >= guard && !shutting_down {
            // snap 为空、手动刷新和故障恢复都不能绕过退避期限。
            let due = schedule.due(now, mode, force_read || stale, snap.is_none());
            if due {
                did_io = true;
                let _busy = BusyGuard::new(&busy);
                let result = d.snapshot();
                drop(_busy);
                if let Some(t) = d.next_io_at() {
                    guard = guard.max(t);
                }
                match result {
                    Ok(s) => {
                        snap = Some(s.clone());
                        schedule.ok(Instant::now());
                        force_read = false;
                        baseline_dirty = false;
                        correction = false;
                        // 手动刷新是显式操作：成功读到实际值后清掉旧的
                        // “写入结果未确认”，但不会把它伪装成写入成功。
                        if manual_refresh && write_unconfirmed {
                            write_unconfirmed = false;
                            emit(
                                &tx,
                                &ctx,
                                DeviceMsg::Write {
                                    pending_confirm: false,
                                    unconfirmed: false,
                                },
                            );
                        }
                        manual_refresh = false;
                        let was_stale = stale;
                        stale = false;
                        let had_error = sync_error.take().is_some();
                        if had_error || was_stale {
                            crate::log::line("INFO", "设备配置同步已恢复");
                            emit(&tx, &ctx, DeviceMsg::Sync(SyncState::default()));
                        }
                        if link != Link::Online {
                            link = Link::Online;
                            emit(&tx, &ctx, DeviceMsg::Link(link.clone()));
                        }
                        let settled = settled_through(&unresolved, max_seen);
                        emit(&tx, &ctx, DeviceMsg::Snapshot { snap: s, settled });
                    }
                    Err(e) => {
                        if !d.link_alive() {
                            drop_reason = Some(e);
                        } else {
                            let was_stale = stale;
                            stale = schedule.failed(Instant::now(), mode);
                            let new_error = is_new_error(&mut sync_error, &e);
                            if new_error {
                                crate::log::line(
                                    "WARN",
                                    &format!("读取设备配置失败（保持在线并重试）：{e}"),
                                );
                            }
                            if new_error || was_stale != stale {
                                emit(
                                    &tx,
                                    &ctx,
                                    DeviceMsg::Sync(SyncState {
                                        error: sync_error.clone(),
                                        stale,
                                    }),
                                );
                            }
                            let next_link = if snap.is_some() {
                                Link::Online
                            } else {
                                Link::Connecting
                            };
                            if link != next_link {
                                link = next_link;
                                emit(&tx, &ctx, DeviceMsg::Link(link.clone()));
                            }
                        }
                    }
                }
            }
        }

        // 4) 电量心跳。失败不影响连接状态，除非句柄失效。
        if !did_io
            && confirm_at.is_none()
            && now >= guard
            && !shutting_down
            && Instant::now() >= next_batt
        {
            let _busy = BusyGuard::new(&busy);
            let result = d.battery();
            drop(_busy);
            if let Some(t) = d.next_io_at() {
                guard = guard.max(t);
            }
            next_batt = Instant::now() + BATTERY_EVERY;
            match result {
                Ok(Some(b)) => {
                    last_batt_error = None;
                    emit(&tx, &ctx, DeviceMsg::Battery(b));
                }
                Ok(None) => last_batt_error = None,
                Err(e) => {
                    if !d.link_alive() {
                        drop_reason = Some(e);
                    } else if is_new_error(&mut last_batt_error, &e) {
                        crate::log::line("WARN", &format!("读取电量失败：{e}"));
                    }
                }
            }
        }

        if let Some(reason) = drop_reason {
            dev = None;
            snap = None;
            link = Link::Offline(reason.clone());
            schedule = SyncSchedule::new(Instant::now());
            next_batt = Instant::now();
            force_read = false;
            manual_refresh = false;
            stale = false;
            sync_error = None;
            baseline_dirty = false;
            correction = false;
            pending.clear();
            pending_since = None;
            awaiting.clear();
            confirm_at = None;
            confirm_retries = 0;
            last_open = Instant::now();
            crate::log::line("WARN", &format!("设备已断开：{reason}"));
            emit(&tx, &ctx, DeviceMsg::Link(link.clone()));
            emit(&tx, &ctx, DeviceMsg::Sync(SyncState::default()));
            emit(
                &tx,
                &ctx,
                DeviceMsg::Write {
                    pending_confirm: false,
                    unconfirmed: write_unconfirmed,
                },
            );
            if !unresolved.is_empty() {
                let items = unresolved
                    .iter()
                    .map(|seq| (*seq, CmdOutcome::Unavailable(reason.clone())))
                    .collect();
                unresolved.clear();
                resolve(&tx, &ctx, &mut unresolved, items);
            }
        }
    }
}

fn demo_worker(
    mut snap: Snapshot,
    battery: Option<u8>,
    rx: Receiver<QueuedCmd>,
    tx: Sender<DeviceMsg>,
    ctx: egui::Context,
) {
    emit(
        &tx,
        &ctx,
        DeviceMsg::Snapshot {
            snap: snap.clone(),
            settled: 0,
        },
    );
    emit(&tx, &ctx, DeviceMsg::Link(Link::Online));
    if let Some(b) = battery {
        emit(&tx, &ctx, DeviceMsg::Battery(b));
    }

    let mut unresolved: BTreeSet<u64> = BTreeSet::new();
    let mut max_seen = 0u64;
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut cmds = drain_queue(&rx, first);
        for q in &cmds {
            max_seen = max_seen.max(q.seq);
            unresolved.insert(q.seq);
        }
        // Same batching rule as the real worker: one write per field per burst,
        // so the demo shell shows the same behaviour the device does.
        let mut results = coalesce_tagged(&mut cmds);
        std::thread::sleep(Duration::from_millis(40));
        for q in cmds {
            if !matches!(q.cmd, Cmd::Refresh) {
                demo::apply(&mut snap, q.cmd);
            }
            results.push((q.seq, CmdOutcome::Confirmed));
        }
        resolve(&tx, &ctx, &mut unresolved, results);
        let settled = settled_through(&unresolved, max_seen);
        emit(
            &tx,
            &ctx,
            DeviceMsg::Snapshot {
                snap: snap.clone(),
                settled,
            },
        );
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
    /// 当前页面的基础读取策略；配置页会把它提升为 `Full`，但只对
    /// `focused` 指向的设备生效，其余设备保持摘要频率。
    mode: ReadMode,
    /// 当前正在查看配置的设备。`None` 表示没有设备需要高频刷新。
    focused: Option<DeviceId>,
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
            mode: ReadMode::Full,
            focused: None,
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

    /// 页面切换时更新读取策略。配置页只让当前设备高频刷新，其余设备回落到
    /// 摘要频率：一台设备的配置页不该把所有设备的 EEPROM 都叫醒。
    pub fn set_read_mode(&mut self, mode: ReadMode, focused: Option<&DeviceId>) {
        // UI 每帧都会调用：只有策略真的变了才广播，别又变成「每帧扫一遍
        // 设备列表」。新插入的设备在 scan() 里按 self.mode_for() 初始化。
        let same_focus = match (focused, self.focused.as_ref()) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        };
        if mode == self.mode && same_focus {
            return;
        }
        self.mode = mode;
        self.focused = focused.cloned();
        for entry in &self.entries {
            entry
                .read_mode
                .store(self.mode_for(&entry.info.id) as u8, Ordering::Relaxed);
        }
    }

    fn mode_for(&self, id: &DeviceId) -> ReadMode {
        match self.mode {
            ReadMode::Full if self.focused.as_ref() != Some(id) => ReadMode::Summary,
            mode => mode,
        }
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
                unacked: BTreeSet::new(),
                path: None,
                spec: None,
                restarts: 0,
                last_restart: None,
                busy: None,
                read_mode: Arc::new(AtomicU8::new(ReadMode::BatteryOnly as u8)),
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
                let read_mode = e.read_mode.clone();
                start_worker(e, &self.ctx, &read_mode);
            }
        }
        for dev in found {
            if self.entries.iter().any(|e| e.info.id.0.as_ref() == dev.id) {
                continue;
            }
            let spec = dev.driver;
            let has_worker = spec.is_some();
            let path = dev.path.clone();
            let id = DeviceId(Arc::from(dev.id));
            let read_mode = Arc::new(AtomicU8::new(self.mode_for(&id) as u8));
            let (tx, rx, join, busy) = match spec {
                Some(spec) => {
                    let (tx, rx, join, busy) =
                        spawn_worker(spec, path.clone(), read_mode.clone(), self.ctx.clone());
                    (Some(tx), Some(rx), Some(join), Some(busy))
                }
                None => (None, None, None, None),
            };
            self.entries.push(Entry {
                info: DeviceInfo {
                    id,
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
                unacked: BTreeSet::new(),
                restarts: 0,
                last_restart: None,
                busy,
                read_mode,
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
                            match msg {
                                DeviceMsg::Snapshot { snap, settled } => {
                                    // 读取发生在最新编辑之前时，这份快照描述的是
                                    // 用户已经越过的状态，应用它会把编辑回滚。
                                    if settled >= entry.sent {
                                        // Logitech receivers all enumerate as "USB
                                        // Receiver"; the mouse tells us its own
                                        // name. 名字里带形态时图标也一起纠正。
                                        if !snap.model.is_empty() && snap.model != entry.info.model
                                        {
                                            if let Some(kind) = DeviceType::from_name(&snap.model) {
                                                entry.info.kind = kind;
                                            }
                                            entry.info.model = snap.model.clone();
                                        }
                                        entry.state.editor = EditorState::from_snapshot(&snap);
                                        entry.state.snap = Some(Arc::new(snap));
                                        entry.state.synced = Some(Instant::now());
                                    }
                                }
                                DeviceMsg::Battery(b) => entry.state.battery = Some(b),
                                DeviceMsg::Link(link) => {
                                    if matches!(link, Link::Offline(_)) {
                                        entry.state.writing = false;
                                        entry.state.pending_confirm = false;
                                        entry.unacked.clear();
                                    }
                                    entry.state.link = link;
                                }
                                DeviceMsg::Sync(sync) => entry.state.sync = sync,
                                DeviceMsg::Write {
                                    pending_confirm,
                                    unconfirmed,
                                } => {
                                    entry.state.pending_confirm = pending_confirm;
                                    entry.state.write_unconfirmed = unconfirmed;
                                }
                                DeviceMsg::Results(results) => {
                                    let mut failed = false;
                                    for (seq, outcome) in results {
                                        // `AwaitingConfirm` 只是阶段反馈：命令
                                        // 还没结算，留在 `unacked` 里，“写入中”
                                        // 才不会被提前收掉。
                                        if !matches!(outcome, CmdOutcome::AwaitingConfirm) {
                                            entry.unacked.remove(&seq);
                                        }
                                        match outcome {
                                            CmdOutcome::Confirmed
                                            | CmdOutcome::Unchanged
                                            | CmdOutcome::Superseded => {}
                                            CmdOutcome::AwaitingConfirm => {
                                                entry.state.pending_confirm = true;
                                            }
                                            CmdOutcome::Unconfirmed(_) => {
                                                entry.state.write_unconfirmed = true;
                                            }
                                            CmdOutcome::Failed(m) | CmdOutcome::Unavailable(m) => {
                                                entry.state.last_error = Some(m);
                                                failed = true;
                                            }
                                        }
                                    }
                                    if entry.unacked.is_empty() {
                                        entry.state.writing = false;
                                        if !failed && !entry.state.pending_confirm {
                                            entry.state.last_error = None;
                                        }
                                    }
                                }
                            }
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
                let read_mode = entry.read_mode.clone();
                restart_worker(entry, &self.ctx, &read_mode);
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
            let seq = entry.sent + 1;
            if tx.send(QueuedCmd { seq, cmd }).is_ok() {
                entry.sent = seq;
                entry.unacked.insert(seq);
                // 刷新只是重新读配置，不冒充“写入中”。
                if !matches!(cmd, Cmd::Refresh) {
                    entry.state.writing = true;
                    // 新操作开始：上一次的失败提示不该继续压着这次结果。
                    entry.state.last_error = None;
                }
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

    /// 合批测试只关心字段合并，序号用出现顺序补齐。
    fn coalesce_cmds(cmds: &mut Vec<Cmd>) -> Vec<(u64, CmdOutcome)> {
        let mut tagged: Vec<QueuedCmd> = cmds
            .drain(..)
            .enumerate()
            .map(|(i, cmd)| QueuedCmd {
                seq: i as u64 + 1,
                cmd,
            })
            .collect();
        let superseded = coalesce_tagged(&mut tagged);
        *cmds = tagged.into_iter().map(|q| q.cmd).collect();
        superseded
    }

    /// Holding "+" queues one command per repeat; only the last value has to
    /// reach the device.
    #[test]
    fn burst_keeps_last_value_per_field() {
        let mut cmds = vec![
            Cmd::Debounce(9, true),
            Cmd::Debounce(10, true),
            Cmd::Debounce(11, true),
        ];
        coalesce_cmds(&mut cmds);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Debounce(11, true)));
    }

    /// Press and release debounce are two different fields, even though they
    /// live in the same EEPROM block.
    #[test]
    fn press_and_release_stay_separate() {
        let mut cmds = vec![Cmd::Debounce(9, true), Cmd::Debounce(4, false)];
        coalesce_cmds(&mut cmds);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::Debounce(9, true)));
        assert!(matches!(cmds[1], Cmd::Debounce(4, false)));
    }

    /// Each DPI stage is its own field: editing stage 2 must not swallow a
    /// pending edit of stage 5.
    #[test]
    fn dpi_stages_stay_separate() {
        let mut cmds = vec![Cmd::Dpi(2, 800), Cmd::Dpi(5, 3200), Cmd::Dpi(2, 900)];
        coalesce_cmds(&mut cmds);
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
        coalesce_cmds(&mut cmds);
        assert_eq!(cmds.len(), 2);
        // Activate(3) 占据了 Activate(2) 的位置：顺序重排是预期行为。
        assert!(matches!(cmds[0], Cmd::Activate(3)));
        assert!(matches!(cmds[1], Cmd::Dpi(2, 1600)));
    }

    #[test]
    fn toggles_of_the_same_switch_collapse_to_the_last_state() {
        let mut cmds = vec![Cmd::Motion(false), Cmd::Motion(true), Cmd::Line(false)];
        coalesce_cmds(&mut cmds);
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

    /// 配置页只该让当前设备高频刷新；其他设备继续摘要频率，避免打开一台
    /// 设备的配置页就把所有设备的 EEPROM 都反复叫醒。
    #[test]
    fn full_read_mode_is_scoped_to_the_open_device() {
        let mut manager = test_manager();
        manager
            .entries
            .push(read_only_entry("c:a", "键盘 A", ConnectionType::Wired));
        manager
            .entries
            .push(read_only_entry("c:b", "键盘 B", ConnectionType::Wired));
        let a = manager.entries[0].info.id.clone();

        manager.set_read_mode(ReadMode::Full, Some(&a));
        assert_eq!(manager.entries[0].read_mode.load(Ordering::Relaxed), 2);
        assert_eq!(manager.entries[1].read_mode.load(Ordering::Relaxed), 1);

        manager.set_read_mode(ReadMode::BatteryOnly, None);
        assert_eq!(manager.entries[0].read_mode.load(Ordering::Relaxed), 0);
        assert_eq!(manager.entries[1].read_mode.load(Ordering::Relaxed), 0);
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

    /// 一次失败后按 250/500ms 快速重试；预算耗尽后转为低频，但调度仍然
    /// 存在（离线只由句柄失效决定，不由读取失败决定）。
    #[test]
    fn read_failures_back_off_then_slow_down() {
        let t0 = Instant::now();
        let mut schedule = SyncSchedule::new(t0);
        assert!(schedule.due(t0, ReadMode::Full, false, false));
        assert!(!schedule.failed(t0, ReadMode::Full));
        assert_eq!(schedule.next_due(ReadMode::Full), Some(t0 + READ_RETRY[0]));

        let t1 = t0 + READ_RETRY[0];
        assert!(schedule.due(t1, ReadMode::Full, false, false));
        assert!(!schedule.failed(t1, ReadMode::Full));
        assert_eq!(schedule.next_due(ReadMode::Full), Some(t1 + READ_RETRY[1]));

        let t2 = t1 + READ_RETRY[1];
        assert!(schedule.failed(t2, ReadMode::Full));
        assert!(schedule.slow);
        assert_eq!(schedule.next_due(ReadMode::Full), Some(t2 + SLOW_FULL));
        assert!(schedule.due(t2 + SLOW_FULL, ReadMode::Full, false, false));
    }

    /// 成功读取后唤醒期限必须是“下一次周期”，不能是“现在”，否则采集
    /// 线程会在整个周期内空转。
    #[test]
    fn successful_read_schedules_the_next_period_not_now() {
        let t0 = Instant::now();
        let mut schedule = SyncSchedule::new(t0);
        schedule.ok(t0);
        assert_eq!(schedule.next_due(ReadMode::Full), Some(t0 + FULL_EVERY));
        assert_eq!(schedule.next_due(ReadMode::BatteryOnly), None);
    }

    /// 浮窗模式没有周期全量读，但失败后的恢复重试仍要发生。
    #[test]
    fn retries_still_happen_in_battery_only_mode() {
        let t0 = Instant::now();
        let mut schedule = SyncSchedule::new(t0);
        schedule.ok(t0);
        assert!(!schedule.due(t0, ReadMode::BatteryOnly, false, false));
        schedule.failed(t0, ReadMode::BatteryOnly);
        assert!(schedule.due(t0 + READ_RETRY[0], ReadMode::BatteryOnly, false, false));
    }

    /// 手动刷新可以提前发起，但不能绕过失败退避。
    #[test]
    fn refresh_cannot_bypass_the_read_backoff() {
        let t0 = Instant::now();
        let mut schedule = SyncSchedule::new(t0);
        schedule.failed(t0, ReadMode::Full);
        assert!(!schedule.due(t0 + READ_RETRY[0] / 2, ReadMode::Full, true, false));
        assert!(schedule.due(t0 + READ_RETRY[0], ReadMode::Full, true, false));
    }

    /// 被合并替代的命令也必须拿到结果，界面不能一直等。
    #[test]
    fn coalescing_reports_superseded_commands() {
        let mut cmds = vec![
            QueuedCmd {
                seq: 1,
                cmd: Cmd::Polling(4),
            },
            QueuedCmd {
                seq: 2,
                cmd: Cmd::Polling(2),
            },
            QueuedCmd {
                seq: 3,
                cmd: Cmd::Motion(true),
            },
        ];
        let superseded = coalesce_tagged(&mut cmds);
        assert_eq!(cmds.len(), 2);
        assert_eq!(superseded, vec![(1, CmdOutcome::Superseded)]);
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
        tx.send(QueuedCmd {
            seq: 1,
            cmd: Cmd::Polling(2),
        })
        .expect("queue polling");
        tx.send(QueuedCmd {
            seq: 2,
            cmd: Cmd::Activate(1),
        })
        .expect("queue activate");
        let first = rx.recv().expect("first command");
        drop(tx);
        let cmds = drain_queue(&rx, Some(first));
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0].cmd, Cmd::Polling(2)));
        assert!(matches!(cmds[1].cmd, Cmd::Activate(1)));
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
            mode: ReadMode::Summary,
            focused: None,
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
            physical: id.to_string(),
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
            unacked: BTreeSet::new(),
            path: None,
            spec: None,
            restarts: 0,
            last_restart: None,
            busy: None,
            read_mode: Arc::new(AtomicU8::new(ReadMode::Summary as u8)),
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

    /// 写命令发出到采集线程回报之间算“写入中”；结果回来就结束，失败由
    /// `last_error` 表达，不继续挂着写入状态。
    #[test]
    fn writing_flag_tracks_the_worker_result() {
        let mut manager = test_manager();
        let mut entry = read_only_entry("c:abc", "G304", ConnectionType::Wireless24G);
        entry.info.has_config = true;
        entry.state.snap = Some(Arc::new(Snapshot::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        entry.tx = Some(cmd_tx);
        let (msg_tx, msg_rx) = mpsc::channel();
        entry.rx = Some(msg_rx);
        manager.entries.push(entry);

        let id = DeviceId(Arc::from("c:abc"));
        manager.send(&id, Cmd::Polling(2));
        assert!(manager.list()[0].state.writing, "写命令发出后应为写入中");
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(QueuedCmd {
                seq: 1,
                cmd: Cmd::Polling(2)
            })
        ));

        msg_tx
            .send(DeviceMsg::Results(vec![(1, CmdOutcome::Confirmed)]))
            .unwrap();
        manager.poll();
        assert!(!manager.list()[0].state.writing, "结果回来应结束写入中");
    }

    /// 提交只是阶段反馈：确认回来之前命令还没结算，界面不能提前结束
    /// “写入中”，否则一次确认失败会悄悄跳过“还在写”的展示。
    #[test]
    fn awaiting_confirm_keeps_the_command_in_flight() {
        let mut manager = test_manager();
        let mut entry = read_only_entry("c:abc", "G304", ConnectionType::Wireless24G);
        entry.info.has_config = true;
        entry.state.snap = Some(Arc::new(Snapshot::default()));
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        entry.tx = Some(cmd_tx);
        let (msg_tx, msg_rx) = mpsc::channel();
        entry.rx = Some(msg_rx);
        manager.entries.push(entry);

        let id = DeviceId(Arc::from("c:abc"));
        manager.send(&id, Cmd::Polling(2));

        msg_tx
            .send(DeviceMsg::Write {
                pending_confirm: true,
                unconfirmed: false,
            })
            .unwrap();
        msg_tx
            .send(DeviceMsg::Results(vec![(1, CmdOutcome::AwaitingConfirm)]))
            .unwrap();
        manager.poll();
        assert!(manager.list()[0].state.writing, "确认前仍应算写入中");
        assert!(manager.list()[0].state.pending_confirm);
        assert_eq!(
            manager.list()[0].unacked.len(),
            1,
            "阶段反馈不能把命令从在途集合里摘掉"
        );

        msg_tx
            .send(DeviceMsg::Results(vec![(1, CmdOutcome::Confirmed)]))
            .unwrap();
        manager.poll();
        assert!(!manager.list()[0].state.writing);
        assert!(manager.list()[0].unacked.is_empty());
    }

    /// 电量是独立心跳：它不能清掉用户操作产生的写入失败。
    #[test]
    fn battery_updates_keep_write_errors() {
        let mut manager = test_manager();
        let mut entry = read_only_entry("c:abc", "G304", ConnectionType::Wireless24G);
        entry.state.last_error = Some("写入失败".into());
        let (msg_tx, msg_rx) = mpsc::channel();
        entry.rx = Some(msg_rx);
        manager.entries.push(entry);

        msg_tx.send(DeviceMsg::Battery(71)).unwrap();
        manager.poll();
        let state = &manager.list()[0].state;
        assert_eq!(state.battery, Some(71));
        assert_eq!(
            state.last_error.as_deref(),
            Some("写入失败"),
            "电量更新不能清掉写入失败"
        );
    }

    /// 旧快照不能覆盖更新的用户编辑；已结算的快照才可以应用。
    #[test]
    fn stale_snapshot_does_not_undo_a_newer_edit() {
        let mut manager = test_manager();
        let mut entry = read_only_entry("c:abc", "G304", ConnectionType::Wireless24G);
        entry.info.has_config = true;
        entry.state.snap = Some(Arc::new(Snapshot::default()));
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        entry.tx = Some(cmd_tx);
        let (msg_tx, msg_rx) = mpsc::channel();
        entry.rx = Some(msg_rx);
        manager.entries.push(entry);

        let id = DeviceId(Arc::from("c:abc"));
        manager.send(&id, Cmd::Polling(2));

        let old = Snapshot {
            polling_ms: 8,
            ..Snapshot::default()
        };
        msg_tx
            .send(DeviceMsg::Snapshot {
                snap: old,
                settled: 0,
            })
            .unwrap();
        manager.poll();
        assert_eq!(
            manager.list()[0].state.snap.as_ref().map(|s| s.polling_ms),
            Some(2),
            "读取发生在编辑之前的快照必须丢弃，保留乐观值"
        );

        let fresh = Snapshot {
            polling_ms: 4,
            ..Snapshot::default()
        };
        msg_tx
            .send(DeviceMsg::Snapshot {
                snap: fresh,
                settled: 1,
            })
            .unwrap();
        manager.poll();
        assert_eq!(
            manager.list()[0].state.snap.as_ref().map(|s| s.polling_ms),
            Some(4)
        );
    }

    /// 首帧读取失败一次、随后恢复的假驱动：验证超时不会把设备切成离线，
    /// 恢复后命令仍能正常结算。
    struct FlakyProbe {
        snapshots: u8,
        applied: Arc<AtomicU8>,
    }

    impl Driver for FlakyProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            self.snapshots += 1;
            if self.snapshots == 1 {
                Err("设备无响应（超时）".into())
            } else {
                Ok(Snapshot::default())
            }
        }

        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }

        fn apply(&mut self, _cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            self.applied.fetch_add(1, Ordering::SeqCst);
            Ok(WriteOutcome::Confirmed)
        }

        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            Ok(ConfirmResult::Verified)
        }

        fn link_alive(&self) -> bool {
            true
        }
    }

    /// 一次超时只进入重试状态：不离线、不重新打开驱动；恢复后命令照常结算。
    #[test]
    fn a_single_timeout_keeps_the_device_online_and_recovers() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let applied = Arc::new(AtomicU8::new(0));
        let count = applied.clone();
        let handle = std::thread::spawn(move || {
            run_worker(
                move || {
                    Ok(Box::new(FlakyProbe {
                        snapshots: 0,
                        applied: count.clone(),
                    }) as Box<dyn Driver>)
                },
                cmd_rx,
                msg_tx,
                egui::Context::default(),
                Arc::new(AtomicU8::new(ReadMode::Full as u8)),
                Arc::new(AtomicBool::new(false)),
            );
        });

        let mut online = false;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline && !online {
            match msg_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(DeviceMsg::Link(Link::Offline(e))) => {
                    panic!("一次读取超时不能进入离线：{e}");
                }
                Ok(DeviceMsg::Link(Link::Online)) => online = true,
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert!(online, "重试成功后应恢复在线");

        cmd_tx
            .send(QueuedCmd {
                seq: 1,
                cmd: Cmd::Polling(2),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut confirmed = false;
        while Instant::now() < deadline && !confirmed {
            if let Ok(DeviceMsg::Results(results)) = msg_rx.recv_timeout(Duration::from_millis(500))
            {
                confirmed = results
                    .iter()
                    .any(|(_, r)| matches!(r, CmdOutcome::Confirmed));
            }
        }
        assert!(confirmed, "恢复后命令应得到明确结果");
        assert_eq!(applied.load(Ordering::SeqCst), 1);

        drop(cmd_tx);
        handle.join().expect("采集线程应正常退出");
    }

    /// 只实现 `apply` 的驱动：默认合批必须保留 Committed，否则采集线程
    /// 会以为不需要再调 confirm。
    struct CommitProbe {
        calls: Arc<AtomicU8>,
    }

    impl Driver for CommitProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            Ok(Snapshot::default())
        }
        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }
        fn apply(&mut self, _cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(WriteOutcome::Committed)
        }
        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            Ok(ConfirmResult::Verified)
        }
        fn link_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn default_batch_preserves_committed() {
        let calls = Arc::new(AtomicU8::new(0));
        let mut dev = CommitProbe {
            calls: calls.clone(),
        };
        let mut snap = Snapshot::default();
        assert_eq!(
            dev.apply_batch(&[Cmd::Polling(2), Cmd::Polling(4)], &mut snap)
                .unwrap(),
            WriteOutcome::Committed
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            dev.apply_batch(&[], &mut snap).unwrap(),
            WriteOutcome::Unchanged
        );
    }

    struct StopOnErrorProbe {
        calls: Arc<AtomicU8>,
    }

    impl Driver for StopOnErrorProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            Ok(Snapshot::default())
        }
        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }
        fn apply(&mut self, cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(cmd, Cmd::Polling(_)) {
                Err(WriteError::Unconfirmed("提交应答超时".into()))
            } else {
                Ok(WriteOutcome::Confirmed)
            }
        }
        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            Ok(ConfirmResult::Verified)
        }
        fn link_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn default_batch_stops_at_the_first_error() {
        let calls = Arc::new(AtomicU8::new(0));
        let mut dev = StopOnErrorProbe {
            calls: calls.clone(),
        };
        let mut snap = Snapshot::default();
        let err = dev
            .apply_batch(&[Cmd::Lod(3), Cmd::Polling(2), Cmd::Line(true)], &mut snap)
            .unwrap_err();
        assert!(matches!(err, WriteError::Unconfirmed(_)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "可能已落盘的失败之后不能再继续写"
        );
    }

    /// 提交成功后驱动却说「没有待确认目标」：契约允许这个结果，采集线程
    /// 必须把在等的命令结算掉，否则 unresolved 永久悬空、之后所有快照都
    /// 会被 UI 判成过期，配置页冻结在乐观值。
    struct ReadOnlyProbe;

    impl Driver for ReadOnlyProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            Ok(Snapshot::default())
        }
        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }
        fn apply(&mut self, _cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            Ok(WriteOutcome::Committed)
        }
        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            Ok(ConfirmResult::ReadOnly)
        }
        fn link_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn readonly_confirmation_still_settles_the_queue() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            run_worker(
                || Ok(Box::new(ReadOnlyProbe) as Box<dyn Driver>),
                cmd_rx,
                msg_tx,
                egui::Context::default(),
                Arc::new(AtomicU8::new(ReadMode::Full as u8)),
                Arc::new(AtomicBool::new(false)),
            );
        });
        cmd_tx
            .send(QueuedCmd {
                seq: 7,
                cmd: Cmd::Polling(2),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut settled = false;
        let mut snapshot_settled = None;
        while Instant::now() < deadline {
            match msg_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(DeviceMsg::Results(results)) => {
                    settled |= results.iter().any(|(seq, outcome)| {
                        *seq == 7 && matches!(outcome, CmdOutcome::Unconfirmed(_))
                    });
                }
                Ok(DeviceMsg::Snapshot { settled: s, .. }) if settled => {
                    snapshot_settled = Some(s);
                    break;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert!(settled, "ReadOnly 确认后命令必须有明确结果");
        assert_eq!(
            snapshot_settled,
            Some(7),
            "结算后 settled 必须追上 sent，否则后续快照会被丢弃"
        );
        drop(cmd_tx);
        handle.join().expect("采集线程应正常退出");
    }

    /// 读配置永远超时、但句柄还活着的设备：配置读不回来时写入会被一直
    /// 挡在队列里，这正是“正在写入设备”可能永远不结束的场景。
    struct DeafProbe;

    impl Driver for DeafProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            Err("设备无响应（超时）".into())
        }
        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }
        fn apply(&mut self, _cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            Ok(WriteOutcome::Confirmed)
        }
        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            Ok(ConfirmResult::Verified)
        }
        fn link_alive(&self) -> bool {
            true
        }
    }

    /// 排队写入要有期限：设备一直读不回配置时，命令必须结算成“未写入”，
    /// 而不是永远挂在队列里让界面停在进行中。
    #[test]
    fn stalled_writes_are_settled_instead_of_hanging() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            run_worker(
                || Ok(Box::new(DeafProbe) as Box<dyn Driver>),
                cmd_rx,
                msg_tx,
                egui::Context::default(),
                Arc::new(AtomicU8::new(ReadMode::Full as u8)),
                Arc::new(AtomicBool::new(false)),
            );
        });
        std::thread::sleep(Duration::from_millis(50));
        cmd_tx
            .send(QueuedCmd {
                seq: 1,
                cmd: Cmd::Polling(2),
            })
            .unwrap();

        let deadline = Instant::now() + WRITE_STALL * 3;
        let mut settled = false;
        while Instant::now() < deadline && !settled {
            if let Ok(DeviceMsg::Results(results)) = msg_rx.recv_timeout(Duration::from_millis(500))
            {
                settled = results
                    .iter()
                    .any(|(seq, r)| *seq == 1 && matches!(r, CmdOutcome::Unavailable(_)));
            }
        }
        assert!(settled, "读不回配置时写入必须超时结算，不能一直挂着");
        drop(cmd_tx);
        handle.join().expect("采集线程应正常退出");
    }

    struct FlushProbe {
        flushed: Arc<AtomicBool>,
        error: Option<&'static str>,
    }
    impl Driver for FlushProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            Ok(Snapshot::default())
        }

        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }

        fn apply(&mut self, _cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            Ok(WriteOutcome::Confirmed)
        }

        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            Ok(ConfirmResult::Verified)
        }

        fn flush(&mut self) -> Result<(), String> {
            self.flushed.store(true, Ordering::SeqCst);
            match self.error {
                Some(e) => Err(e.to_string()),
                None => Ok(()),
            }
        }

        fn link_alive(&self) -> bool {
            true
        }
    }

    /// 起一个只跑一轮的采集线程：等它真正打开驱动并上报首帧快照，再关命令
    /// 通道，让它走到“退出前 flush”这一支。
    fn run_flush_probe(error: Option<&'static str>) -> Arc<AtomicBool> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let flushed = Arc::new(AtomicBool::new(false));
        let flag = flushed.clone();
        let handle = std::thread::spawn(move || {
            run_worker(
                move || {
                    Ok(Box::new(FlushProbe {
                        flushed: flag.clone(),
                        error,
                    }) as Box<dyn Driver>)
                },
                cmd_rx,
                msg_tx,
                egui::Context::default(),
                Arc::new(AtomicU8::new(ReadMode::Full as u8)),
                Arc::new(AtomicBool::new(false)),
            );
        });
        msg_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("采集线程应上报首帧快照");
        drop(cmd_tx);
        handle.join().expect("采集线程应正常退出");
        flushed
    }

    /// 退出时采集线程必须先 flush 再结束：切档后立即退出，重新上电仍应是
    /// 用户最后选的那一档。
    #[test]
    fn worker_flushes_deferred_writes_before_exit() {
        let flushed = run_flush_probe(None);
        assert!(flushed.load(Ordering::SeqCst), "退出前必须调用 flush");
    }

    /// flush 失败不能让退出流程 panic，也不能阻止线程结束。
    #[test]
    fn worker_exits_even_when_flush_fails() {
        let flushed = run_flush_probe(Some("保存失败"));
        assert!(flushed.load(Ordering::SeqCst));
    }

    /// 提交与确认都会推进通信保护期：第一批写入后关通道，第二批修改还卡
    /// 在保护期里，退出前必须把它写完，而不是只 flush 就丢掉。
    struct GuardedProbe {
        applied: Arc<std::sync::Mutex<Vec<Cmd>>>,
        flushed: Arc<AtomicBool>,
        guard_until: Instant,
    }
    impl Driver for GuardedProbe {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            Ok(Snapshot::default())
        }

        fn battery(&mut self) -> Result<Option<u8>, String> {
            Ok(None)
        }

        fn apply(&mut self, cmd: Cmd, _snap: &mut Snapshot) -> Result<WriteOutcome, WriteError> {
            let mut applied = self.applied.lock().unwrap();
            let first = applied.is_empty();
            applied.push(cmd);
            self.guard_until = Instant::now() + Duration::from_millis(150);
            Ok(if first {
                WriteOutcome::Committed
            } else {
                WriteOutcome::Confirmed
            })
        }

        fn confirm(&mut self, _snap: &mut Snapshot) -> Result<ConfirmResult, String> {
            self.guard_until = Instant::now() + Duration::from_millis(150);
            Ok(ConfirmResult::Verified)
        }

        fn next_io_at(&self) -> Option<Instant> {
            Some(self.guard_until)
        }

        fn flush(&mut self) -> Result<(), String> {
            self.flushed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn link_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn worker_drains_guarded_pending_writes_before_exit() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let applied = Arc::new(std::sync::Mutex::new(Vec::new()));
        let flushed = Arc::new(AtomicBool::new(false));
        let probe_applied = applied.clone();
        let probe_flushed = flushed.clone();
        let handle = std::thread::spawn(move || {
            run_worker(
                move || {
                    Ok(Box::new(GuardedProbe {
                        applied: probe_applied.clone(),
                        flushed: probe_flushed.clone(),
                        guard_until: Instant::now(),
                    }) as Box<dyn Driver>)
                },
                cmd_rx,
                msg_tx,
                egui::Context::default(),
                Arc::new(AtomicU8::new(ReadMode::Full as u8)),
                Arc::new(AtomicBool::new(false)),
            );
        });
        cmd_tx
            .send(QueuedCmd {
                seq: 1,
                cmd: Cmd::Polling(2),
            })
            .unwrap();
        // 等第一批真正写入后再发第二批并关通道：此刻 worker 在保护期里，
        // 第二批只能留在 pending，正好复现“退出丢队列”的现场。
        let deadline = Instant::now() + Duration::from_secs(5);
        while applied.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "没等到第一批写入");
            std::thread::sleep(Duration::from_millis(5));
        }
        cmd_tx
            .send(QueuedCmd {
                seq: 2,
                cmd: Cmd::Polling(4),
            })
            .unwrap();
        drop(cmd_tx);

        let mut second = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && second.is_none() {
            match msg_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(DeviceMsg::Results(results)) => {
                    for (seq, r) in results {
                        if seq == 2 {
                            second = Some(r);
                        }
                    }
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.join().expect("采集线程应正常退出");
        assert!(
            matches!(second, Some(CmdOutcome::Confirmed)),
            "退出前排队修改必须写入并确认，实际 {second:?}"
        );
        let applied = applied.lock().unwrap();
        assert_eq!(applied.len(), 2, "两批修改都要写到设备");
        assert!(matches!(applied[0], Cmd::Polling(2)));
        assert!(matches!(applied[1], Cmd::Polling(4)));
        assert!(flushed.load(Ordering::SeqCst), "排空后仍要 flush");
    }
}
