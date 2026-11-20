use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CM_Register_Notification, CM_Unregister_Notification,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDeviceRegistryPropertyW, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
    CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_2, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HCMNOTIFICATION, SPDRP_BASE_CONTAINERID,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetManufacturerString,
    HidD_GetPreparsedData, HidD_GetProductString, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::driver::{self, DeviceType, DriverSpec, Limits};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionType {
    Wireless24G,
    Bluetooth,
    Wired,
    Unknown,
}

impl ConnectionType {
    pub fn label(self) -> &'static str {
        match self {
            ConnectionType::Wireless24G => "2.4G",
            ConnectionType::Bluetooth => "蓝牙",
            ConnectionType::Wired => "有线",
            ConnectionType::Unknown => "未知",
        }
    }

    pub fn is_wired(self) -> bool {
        matches!(self, ConnectionType::Wired)
    }
}

#[derive(Clone, Debug)]
pub struct HidDevice {
    pub id: String,
    pub path: Vec<u16>,
    pub vid: u16,
    pub pid: u16,
    pub brand: String,
    pub model: String,
    /// 设备形态，界面用来选图标。
    pub kind: DeviceType,
    pub connection: ConnectionType,
    /// Protocol driver this receiver speaks, `None` when it has no writable
    /// vendor channel (the device is then listed read-only).
    pub driver: Option<DriverSpec>,
    /// 该型号允许的取值范围；只读设备使用默认值，仅用于未用到的 UI 路径。
    pub limits: Limits,
    pub reason: Option<&'static str>,
}

impl HidDevice {
    /// Whether the panel can read and write this device at all.
    pub fn has_config(&self) -> bool {
        self.driver.is_some()
    }
}

struct Interface {
    path: Vec<u16>,
    instance: String,
    group: String,
    /// 挂在 ROOT/VHF/ACPI/SWD 等系统树下的接口，不是外部外设。
    system: bool,
    vid: u16,
    pid: u16,
    product: String,
    manufacturer: String,
    usage_page: u16,
    usage: u16,
}

