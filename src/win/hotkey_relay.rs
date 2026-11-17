//! 提权热键中继。
//!
//! Windows 的 UIPI 会阻止普通完整性进程安装的低级键盘钩子接收发往
//! **管理员窗口**的按键：管理员终端在前台时，主界面的双击 Ctrl 完全收不到。
//! 提权的 index worker 不受这个限制，因此让它也观察同一个热键；只有当前
//! 前台窗口属于提权进程时，才通过命名事件通知主界面唤起，避免与主进程
//! 自己的钩子重复触发。
//!
//! 限制：worker 未运行（索引关闭/UAC 被拒绝）时，管理员窗口在前台仍无法
//! 唤起；这是操作系统安全边界，不是应用层能绕过的。

use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, OpenProcess, OpenProcessToken, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

// 事件名必须与 `create` / `signal_relay` 里的 `w!("Local\\lstr-hotkey-relay-9c1f3a")`
// 保持一致（`w!` 只接受字面量，无法共用常量）。

/// 主界面侧的事件句柄：每帧非阻塞地取一次「提权 hotkey 已触发」信号。
pub struct HotkeyRelay {
    event: HANDLE,
    /// P2-16：独立的唤醒事件。中继线程 SetEvent 后，这里的常驻线程
    /// WaitForSingleObject → request_repaint，隐藏态不必等 2s 保险丝重绘。
    /// 与 `event` 分开是因为自动复位事件会被任一 waiter 消费：若共用，
    /// 唤醒线程会把本该由 `take()` 消费的 toggle 信号吃掉。
    wake_event: HANDLE,
}

struct SendHandle(isize);
unsafe impl Send for SendHandle {}

impl HotkeyRelay {
    /// 在启动 index worker **之前**创建，worker 才能 OpenEvent 到它。
    pub fn create(ctx: Option<eframe::egui::Context>) -> Option<Self> {
        let event =
            unsafe { CreateEventW(None, false, false, w!("Local\\lstr-hotkey-relay-9c1f3a")) }
                .ok()?;
        let wake_event = unsafe {
            CreateEventW(None, false, false, w!("Local\\lstr-hotkey-relay-wake-9c1f3a"))
        }
        .ok()?;
        if let Some(ctx) = ctx {
            let wake = SendHandle(wake_event.0 as isize);
            let _ = std::thread::Builder::new()
                .name("lstr-relay-wake".to_string())
                .spawn(move || {
                    let handle =
                        HANDLE(wake.0 as *mut core::ffi::c_void);
                    loop {
                        let _ = unsafe { WaitForSingleObject(handle, u32::MAX) };
                        ctx.request_repaint();
                    }
                });
        }
        Some(Self {
            event,
            wake_event,
        })
    }

    /// 有信号返回 true（自动复位事件已清除），供主循环 toggle 一次。
    pub fn take(&self) -> bool {
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for HotkeyRelay {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.wake_event);
        }
    }
}

/// worker 侧：启动键盘钩子，管理员窗口前台时把双击 Ctrl 转成事件信号。
/// 由 `main` 的 `--index-worker` 分支在进入数据泵循环前调用。
pub fn start_worker_relay() {
    let cfg = crate::config::Config::load();
    let mut hotkey = cfg.hotkey;
    // 只有双击修饰键走 LL 钩子、会被 UIPI 屏蔽才需要中继；组合键走
    // RegisterHotKey，若在 worker 里再注册一次会和主界面抢同一个组合键，
    // 反而让主界面注册失败。
    let mut service = double_tap_hotkey(&hotkey)
        .then(|| {
            crate::win::hotkey::HotkeyService::start(
                eframe::egui::Context::default(),
                &hotkey,
            )
            .ok()
        })
        .flatten();
    if service.is_some() {
        lstr_index::snapshot::debug_log(&format!(
            "hotkey-relay: worker hook ready hotkey={hotkey}"
        ));
    }
    // worker 进程与钩子同寿命：service 移进常驻线程，线程不退出，
    // Drop（卸载钩子）自然不会发生，进程退出时由系统回收。
    let _ = std::thread::Builder::new()
        .name("lstr-hotkey-relay".to_string())
        .spawn(move || {
            let mut last_config_check = Instant::now();
            loop {
                // 设置页改热键后 GUI 的钩子立即换新；worker 的中继也必须跟着换，
                // 否则管理员窗口前台时还在等旧键（定时读配置，3s 内对齐）。
                if last_config_check.elapsed() >= Duration::from_secs(3) {
                    last_config_check = Instant::now();
                    if let Some(next) = configured_hotkey() {
                        if next != hotkey {
                            if double_tap_hotkey(&next) {
                                if let Ok(reloaded) = crate::win::hotkey::HotkeyService::start(
                                    eframe::egui::Context::default(),
                                    &next,
                                ) {
                                    lstr_index::snapshot::debug_log(&format!(
                                        "hotkey-relay: reload {hotkey} -> {next}"
                                    ));
                                    service = Some(reloaded);
                                    hotkey = next;
                                }
                            } else {
                                // 切到组合键：撤掉 worker 钩子，注册权交还主界面。
                                lstr_index::snapshot::debug_log(&format!(
                                    "hotkey-relay: stop (combo hotkey {next})"
                                ));
                                service = None;
                                hotkey = next;
                            }
                        }
                    }
                }
                let Some(service) = service.as_ref() else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                match service.poll() {
                    Some(crate::win::hotkey::HotkeyEvent::Toggle) => {
                        if foreground_is_elevated() {
                            lstr_index::snapshot::debug_log(
                                "hotkey-relay: signal for elevated foreground",
                            );
                            signal_relay();
                        }
                    }
                    Some(_) => {}
                    None => std::thread::sleep(Duration::from_millis(40)),
                }
            }
        });
}

fn double_tap_hotkey(hotkey: &str) -> bool {
    matches!(
        crate::hotkey::parse_hotkey(hotkey),
        Some(crate::hotkey::WakeHotkey::DoubleTap(_))
    )
}

/// 只读热键字段，避免在 worker 里跑完整 Config::load（会归一化/可能写盘）。
fn configured_hotkey() -> Option<String> {
    let text = std::fs::read_to_string(crate::config::Config::path()).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value.get("hotkey")?.as_str().map(str::to_string)
}

fn signal_relay() {
    unsafe {
        if let Ok(event) = OpenEventW(
            EVENT_MODIFY_STATE,
            false,
            w!("Local\\lstr-hotkey-relay-9c1f3a"),
        ) {
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
        }
        // P2-16：再敲一下唤醒事件，让隐藏态的主界面立刻 request_repaint。
        if let Ok(event) = OpenEventW(
            EVENT_MODIFY_STATE,
            false,
            w!("Local\\lstr-hotkey-relay-wake-9c1f3a"),
        ) {
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
        }
    }
}

/// 当前前台窗口是否属于提权进程。只用于决定是否转发，不影响普通窗口：
/// 普通窗口由主进程自己的钩子处理，避免同一次双击触发两次 toggle。
pub(crate) fn foreground_is_elevated() -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(foreground, Some(&mut pid));
        if pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok();
        let _ = CloseHandle(process);
        if !opened {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        queried && elevation.TokenIsElevated != 0
    }
}
