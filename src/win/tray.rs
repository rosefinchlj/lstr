use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

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
    /// 退出并连带停掉提权索引服务（常驻模式下默认只退出 GUI）。
    QuitAndStopWorker,
    /// 右键:由 `poll` 在 owner 窗口上弹原生菜单(不返回给 App)。
    Menu,
}

/// 事件队列:托盘/菜单事件在 `install_wake` 注册的全局 handler 里入队并
/// `request_repaint`,主循环 `poll` 消费 —— 隐藏态不再靠周期重绘轮询托盘。
fn pending() -> &'static Mutex<VecDeque<TrayAction>> {
    static PENDING: OnceLock<Mutex<VecDeque<TrayAction>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn push(action: TrayAction) {
    if let Ok(mut queue) = pending().lock() {
        queue.push_back(action);
    }
}

/// 安装托盘事件唤醒器:托盘点击/菜单项一来就 `request_repaint`(窗口隐藏时
/// 也能立即响应),同时把事件转成 [`TrayAction`] 入队。
///
/// 注意:`tray-icon` 的 `set_event_handler` 与 `receiver()` 互斥 —— 装了
/// handler 之后接收通道不再有事件,所以消费路径统一走本队列。
pub fn install_wake(ctx: &eframe::egui::Context) {
    let ctx_tray = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button,
            button_state,
            ..
        } = &event
        {
            if *button_state == MouseButtonState::Up {
                match button {
                    MouseButton::Left => push(TrayAction::Show),
                    MouseButton::Right => push(TrayAction::Menu),
                    _ => {}
                }
            }
        }
        ctx_tray.request_repaint();
    }));
    let ctx_menu = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id.0.as_str() {
            "tray.show" => Some(TrayAction::Show),
            "tray.settings" => Some(TrayAction::Settings),
            "tray.quit" => Some(TrayAction::Quit),
            "tray.quit_stop_worker" => Some(TrayAction::QuitAndStopWorker),
            _ => None,
        };
        if let Some(action) = action {
            push(action);
        }
        ctx_menu.request_repaint();
    }));
}

pub fn create() -> Result<TrayState, String> {
    let menu = Menu::new();
    let show = MenuItem::with_id("tray.show", "显示 lstr", true, None::<Accelerator>);
    let settings = MenuItem::with_id("tray.settings", "设置", true, None::<Accelerator>);
    let quit = MenuItem::with_id("tray.quit", "退出（保留索引服务）", true, None::<Accelerator>);
    let quit_stop = MenuItem::with_id(
        "tray.quit_stop_worker",
        "退出并停止索引服务",
        true,
        None::<Accelerator>,
    );
    menu.append(&show)
        .and_then(|_| menu.append(&settings))
        .and_then(|_| menu.append(&PredefinedMenuItem::separator()))
        .and_then(|_| menu.append(&quit))
        .and_then(|_| menu.append(&quit_stop))
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
    /// 处理已入队的托盘事件；右键时在 owner 窗口上弹出原生菜单。
    pub fn poll(&self, owner_hwnd: isize) -> Option<TrayAction> {
        loop {
            let action = pending().lock().ok().and_then(|mut queue| queue.pop_front());
            let Some(action) = action else { break };
            match action {
                TrayAction::Menu => unsafe {
                    self.menu.show_context_menu_for_hwnd(owner_hwnd, None);
                },
                other => return Some(other),
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
