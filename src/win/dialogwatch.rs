//! WinEvent hook：前台窗口变化、或有 `#32770` 显示时通知主界面，
//! 由主循环判断是不是文件对话框。
//!
//! 前台事件不再按类名过滤：Win11 记事本等 XAML 宿主打开 `#32770` 后，
//! 前台仍停在主窗口，事件里的 hwnd 不是对话框本身，提前过滤会把这类情况
//! 全部丢掉。同时监听 `EVENT_OBJECT_SHOW`（只认 `#32770`）：前台没有变化
//! 时，对话框显示本身也能触发探测。完整校验（进程/编辑框链/可见性）交给
//! egui 主循环的 `dialog::foreground_dialog()`。
//! `WINEVENT_OUTOFCONTEXT` 的回调在本线程泵消息时触发，线程只需一个裸
//! `GetMessageW` 循环（不需要窗口）。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClassNameW, GetMessageW, MSG, WINEVENT_OUTOFCONTEXT,
};

const DIALOG_CLASS: &str = "#32770";
const EVENT_SYSTEM_FOREGROUND: u32 = 3;
/// EVENT_OBJECT_SHOW：对象显示。范围事件，回调里靠类名过滤。
const EVENT_OBJECT_SHOW: u32 = 0x8002;
/// EVENT_OBJECT_DESTROY：对象销毁（用来发现锚定的对话框被关掉）。
const EVENT_OBJECT_DESTROY: u32 = 0x8001;
/// EVENT_OBJECT_LOCATIONCHANGE：对象移动/缩放（用来跟随停靠条，替代轮询）。
const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
const OBJID_WINDOW: i32 = 0;
const CHILDID_SELF: i32 = 0;

static EVENT_TX: OnceLock<Sender<isize>> = OnceLock::new();
/// UI 唤醒器:hook 事件一到就 `request_repaint`,隐藏窗口无需周期重绘。
static WAKE_CTX: OnceLock<eframe::egui::Context> = OnceLock::new();
/// 当前正在跟随的窗口（贴角浮层的资源管理器 / 停靠条的文件对话框）。
/// 只有它自己的位置变化才会唤醒 UI，别的窗口乱动不会触发重绘。
static ANCHOR: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);

/// 设置/清除跟随目标。`None` 表示当前没有浮层，不关心任何窗口的位置变化。
pub fn set_anchor(hwnd: Option<isize>) {
    ANCHOR.store(hwnd.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}

fn anchor_is(hwnd: HWND) -> bool {
    let anchor = ANCHOR.load(std::sync::atomic::Ordering::Relaxed);
    anchor != 0 && hwnd.0 as isize == anchor
}

/// 对话框自动唤出监听器；Drop 时反挂 hook 并结束线程。
pub struct DialogWatch {
    rx: Receiver<isize>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl DialogWatch {
    pub fn start(ctx: &eframe::egui::Context) -> Result<Self, String> {
        let _ = WAKE_CTX.set(ctx.clone());
        let (tx, rx) = mpsc::channel();
        let _ = EVENT_TX.set(tx);
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("lstr-dialog-watch".to_string())
            .spawn(move || run(ready_tx))
            .map_err(|err| format!("spawn dialog-watch 线程失败: {err}"))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                rx,
                thread_id,
                join: Some(join),
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(err)
            }
            Err(_) => {
                let _ = join.join();
                Err("dialog-watch 线程未就绪就退出了".to_string())
            }
        }
    }

    /// 最近一次「前台变成 #32770」事件的目标 hwnd；无事件返回 None。
    pub fn poll(&self) -> Option<isize> {
        let mut latest = None;
        while let Ok(hwnd) = self.rx.try_recv() {
            latest = Some(hwnd);
        }
        latest
    }
}

