use std::ffi::c_void;

use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "PeriphHub";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn is_enabled() -> bool {
    let sub = wide(RUN_KEY);
    let name = wide(VALUE);
    let mut size = 0u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    rc == 0
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let sub = wide(RUN_KEY);
    let name = wide(VALUE);
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败：{e}"))?;
        let data = wide(&format!("\"{}\"", exe.display()));
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr() as *const c_void,
                (data.len() * 2) as u32,
            )
        };
        if rc != 0 {
            return Err(format!("写入自启动注册表失败（错误码 {rc}）"));
        }
    } else {
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr()) };
        // 2 = ERROR_FILE_NOT_FOUND, already absent.
        if rc != 0 && rc != 2 {
            return Err(format!("删除自启动注册表失败（错误码 {rc}）"));
        }
    }
    Ok(())
}
