#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 整个程序直接调用 SetupAPI / hid.dll / 注册表，没有任何跨平台回退路径。
// 让别的平台在编译期就得到一句人话，而不是一屏链接错误。
#[cfg(not(windows))]
compile_error!(
    "PeriphHub 直接调用 Win32 API（SetupAPI / hid.dll / 注册表），只能在 Windows 上构建。"
);

mod app;
mod autostart;
mod config;
mod demo;
mod devices;
mod driver;
mod hid;
mod log;
mod sysinput;
mod theme;
mod tray;
mod ui;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // 双开没有任何好处：两个进程会互相覆盖 config.json（原子替换防不住两个
    // 写者）、出现两个托盘图标和两个浮窗。第二个实例把已有窗口亮出来就退出。
    if !single_instance() {
        return Ok(());
    }
    log::init();
    let cfg = config::load();

    // 启动固定从浮窗开始：主窗口随时能一键切回来，但一启动就盖住桌面
    // 不是这个应用想要的形态，因此不沿用上次退出时的模式（Panel::new
    // 同样强制 Mini）。运行期切到主界面后，窗口形状由 apply_shell 接管。
    //
    // The window is created transparent in both shells: the main UI paints an
    // opaque surface over it, the widget paints a rounded card with a shadow
    // and lets the desktop show through the corners.
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("PeriphHub")
        .with_decorations(false)
        // The widget is sized to its content, so it is never user-resizable.
        .with_resizable(false)
        .with_transparent(true)
        // 启动固定是浮窗：只留托盘图标，不占任务栏。切到主界面时
        // `Panel::sync_taskbar` 再把它加回来。
        .with_taskbar(false)
        .with_inner_size(ui::mini::window_size(ui::mini::WIDTH, 240.0))
        .with_min_inner_size(ui::mini::window_size(ui::mini::WIDTH, 120.0))
        .with_window_level(if cfg.mini_pinned {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        })
        .with_icon(egui::IconData {
            rgba: theme::icon_rgba(64),
            width: 64,
            height: 64,
        });

    // Reopening where the user left off beats re-centering every launch, and
    // it also avoids a visible jump on the first frame.
    if let Some([x, y]) = cfg.mini_pos {
        viewport = viewport.with_position([x, y]);
    }

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "PeriphHub",
        options,
        Box::new(|cc| Ok(Box::new(app::Panel::new(cc)))),
    )
}

/// 是否本进程是第一个实例。已有一个实例在跑时，把它的窗口拉到前台。
fn single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_SHOW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    // SAFETY: 创建互斥体和按标题找窗口都是常规 Win32 调用，无共享状态竞争。
    // GetLastError 必须紧跟在 CreateMutexW 之后调用，中间不能插入任何可能
    // 改写 last-error 的调用。
    unsafe {
        let name = wide("PeriphHub-SingleInstance");
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if mutex.is_null() {
            // 创建失败（极端情况）：宁可放行，也别让应用永远起不来。
            return true;
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            // 句柄故意不关：锁要活到进程结束才一直有效。HANDLE 是裸指针，
            // 本就没有析构，绑定存活到 main 结束即可，进程退出时系统回收。
            let _ = mutex;
            return true;
        }
        // 已有实例。主界面和浮窗共用同一个原生窗口，标题都是 "PeriphHub"，
        // 把它亮出来，顺便告诉用户程序其实一直在。
        let title = wide("PeriphHub");
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
        false
    }
}
