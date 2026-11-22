//! 卡片投影：一个不参与命中测试的分层窗口。
//!
//! 卡片四周留 `mini::SHADOW_PAD` 的透明边距给投影落脚，可窗口是矩形的：这圈
//! 像素仍然属于客户区，落在上面的点击会被本窗口吃掉。`WM_NCHITTEST` 返回
//! `HTTRANSPARENT` 解决不了这个问题——按 Win32 文档，它只在**同线程**窗口
//! 之间继续查找（<https://learn.microsoft.com/windows/win32/inputdev/wm-nchittest>），
//! 对桌面和其他应用无效，那圈边距依旧是一块"隐形挡板"。
//!
//! 能跨进程放行的是窗口区域：`SetWindowRgn` 把主窗口裁成卡片形状后，区域外
//! 的像素根本不属于这个窗口，点击直接落到下层。代价是区域同样裁掉绘制，投影
//! 不能再画在主窗口里——所以它搬到了这个窗口上：
//!
//! - `WS_EX_LAYERED` 给逐像素 alpha（`UpdateLayeredWindow`），阴影才能半透明；
//! - `WS_EX_TRANSPARENT` 让它完全不参与命中测试，这一圈点击穿透到底下；
//! - `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` 让它不抢焦点、不进 Alt+Tab。
//!
//! 它永远贴在主窗口正下方，画的就是卡片外围那一圈。像素算法与
//! `ui::widgets::shadow_card` 同源（同一组高斯环），两条绘制路径出来的影子
//! 一模一样：这个窗口建不起来时，主窗口仍按老办法自绘投影。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowRect, IsIconic, RegisterClassW, SetWindowPos,
    ShowWindow, UpdateLayeredWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::ui::widgets::{ring_alpha, shadow_lobes};

/// 投影窗口的句柄；0 表示没有（建不起来，或还没建）。
static HANDLE: AtomicIsize = AtomicIsize::new(0);
/// 当前是否被隐藏，避免每帧重复 `ShowWindow`。
static HIDDEN: AtomicBool = AtomicBool::new(true);

/// 影响像素的一组输入。只在它变化时重画，挪窗口不重画。
#[derive(Clone, Copy, PartialEq)]
struct Face {
    /// 可见卡片到窗口边缘的距离（物理像素）。
    pad: f32,
    /// 卡片圆角（物理像素）。
    radius: f32,
    /// 逻辑像素到物理像素的换算：高斯环按它缩放。
    scale: f32,
    dark: bool,
    focused: bool,
}

/// 已经画出来的那一份：外观、尺寸与落点。
struct Painted {
    face: Face,
    w: i32,
    h: i32,
    at: (i32, i32),
}

static PAINTED: Mutex<Option<Painted>> = Mutex::new(None);

/// 建好投影窗口并挂到 `owner` 底下。返回 `false` 时调用方要退回自绘投影。
pub fn attach(owner: HWND) -> bool {
    if HANDLE.load(Ordering::Relaxed) != 0 {
        return true;
    }
    // SAFETY: 只创建本进程的窗口；类名、过程指针都在本进程内。
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        if hinst.is_null() {
            return false;
        }
        let class: Vec<u16> = "PeriphHubShadow\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(shadow_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        // 类名重复注册会失败：多实例已被单实例锁挡住，这里忽略返回值即可。
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return false;
        }
        // 先挂到主窗口下方；之后每次同步都会重申这个 z 序。
        SetWindowPos(
            hwnd,
            owner,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        HIDDEN.store(false, Ordering::Relaxed);
        HANDLE.store(hwnd as isize, Ordering::Relaxed);
        true
    }
}

/// 主窗口每帧调用：几何与外观一起对齐。
pub fn follow(owner: HWND, pad: f32, radius: f32, scale: f32, dark: bool, focused: bool) {
    let face = Face {
        pad,
        radius,
        scale,
        dark,
        focused,
    };
    sync(owner, Some(face));
}

/// 窗口过程里调用：拖动、缩放、改 z 序时几何必须立刻跟上，不能等下一帧——
/// 系统拖动窗口时是模态循环，界面那侧根本不会再跑。
pub fn follow_geometry(owner: HWND) {
    sync(owner, None);
}

