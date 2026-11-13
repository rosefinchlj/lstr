use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use std::num::NonZeroIsize;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, RedrawWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, SetActiveWindow, SetFocus, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    SwitchToThisWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

pub fn hwnd_of_frame(frame: &eframe::Frame) -> Option<isize> {
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

struct HwndHandle(isize);

impl HasWindowHandle for HwndHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let hwnd = NonZeroIsize::new(self.0).ok_or(HandleError::Unavailable)?;
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
        unsafe { Ok(WindowHandle::borrow_raw(raw)) }
    }
}

fn hwnd(value: isize) -> HWND {
    HWND(value as *mut _)
}

pub fn apply_effects(hwnd_value: isize, light: bool, opacity: f32) -> bool {
    let target = hwnd(hwnd_value);
    // tint 的 alpha 压低，让亚克力模糊透出来；不透明度滑条只做微调。
    let tint = if light {
        (
            245,
            246,
            247,
            (opacity.clamp(0.6, 1.0) * 255.0 * 0.5) as u8,
        )
    } else {
        (
            20,
            24,
            30,
            (opacity.clamp(0.6, 1.0) * 255.0 * 0.35) as u8,
        )
    };
    let acrylic = match window_vibrancy::apply_acrylic(HwndHandle(hwnd_value), Some(tint)) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("lstr: acrylic 不可用: {err}");
            false
        }
    };
    set_dark_mode(target, !light);
    set_round_corners(target);
    let _ = unsafe {
        SetWindowPos(
            target,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            // 缺 SWP_NOMOVE 会把窗口挪到 (0,0)：重应用亚克力/主题时位置必须保持不变。
            SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE,
        )
    };
    acrylic
}

fn set_dark_mode(target: HWND, dark: bool) {
    let value: i32 = if dark { 1 } else { 0 };
    unsafe {
        let _ = DwmSetWindowAttribute(
            target,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &value as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

fn set_round_corners(target: HWND) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            target,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &DWMWCP_ROUND as *const _ as *const _,
            std::mem::size_of_val(&DWMWCP_ROUND) as u32,
        );
    }
}

