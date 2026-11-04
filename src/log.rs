//! 极简文件日志：只做一件 release 版本必须做的事——让失败留下痕迹。
//!
//! release 带 `windows_subsystem = "windows"`，没有控制台，`eprintln!` 和
//! 线程 panic 默认全都看不见。设备采集线程一旦崩掉，用户端只剩"界面卡在
//! 连接中"，没有任何可诊断的信息。这里把 panic 和关键错误写进
//! `%LOCALAPPDATA%\PeriphHub\log\periphhub.log`，超过 1 MB 就滚动成 `.1`。
//!
//! 刻意不引第三方日志库：整个程序只有几百字节的日志量，加依赖不划算。

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

/// 单个日志文件的上限，超过就滚到 `.1`（只保留一代，够定位一次现场）。
const MAX_BYTES: u64 = 1 << 20;

/// 打开着的日志文件与已写字节数。字节数记在这里，是因为滚动判断必须放在
/// 每次写之前——只在进程内第一次写时判断的话，长驻进程会一路写下去，
/// 再也不滚第二次。
struct LogFile {
    file: std::fs::File,
    written: u64,
}

static FILE: Mutex<Option<LogFile>> = Mutex::new(None);

/// 安装 panic hook，并写一行启动记录（同时起到"创建目录、验证可写"的作用；
/// 目录不可写时静默降级为不记录）。
pub fn init() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".into());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "非字符串 panic".into());
        line("PANIC", &format!("thread={name} at {at}: {payload}"));
        prev(info);
    }));
    line(
        "INFO",
        &format!(
            "启动 PeriphHub {}（{}）",
            env!("CARGO_PKG_VERSION"),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        ),
    );
}

/// 追加一行日志。写失败就放弃（日志不能反过来影响主流程）。
pub fn line(level: &str, msg: &str) {
    let stamp = timestamp();
    with_file(|f| {
        let _ = writeln!(f, "[{stamp}] {level:<5} {msg}");
    });
}

fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| {
        PathBuf::from(base)
            .join("PeriphHub")
            .join("log")
            .join("periphhub.log")
    })
}

fn with_file(f: impl FnOnce(&mut std::fs::File)) {
    let mut guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
    // 超限就先关句柄再滚动：Windows 上打开着的文件改不了名。
    if guard.as_ref().is_some_and(|l| l.written > MAX_BYTES) {
        *guard = None;
        if let Some(p) = path() {
            let _ = std::fs::rename(&p, p.with_extension("log.1"));
        }
    }
    if guard.is_none() {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        *guard = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .ok()
            .map(|file| LogFile {
                // 追加模式打开的是已有文件（上一代日志），从它的真实长度
                // 接着算，否则刚启动就会误判成"还没写满"。
                written: file.metadata().map(|m| m.len()).unwrap_or(0),
                file,
            });
    }
    if let Some(log) = guard.as_mut() {
        f(&mut log.file);
        if let Ok(m) = log.file.metadata() {
            log.written = m.len();
        }
    }
}

/// `2026-10-31 22:47:03`，本地时间。时间读不到时退化成 `-`。
fn timestamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    // SAFETY: 纯输出参数，`st` 是栈上有效的 SYSTEMTIME。
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    if st.wYear == 0 {
        return "-".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}