/// 从定长缓冲里取一个以 NUL 结尾的宽字符串。`max` 是缓冲能容纳的 u16 个数，
/// 没有终止符时最多读 `max` 个，避免一路扫描到缓冲区外。
unsafe fn wide_path(p: *const u16, max: usize) -> Vec<u16> {
    let mut n = 0usize;
    while n < max && *p.add(n) != 0 {
        n += 1;
    }
    std::slice::from_raw_parts(p, (n + 1).min(max)).to_vec()
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

pub fn classify_connection(instance: &str, vid: u16, pid: u16, product: &str) -> ConnectionType {
    let lower = instance.to_ascii_lowercase();
    if lower.starts_with("bth") {
        ConnectionType::Bluetooth
    } else if let Some(p) = driver::profile(vid, pid) {
        // 接收器也是长在 USB 上的，实例路径分不出来，型号表说了算：
        // 同一个鼠标插线（0x4414）和走接收器（0x1214）是两个不同的 PID。
        if p.wired {
            ConnectionType::Wired
        } else {
            ConnectionType::Wireless24G
        }
    } else {
        // 未知设备不能仅凭 USB 实例路径断定有线：无线接收器也枚举成 USB。
        // 产品串里明确写了接收器/无线才敢标 2.4G，否则老实说未知。
        if is_receiver_product(product) || is_wireless_product(product) {
            ConnectionType::Wireless24G
        } else {
            ConnectionType::Unknown
        }
    }
}

fn is_receiver_product(product: &str) -> bool {
    let lower = product.to_ascii_lowercase();
    lower.contains("receiver") || lower.contains("dongle") || product.contains("接收器")
}

fn is_wireless_product(product: &str) -> bool {
    let lower = product.to_ascii_lowercase();
    lower.contains("wireless") || product.contains("无线")
}

const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_MOUSE: u16 = 0x02;
const HID_USAGE_KEYBOARD: u16 = 0x06;

/// 未登记型号的形态推断：产品串优先，其次看 HID 用途页。只影响图标和
/// 文案，不会因此启用任何驱动；已知型号仍以 `PROFILES` 的声明为准。
fn infer_kind(product: &str, usages: &[(u16, u16)]) -> DeviceType {
    if is_receiver_product(product) {
        return DeviceType::Receiver;
    }
    if let Some(kind) = DeviceType::from_name(product) {
        return kind;
    }
    // 厂商用途页 + 泛化产品名（如 "Gaming Device"）更可能是接收器/复合设备，
    // 仅凭键盘/鼠标集合猜形态会猜错；保留未知比给错图标好。
    if usages.iter().any(|(page, _)| *page >= 0xFF00) {
        return DeviceType::Unknown;
    }
    let has = |page: u16, usage: u16| usages.contains(&(page, usage));
    if has(HID_USAGE_PAGE_GENERIC_DESKTOP, HID_USAGE_KEYBOARD) {
        DeviceType::Keyboard
    } else if has(HID_USAGE_PAGE_GENERIC_DESKTOP, HID_USAGE_MOUSE) {
        DeviceType::Mouse
    } else {
        DeviceType::Unknown
    }
}

/// 未适配设备在只读卡片/配置页里展示的原因。
const UNSUPPORTED_REASON: &str =
    "已识别到 HID 设备，但该型号尚未适配；仅显示身份与连接状态，不会写入厂商协议。";

/// 已知型号但没找到可用厂商集合时的原因。
const NO_CHANNEL_REASON: &str =
    "已识别到设备，但没有找到可用的厂商配置接口；仅显示身份与连接状态。";

/// Windows 的虚拟/内置 HID 树（ROOT/VHF/ACPI/SWD）不是外设；列出来只会给
/// 工作台添噪音。只过滤这几类系统总线，真实 USB/蓝牙设备仍保留。
fn is_system_hid_group(group: &str) -> bool {
    let group = group.to_ascii_uppercase();
    [
        "P:ROOT\\", "P:VHF\\", "P:ACPI\\", "P:SWD\\", "ROOT\\", "VHF\\", "ACPI\\", "SWD\\",
    ]
    .iter()
    .any(|prefix| group.starts_with(prefix))
}

fn group_key(instance: &str) -> String {
    let lower = instance.to_ascii_lowercase();
    // 复合设备接口是 `…&MI_01\7&哈希&0&0000`，集合是 `…&Col02`：都要截掉，
    // 同一物理设备的接口才会落到同一组。父设备 ID 可用时轮不到这里兜底。
    let cut = [lower.find("&col"), lower.find("&mi_")]
        .into_iter()
        .flatten()
        .min();
    match cut {
        Some(i) => lower[..i].to_string(),
        None => lower,
    }
}

/// 把 `SPDRP_BASE_CONTAINERID` 返回的 REG_SZ 宽字符 GUID 转成稳定的分组键。
/// 传入的是原始字节，尾部允许有未使用的零。
fn container_key(bytes: &[u8]) -> Option<String> {
    let wide: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let text = wide_to_string(&wide);
    // 不支持容器 ID 的设备会返回这个固定的“空 GUID”；它不能当分组键，
    // 否则多台这类设备会被合并成一张卡片。
    let null_guid = text.eq_ignore_ascii_case("{00000000-0000-0000-ffff-ffffffffffff}")
        || text.eq_ignore_ascii_case("{00000000-0000-0000-0000-000000000000}");
    (!text.is_empty() && !null_guid).then(|| format!("c:{}", text.to_ascii_lowercase()))
}

unsafe fn container_id(set: isize, info: &SP_DEVINFO_DATA) -> Option<String> {
    let mut ty = 0u32;
    let mut needed = 0u32;
    // SPDRP_BASE_CONTAINERID 是 REG_SZ 的 GUID 字符串（约 78 字节），不是
    // 16 字节二进制；按旧缓冲大小查询会一直因为空间不足而走兜底分组。
    let mut buf = [0u8; 256];
    let ok = SetupDiGetDeviceRegistryPropertyW(
        set,
        info,
        SPDRP_BASE_CONTAINERID,
        &mut ty,
        buf.as_mut_ptr(),
        buf.len() as u32,
        &mut needed,
    );
    if ok == 0 {
        return None;
    }
    debug_assert!(needed as usize <= buf.len());
    container_key(&buf)
}

/// 备用分组键：接口所属 USB 物理设备的实例 ID。ContainerId 查不到时用
/// 它兜底，否则一个复合设备的几个 HID 接口
/// （`…&MI_00`、`…&MI_01`）会被当成几台设备各出一张卡片——同一把键盘
/// 在首页出现两次就是这么来的。
///
/// 节点层级是 `HID\…&MI_00\…` → `USB\…&MI_00\…` → `USB\…\…`（物理设备），
/// 所以一路向上爬，取第一个不带 MI 后缀的实例 ID。
unsafe fn parent_id(info: &SP_DEVINFO_DATA) -> Option<String> {
    let mut node = info.DevInst;
    for _ in 0..3 {
        let mut next = 0u32;
        if CM_Get_Parent(&mut next, node, 0) != CR_SUCCESS {
            return None;
        }
        node = next;
        let mut buf = [0u16; 512];
        if CM_Get_Device_IDW(node, buf.as_mut_ptr(), buf.len() as u32, 0) != CR_SUCCESS {
            return None;
        }
        let id = wide_to_string(&buf);
        if id.is_empty() {
            return None;
        }
        if !id.to_ascii_lowercase().contains("&mi_") {
            return Some(format!("p:{id}"));
        }
    }
    None
}

unsafe fn instance_id(set: isize, info: &SP_DEVINFO_DATA) -> String {
    let mut buf = [0u16; 512];
    if SetupDiGetDeviceInstanceIdW(
        set,
        info,
        buf.as_mut_ptr(),
        buf.len() as u32,
        std::ptr::null_mut(),
    ) == 0
    {
        return String::new();
    }
    wide_to_string(&buf)
}

/// 只为了读属性而打开设备。键盘/鼠标的顶层集合常被系统独占，用读写权限打开
/// 会直接被拒（设备于是从列表里彻底消失）；`HidD_*` 的元数据查询允许 0 访问
/// 权限，所以失败时退一步再试。
unsafe fn open_for_query(path: &[u16]) -> HANDLE {
    let open = |access: u32| {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    let h = open(GENERIC_READ | GENERIC_WRITE);
    if h != INVALID_HANDLE_VALUE {
        return h;
    }
    open(0)
}

unsafe fn read_interface(path: &[u16]) -> Option<(u16, u16, String, String, u16, u16)> {
    let h = open_for_query(path);
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
    attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    if HidD_GetAttributes(h, &mut attr) == 0 {
        CloseHandle(h);
        return None;
    }
    let mut product = String::new();
    let mut manufacturer = String::new();
    let mut buf = [0u16; 128];
    if HidD_GetProductString(h, buf.as_mut_ptr() as *mut c_void, (buf.len() * 2) as u32) != 0 {
        product = wide_to_string(&buf);
    }
    let mut buf = [0u16; 128];
    if HidD_GetManufacturerString(h, buf.as_mut_ptr() as *mut c_void, (buf.len() * 2) as u32) != 0 {
        manufacturer = wide_to_string(&buf);
    }
    let mut usage_page = 0;
    let mut usage = 0;
    let mut pp: isize = 0;
    if HidD_GetPreparsedData(h, &mut pp) != 0 {
        let mut caps: HIDP_CAPS = std::mem::zeroed();
        if HidP_GetCaps(pp, &mut caps) >= 0 {
            usage_page = caps.UsagePage;
            usage = caps.Usage;
        }
        HidD_FreePreparsedData(pp);
    }
    CloseHandle(h);
    Some((
        attr.VendorID,
        attr.ProductID,
        product,
        manufacturer,
        usage_page,
        usage,
    ))
}

fn collect_interfaces() -> Vec<Interface> {
    unsafe {
        let mut guid: windows_sys::core::GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == -1 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut did: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut did) == 0 {
                break;
            }
            index += 1;
            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &did,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            );
            if needed == 0 {
                continue;
            }
            // 这块内存会被当成 `SP_DEVICE_INTERFACE_DETAIL_DATA_W` 写：`Vec<u8>`
            // 只保证 1 字节对齐，写结构体是未定义行为。用 u64 承载保证对齐，
            // 路径偏移也不再硬编码 4，交给 offset_of 算。
            let mut buf = vec![0u64; needed as usize / 8 + 1];
            let base = buf.as_mut_ptr() as *mut u8;
            let detail = base as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, detail, needed, &mut needed, &mut info)
                == 0
            {
                continue;
            }
            // SAFETY: `base` 覆盖 `needed` 字节，路径字段按定义落在缓冲区内部。
            let path_off = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            let path_max = (needed as usize).saturating_sub(path_off) / 2;
            let path = wide_path(base.add(path_off) as *const u16, path_max);
            let instance = instance_id(set, &info);
            // 所有 HID 接口都要打开一次才知道 usage/产品串；扫描在后台线程，
            // 这点开销换来未知设备也能以只读卡片出现在列表里。
            let Some((vid, pid, product, manufacturer, usage_page, usage)) = read_interface(&path)
            else {
                continue;
            };
            // 分组优先级：ContainerId（Windows 帮我们算好的物理设备归属）
            // → 父设备实例（复合设备的几个接口父节点相同）→ 实例路径截断。
            // 父设备树同时用来识别内置/虚拟 HID，即使它有 ContainerId。
            let parent = parent_id(&info);
            let system = parent.as_deref().is_some_and(is_system_hid_group);
            let group = container_id(set, &info)
                .or(parent)
                .unwrap_or_else(|| group_key(&instance));
            out.push(Interface {
                path,
                instance,
                group,
                system,
                vid,
                pid,
                product,
                manufacturer,
                usage_page,
                usage,
            });
        }
        SetupDiDestroyDeviceInfoList(set);
        out
    }
}