pub fn position_center(hwnd_value: isize) {
    let target = hwnd(hwnd_value);
    unsafe {
        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_err() {
            return;
        }
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(target, &mut rect).is_err() {
            return;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let work_width = info.rcWork.right - info.rcWork.left;
        let work_height = info.rcWork.bottom - info.rcWork.top;
        let x = info.rcWork.left + (work_width - width) / 2;
        let y = info.rcWork.top + (work_height - height) / 2;
        let _ = SetWindowPos(
            target,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn position_near_dialog(hwnd_value: isize, dialog_hwnd: isize) -> bool {
    if dialog_hwnd == 0 {
        return false;
    }
    let target = hwnd(hwnd_value);
    let dialog = hwnd(dialog_hwnd);
    unsafe {
        let mut dialog_rect = RECT::default();
        if GetWindowRect(dialog, &mut dialog_rect).is_err() {
            return false;
        }
        let mut window_rect = RECT::default();
        if GetWindowRect(target, &mut window_rect).is_err() {
            return false;
        }
        let window_w = window_rect.right - window_rect.left;
        let window_h = window_rect.bottom - window_rect.top;
        let center = POINT {
            x: (dialog_rect.left + dialog_rect.right) / 2,
            y: (dialog_rect.top + dialog_rect.bottom) / 2,
        };
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return false;
        }
        let work = info.rcWork;
        let max_x = (work.right - window_w - 8).max(work.left + 8);
        // 左对齐停靠：与对话框左缘对齐（缩进 8px）。此前是水平居中于对话框，
        // 而对话框通常在屏幕中央——看起来就像「弹了一下又移到居中」。
        let x = (dialog_rect.left + 8).clamp(work.left + 8, max_x);
        // P0-3：贴边自适应。对话框靠近工作区底部时，把快速切换窗口
        // 翻到对话框上方（Listary V7 的做法），否则仍贴在下方。
        let mut y = if dialog_rect.top + 56 + window_h <= work.bottom - 8 {
            dialog_rect.top + 56
        } else if dialog_rect.top - window_h - 8 >= work.top + 8 {
            dialog_rect.top - window_h - 8
        } else {
            dialog_rect.bottom - window_h - 48
        };
        if y + window_h > work.bottom - 8 {
            y = dialog_rect.bottom - window_h - 48;
        }
        let max_y = (work.bottom - window_h - 8).max(work.top + 8);
        let y = y.clamp(work.top + 8, max_y);
        let _ = SetWindowPos(
            target,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        true
    }
}

pub fn show(hwnd_value: isize) {
    let target = hwnd(hwnd_value);
    unsafe {
        let _ = ShowWindow(target, SW_SHOW);
    }
    force_foreground(hwnd_value);
    unsafe {
        let _ = RedrawWindow(
            Some(target),
            None,
            None,
            RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
        );
    }
}

pub fn hide(hwnd_value: isize) {
    // 移到屏幕外而不是 ShowWindow(SW_HIDE)：隐藏窗口会让 eframe 停止 update，
    // 热键线程的 request_repaint 无法再唤醒它。保持在屏幕外可让事件循环继续跑。
    let target = hwnd(hwnd_value);
    unsafe {
        let _ = SetWindowPos(
            target,
            Some(HWND_TOPMOST),
            -30000,
            -30000,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 启动时把窗口从任务栏 / Alt+Tab 摘掉。
///
/// 常驻启动器的「隐藏」只是把窗口移到屏幕外（见 [`hide`]），窗口仍是 visible
/// 状态；若保留任务栏按钮，用户会看到一个点不动的图标（点击只是激活一个
/// -30000 位置的窗口）。WS_EX_TOOLWINDOW 是 Windows 官方用来排除任务栏与
/// Alt+Tab 的样式；配合 SWP_FRAMECHANGED 让样式立即生效。
pub fn detach_from_taskbar(hwnd_value: isize) {
    if hwnd_value == 0 {
        return;
    }
    let target = hwnd(hwnd_value);
    unsafe {
        let style = GetWindowLongPtrW(target, GWL_EXSTYLE);
        let app_window = style & (WS_EX_APPWINDOW.0 as isize) != 0;
        let tool_window = style & (WS_EX_TOOLWINDOW.0 as isize) != 0;
        if !app_window && tool_window {
            // 已经是目标状态：热路径（每帧调用）零改动。
            return;
        }
        let next = (style & !(WS_EX_APPWINDOW.0 as isize)) | WS_EX_TOOLWINDOW.0 as isize;
        SetWindowLongPtrW(target, GWL_EXSTYLE, next);
        let _ = SetWindowPos(
            target,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        delete_taskbar_tab(hwnd_value);
    }
}

/// 摘掉任务栏按钮（winit 创建窗口时也做过一次，但在标志重同步后按钮会
/// 像幽灵一样回来；配合 WS_EX_TOOLWINDOW 才彻底不再出现）。
fn delete_taskbar_tab(hwnd_value: isize) {
    unsafe {
        // UI 线程通常已经初始化 COM；重复初始化返回 S_FALSE，可忽略。
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(list): Result<ITaskbarList, _> =
            CoCreateInstance(&TaskbarList, None, CLSCTX_ALL)
        else {
            return;
        };
        if list.HrInit().is_ok() {
            let _ = list.DeleteTab(hwnd(hwnd_value));
        }
    }
}

pub fn force_foreground(hwnd_value: isize) {
    if hwnd_value == 0 {
        return;
    }
    let target = hwnd(hwnd_value);
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0 == target.0 {
            return;
        }
        let current_thread = GetCurrentThreadId();
        let mut fg_pid = 0u32;
        let fg_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, Some(&mut fg_pid))
        };
        let attached = fg_thread != 0 && fg_thread != current_thread;
        if attached {
            let _ = AttachThreadInput(current_thread, fg_thread, true);
        }
        let _ = ShowWindow(target, SW_SHOW);
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
        let _ = SetActiveWindow(target);
        let _ = SetFocus(Some(target));
        if attached {
            let _ = AttachThreadInput(current_thread, fg_thread, false);
        }
        // 前台是提权窗口（如「管理员: 终端」）时，UIPI 会静默拒绝
        // SetForegroundWindow，普通用户态进程抢不到前台。合成一次裸 Alt
        // 让系统把本进程视为「最后输入来源」，再抢一次；仍失败则走
        // SwitchToThisWindow（shell 的切换入口，不受前台锁限制）。
        if GetForegroundWindow().0 != target.0 {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            let _ = SetForegroundWindow(target);
            let _ = SetActiveWindow(target);
            let _ = SetFocus(Some(target));
        }
        if GetForegroundWindow().0 != target.0 {
            SwitchToThisWindow(target, true);
        }
    }
}