impl Drop for DialogWatch {
    fn drop(&mut self) {
        // WM_QUIT 让 GetMessageW 返回 0，线程收尾反挂 hook。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(ready_tx: Sender<Result<u32, String>>) {
    unsafe {
        let foreground = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let shown = SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let moved = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let destroyed = SetWinEventHook(
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_DESTROY,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if foreground.0.is_null()
            || shown.0.is_null()
            || moved.0.is_null()
            || destroyed.0.is_null()
        {
            for hook in [foreground, shown, moved, destroyed] {
                if !hook.0.is_null() {
                    let _ = UnhookWinEvent(hook);
                }
            }
            let _ = ready_tx.send(Err("SetWinEventHook 失败".to_string()));
            return;
        }
        // 消息循环开始即视为就绪：hook 已生效，事件会排队等待 poll。
        let _ = ready_tx.send(Ok(GetCurrentThreadId()));

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&message);
        }
        let _ = UnhookWinEvent(foreground);
        let _ = UnhookWinEvent(shown);
        let _ = UnhookWinEvent(moved);
        let _ = UnhookWinEvent(destroyed);
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    // panic 穿过 FFI 边界会 fast-fail 整个进程，这里兜底。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if idobject != OBJID_WINDOW || idchild != CHILDID_SELF || hwnd.0.is_null() {
            return;
        }
        // 跟随目标自己的移动/缩放：只叫醒 UI 重算位置，不进事件通道——
        // 通道是「有对话框出现/被激活」的语义，混进来会让停靠条误挂。
        if event == EVENT_OBJECT_LOCATIONCHANGE {
            if anchor_is(hwnd) {
                if let Some(ctx) = WAKE_CTX.get() {
                    ctx.request_repaint();
                }
            }
            return;
        }
        // 跟随目标被销毁：只叫醒 UI 收起浮层（`poll_attach` 里的
        // `is_window` 检查负责清理）。**不进事件通道**——通道语义是「有对话框
        // 出现/被激活」，混进销毁事件会白开一轮 2.5s 探测。
        if event == EVENT_OBJECT_DESTROY {
            if anchor_is(hwnd) {
                ANCHOR.store(0, std::sync::atomic::Ordering::Relaxed);
                if let Some(ctx) = WAKE_CTX.get() {
                    ctx.request_repaint();
                }
            }
            return;
        }
        // 前台事件不带类名（可能是不抢前台的 XAML 宿主）；显示事件量大，
        // 只认 `#32770`。类名查询不发消息，跨进程安全。
        if event == EVENT_OBJECT_SHOW {
            if !class_is_dialog(hwnd) {
                return;
            }
        } else if event != EVENT_SYSTEM_FOREGROUND {
            return;
        }
        if let Some(tx) = EVENT_TX.get() {
            let _ = tx.send(hwnd.0 as isize);
        }
        // 事件线程直接唤醒 UI：隐藏态不再依赖主循环的周期重绘。
        if let Some(ctx) = WAKE_CTX.get() {
            ctx.request_repaint();
        }
    }));
}

/// 回调线程里只做类名过滤（GetClassNameW 不发消息，跨进程安全）。
fn class_is_dialog(hwnd: HWND) -> bool {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if length <= 0 {
        return false;
    }
    let end = (length as usize).min(buffer.len());
    let mut slice = &buffer[..end];
    while let [rest @ .., 0] = slice {
        slice = rest;
    }
    String::from_utf16_lossy(slice).eq_ignore_ascii_case(DIALOG_CLASS)
}

#[cfg(test)]
mod tests {
    #[test]
    fn constants_match_win32_semantics() {
        // EVENT_SYSTEM_FOREGROUND=3 / EVENT_OBJECT_SHOW=0x8002 /
        // EVENT_OBJECT_DESTROY=0x8001 / EVENT_OBJECT_LOCATIONCHANGE=0x800B。
        assert_eq!(super::EVENT_SYSTEM_FOREGROUND, 3);
        assert_eq!(super::EVENT_OBJECT_SHOW, 0x8002);
        assert_eq!(super::EVENT_OBJECT_DESTROY, 0x8001);
        assert_eq!(super::EVENT_OBJECT_LOCATIONCHANGE, 0x800B);
        assert_eq!(super::DIALOG_CLASS, "#32770");
        assert_eq!(super::OBJID_WINDOW, 0);
        assert_eq!(super::CHILDID_SELF, 0);
    }
}