fn identity(vid: u16, pid: u16, product: &str, manufacturer: &str) -> (String, String) {
    if let Some(p) = driver::profile(vid, pid) {
        return (p.brand.into(), p.model.into());
    }
    let brand = if manufacturer.is_empty() {
        "未知品牌".to_string()
    } else {
        manufacturer.to_string()
    };
    let model = if product.is_empty() {
        "HID 设备".to_string()
    } else {
        product.to_string()
    };
    (brand, model)
}

pub fn enumerate() -> Vec<HidDevice> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<String, Vec<Interface>> = BTreeMap::new();
    for it in collect_interfaces() {
        groups.entry(it.group.clone()).or_default().push(it);
    }

    let mut devices: Vec<HidDevice> = Vec::new();
    for (id, group) in groups {
        // 内置触摸板、虚拟 HID、ACPI 设备都挂在系统树上；它们不是外设，
        // 也不该出现在“我的设备”里。真实 USB/BTH 设备没有这些前缀。
        if is_system_hid_group(&id) || group.iter().all(|i| i.system) {
            continue;
        }
        let vid = group[0].vid;
        let pid = group[0].pid;
        // A receiver speaks one protocol; find the collection that protocol
        // lives on rather than assuming the layout.
        let profile = driver::profile(vid, pid);
        let spec = profile.and_then(|p| p.driver);
        let channel = spec.and_then(|s| {
            let (page, usage) = s.protocol.channel();
            group
                .iter()
                .find(|i| i.usage_page == page && i.usage == usage)
                .map(|i| (s, i))
        });
        let info = channel.map(|(_, i)| i).unwrap_or(&group[0]);
        let connection = classify_connection(&info.instance, info.vid, info.pid, &info.product);
        let (brand, model) = identity(vid, pid, &info.product, &info.manufacturer);
        // 已知型号的形态取表里的声明；未登记型号按产品串/用途页推断，
        // 只影响图标，不会因此启用驱动。
        let usages: Vec<(u16, u16)> = group.iter().map(|i| (i.usage_page, i.usage)).collect();
        // 识别范围只保留键鼠：耳机、手柄、摄像头等也会以 HID 枚举出来，
        // 但它们不在这个应用的能力范围内。已登记型号（含接收器）不受此限。
        let keyboard_or_mouse = usages.iter().any(|(page, usage)| {
            *page == HID_USAGE_PAGE_GENERIC_DESKTOP
                && (*usage == HID_USAGE_MOUSE || *usage == HID_USAGE_KEYBOARD)
        });
        if profile.is_none() && !keyboard_or_mouse {
            continue;
        }
        let kind = profile
            .map(|p| p.kind)
            .unwrap_or_else(|| infer_kind(&info.product, &usages));
        let has_config = channel.is_some();
        let reason = if has_config {
            None
        } else if let Some(p) = profile {
            p.reason.or(Some(NO_CHANNEL_REASON))
        } else {
            Some(UNSUPPORTED_REASON)
        };
        devices.push(HidDevice {
            id,
            path: info.path.clone(),
            vid,
            pid,
            brand,
            model,
            kind,
            connection,
            driver: channel.map(|(s, _)| s),
            limits: spec.map(|s| s.limits).unwrap_or_default(),
            reason,
        });
    }

    // Composite USB functions can land in different containers; fold the
    // read-only leftovers into the matching configurable device.
    let configs: Vec<(u16, u16, ConnectionType)> = devices
        .iter()
        .filter(|d| d.has_config())
        .map(|d| (d.vid, d.pid, d.connection))
        .collect();
    devices.retain(|d| d.has_config() || !configs.contains(&(d.vid, d.pid, d.connection)));
    // 排序键里有两个 String：用 cached_key，别在每次比较时重新分配。
    devices.sort_by_cached_key(|d| (!d.has_config(), d.model.to_ascii_lowercase(), d.id.clone()));
    devices
}

