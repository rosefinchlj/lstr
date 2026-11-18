use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use std::num::NonZeroIsize;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, RedrawWindow, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, SetActiveWindow, SetFocus, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsWindow, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, SwitchToThisWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
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
        // C13：居中后仍要 clamp 到工作区。窗口比工作区还大时（小屏/高缩放下
        // 窗口被撑到 1000pt），居中公式会得到负偏移，导致标题栏跑到屏幕外。
        let max_x = (info.rcWork.right - width).max(info.rcWork.left);
        let max_y = (info.rcWork.bottom - height).max(info.rcWork.top);
        let x = (info.rcWork.left + (work_width - width) / 2).clamp(info.rcWork.left, max_x);
        let y = (info.rcWork.top + (work_height - height) / 2).clamp(info.rcWork.top, max_y);
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

/// 把窗口夹回当前显示器工作区：只在越界时移动，**不重新居中**。
///
/// 用在「窗口变大之后」的场景（打开设置面板）：用户在哪儿摆的就留在哪儿，
/// 只是不许被顶到屏幕外。已经是完整可见的窗口位置不会被改动。
pub fn clamp_into_work_area(hwnd_value: isize) {
    if hwnd_value == 0 {
        return;
    }
    let target = hwnd(hwnd_value);
    unsafe {
        let Some([work_l, work_t, work_r, work_b]) = work_area_for(hwnd_value) else {
            return;
        };
        let mut rect = RECT::default();
        if GetWindowRect(target, &mut rect).is_err() {
            return;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        // 窗口比工作区还大时贴住左上角，右下角自然溢出（与 position_center 一致）。
        let max_x = (work_r - width).max(work_l);
        let max_y = (work_b - height).max(work_t);
        let x = rect.left.clamp(work_l, max_x);
        let y = rect.top.clamp(work_t, max_y);
        if x == rect.left && y == rect.top {
            return;
        }
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

/// 窗口所在显示器的工作区 `(left, top, right, bottom)`（已扣掉任务栏）。
///
/// 文件对话框停靠条要夹在工作区内：对话框贴边/最大化时，条子不能跑到屏幕外。
pub fn work_area_for(hwnd_value: isize) -> Option<[i32; 4]> {
    if hwnd_value == 0 {
        return None;
    }
    unsafe {
        let monitor = MonitorFromWindow(hwnd(hwnd_value), MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        Some([
            info.rcWork.left,
            info.rcWork.top,
            info.rcWork.right,
            info.rcWork.bottom,
        ])
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

/// 窗口「可见边框」的物理像素矩形 `(left, top, right, bottom)`。
///
/// 贴角浮层必须贴着 Explorer 真正看得见的边缘：Win10+ 的 `GetWindowRect`
/// 含一圈不可见的阴影边框（最大化时左右各多出 7~8px），直接拿它当锚点
/// 会在右侧/底边留一条缝，看起来像没贴住。优先用 DWM 扩展边框，取不到
/// 再退回 `GetWindowRect`。
pub fn frame_bounds(hwnd_value: isize) -> Option<[i32; 4]> {
    if hwnd_value == 0 {
        return None;
    }
    let target = hwnd(hwnd_value);
    unsafe {
        let mut rect = RECT::default();
        let dwm = DwmGetWindowAttribute(
            target,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        if dwm.is_err() || rect.right <= rect.left || rect.bottom <= rect.top {
            rect = RECT::default();
            if GetWindowRect(target, &mut rect).is_err() {
                return None;
            }
        }
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return None;
        }
        Some([rect.left, rect.top, rect.right, rect.bottom])
    }
}

/// 一次 `SetWindowPos` 同时改位置与尺寸（贴角模式 B1）。
///
/// 不走 `ViewportCommand::InnerSize`：那条路径每次 resize 都要等 winit
/// 下一帧同步，连续拖动 Explorer 时高度变化会追赶不及、肉眼可见抖动。
/// 位置+尺寸合一次系统调用，浮层和资源管理器基本同帧移动。
pub fn set_rect(hwnd_value: isize, x: i32, y: i32, width: i32, height: i32) {
    if hwnd_value == 0 {
        return;
    }
    let _ = unsafe {
        SetWindowPos(
            hwnd(hwnd_value),
            Some(HWND_TOPMOST),
            x,
            y,
            width.max(1),
            height.max(1),
            SWP_NOACTIVATE,
        )
    };
}

/// 锚点窗口是否还活着（Explorer 关窗/换窗口时贴角浮层要跟着收）。
pub fn is_window(hwnd_value: isize) -> bool {
    if hwnd_value == 0 {
        return false;
    }
    unsafe { IsWindow(Some(hwnd(hwnd_value))).as_bool() }
}

/// 把前台交还给锚点窗口（贴角浮层收起后回到 Explorer）。
///
/// 浮层是「抢焦点」模式（F1，中文输入法必须靠窗口焦点），收起时如果
/// 只做 `window::hide`，前台会落到别的窗口（甚至桌面）；用户在资源管理器
/// 里刚打完字、按 Esc 关掉浮层后应当继续在 Explorer 里操作。
pub fn restore_foreground(hwnd_value: isize) {
    if hwnd_value == 0 {
        return;
    }
    let target = hwnd(hwnd_value);
    unsafe {
        if GetForegroundWindow().0 == target.0 {
            return;
        }
        // 本进程此刻通常就是前台（浮层刚抢过焦点），SetForegroundWindow 允许。
        let _ = SetForegroundWindow(target);
        let _ = SetActiveWindow(target);
        let _ = SetFocus(Some(target));
        if GetForegroundWindow().0 == target.0 {
            return;
        }
        // 兜底：附着到目标线程再抢一次（跨进程前台锁/提权窗口场景）。
        let mut pid = 0u32;
        let target_thread = GetWindowThreadProcessId(target, Some(&mut pid));
        let current_thread = GetCurrentThreadId();
        if target_thread != 0 && target_thread != current_thread {
            let _ = AttachThreadInput(current_thread, target_thread, true);
            let _ = SetForegroundWindow(target);
            let _ = SetActiveWindow(target);
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        if GetForegroundWindow().0 != target.0 {
            SwitchToThisWindow(target, true);
        }
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