/// 隐藏投影（最小化、最大化、没有可见卡片时）。
pub fn hide() {
    let hwnd = HANDLE.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() || HIDDEN.swap(true, Ordering::Relaxed) {
        return;
    }
    // 尺寸/外观缓存一起丢掉：再显示时按当时的参数重画一次。
    if let Ok(mut painted) = PAINTED.lock() {
        *painted = None;
    }
    // SAFETY: hwnd 是本模块创建的窗口。
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

/// 投影窗口是否可用。不可用时界面走自绘投影，窗口也不裁成卡片形状。
pub fn attached() -> bool {
    HANDLE.load(Ordering::Relaxed) != 0
}

fn sync(owner: HWND, face: Option<Face>) {
    let hwnd = HANDLE.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() {
        return;
    }
    // 从窗口过程来的调用：几何或 z 序刚变过，落点无论如何都要重申一次。
    let from_hook = face.is_none();
    let Some((x, y, w, h)) = window_rect(owner) else {
        hide();
        return;
    };
    // SAFETY: owner 是主窗口，IsIconic 只读状态。
    if w <= 0 || h <= 0 || unsafe { IsIconic(owner) } != 0 {
        hide();
        return;
    }
    let Ok(mut painted) = PAINTED.lock() else {
        return;
    };
    let Some(face) = face.or_else(|| painted.as_ref().map(|p| p.face)) else {
        return;
    };
    // 整块卡片都是卡片本身：没有边距就没有投影，主窗口这时也不裁区域。
    if face.pad < 0.5 {
        drop(painted);
        hide();
        return;
    }
    let need_paint = painted
        .as_ref()
        .is_none_or(|p| p.face != face || p.w != w || p.h != h);
    if need_paint && !paint(hwnd, (x, y), (w, h), face) {
        drop(painted);
        hide();
        return;
    }
    // `UpdateLayeredWindow` 已经连同像素把落点摆好了，只剩 z 序要重申。
    let need_place = from_hook || need_paint || painted.as_ref().is_some_and(|p| p.at != (x, y));
    *painted = Some(Painted {
        face,
        w,
        h,
        at: (x, y),
    });
    drop(painted);

    // SAFETY: 都是本进程的窗口；`owner` 作为 z 序参照，让投影始终贴在主窗口
    // 下沿——主窗口置顶、或被别的窗口盖住时，影子跟着一起走。
    unsafe {
        if need_place {
            SetWindowPos(hwnd, owner, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
        if HIDDEN.swap(false, Ordering::Relaxed) {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

/// 主窗口的屏幕矩形：`(x, y, w, h)`。
fn window_rect(owner: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: rc 在栈上，owner 是主窗口。
    if unsafe { GetWindowRect(owner, &mut rc) } == 0 {
        return None;
    }
    Some((rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top))
}

/// 把投影画进分层窗口：原地在 `(x, y)`、尺寸 `w × h` 处交给系统合成。
fn paint(hwnd: HWND, at: (i32, i32), size: (i32, i32), face: Face) -> bool {
    let (w, h) = size;
    let pixels = shadow_pixels(w, h, face);
    // SAFETY: 屏幕 DC 由本函数获取并在结尾释放；DIB 与内存 DC 同样在本函数
    // 内成对创建/删除，窗口句柄是本模块创建的。
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return false;
        }
        let mem = CreateCompatibleDC(screen);
        if mem.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen);
            return false;
        }
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // 负高度 = 自上而下，和像素缓冲的行序一致。
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            mem,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if dib.is_null() || bits.is_null() {
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            return false;
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
        let old = SelectObject(mem, dib);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT { x: at.0, y: at.1 };
        let sz = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let ok = UpdateLayeredWindow(hwnd, screen, &dst, &sz, mem, &src, 0, &blend, ULW_ALPHA) != 0;
        SelectObject(mem, old);
        DeleteObject(dib);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        ok
    }
}

unsafe extern "system" fn shadow_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    // 分层窗口的像素全部由 `UpdateLayeredWindow` 接管，这里不需要自绘。
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 投影像素：预乘 BGRA，卡片范围内全透明。
///
/// 和 `ui::widgets::shadow_card` 是同一套几何：卡片外 `r` 像素处正好落在
/// "整块矩形向外平移 r、圆角半径也加 r"那一环上，而这正是圆角矩形向外
/// 平移 r 的轮廓。两瓣高斯各自离散成每 1px 一环的 alpha，再由外向内叠在
/// 一起——所以这里按"像素落在第 r 环、叠上 r..reach 所有环"来合成。
fn shadow_pixels(w: i32, h: i32, face: Face) -> Vec<u8> {
    paint_shadow(w, h, face, true)
}

/// 逐像素版本：不跳过卡片内部。只有测试用它和上面那份对拍。
#[cfg(test)]
fn shadow_pixels_full_scan(w: i32, h: i32, face: Face) -> Vec<u8> {
    paint_shadow(w, h, face, false)
}

/// `skip_inside` 打开时只遍历可能落在卡片外面的像素：卡片内部（含圆角以内）
/// 的 alpha 恒为 0，遍历它们纯属空转。缩放消息是在窗口线程里同步处理投影的，
/// 4K 尺寸下整图扫两遍要一百多毫秒，足以把实时缩放拖成幻灯片。
fn paint_shadow(w: i32, h: i32, face: Face, skip_inside: bool) -> Vec<u8> {
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let count = (w as usize) * (h as usize);
    // 预乘 BGRA，先全透明：卡片内部和窗外都保持 0，只有边距里那一圈会被写穿。
    let mut out = vec![0u8; count * 4];
    // 没有边距就没有落影的地方（最大化时就是这样）。
    if face.pad <= 0.0 {
        return out;
    }
    let (cx, cy, hx, hy) = (
        w as f32 * 0.5,
        h as f32 * 0.5,
        (w as f32 * 0.5 - face.pad).max(0.0),
        (h as f32 * 0.5 - face.pad).max(0.0),
    );
    let radius = face.radius.min(hx).min(hy).max(0.0);

    for (sigma, dy, peak) in shadow_lobes(face.dark, face.focused) {
        let sigma = (sigma * face.scale).max(0.5);
        let dy = dy * face.scale;
        let reach = ((2.7 * sigma).ceil() as i32).max(1);
        let table = ring_table(sigma, peak, reach);
        let lobe_cy = cy + dy;
        for y in 0..h {
            let py = y as f32 + 0.5;
            let row = (y as usize) * (w as usize);
            // 这一行里落在卡片内部的 x 区间：行内跨过圆角时边界是圆弧，半宽
            // 得按纵向距离解出来，不能直接拿 hx 当边界。
            let inside_half = if skip_inside {
                inside_half_width(py, lobe_cy, hx, hy, radius)
            } else {
                0.0
            };
            let mut shade = |from: i32, to: i32| {
                for x in from..to {
                    let d = rounded_distance(x as f32 + 0.5, py, cx, lobe_cy, hx, hy, radius);
                    let a = ring_at(&table, d) as u32;
                    if a == 0 {
                        continue;
                    }
                    // 反向预乘的黑色半透明覆盖：prev + a - prev·a/255。
                    let slot = &mut out[(row + x as usize) * 4 + 3];
                    let prev = *slot as u32;
                    *slot = (prev + a - prev * a / 255) as u8;
                }
            };
            match skip_range(cx, inside_half, w) {
                Some((left_end, right_start)) => {
                    shade(0, left_end);
                    shade(right_start, w);
                }
                None => shade(0, w),
            }
        }
    }
    out
}

/// 一行里"到卡片边缘的距离 ≤ 0"那一段的半宽（以卡片中线为轴）。
///
/// 行没越过圆角中心线（纵向 `qy ≤ 0`）时边界就是直边，半宽等于 `hx`；越过
/// 之后边界是圆弧，`hw = (hx - r) + sqrt(r² - qy²)`；`qy ≥ r` 时整行都在
/// 卡片外，半宽为 0。
fn inside_half_width(py: f32, cy: f32, hx: f32, hy: f32, radius: f32) -> f32 {
    let qy = (py - cy).abs() - (hy - radius);
    if qy <= 0.0 {
        return hx;
    }
    let t = radius * radius - qy * qy;
    if t <= 0.0 {
        return 0.0;
    }
    (hx - radius) + t.sqrt()
}

/// 这一行要跳过的像素区间 `[左段终点, 右段起点)`；`None` 表示整行都要算。
///
/// 像素中心在 `x + 0.5`，落在内部等价于 `|x + 0.5 - cx| ≤ 半宽`。半宽再收
/// 半个像素：只有离边界还有余量的像素才叫"肯定在卡片里"，免得在圆角处把
/// 贴着边界、还剩 1/255 alpha 的那一圈也跳掉。
fn skip_range(cx: f32, inside_half: f32, w: i32) -> Option<(i32, i32)> {
    let half = inside_half - 0.5;
    if half <= 0.0 {
        return None;
    }
    let left_end = ((cx - half - 0.5).ceil() as i32).clamp(0, w);
    let right_start = (((cx + half - 0.5).floor() as i32) + 1).clamp(0, w);
    if left_end >= right_start {
        // 内部区间是空的：没什么可跳的。
        return None;
    }
    Some((left_end, right_start))
}

/// 逐环 alpha 的累加：`table[r]` 是"落在第 r 环时，r..reach 各环叠起来"的
/// 总不透明度。
fn ring_table(sigma: f32, peak: f32, reach: i32) -> Vec<u8> {
    let mut table = vec![0u8; reach as usize + 1];
    let mut product = 1.0f32;
    for r in (1..=reach).rev() {
        product *= 1.0 - ring_alpha(sigma, peak, r) as f32 / 255.0;
        table[r as usize] = ((1.0 - product) * 255.0).round() as u8;
    }
    table
}

/// 距卡片边缘 `d` 像素处的总 alpha；卡片内部与遥远处都是 0。
fn ring_at(table: &[u8], d: f32) -> u8 {
    if d <= 0.0 {
        return 0;
    }
    // 第 1 环覆盖 (0, 1]，第 r 环覆盖 (r-1, r]：像素落在哪一环由距离上取整
    // 决定，和 `rect.expand(r)` 的覆盖范围一致。
    let r = d.ceil() as usize;
    table.get(r).copied().unwrap_or(0)
}

/// 圆角矩形的有符号距离：内部为负，外部为正（像素）。
fn rounded_distance(x: f32, y: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hx - r);
    let qy = (y - cy).abs() - (hy - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(pad: f32) -> Face {
        Face {
            pad,
            radius: 12.0,
            scale: 1.0,
            dark: false,
            focused: true,
        }
    }

    fn alpha_at(px: &[u8], w: i32, x: i32, y: i32) -> u8 {
        px[((y * w + x) * 4 + 3) as usize]
    }

    /// 卡片本身必须是全透明的：那块地方由主窗口自己画，两个窗口都涂一层
    /// 会在圆角处叠出深色边。
    #[test]
    fn the_card_itself_stays_clear() {
        let (w, h) = (200, 160);
        let px = shadow_pixels(w, h, face(16.0));
        assert_eq!(px.len(), (w * h * 4) as usize);
        for (x, y) in [(100, 80), (30, 30), (170, 130)] {
            assert_eq!(alpha_at(&px, w, x, y), 0, "卡片内 ({x},{y}) 不该有投影");
        }
    }

    /// 边距里要有影子，而且越往外越淡：出了窗口就是硬边，淡出必须在这里
    /// 走完（`visible_shadow_stays_inside_the_padding` 盯着 pad 够不够）。
    #[test]
    fn the_margin_fades_outwards() {
        let (w, h) = (200, 160);
        let px = shadow_pixels(w, h, face(16.0));
        let mut last = u8::MAX;
        let mut seen = false;
        for x in (0..16).rev() {
            let a = alpha_at(&px, w, x, h / 2);
            assert!(a <= last, "靠外的一环 {x} 反而更深：{a} > {last}");
            seen |= a > 0;
            last = a;
        }
        assert!(seen, "边距里没有影子");
    }

    /// 四角是圆角：角上的投影像素先衰减到 0，不会被直角切出一个方角阴影。
    #[test]
    fn corners_are_rounded_not_square() {
        let (w, h) = (200, 160);
        let px = shadow_pixels(w, h, face(16.0));
        assert_eq!(alpha_at(&px, w, 0, 0), 0, "窗口角上不该还有影子");
        assert!(
            alpha_at(&px, w, w / 2, 10) > 0,
            "同一条边距里，直边中点应该有影子"
        );
    }

    /// 失焦时整体收淡：和系统窗口"不活动时影子更浅"的行为对齐。
    #[test]
    fn unfocused_shadows_are_lighter() {
        let focused = shadow_pixels(200, 160, face(16.0));
        let idle = shadow_pixels(
            200,
            160,
            Face {
                focused: false,
                ..face(16.0)
            },
        );
        let a = alpha_at(&focused, 200, 4, 80);
        let b = alpha_at(&idle, 200, 4, 80);
        assert!(a > b, "失焦投影应变淡：{a} vs {b}");
    }

    /// 跳过卡片内部不能改变任何一个像素：逐像素版本只多算了 alpha 恒为 0
    /// 的那些点。
    #[test]
    fn skipping_the_card_interior_changes_nothing() {
        for pad in [8.0, 16.0, 40.0] {
            for radius in [0.0, 12.0, 30.0] {
                for dark in [false, true] {
                    for scale in [1.0, 1.25, 2.0] {
                        let face = Face {
                            pad,
                            radius,
                            scale,
                            dark,
                            focused: true,
                        };
                        assert_eq!(
                            shadow_pixels(240, 180, face),
                            shadow_pixels_full_scan(240, 180, face),
                            "pad={pad} radius={radius} dark={dark} scale={scale}"
                        );
                    }
                }
            }
        }
    }

    /// 没有边距、没有圆角（最大化时的形状）就没有投影：主窗口铺满整屏，
    /// 不该再留一圈影子。
    #[test]
    fn no_padding_means_no_shadow() {
        let px = shadow_pixels(
            200,
            160,
            Face {
                pad: 0.0,
                radius: 0.0,
                ..face(0.0)
            },
        );
        assert!(px.iter().all(|&b| b == 0), "没有边距时不该画影子");
    }
}
