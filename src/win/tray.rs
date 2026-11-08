use tray_icon::menu::accelerator::Accelerator;
use tray_icon::menu::{ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub struct TrayState {
    _tray: TrayIcon,
    menu: Menu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Settings,
    Quit,
}

pub fn create() -> Result<TrayState, String> {
    let menu = Menu::new();
    let show = MenuItem::with_id("tray.show", "显示 lstr", true, None::<Accelerator>);
    let settings = MenuItem::with_id("tray.settings", "设置", true, None::<Accelerator>);
    let quit = MenuItem::with_id("tray.quit", "退出", true, None::<Accelerator>);
    menu.append(&show)
        .and_then(|_| menu.append(&settings))
        .and_then(|_| menu.append(&PredefinedMenuItem::separator()))
        .and_then(|_| menu.append(&quit))
        .map_err(|e| format!("托盘菜单创建失败: {e}"))?;

    let icon = build_icon().ok_or_else(|| "托盘图标创建失败".to_string())?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("lstr")
        .with_icon(icon)
        .build()
        .map_err(|e| format!("托盘创建失败: {e}"))?;
    Ok(TrayState { _tray: tray, menu })
}

impl TrayState {
    /// 处理托盘事件；右键时在 owner 窗口上弹出原生菜单。
    pub fn poll(&self, owner_hwnd: isize) -> Option<TrayAction> {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if button_state == MouseButtonState::Up {
                    match button {
                        MouseButton::Left => return Some(TrayAction::Show),
                        MouseButton::Right => {
                            unsafe {
                                self.menu.show_context_menu_for_hwnd(owner_hwnd, None);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                "tray.show" => return Some(TrayAction::Show),
                "tray.settings" => return Some(TrayAction::Settings),
                "tray.quit" => return Some(TrayAction::Quit),
                _ => {}
            }
        }
        None
    }
}

fn build_icon() -> Option<Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = SIZE as f32 / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = (x as f32 + 0.5 - center).abs();
            let dy = (y as f32 + 0.5 - center).abs();
            let inside = dx <= 12.0 && dy <= 12.0;
            if !inside {
                continue;
            }
            // 圆角
            let corner = (dx > 9.0 && dy > 9.0) && {
                let cx = dx - 9.0;
                let cy = dy - 9.0;
                cx * cx + cy * cy > 9.0
            };
            if corner {
                continue;
            }
            let i = ((y * SIZE + x) * 4) as usize;
            let needle = dx <= 1.5 && dy <= 7.0;
            if needle {
                rgba[i] = 20;
                rgba[i + 1] = 24;
                rgba[i + 2] = 30;
                rgba[i + 3] = 255;
            } else {
                rgba[i] = 224;
                rgba[i + 1] = 160;
                rgba[i + 2] = 64;
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).ok()
}
