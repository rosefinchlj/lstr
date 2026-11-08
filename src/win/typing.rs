//! P0-2：资源管理器内「直接键入搜索」（type-to-search）。
//!
//! 与 `hotkey.rs` 的 Raw Input 通道严格分离：
//! - Raw Input（`RIDEV_INPUTSINK`）只**观察**按键，用来识别双击 Space，不能拦截；
//! - 本模块用 `WH_KEYBOARD_LL` 低级钩子，是唯一能**吞键**的通道。
//!
//! 安全边界（这是全局钩子，判定错了会吃掉用户正常打字）：
//! 1. 只有 `config.explorer_typing = true` 时才由 app 启动，默认关闭；
//! 2. 只在前台窗口类属于 Explorer/桌面白名单时生效；
//! 3. 焦点落在任何编辑框（地址栏、搜索框、重命名框）时放行；
//! 4. Ctrl / Alt / Win 组合、IME 组合键（VK_PROCESSKEY）一律放行；
//! 5. 只吞「可打印字符」的第一个字符；lstr 窗口被唤出后前台不再是 Explorer，
//!    后续输入交回 egui，因此不会破坏 Explorer 的 Enter / Esc / F2 等操作。
//!
//! 注意：这是简化形态——复用居中启动器窗口，不是贴在 Explorer 右下角的独立浮层。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo,
    GetMessageW, KBDLLHOOKSTRUCT, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    GUITHREADINFO, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

/// 用 Mutex<Option<Sender>> 而不是 OnceLock<Sender>：设置页可以反复开关钩子，
/// 每次 start 都必须换上新通道，否则事件会发给已经丢弃的旧接收者。
static EVENT_TX: OnceLock<Mutex<Option<Sender<char>>>> = OnceLock::new();

fn current_sender() -> Option<Sender<char>> {
    EVENT_TX
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
}
static MOD_CTRL: AtomicBool = AtomicBool::new(false);
static MOD_ALT: AtomicBool = AtomicBool::new(false);
static MOD_WIN: AtomicBool = AtomicBool::new(false);
static MOD_SHIFT: AtomicBool = AtomicBool::new(false);

/// 前台窗口类白名单：资源管理器、桌面。
const EXPLORER_CLASSES: [&str; 4] = [
    "CabinetWClass",
    "ExploreWClass",
    "Progman",
    "WorkerW",
];

/// 焦点控件类名里出现这些片段就认为在编辑文本，必须放行。
const EDIT_CLASS_HINTS: [&str; 3] = ["edit", "combo", "search"];

pub struct TypingWatch {
    rx: Receiver<char>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl TypingWatch {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<char>();
        let slot = EVENT_TX.get_or_init(|| Mutex::new(None));
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(tx);
        }
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("lstr-typing".to_string())
            .spawn(move || run(ready_tx))
            .map_err(|err| format!("spawn typing 线程失败: {err}"))?;
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
                Err("typing 线程未就绪就退出了".to_string())
            }
        }
    }

    /// 取走最近一次被截获的字符；没有则 None。
    pub fn poll(&self) -> Option<char> {
        let mut latest = None;
        while let Ok(ch) = self.rx.try_recv() {
            latest = Some(ch);
        }
        latest
    }
}

