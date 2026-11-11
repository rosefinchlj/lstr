//! WinEvent hook：前台出现标准文件对话框（`#32770`）时通知主界面自动唤出。
//! `WINEVENT_OUTOFCONTEXT` 的回调在本线程泵消息时触发，线程只需一个裸
//! `GetMessageW` 循环（不需要窗口）。回调里只做类名过滤这种最轻的判断，
//! 完整校验（编辑框链、可见性）交给 egui 主循环的 `dialog::foreground_dialog()`。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetClassNameW, MSG, WINEVENT_OUTOFCONTEXT,
};

const DIALOG_CLASS: &str = "#32770";
const EVENT_SYSTEM_FOREGROUND: u32 = 3;
const OBJID_WINDOW: i32 = 0;
const CHILDID_SELF: i32 = 0;

static EVENT_TX: OnceLock<Sender<isize>> = OnceLock::new();

/// 对话框自动唤出监听器；Drop 时反挂 hook 并结束线程。
pub struct DialogWatch {
    rx: Receiver<isize>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl DialogWatch {
    pub fn start() -> Result<Self, String> {
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
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.0.is_null() {
            let _ = ready_tx.send(Err("SetWinEventHook 失败".to_string()));
            return;
        }
        // 消息循环开始即视为就绪：hook 已生效，事件会排队等待 poll。
        let _ = ready_tx.send(Ok(GetCurrentThreadId()));

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&message);
        }
        let _ = UnhookWinEvent(hook);
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
        if event != EVENT_SYSTEM_FOREGROUND
            || idobject != OBJID_WINDOW
            || idchild != CHILDID_SELF
            || hwnd.0.is_null()
        {
            return;
        }
        if !class_is_dialog(hwnd) {
            return;
        }
        if let Some(tx) = EVENT_TX.get() {
            let _ = tx.send(hwnd.0 as isize);
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
        // EVENT_SYSTEM_FOREGROUND=3 / OBJID_WINDOW=0 / CHILDID_SELF=0。
        assert_eq!(super::EVENT_SYSTEM_FOREGROUND, 3);
        assert_eq!(super::DIALOG_CLASS, "#32770");
        assert_eq!(super::OBJID_WINDOW, 0);
        assert_eq!(super::CHILDID_SELF, 0);
    }
}
