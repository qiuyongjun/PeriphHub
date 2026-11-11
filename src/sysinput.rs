//! Windows 全局输入参数：指针速度、滚轮行数。
//!
//! 这几项不是设备里的 EEPROM 配置，而是系统级设置（和「设置 → 蓝牙和其他设备 →
//! 鼠标」里的滑块是同一个值），所以不走 HID，也不受"改完即写入设备"那套约束。
//! 官方驱动同样把这一组放在"设备概况 / 参数设置"里，但改的是 Windows。

use std::ffi::c_void;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_GETMOUSESPEED,
    SPI_GETWHEELSCROLLLINES, SPI_SETMOUSESPEED, SPI_SETWHEELSCROLLLINES,
};

/// `SPI_GETWHEELSCROLLLINES` 返回这个值表示"一次滚动一个屏幕"。
pub const WHEEL_PAGESCROLL: u32 = u32::MAX;

/// Windows 指针速度：1–20，10 是默认（滑块正中）。
pub const MOUSE_SPEED_MIN: u32 = 1;
pub const MOUSE_SPEED_MAX: u32 = 20;
pub const MOUSE_SPEED_DEFAULT: u32 = 10;

/// 每格滚动的行数上限。Windows 自己允许 1–100，这里照抄系统范围而不是收紧到
/// 一个"够用"的值：用户原本就设了 30 行的话，按一下 `+` 应该变成 31，而不是被
/// 拉回 15。界面上的"推荐 3 行"负责引导，不靠截断。
pub const WHEEL_LINES_MAX: u32 = 100;
pub const WHEEL_LINES_DEFAULT: u32 = 3;

fn get_u32(action: u32) -> Option<u32> {
    let mut value: u32 = 0;
    let ok = unsafe { SystemParametersInfoW(action, 0, &mut value as *mut u32 as *mut c_void, 0) };
    (ok != 0).then_some(value)
}

/// 值通过 `uiParam` 传递的设置项（如滚轮行数）。fWinIni 必须带
/// SPIF_UPDATEINIFILE | SPIF_SENDCHANGE：只改内存的话重启后设置丢失，
/// 运行中的其他程序也收不到 WM_SETTINGCHANGE。
fn set_ui_param(action: u32, value: u32) -> bool {
    let ok = unsafe {
        SystemParametersInfoW(
            action,
            value,
            std::ptr::null_mut(),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
    };
    ok != 0
}

pub fn mouse_speed() -> u32 {
    get_u32(SPI_GETMOUSESPEED)
        .unwrap_or(MOUSE_SPEED_DEFAULT)
        .clamp(MOUSE_SPEED_MIN, MOUSE_SPEED_MAX)
}

/// Returns false when Windows refuses the change (policy, security software,
/// or a hooked `SystemParametersInfo`). The caller surfaces that instead of
/// letting the number silently snap back.
pub fn set_mouse_speed(speed: u32) -> bool {
    let v = speed.clamp(MOUSE_SPEED_MIN, MOUSE_SPEED_MAX);
    // SPI_SETMOUSESPEED 与滚轮行数相反：`uiParam` 必须为 0，速度整数本身
    // 直接放在 `pvParam` 里（不是指向整数的指针）。传错时 Windows 不会改
    // 任何东西。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_SETMOUSESPEED,
            0,
            v as usize as *mut c_void,
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
    };
    ok != 0
}

/// 返回每格行数；`WHEEL_PAGESCROLL` 表示整屏滚动。
pub fn wheel_lines() -> u32 {
    get_u32(SPI_GETWHEELSCROLLLINES).unwrap_or(WHEEL_LINES_DEFAULT)
}

/// Returns false when Windows refuses the change; see `set_mouse_speed`.
pub fn set_wheel_lines(lines: u32) -> bool {
    let v = if lines == WHEEL_PAGESCROLL {
        WHEEL_PAGESCROLL
    } else {
        lines.clamp(1, WHEEL_LINES_MAX)
    };
    set_ui_param(SPI_SETWHEELSCROLLLINES, v)
}

/// 把系统值翻译成界面文案：整屏滚动没有数字可显示。
pub fn wheel_label(lines: u32) -> String {
    if lines == WHEEL_PAGESCROLL {
        "整屏".to_string()
    } else {
        format!("{lines} 行")
    }
}
