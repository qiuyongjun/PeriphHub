use std::ffi::c_void;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDeviceRegistryPropertyW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SPDRP_BASE_CONTAINERID, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SP_DEVINFO_DATA,
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

pub fn classify_connection(instance: &str, vid: u16, pid: u16) -> ConnectionType {
    let lower = instance.to_ascii_lowercase();
    if lower.starts_with("bth") {
        ConnectionType::Bluetooth
    } else if driver::profile(vid, pid).is_some() {
        ConnectionType::Wireless24G
    } else if lower.starts_with("usb") {
        ConnectionType::Wired
    } else {
        ConnectionType::Unknown
    }
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
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
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
            let Some((vid, pid, product, manufacturer, usage_page, usage)) = read_interface(&path)
            else {
                continue;
            };
            let instance = instance_id(set, &info);
            // 分组优先级：ContainerId（Windows 帮我们算好的物理设备归属）
            // → 父设备实例（复合设备的几个接口父节点相同）→ 实例路径截断。
            let group = container_id(set, &info)
                .or_else(|| parent_id(&info))
                .unwrap_or_else(|| group_key(&instance));
            out.push(Interface {
                path,
                instance,
                group,
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
        let vid = group[0].vid;
        let pid = group[0].pid;
        // A receiver speaks one protocol; find the collection that protocol
        // lives on rather than assuming the layout.
        let profile = driver::profile(vid, pid);
        let spec = profile.and_then(|p| p.driver);
        let channel = spec.and_then(|s| {
            let (page, usage) = s.kind.channel();
            group
                .iter()
                .find(|i| i.usage_page == page && i.usage == usage)
                .map(|i| (s, i))
        });
        let info = channel.map(|(_, i)| i).unwrap_or(&group[0]);
        let connection = classify_connection(&info.instance, info.vid, info.pid);
        let (brand, model) = identity(vid, pid, &info.product, &info.manufacturer);
        // 能出现在列表里的设备都有 profile（见文件末尾的 retain），形态取表里的
        // 声明；`Unknown` 只是查不到 profile 时的兜底。
        let kind = profile.map(|p| p.kind).unwrap_or(DeviceType::Unknown);
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
            reason: profile.and_then(|p| p.reason),
        });
    }

    devices.retain(|d| d.has_config() || driver::profile(d.vid, d.pid).is_some());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_bluetooth_first() {
        assert_eq!(
            classify_connection(
                "BTHENUM\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0001004c",
                0x24AE,
                0x1214
            ),
            ConnectionType::Bluetooth
        );
        assert_eq!(
            classify_connection(
                "BTHLEDEVICE\\{00001812-0000-1000-8000-00805f9b34fb}",
                0x1234,
                0x5678
            ),
            ConnectionType::Bluetooth
        );
    }

    #[test]
    fn classifies_known_receiver() {
        assert_eq!(
            classify_connection("USB\\VID_24AE&PID_1214\\5&2A1B3C4D&0&1", 0x24AE, 0x1214),
            ConnectionType::Wireless24G
        );
        assert_eq!(
            classify_connection("USB\\VID_3554&PID_FA09\\5&6D6F5B5&0&1", 0x3554, 0xFA09),
            ConnectionType::Wireless24G
        );
    }

    #[test]
    fn identifies_keyboard_receiver() {
        let (brand, model) = identity(0x3554, 0xFA09, "2.4G Wireless Receiver", "Compx");
        assert_eq!((brand.as_str(), model.as_str()), ("AULA", "F87"));
    }

    #[test]
    fn classifies_wired_and_unknown() {
        assert_eq!(
            classify_connection("USB\\VID_046D&PID_C52B\\6&2B0E7F1&0&1", 0x046D, 0xC52B),
            ConnectionType::Wired
        );
        assert_eq!(
            classify_connection("HID\\VID_046D&PID_C52B", 0x046D, 0xC52B),
            ConnectionType::Unknown
        );
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
                .map(|s| s.kind)
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
