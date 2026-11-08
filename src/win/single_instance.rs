use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE,
};

/// 第二个实例通过命名事件通知第一个实例“用户又点了一次启动”，
/// 而不是像早期实现那样只 ShowWindow（窗口在屏幕外，看起来毫无反应）。
static ACTIVATE: AtomicBool = AtomicBool::new(false);

pub struct SingleInstance {
    handle: HANDLE,
    event: HANDLE,
}

/// 读取并清除“另一个实例请求显示窗口”的标记。
pub fn activation_requested() -> bool {
    ACTIVATE.swap(false, Ordering::SeqCst)
}

pub fn acquire() -> Option<SingleInstance> {
    let handle =
        unsafe { CreateMutexW(None, true, w!("Local\\lstr-launcher-9c1f3a")) }.ok()?;
    let already = unsafe { GetLastError() == ERROR_ALREADY_EXISTS };
    if already {
        unsafe {
            if let Ok(event) =
                OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\lstr-activate-9c1f3a"))
            {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
            let _ = CloseHandle(handle);
        }
        return None;
    }
    let event =
        unsafe { CreateEventW(None, false, false, w!("Local\\lstr-activate-9c1f3a")) }
            .unwrap_or_default();
    Some(SingleInstance { handle, event })
}

impl SingleInstance {
    pub fn event_handle(&self) -> isize {
        self.event.0 as isize
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 后台等待命名事件；命中后把 UI 线程唤醒，由 `LstrApp` 执行 show()。
pub fn start_activation_watcher(ctx: eframe::egui::Context, event: isize) {
    if event == 0 {
        return;
    }
    let builder = std::thread::Builder::new().name("lstr-activate".to_string());
    let _ = builder.spawn(move || unsafe {
        let event = HANDLE(event as *mut _);
        loop {
            let wait = WaitForSingleObject(event, 500);
            if wait.0 == WAIT_OBJECT_0.0 {
                ACTIVATE.store(true, Ordering::SeqCst);
                ctx.request_repaint();
            }
        }
    });
}
