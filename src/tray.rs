use std::sync::{Arc, Mutex};

use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    ShowDesktop,
    EnterMini,
    ToggleTheme,
    ToggleAutostart,
    Rescan,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
    theme_item: CheckMenuItem,
    autostart_item: CheckMenuItem,
    mini_item: CheckMenuItem,
}

impl Tray {
    pub fn set_theme_dark(&self, dark: bool) {
        self.theme_item.set_checked(dark);
    }

    pub fn set_autostart(&self, enabled: bool) {
        self.autostart_item.set_checked(enabled);
    }

    pub fn set_mini(&self, mini: bool) {
        self.mini_item.set_checked(mini);
    }
}

pub fn install(
    ctx: &egui::Context,
    dark: bool,
    autostart: bool,
    mini: bool,
    inbox: Arc<Mutex<Vec<TrayAction>>>,
) -> Result<Tray, String> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "打开主界面", true, None);
    let mini_item = CheckMenuItem::with_id("tray", "桌面浮窗", true, mini, None);
    let theme_item = CheckMenuItem::with_id("theme", "深色主题", true, dark, None);
    let autostart_item = CheckMenuItem::with_id("auto", "开机自启动", true, autostart, None);
    let rescan = MenuItem::with_id("rescan", "重新扫描设备", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    menu.append_items(&[
        &show,
        &mini_item,
        &PredefinedMenuItem::separator(),
        &theme_item,
        &autostart_item,
        &rescan,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .map_err(|e| format!("创建托盘菜单失败：{e}"))?;

    let icon = Icon::from_rgba(theme::icon_rgba(32), 32, 32)
        .map_err(|e| format!("创建托盘图标失败：{e:?}"))?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("PeriphHub")
        .with_icon(icon)
        .build()
        .map_err(|e| format!("创建托盘失败：{e}"))?;

    let menu_inbox = inbox.clone();
    let menu_ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id().as_ref() {
            "show" => Some(TrayAction::ShowDesktop),
            "tray" => Some(TrayAction::EnterMini),
            "theme" => Some(TrayAction::ToggleTheme),
            "auto" => Some(TrayAction::ToggleAutostart),
            "rescan" => Some(TrayAction::Rescan),
            "quit" => Some(TrayAction::Quit),
            _ => None,
        };
        if let Some(action) = action {
            // 回调线程 panic 后锁会中毒，不能让它把 UI 线程也带崩。
            menu_inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(action);
            menu_ctx.request_repaint();
        }
    }));

    let tray_inbox = inbox;
    let tray_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            tray_inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(TrayAction::ShowDesktop);
            tray_ctx.request_repaint();
        }
    }));

    Ok(Tray {
        _icon: tray,
        theme_item,
        autostart_item,
        mini_item,
    })
}
