//! 驱动与 Windows HID 之间的窄接口。
//!
//! 只保留三个动作：发一帧、收一帧、句柄是否存活。overlapped 读取的取消、
//! 事件和缓冲生命周期都封在 Windows 实现里，协议层不再直接碰 HANDLE；
//! 测试因此可以用假实现模拟超时、写入失败和断连，不必接真机。

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetAttributes, HidD_GetInputReport, HidD_SetOutputReport, HIDD_ATTRIBUTES,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn ReadFile(h: HANDLE, buf: *mut c_void, n: u32, read: *mut u32, ov: *mut Overlapped) -> i32;
    fn CreateEventW(attr: *mut c_void, manual: i32, initial: i32, name: *const u16) -> HANDLE;
    fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    fn GetOverlappedResult(h: HANDLE, ov: *mut Overlapped, transferred: *mut u32, wait: i32)
        -> i32;
    fn CancelIo(h: HANDLE) -> i32;
    fn GetLastError() -> u32;
}

const ERROR_IO_PENDING: u32 = 997;
const ERROR_IO_INCOMPLETE: u32 = 996;
const ERROR_OPERATION_ABORTED: u32 = 995;
/// 取消一次挂起的读取后，最多再等这么久让它真正完成。等不到就放弃这次
/// 读取并泄漏那一小块堆内存，避免内核稍后写回已释放的栈变量。
const CANCEL_WAIT_MS: u32 = 250;

/// 驱动只需要这三件事。
///
/// `timeout` 是上限而不是保证：没有数据时实现可以提前返回 `Ok(None)`，
/// 轮询节奏由调用方掌握。
pub(crate) trait HidTransport: Send {
    fn send(&mut self, report: &[u8]) -> Result<(), String>;
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String>;
    fn alive(&self) -> bool;
}

fn wide_path(path: &[u16]) -> Vec<u16> {
    let mut p = path.to_vec();
    if p.last() != Some(&0) {
        p.push(0);
    }
    p
}

fn handle_alive(handle: HANDLE) -> bool {
    unsafe {
        let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
        attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
        HidD_GetAttributes(handle, &mut attr) != 0
    }
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: HANDLE,
}

/// 一次 overlapped 读取需要的缓冲区和事件。放在堆上，超时取消后即使内核
/// 稍后才完成，也不会写到已经退栈的内存；最坏情况下故意泄漏这一块。
///
/// 事件必须每次读取独占：共用一个事件时，上一次被放弃的读取稍后完成会把
/// 事件置位，新读取的等待被提前唤醒，`GetOverlappedResult` 却报
/// `ERROR_IO_INCOMPLETE`，此时释放缓冲就是释放后使用。
struct PendingRead {
    ov: Overlapped,
    buf: Vec<u8>,
    event: HANDLE,
}

impl Drop for PendingRead {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.event) };
    }
}

/// HID++ 使用的 overlapped 通道。
pub(crate) struct OverlappedHid {
    handle: HANDLE,
    /// 每次读取的缓冲长度，由协议层的报告长度决定。
    report_len: usize,
    /// 内核持有 `ov` 的地址，Vec 扩容搬移元素会让指针失效，所以每项单独
    /// 装箱；缓冲本身已经在堆上，但这里要固定的是 OVERLAPPED 的地址。
    #[allow(clippy::vec_box)]
    abandoned: Vec<Box<PendingRead>>,
}

impl OverlappedHid {
    pub(crate) fn open(path: &[u16], report_len: usize) -> Result<Self, String> {
        let p = wide_path(path);
        let handle = unsafe {
            CreateFileW(
                p.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "打开接收器失败（错误码 {}）",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            handle,
            report_len,
            abandoned: Vec::new(),
        })
    }

    /// One overlapped read, bounded by `ms`.
    ///
    /// 超时这一路不能“取消完就返回”：`CancelIo` 只是请求取消，内核仍可能
    /// 稍后才完成这次读。读取缓冲在堆上，取消后用有界等待确认结束；等不到
    /// 就把这块缓冲挂到 `abandoned`，由 `Drop` 再确认一次——既不会
    /// use-after-free，也不会让采集线程永久卡住。
    fn read_frame(&mut self, ms: u32) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
            if event.is_null() {
                return Err("创建读取事件失败".into());
            }
            let mut pending = Box::new(PendingRead {
                ov: Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    h_event: event,
                },
                buf: vec![0u8; self.report_len],
                event,
            });
            let mut read = 0u32;
            let len = pending.buf.len() as u32;
            let ok = ReadFile(
                self.handle,
                pending.buf.as_mut_ptr() as *mut c_void,
                len,
                &mut read,
                &mut pending.ov,
            );
            let taken = |read: u32, buf: &[u8]| buf[..(read as usize).min(buf.len())].to_vec();
            if ok != 0 {
                return Ok(Some(taken(read, &pending.buf)));
            }
            if GetLastError() != ERROR_IO_PENDING {
                return Err("读取应答失败".into());
            }
            if WaitForSingleObject(event, ms) == 0 {
                if GetOverlappedResult(self.handle, &mut pending.ov, &mut read, 0) == 0 {
                    return Err("读取应答失败".into());
                }
                return Ok(Some(taken(read, &pending.buf)));
            }
            // 超时：请求取消，但只等有限时间。等不到就放弃这块缓冲。
            CancelIo(self.handle);
            if WaitForSingleObject(event, CANCEL_WAIT_MS) != 0 {
                self.abandoned.push(pending);
                return Err("取消读取请求超时".into());
            }
            if GetOverlappedResult(self.handle, &mut pending.ov, &mut read, 0) != 0 {
                return Ok(Some(taken(read, &pending.buf)));
            }
            if GetLastError() != ERROR_OPERATION_ABORTED {
                return Err("读取应答失败".into());
            }
            Ok(None)
        }
    }
}

