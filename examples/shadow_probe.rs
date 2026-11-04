//! Throwaway probe (not part of the app): with the window ACTIVE, does the
//! "keep WS_CAPTION|WS_THICKFRAME + zero the non-client area" recipe get a
//! usable DWM shadow on this Win10 build?
//!
//! Modes:
//!   plain   - normal WS_OVERLAPPEDWINDOW (control: full native chrome)
//!   caption - WS_OVERLAPPEDWINDOW + WM_NCCALCSIZE -> 0 (Chromium recipe)
//!   extend  - caption + DwmExtendFrameIntoClientArea 1px (classic variant)
//!
//! A dark card is shown over a white backdrop window so the shadow's grey
//! falloff is measurable on screen. The window is shown SW_SHOW and brought to
//! the foreground: the previous probe used SW_SHOWNOACTIVATE, and an inactive
//! window's DWM shadow is nearly invisible, which invalidated that round.

use std::sync::atomic::{AtomicU32, Ordering};

type Hwnd = *mut core::ffi::c_void;
type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;

const WM_NCCALCSIZE: u32 = 0x0083;
const WM_TIMER: u32 = 0x0113;
const WM_CLOSE: u32 = 0x0010;
const WM_DESTROY: u32 = 0x0002;
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const SW_SHOW: i32 = 5;
const CS_HREDRAW: u32 = 0x0002;
const CS_VREDRAW: u32 = 0x0001;
const IDC_ARROW: u32 = 32512;

// 这两个结构只出现在 FFI 签名里从不实例化，clippy 的 dead_code 会报它们。
#[allow(dead_code)]
#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[allow(dead_code)]
#[repr(C)]
struct NcCalcSizeParams {
    rgrc: [Rect; 3],
    lppos: *mut core::ffi::c_void,
}

#[repr(C)]
struct Margins {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: Point,
    lprivate: u32,
}

#[repr(C)]
struct WndClassW {
    style: u32,
    lpfn_wnd_proc: Option<WndProc>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    hinstance: *mut core::ffi::c_void,
    hicon: *mut core::ffi::c_void,
    hcursor: *mut core::ffi::c_void,
    hbr_background: *mut core::ffi::c_void,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(class: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Hwnd,
        menu: Hwnd,
        instance: *mut core::ffi::c_void,
        param: *mut core::ffi::c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn SetWindowPos(
        hwnd: Hwnd,
        insert_after: Hwnd,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn SetTimer(hwnd: Hwnd, id: usize, timeout_ms: u32, func: *mut core::ffi::c_void) -> usize;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn PostQuitMessage(code: i32);
    fn LoadCursorW(instance: *mut core::ffi::c_void, name: *const u16) -> *mut core::ffi::c_void;
    fn CreateSolidBrush(color: u32) -> *mut core::ffi::c_void;
    fn SetProcessDPIAware() -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn DeleteObject(obj: *mut core::ffi::c_void) -> i32;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmExtendFrameIntoClientArea(hwnd: Hwnd, margins: *const Margins) -> i32;
}

const MODE_PLAIN: u32 = 0;
const MODE_CAPTION: u32 = 1;
const MODE_EXTEND: u32 = 2;

static MODE: AtomicU32 = AtomicU32::new(MODE_CAPTION);

unsafe extern "system" fn wndproc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_NCCALCSIZE => {
            if wparam == 0 || MODE.load(Ordering::Relaxed) == MODE_PLAIN {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            0
        }
        WM_TIMER => {
            if wparam == 1 {
                // Active window: this is the state a real app window is in,
                // and the only state where the DWM shadow reaches full strength.
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
                SetTimer(hwnd, 2, 9_000, std::ptr::null_mut());
            } else {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn register(name: &str, brush: *mut core::ffi::c_void) {
    let wide: Vec<u16> = format!("{name}\0").encode_utf16().collect();
    let class = WndClassW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfn_wnd_proc: Some(wndproc),
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        hinstance: GetModuleHandleW(std::ptr::null()),
        hicon: std::ptr::null_mut(),
        hcursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW as usize as *const u16),
        hbr_background: brush,
        lpsz_menu_name: std::ptr::null(),
        lpsz_class_name: wide.as_ptr(),
    };
    RegisterClassW(&class);
}

fn main() {
    let arg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "caption".to_string());
    let mode = match arg.as_str() {
        "plain" => MODE_PLAIN,
        "extend" => MODE_EXTEND,
        _ => MODE_CAPTION,
    };

    unsafe {
        SetProcessDPIAware();
        register("ShadowProbeClass", CreateSolidBrush(0x0018_1818)); // dark card
        register("ShadowBackClass", CreateSolidBrush(0x00FF_FFFF)); // white field
        let instance = GetModuleHandleW(std::ptr::null());

        let probe_class: Vec<u16> = "ShadowProbeClass\0".encode_utf16().collect();
        let back_class: Vec<u16> = "ShadowBackClass\0".encode_utf16().collect();
        let title: Vec<u16> = "shadow probe\0".encode_utf16().collect();

        let backdrop = CreateWindowExW(
            0,
            back_class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_VISIBLE,
            200,
            120,
            620,
            480,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if backdrop.is_null() {
            eprintln!("backdrop CreateWindowExW failed");
            return;
        }
        MODE.store(mode, Ordering::Relaxed);

        let hwnd = CreateWindowExW(
            0,
            probe_class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            300,
            200,
            420,
            300,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            eprintln!("CreateWindowExW failed");
            return;
        }

        // A fresh window keeps the frame it was created with until the styles
        // are re-applied; SWP_FRAMECHANGED is what actually triggers a second
        // WM_NCCALCSIZE that our zero-client override sticks to.
        {
            const SWP_NOSIZE: u32 = 0x0001;
            const SWP_NOMOVE: u32 = 0x0002;
            const SWP_NOZORDER: u32 = 0x0004;
            const SWP_NOACTIVATE: u32 = 0x0010;
            const SWP_FRAMECHANGED: u32 = 0x0020;
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }

        if mode == MODE_EXTEND {
            let margins = Margins {
                left: 1,
                right: 1,
                top: 1,
                bottom: 1,
            };
            DwmExtendFrameIntoClientArea(hwnd, &margins);
        }

        // Show after a short delay so the capture harness can settle.
        SetTimer(hwnd, 1, 2_000, std::ptr::null_mut());

        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        DeleteObject(CreateSolidBrush(0)); // noop keep gdi32 linked deterministically
    }
}