/// 一个 HID 接口插拔订阅的句柄。
///
/// 只有 Drop 有意义：注销订阅并释放回调上下文。持有它的对象（`DeviceManager`）
/// 活着期间，回调才有一块稳定可读的上下文。
pub struct DeviceWatch {
    handle: HCMNOTIFICATION,
    cx: *mut WatchCtx,
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_null() {
                let result = CM_Unregister_Notification(self.handle);
                if result != CR_SUCCESS {
                    // 注销失败意味着系统回调可能还会访问上下文。宁可泄漏这块
                    // 小内存，也不能释放后留下悬垂指针。
                    crate::log::line(
                        "ERROR",
                        &format!("注销设备插拔通知失败（{result}），回调上下文暂不释放"),
                    );
                    std::mem::forget(Box::from_raw(self.cx));
                    self.cx = std::ptr::null_mut();
                    return;
                }
            }
            if !self.cx.is_null() {
                drop(Box::from_raw(self.cx));
            }
        }
    }
}

/// 回调线程与 UI 线程之间唯一的共享状态：一个标志位加一个叫醒句柄。
struct WatchCtx {
    changed: Arc<AtomicBool>,
    ctx: egui::Context,
}

/// 系统线程上的插拔回调。**只置位，不枚举**：`enumerate` 要给每个候选接口
/// 开句柄，几十个接口就是几百毫秒，压在 PnP 通知链里会拖慢系统。
unsafe extern "system" fn on_device_change(
    _notify: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    // 只认接口到达/移除；query-remove 之类的中间动作后面还会跟着正式事件，
    // 重复置位只会多一次被合并掉的扫描。
    if matches!(
        action,
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL | CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
    ) && !context.is_null()
    {
        // SAFETY: 注册时传进去的是 `Box::into_raw` 出来的 `WatchCtx`，只有
        // `DeviceWatch::drop` 会在注销订阅之后释放它，所以回调活着时这块内存
        // 一定有效；里面只有 `Arc<AtomicBool>` 和 `egui::Context`，都是线程安全的。
        let cx = &*(context as *const WatchCtx);
        cx.changed.store(true, Ordering::Relaxed);
        cx.ctx.request_repaint();
    }
    0 // ERROR_SUCCESS
}

