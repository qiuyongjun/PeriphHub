use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::theme::ThemeMode;

/// Which shell the single window is wearing.
///
/// `Mini` is the default: the point of the app is to keep device status in the
/// corner of the eye, and the workbench is one click away from the widget. A
/// first launch that opens a full window over whatever the user was doing gets
/// in the way.
///
/// 这是**运行时状态，不写进 config.json**：启动固定从浮窗开始（见
/// `Panel::new`），切换发生在运行期，落盘只会把"上次退出时在哪"存下来，
/// 而那正是启动逻辑刻意不沿用的东西。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunMode {
    Desktop,
    #[default]
    Mini,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: ThemeMode,
    /// 当前处于哪种壳。`skip`：这是运行时状态，启动逻辑固定从浮窗开始，
    /// 存进 config.json 只会产生一个永远被覆盖的死值。
    #[serde(skip)]
    pub mode: RunMode,
    /// Outer top-left of the mini widget in screen coordinates.
    pub mini_pos: Option<[f32; 2]>,
    /// Keep the widget above other windows.
    pub mini_pinned: bool,
    /// Last geometry of the main window, so leaving the widget restores what
    /// the user actually had instead of a hard-coded default.
    pub main_pos: Option<[f32; 2]>,
    pub main_size: Option<[f32; 2]>,
    /// 每台鼠标首次读取到的各档 DPI，作为“恢复初始值”的基线。
    /// 键是设备 id + 型号：同一个接收器换鼠标后型号会变，不能共用旧值。
    pub dpi_baseline: BTreeMap<String, Vec<u16>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            mode: RunMode::default(),
            mini_pos: None,
            mini_pinned: true,
            main_pos: None,
            main_size: None,
            dpi_baseline: BTreeMap::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|base| PathBuf::from(base).join("PeriphHub").join("config.json"))
}

pub fn load() -> Config {
    let Some(path) = path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            crate::log::line("ERROR", &format!("config parse error: {e}"));
            // 先备份损坏文件再回退默认值，否则下次 save 会把用户原始配置
            // 直接覆盖掉，连排查现场都没有。
            let bak = path.with_extension("json.bak");
            let _ = std::fs::remove_file(&bak);
            let _ = std::fs::rename(&path, bak);
            Config::default()
        }
    }
}

pub fn save(cfg: &Config) -> Result<(), String> {
    let path = path().ok_or_else(|| "无法定位 APPDATA 目录".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化配置失败：{e}"))?;
    // 直接写目标文件意味着"截断成功、写入失败"会留下半个 JSON；而 load() 遇到
    // 损坏文件会重置为默认值，用户所有设置就没了。先写临时文件再原子替换。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入配置失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("保存配置失败：{e}"))
}