impl HidTransport for OverlappedHid {
    fn send(&mut self, report: &[u8]) -> Result<(), String> {
        let ok = unsafe {
            HidD_SetOutputReport(
                self.handle,
                report.as_ptr() as *const c_void,
                report.len() as u32,
            )
        };
        if ok == 0 {
            return Err("发送命令失败".into());
        }
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        self.read_frame(timeout.as_millis().max(1) as u32)
    }

    fn alive(&self) -> bool {
        handle_alive(self.handle)
    }
}

impl Drop for OverlappedHid {
    fn drop(&mut self) {
        unsafe {
            if !self.abandoned.is_empty() {
                // 这些读取已经取消过但没等到完成；关句柄前再确认一次，
                // 免得内核在缓冲释放之后才写回。CancelIo 只取消本线程的
                // 请求，采集线程正是读取的发起者。
                CancelIo(self.handle);
                // 每次读取有独立事件，但仍要逐个 OVERLAPPED 查是否真的
                // 结束：事件没等到只说明期限内没完成，不能据此释放缓冲。
                // 总等待时长有上限，退出的响应时间不能被它拖住。
                let deadline = Instant::now() + Duration::from_millis(CANCEL_WAIT_MS as u64);
                let mut stalled = Vec::new();
                for mut p in self.abandoned.drain(..) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if !left.is_zero() {
                        WaitForSingleObject(p.event, left.as_millis().max(1) as u32);
                    }
                    let mut n = 0u32;
                    if GetOverlappedResult(self.handle, &mut p.ov, &mut n, 0) == 0
                        && GetLastError() == ERROR_IO_INCOMPLETE
                    {
                        stalled.push(p);
                    }
                    // 已完成（含被取消）的缓冲随 p 一起释放，事件由
                    // `PendingRead::drop` 关闭。
                }
                if !stalled.is_empty() {
                    // 极端情况：宁可漏掉这块堆内存和它的事件，也不能让
                    // 内核写到已释放的地址上。
                    std::mem::forget(stalled);
                }
            }
            CloseHandle(self.handle);
        }
    }
}

// 句柄只在拥有驱动的采集线程里使用；trait 的 Send 是把它移进线程所必需的。
unsafe impl Send for OverlappedHid {}

/// Rapoo 使用的同步查询通道。
pub(crate) struct BlockingHid {
    handle: HANDLE,
    report_id: u8,
    report_len: usize,
}

impl BlockingHid {
    pub(crate) fn open(path: &[u16], report_id: u8, report_len: usize) -> Result<Self, String> {
        let p = wide_path(path);
        let handle = unsafe {
            CreateFileW(
                p.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "打开设备失败（错误码 {}）",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            handle,
            report_id,
            report_len,
        })
    }

    /// 打开时校验 VID/PID 用；之后驱动不再需要设备属性。
    pub(crate) fn ids(&self) -> Option<(u16, u16)> {
        unsafe {
            let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
            attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
            if HidD_GetAttributes(self.handle, &mut attr) == 0 {
                return None;
            }
            Some((attr.VendorID, attr.ProductID))
        }
    }
}

impl HidTransport for BlockingHid {
    fn send(&mut self, report: &[u8]) -> Result<(), String> {
        let ok = unsafe {
            HidD_SetOutputReport(
                self.handle,
                report.as_ptr() as *const c_void,
                report.len() as u32,
            )
        };
        if ok == 0 {
            return Err("发送命令失败".into());
        }
        Ok(())
    }

    fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        let mut resp = vec![0u8; self.report_len];
        // HidD_GetInputReport 要求缓冲首字节是报告 ID。
        resp[0] = self.report_id;
        let ok = unsafe {
            HidD_GetInputReport(
                self.handle,
                resp.as_mut_ptr() as *mut c_void,
                resp.len() as u32,
            )
        };
        Ok((ok != 0).then_some(resp))
    }

    fn alive(&self) -> bool {
        handle_alive(self.handle)
    }
}

impl Drop for BlockingHid {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

// 同 OverlappedHid：句柄只随驱动在单个采集线程内移动。
unsafe impl Send for BlockingHid {}