/// 订阅 HID 接口的插拔通知，把"有变化"记到 `changed` 上并叫醒界面。
///
/// 返回 `None` 表示订阅失败（系统策略或旧系统），调用方退回"只能手动重新
/// 扫描"的老行为；界面照常可用，不是致命错误。
pub fn watch(ctx: &egui::Context, changed: Arc<AtomicBool>) -> Option<DeviceWatch> {
    unsafe {
        let mut guid: windows_sys::core::GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let filter = CM_NOTIFY_FILTER {
            cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
            Flags: 0,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            Reserved: 0,
            u: CM_NOTIFY_FILTER_0 {
                DeviceInterface: CM_NOTIFY_FILTER_0_2 { ClassGuid: guid },
            },
        };
        let cx = Box::into_raw(Box::new(WatchCtx {
            changed,
            ctx: ctx.clone(),
        }));
        let mut handle: HCMNOTIFICATION = std::ptr::null_mut();
        let registered = CM_Register_Notification(
            &filter,
            cx as *const c_void,
            Some(on_device_change),
            &mut handle,
        );
        if registered != CR_SUCCESS {
            drop(Box::from_raw(cx));
            return None;
        }
        Some(DeviceWatch { handle, cx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::CM_NOTIFY_ACTION_DEVICEQUERYREMOVE;

    /// 订阅本身必须能建起来：它是"拔掉就消失"的唯一驱动源。订阅失败只能
    /// 手动重扫，所以把这条钉成测试，别让某次重构把它悄悄弄坏。
    #[test]
    fn device_watch_registers_and_drops() {
        let flag = Arc::new(AtomicBool::new(false));
        let subscription = watch(&egui::Context::default(), flag.clone());
        assert!(subscription.is_some(), "订阅 HID 插拔通知失败");
        assert!(!flag.load(Ordering::Relaxed), "还没插拔就不该置位");
        drop(subscription);
    }

    /// 回调函数本身必须可测：它只置位并叫醒界面，不直接枚举设备。到达/移除
    /// 都要置位，中间态和空上下文则不能误触发。
    #[test]
    fn device_change_callback_sets_the_dirty_flag() {
        for action in [
            CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
            CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL,
        ] {
            let flag = Arc::new(AtomicBool::new(false));
            let cx = Box::into_raw(Box::new(WatchCtx {
                changed: flag.clone(),
                ctx: egui::Context::default(),
            }));
            let result = unsafe {
                on_device_change(
                    std::ptr::null_mut(),
                    cx as *const c_void,
                    action,
                    std::ptr::null(),
                    0,
                )
            };
            assert_eq!(result, 0);
            assert!(flag.load(Ordering::Relaxed), "接口到达/移除必须置位");
            unsafe { drop(Box::from_raw(cx)) };
        }

        // query-remove 后面还会跟着正式事件，不能在这里发起额外扫描。
        let flag = Arc::new(AtomicBool::new(false));
        let cx = Box::into_raw(Box::new(WatchCtx {
            changed: flag.clone(),
            ctx: egui::Context::default(),
        }));
        let result = unsafe {
            on_device_change(
                std::ptr::null_mut(),
                cx as *const c_void,
                CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(result, 0);
        assert!(!flag.load(Ordering::Relaxed));
        unsafe { drop(Box::from_raw(cx)) };
    }

    #[test]
    fn classifies_bluetooth_first() {
        assert_eq!(
            classify_connection(
                "BTHENUM\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0001004c",
                0x24AE,
                0x1214,
                ""
            ),
            ConnectionType::Bluetooth
        );
        assert_eq!(
            classify_connection(
                "BTHLEDEVICE\\{00001812-0000-1000-8000-00805f9b34fb}",
                0x1234,
                0x5678,
                ""
            ),
            ConnectionType::Bluetooth
        );
    }

    #[test]
    fn classifies_known_receiver() {
        assert_eq!(
            classify_connection("USB\\VID_24AE&PID_1214\\5&2A1B3C4D&0&1", 0x24AE, 0x1214, ""),
            ConnectionType::Wireless24G
        );
        assert_eq!(
            classify_connection("USB\\VID_3554&PID_FA09\\5&6D6F5B5&0&1", 0x3554, 0xFA09, ""),
            ConnectionType::Wireless24G
        );
    }

    /// 同一个鼠标插线后是另一个 PID，连接方式必须跟着型号表走，不能因为
    /// "在型号表里"就一律当成 2.4G——那样有线时电量会按无线那套显示。
    #[test]
    fn classifies_wired_variants_of_known_models() {
        assert_eq!(
            classify_connection(
                "USB\\VID_24AE&PID_4414&MI_00\\6&26AC346C&0&0000",
                0x24AE,
                0x4414,
                ""
            ),
            ConnectionType::Wired
        );
        assert_eq!(
            classify_connection(
                "USB\\VID_258A&PID_010C&MI_00\\6&32356D05&0&0000",
                0x258A,
                0x010C,
                ""
            ),
            ConnectionType::Wired
        );
    }

    #[test]
    fn identifies_keyboard_receiver() {
        let (brand, model) = identity(0x3554, 0xFA09, "2.4G Wireless Receiver", "Compx");
        assert_eq!((brand.as_str(), model.as_str()), ("AULA", "F87"));
    }

    /// 未登记型号不能仅凭 USB 实例路径就当有线：无线接收器也枚举成 USB。
    #[test]
    fn unknown_devices_are_not_assumed_wired() {
        assert_eq!(
            classify_connection(
                "USB\\VID_046D&PID_C52B\\6&2B0E7F1&0&1",
                0x046D,
                0xC52B,
                "USB Optical Mouse"
            ),
            ConnectionType::Unknown
        );
        assert_eq!(
            classify_connection(
                "USB\\VID_046D&PID_C52B\\6&2B0E7F1&0&1",
                0x046D,
                0xC52B,
                "USB Receiver"
            ),
            ConnectionType::Wireless24G
        );
        assert_eq!(
            classify_connection(
                "USB\\VID_046D&PID_C52B\\6&2B0E7F1&0&1",
                0x046D,
                0xC52B,
                "Wireless Mouse"
            ),
            ConnectionType::Wireless24G
        );
        assert_eq!(
            classify_connection(
                "USB\\VID_046D&PID_C52B\\6&2B0E7F1&0&1",
                0x046D,
                0xC52B,
                "无线接收器"
            ),
            ConnectionType::Wireless24G
        );
        assert_eq!(
            classify_connection("HID\\VID_046D&PID_C52B", 0x046D, 0xC52B, ""),
            ConnectionType::Unknown
        );
    }

    #[test]
    fn unknown_devices_keep_their_reported_identity() {
        let (brand, model) = identity(0x1234, 0x5678, "Some Mouse", "Acme");
        assert_eq!((brand.as_str(), model.as_str()), ("Acme", "Some Mouse"));
        let (brand, model) = identity(0x1234, 0x5678, "", "");
        assert_eq!((brand.as_str(), model.as_str()), ("未知品牌", "HID 设备"));
    }

    #[test]
    fn infers_unknown_device_kind() {
        assert_eq!(infer_kind("USB Receiver", &[]), DeviceType::Receiver);
        assert_eq!(infer_kind("ACME Gaming Mouse", &[]), DeviceType::Mouse);
        assert_eq!(infer_kind("ACME Keyboard", &[]), DeviceType::Keyboard);
        assert_eq!(infer_kind("无线接收器", &[]), DeviceType::Receiver);
        assert_eq!(
            infer_kind("USB Composite Device", &[(0x01, 0x06)]),
            DeviceType::Keyboard
        );
        assert_eq!(
            infer_kind("USB Composite Device", &[(0x01, 0x02)]),
            DeviceType::Mouse
        );
        // 厂商页 + 泛化名字更像接收器/复合设备，不能仅凭键盘集合猜形态。
        assert_eq!(
            infer_kind("Gaming Device", &[(0x01, 0x06), (0xFF00, 0x0E)]),
            DeviceType::Unknown
        );
        assert_eq!(infer_kind("USB Device", &[]), DeviceType::Unknown);
    }

    #[test]
    fn recognises_the_new_receivers() {
        let (brand, model) = identity(0x046D, 0xC53F, "USB Receiver", "Logitech");
        assert_eq!(
            (brand.as_str(), model.as_str()),
            ("Logitech", "Lightspeed 接收器")
        );
        let (brand, model) = identity(0x25A7, 0xFA70, "2.4G Wireless Receiver", "Compx");
        assert_eq!((brand.as_str(), model.as_str()), ("RK", "RK98"));
    }

    #[test]
    fn picks_the_collection_each_protocol_lives_on() {
        let kind = |vid, pid| {
            driver::profile(vid, pid)
                .and_then(|p| p.driver)
                .map(|s| s.protocol.kind())
        };
        assert_eq!(kind(0x24AE, 0x1214), Some(driver::Kind::Rapoo));
        assert_eq!(kind(0x046D, 0xC53F), Some(driver::Kind::Hidpp));
        // RK98 的接收器没有可写通道，不该被当成可配置设备。
        assert_eq!(kind(0x25A7, 0xFA70), None);
        assert_eq!(driver::Kind::Rapoo.channel(), (0xFF00, 0x000E));
        assert_eq!(driver::Kind::Hidpp.channel(), (0xFF00, 0x0002));
    }

    /// ContainerID 是 REG_SZ GUID；解析后要稳定、大小写归一，空值不产生键。
    #[test]
    fn parses_container_id_from_reg_sz_guid() {
        let text = "{5D951AEF-E91B-5CB3-BAD7-BCD9762A0AAD}";
        let mut bytes = Vec::new();
        for ch in text.encode_utf16() {
            bytes.extend_from_slice(&ch.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        assert_eq!(
            container_key(&bytes).as_deref(),
            Some("c:{5d951aef-e91b-5cb3-bad7-bcd9762a0aad}")
        );
        assert_eq!(container_key(&[0, 0]), None);
        let null = "{00000000-0000-0000-ffff-ffffffffffff}";
        let bytes: Vec<u8> = null
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain([0, 0])
            .collect();
        assert_eq!(container_key(&bytes), None);
    }

    #[test]
    fn skips_system_virtual_hid_groups() {
        assert!(is_system_hid_group("p:ROOT\\SYNHIDMINI\\0000"));
        assert!(is_system_hid_group(
            "p:VHF\\HID_DEVICE_SYSTEM_VHF\\1&1A590E2C&0&{BA465EFF}"
        ));
        assert!(is_system_hid_group("p:ACPI\\PNPC000\\1"));
        assert!(is_system_hid_group("p:SWD\\HID\\1"));
        assert!(!is_system_hid_group(
            "c:{5D951AEF-E91B-5CB3-BAD7-BCD9762A0AAD}"
        ));
        assert!(!is_system_hid_group(
            "p:USB\\VID_24AE&PID_1214\\5&2A1B3C4D&0&1"
        ));
        assert!(!is_system_hid_group(
            "p:HID\\VID_258A&PID_010C&MI_00\\7&3A75499D&0&0000"
        ));
        assert!(!is_system_hid_group(
            "p:BTHENUM\\{00001124-0000-1000-8000-00805f9b34fb}"
        ));
    }

    #[test]
    fn strips_collection_suffix_from_instance() {
        assert_eq!(
            group_key("USB\\VID_24AE&PID_1214\\5&2A1B3C4D&0&1&Col02"),
            "usb\\vid_24ae&pid_1214\\5&2a1b3c4d&0&1"
        );
    }

    /// 复合设备的几个接口（MI_00 键盘、MI_01 厂商通道）实例路径不同，但都
    /// 要落进同一组——不然同一把键盘会在首页出两张卡片。
    #[test]
    fn strips_interface_suffix_from_instance() {
        let mi_00 = group_key("USB\\VID_3554&PID_FA09&MI_00\\7&169E0BDB&0&0000");
        let mi_01 = group_key("USB\\VID_3554&PID_FA09&MI_01\\7&3A75499D&0&0000");
        assert_eq!(mi_00, "usb\\vid_3554&pid_fa09");
        assert_eq!(mi_01, mi_00);
    }
}

#[cfg(test)]
mod dump {
    #[test]
    #[ignore]
    fn dump_devices() {
        for d in super::enumerate() {
            println!("{:?}", d);
        }
    }
}