impl Drop for TypingWatch {
    fn drop(&mut self) {
        // 卸载后不再向无人接收的通道投递事件。
        if let Some(slot) = EVENT_TX.get() {
            if let Ok(mut guard) = slot.lock() {
                *guard = None;
            }
        }
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(ready_tx: Sender<Result<u32, String>>) {
    let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
        Ok(hook) => hook,
        Err(err) => {
            let _ = ready_tx.send(Err(format!("SetWindowsHookExW 失败: {err}")));
            return;
        }
    };
    let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
    let mut message = MSG::default();
    // 低级钩子要求安装线程自己泵消息，否则回调不会被调用。
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe { DispatchMessageW(&message) };
    }
    let _ = unsafe { UnhookWindowsHookEx(hook) };
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 回调穿过 FFI 边界，panic 必须就地兜住；同时把「是否吞键」作为返回值传出来，
    // 不能像 WinEvent 回调那样只忽略结果，否则无论如何都会放行。
    let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> LRESULT {
        if code < 0 || lparam.0 == 0 {
            return LRESULT(0);
        }
        let message = wparam.0 as u32;
        let is_down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let is_up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
        if !is_down && !is_up {
            return LRESULT(0);
        }
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        update_modifiers(kb.vkCode, is_down);
        if !is_down {
            return LRESULT(0);
        }
        if let Some(ch) = classify(kb) {
            if let Some(tx) = current_sender() {
                let _ = tx.send(ch);
            }
            return LRESULT(1);
        }
        LRESULT(0)
    }));
    if let Ok(result) = handled {
        if result.0 == 1 {
            return result;
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn update_modifiers(vk: u32, is_down: bool) {
    let slot: Option<&AtomicBool> = match vk {
        0x10 | 0xA0 | 0xA1 => Some(&MOD_SHIFT),
        0x11 | 0xA2 | 0xA3 => Some(&MOD_CTRL),
        0x12 | 0xA4 | 0xA5 => Some(&MOD_ALT),
        0x5B | 0x5C => Some(&MOD_WIN),
        _ => None,
    };
    if let Some(slot) = slot {
        slot.store(is_down, Ordering::Relaxed);
    }
}

/// 判定这个按键是否应该被 lstr 接管。这里任何一个条件放宽都会吃掉用户正常输入。
unsafe fn classify(kb: &KBDLLHOOKSTRUCT) -> Option<char> {
    // VK_PROCESSKEY：输入法正在组合，交给 IME，不吞。
    if kb.vkCode == 0xE5 {
        return None;
    }
    if MOD_CTRL.load(Ordering::Relaxed)
        || MOD_ALT.load(Ordering::Relaxed)
        || MOD_WIN.load(Ordering::Relaxed)
    {
        return None;
    }
    if !foreground_is_explorer() || focus_is_edit() {
        return None;
    }
    vk_to_char(kb.vkCode, MOD_SHIFT.load(Ordering::Relaxed))
}

unsafe fn foreground_is_explorer() -> bool {
    let hwnd = GetForegroundWindow();
    if hwnd.0.is_null() {
        return false;
    }
    let mut buffer = [0u16; 64];
    let length = GetClassNameW(hwnd, &mut buffer);
    if length <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buffer[..length as usize]);
    EXPLORER_CLASSES
        .iter()
        .any(|class| name.eq_ignore_ascii_case(class))
}

unsafe fn focus_is_edit() -> bool {
    let mut info: GUITHREADINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    if GetGUIThreadInfo(0, &mut info).is_err() || info.hwndFocus.0.is_null() {
        return false;
    }
    let mut buffer = [0u16; 64];
    let length = GetClassNameW(info.hwndFocus, &mut buffer);
    if length <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buffer[..length as usize]).to_lowercase();
    EDIT_CLASS_HINTS.iter().any(|hint| name.contains(hint))
}

/// 虚拟键 → 字符。只覆盖主键盘可打印区；符号的 Shift 变体不做映射，
/// 中文由 IME 产生（本模块不接管 IME 输出），这两点都是有意的范围限制。
fn vk_to_char(vk: u32, shift: bool) -> Option<char> {
    let ch = match vk {
        0x20 => ' ',
        0x30..=0x39 => (b'0' + (vk - 0x30) as u8) as char,
        0x41..=0x5A => {
            let base = (vk - 0x41) as u8;
            if shift {
                (b'A' + base) as char
            } else {
                (b'a' + base) as char
            }
        }
        0x08 | 0x09 => return None,
        0xBD => '-',
        0xBE => '.',
        0xBF => '/',
        0xBC => ',',
        0xBA => ';',
        0xDE => '\'',
        0xDB => '[',
        0xDD => ']',
        0xDC => '\\',
        0xC0 => '`',
        _ => return None,
    };
    Some(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_letters_follow_shift() {
        assert_eq!(vk_to_char(0x41, false), Some('a'));
        assert_eq!(vk_to_char(0x41, true), Some('A'));
        assert_eq!(vk_to_char(0x35, false), Some('5'));
        assert_eq!(vk_to_char(0x20, false), Some(' '));
    }

    #[test]
    fn navigation_keys_are_not_captured() {
        assert_eq!(vk_to_char(0x0D, false), None);
        assert_eq!(vk_to_char(0x1B, false), None);
        assert_eq!(vk_to_char(0x08, false), None);
        assert_eq!(vk_to_char(0xE5, false), None);
    }
}
