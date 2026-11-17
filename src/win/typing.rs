//! P0-2：资源管理器内「直接键入搜索」（type-to-search）。
//!
//! 与 `hotkey.rs` 的唤起热键通道严格分离：
//! - 唤起热键（`hotkey.rs`）的 LL 钩子只**观察**按键（双击修饰键 / Ctrl+G），
//!   无条件 `CallNextHookEx`，不能拦截；
//! - 本模块用 `WH_KEYBOARD_LL` 低级钩子，是唯一能**吞键**的通道。
//!
//! 安全边界（这是全局钩子，判定错了会吃掉用户正常打字）：
//! 1. 只有 `config.explorer_typing = true` 时才由 app 启动（**默认关闭**,
//!    设置页显式开启；P0-5 隐私：诊断日志不记录字符内容）；
//! 2. 只在前台窗口类属于 Explorer/桌面白名单时生效；
//! 3. 焦点落在任何编辑框（地址栏、搜索框、重命名框）时放行；
//! 4. Ctrl / Alt / Win 组合、IME 组合键（VK_PROCESSKEY）一律放行；修饰键状态
//!    一律实时向系统查询（`GetAsyncKeyState`），不在回调里维护标志位——事件式
//!    标志漏一次 keyup 就永久粘滞，「键入即搜」会彻底失效（见 `modifier_down`）；
//! 5. 只吞「可打印字符」的第一个字符；lstr 窗口被唤出后前台不再是 Explorer，
//!    后续输入交回 egui，因此不会破坏 Explorer 的 Enter / Esc / F2 等操作。
//!
//! 可靠性约束（低级钩子回调必须在 `LowLevelHooksTimeout` 内返回，默认 300ms，
//! 否则会被系统静默摘钩）：
//! - 回调里**不做任何文件 I/O**：诊断日志只 `try_send` 进有界队列，由专门的
//!   `lstr-typing-log` 线程落盘；队列满就丢（日志不承担可靠性）；
//! - 回调里也不做无谓的 `String` 分配：判定用 UTF-16 缓冲直接比较，只有真正
//!   要落盘时才格式化；
//! - 收到字符后立即 `ctx.request_repaint()`，隐藏态主循环不再依赖 250ms 节拍；
//! - `poll()` 一次取走队列里的所有字符，快速连打不会只剩最后一个。
//!
//! 注意：这是简化形态——复用居中启动器窗口，不是贴在 Explorer 右下角的独立浮层。

use eframe::egui::Context;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo,
    GetMessageW, KBDLLHOOKSTRUCT, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    GUITHREADINFO, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

/// 事件通道：字符发送端 + egui `Context`。
///
/// `Context` 是 `Send + Sync` 的，官方推荐跨线程调用 `request_repaint()`
/// 唤醒主循环；放在这里而不是 `TypingWatch` 里，是为了让钩子回调能直接拿到它。
type EventSink = (Sender<char>, Context);

/// 用 `Mutex<Option<..>>` 而不是 `OnceLock`：设置页可以反复开关钩子，
/// 每次 start 都必须换上新通道，否则事件会发给已经丢弃的旧接收者。
static EVENT_TX: OnceLock<Mutex<Option<EventSink>>> = OnceLock::new();

fn current_sink() -> Option<EventSink> {
    EVENT_TX
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
}

/// 诊断日志队列：钩子回调只 `try_send`，落盘由独立线程完成。
static LOG_TX: OnceLock<Mutex<Option<SyncSender<String>>>> = OnceLock::new();
/// 队列容量：日志是尽力而为，满了就丢，绝不能反压钩子线程。
const LOG_QUEUE_CAP: usize = 256;
/// 日志文件上限：超过就清空重写，避免无限增长。
const LOG_LIMIT_BYTES: u64 = 1 << 20;

/// 前台窗口类白名单：资源管理器、桌面。
const EXPLORER_CLASSES: [&str; 4] = ["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"];

/// 只放行明确的文本编辑控件；宽泛的 contains 匹配会误伤资源管理器文件列表。
const EDIT_CLASSES: [&str; 8] = [
    "edit",
    "combobox",
    "comboboxex32",
    "searcheditboxwrapperclass",
    "richedit",
    "richedit20w",
    "textbox",
    "windows.ui.xaml.controls.textbox",
];

// ---------------------------------------------------------------------------
// 诊断日志：回调线程只入队，独立线程落盘
// ---------------------------------------------------------------------------

fn log_rate_allowed() -> bool {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 1000 {
        return false;
    }
    LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// 只把日志行塞进队列，绝不做文件 I/O（可能在低级钩子回调里调用）。
fn enqueue_log(line: String) {
    let tx = LOG_TX
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()));
    if let Some(tx) = tx {
        // 队列满 = 丢弃；日志线程比钩子线程慢是常态，不能反压。
        let _ = tx.try_send(line);
    }
}

/// 1 秒限流的判定日志；`line` 只在允许落盘时才求值（避免每键一次 `format!`）。
fn log_decision(line: impl FnOnce() -> String) {
    if !log_rate_allowed() {
        return;
    }
    enqueue_log(line());
}

fn log_event(line: &str) {
    enqueue_log(line.to_string());
}

fn spawn_logger() -> Result<(SyncSender<String>, JoinHandle<()>), String> {
    let (tx, rx) = mpsc::sync_channel::<String>(LOG_QUEUE_CAP);
    let join = thread::Builder::new()
        .name("lstr-typing-log".to_string())
        .spawn(move || {
            use std::io::Write as _;
            let path = crate::config::attach_dir().join("typing.log");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok();
            while let Ok(line) = rx.recv() {
                let Some(f) = file.as_mut() else {
                    continue;
                };
                let _ = writeln!(f, "{line}");
                let too_big = f
                    .metadata()
                    .map(|meta| meta.len() > LOG_LIMIT_BYTES)
                    .unwrap_or(false);
                if too_big {
                    // 简单轮转：清空重写，避免日志文件无限增长。
                    let _ = std::fs::File::create(&path);
                }
            }
        })
        .map_err(|err| format!("spawn typing-log 线程失败: {err}"))?;
    Ok((tx, join))
}

fn clear_event_sink() {
    if let Some(slot) = EVENT_TX.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

fn clear_log_sink() {
    if let Some(slot) = LOG_TX.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

// ---------------------------------------------------------------------------
// TypingWatch
// ---------------------------------------------------------------------------

pub struct TypingWatch {
    rx: Receiver<char>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
    logger: Option<JoinHandle<()>>,
}

impl TypingWatch {
    /// `ctx` 用于在截获字符后立即唤醒主循环，把「按键 → 弹窗」的延迟从
    /// 隐藏态 250ms 节拍降到约一帧。
    pub fn start(ctx: Context) -> Result<Self, String> {
        // 只允许一个实例：重复 start() 会装第二个 WH_KEYBOARD_LL 钩子，
        // 两个钩子各自吞键/发送，字符会重复，且旧实例 Drop 时清的是同一个全局槽。
        let already_running = EVENT_TX
            .get()
            .and_then(|slot| slot.lock().ok().map(|guard| guard.is_some()))
            .unwrap_or(false);
        if already_running {
            return Err("键入即搜钩子已在运行".to_string());
        }

        let (tx, rx) = mpsc::channel::<char>();
        let slot = EVENT_TX.get_or_init(|| Mutex::new(None));
        if let Ok(mut guard) = slot.lock() {
            *guard = Some((tx, ctx));
        }

        // 日志线程失败不影响主功能，只是没有诊断日志。
        let logger = match spawn_logger() {
            Ok((log_tx, join)) => {
                let slot = LOG_TX.get_or_init(|| Mutex::new(None));
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(log_tx);
                }
                Some(join)
            }
            Err(err) => {
                eprintln!("lstr: typing 日志线程启动失败: {err}");
                None
            }
        };

        let (ready_tx, ready_rx) = mpsc::channel();
        let join = match thread::Builder::new()
            .name("lstr-typing".to_string())
            .spawn(move || run(ready_tx))
        {
            Ok(join) => join,
            Err(err) => {
                // 线程都没起来：把已装的全局状态全部回滚，避免下次 start() 误判「已在运行」。
                clear_event_sink();
                clear_log_sink();
                return Err(format!("spawn typing 线程失败: {err}"));
            }
        };
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                rx,
                thread_id,
                join: Some(join),
                logger,
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                clear_event_sink();
                clear_log_sink();
                Err(err)
            }
            Err(_) => {
                let _ = join.join();
                clear_event_sink();
                clear_log_sink();
                Err("typing 线程未就绪就退出了".to_string())
            }
        }
    }

    /// 取走队列里所有被截获的字符（保持先后顺序）；没有则返回空 `Vec`。
    ///
    /// 不能只保留最后一个：20 次/秒的连打间隔 50ms，远小于隐藏态轮询周期。
    pub fn poll(&self) -> Vec<char> {
        let mut out = Vec::new();
        while let Ok(ch) = self.rx.try_recv() {
            out.push(ch);
        }
        out
    }
}

impl Drop for TypingWatch {
    fn drop(&mut self) {
        // 先断开事件通道：卸载过程中到达的键不再被吞（hook_proc 拿不到 sender 就放行）。
        clear_event_sink();
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // 钩子线程退出后再关日志线程，保证 "hook removed" 这条记录能落盘。
        clear_log_sink();
        if let Some(join) = self.logger.take() {
            let _ = join.join();
        }
    }
}

fn run(ready_tx: Sender<Result<u32, String>>) {
    let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
        Ok(hook) => {
            log_event("hook installed");
            hook
        }
        Err(err) => {
            log_event(&format!("hook install FAILED: {err}"));
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
    log_event("hook removed");
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
        if !is_down {
            return LRESULT(0);
        }
        if let Some(ch) = classify(kb) {
            // 先确认真的有人接收再吞键：sender 缺失（正在卸载/已 Drop）或发送
            // 失败时必须放行，否则用户的按键会被静默吃掉。
            if let Some((tx, ctx)) = current_sink() {
                if tx.send(ch).is_ok() {
                    // 立刻唤醒 egui 主循环，不必等隐藏态 250ms 节拍。
                    ctx.request_repaint();
                    return LRESULT(1);
                }
            }
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

/// 实时查询修饰键的物理状态（`GetAsyncKeyState` 第 15 位 = 当前是否按下）。
///
/// 这里**不能**沿用「在 hook 回调里维护 `static MOD_*` 标志位」的做法（旧实现）：
/// 漏掉一次 keyup，标志就永久点亮，之后 `classify()` 每次都在修饰键检查处提前
/// 返回 `None` —— 钩子不再吞键、`poll_typing()` 收不到字符，「键入即搜」表现为
/// 彻底唤不醒（typing.log 里是成百上千行 `skip: modifier held`，只有重启应用
/// 才恢复）。会漏 keyup 的路径都是真实存在的：
/// - 更靠前的低级钩子把 keyup 吞掉（Win 键唤出开始菜单、远程桌面 / 输入法 /
///   安全软件挂的 LL 钩子），我们的回调根本看不到那次抬起；
/// - 用户按住修饰键时设置页重装钩子（`start()` 从不重置这些标志）；
/// - UAC 安全桌面期间按下的键，抬起发生在切换之后。
/// 实时查询没有任何可残留的状态：系统说没按就是没按。
unsafe fn modifier_down(vk: VIRTUAL_KEY) -> bool {
    GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0
}

/// 判定这个按键是否应该被 lstr 接管，并把原因写进 typing.log（1 秒限流）。
///
/// 热路径只做 UTF-16 比较与位运算；`format!` 全部包在 `log_decision` 的闭包里，
/// 只有真正要落盘的那一次才求值。
unsafe fn classify(kb: &KBDLLHOOKSTRUCT) -> Option<char> {
    if kb.vkCode == 0xE5 {
        log_decision(|| "skip: IME composition (VK_PROCESSKEY)".to_string());
        return None;
    }
    // 修饰键状态实时向系统查（见 `modifier_down`）：任何组合键都放行，
    // 且不会因为漏掉一次 keyup 就永久卡在「修饰键按着」。
    let ctrl = modifier_down(VK_CONTROL);
    let alt = modifier_down(VK_MENU);
    let win = modifier_down(VK_LWIN) || modifier_down(VK_RWIN);
    if ctrl || alt || win {
        log_decision(|| {
            format!(
                "skip: modifier held (ctrl={} alt={} win={})",
                ctrl as u8, alt as u8, win as u8
            )
        });
        return None;
    }
    if !foreground_is_explorer() {
        log_decision(|| format!("skip: foreground class={}", foreground_class_name()));
        return None;
    }
    if focus_is_editing() {
        log_decision(|| format!("skip: focus class={}", focus_class_name()));
        return None;
    }
    let ch = vk_to_char(kb.vkCode, modifier_down(VK_SHIFT))?;
    // P0-5（隐私）：诊断日志绝不落盘实际字符。旧版这里写 `capture: ch=s`，
    // 会把用户在资源管理器里敲下的每个字母明文写进 %APPDATA%\lstr\typing.log
    // （默认开启的全局钩子）。只保留窗口类名，排障足够、且不含用户输入。
    log_decision(|| {
        format!(
            "capture: foreground={} focus={}",
            foreground_class_name(),
            focus_class_name()
        )
    });
    Some(ch)
}

/// UTF-16 窗口类名与 ASCII 白名单比较；不做 `String` 分配。
fn class_eq_ignore_ascii(class: &[u16], name: &str) -> bool {
    class.len() == name.len()
        && class
            .iter()
            .zip(name.as_bytes())
            .all(|(&unit, &byte)| unit < 0x80 && (unit as u8).eq_ignore_ascii_case(&byte))
}

fn foreground_is_explorer() -> bool {
    let mut buffer = [0u16; 64];
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let length = GetClassNameW(hwnd, &mut buffer);
        if length <= 0 {
            return false;
        }
        let class = &buffer[..length as usize];
        EXPLORER_CLASSES
            .iter()
            .any(|name| class_eq_ignore_ascii(class, name))
    }
}

/// 仅用于诊断日志，真正判定走 `foreground_is_explorer`。
fn foreground_class_name() -> String {
    let mut buffer = [0u16; 64];
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }
        let length = GetClassNameW(hwnd, &mut buffer);
        if length <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..length as usize])
    }
}

fn focus_is_editing() -> bool {
    let mut buffer = [0u16; 64];
    unsafe {
        let Some(length) = focus_class_len(&mut buffer) else {
            return false;
        };
        let class = &buffer[..length];
        EDIT_CLASSES
            .iter()
            .any(|name| class_eq_ignore_ascii(class, name))
    }
}

/// 仅用于诊断日志，真正判定走 `focus_is_editing`。
fn focus_class_name() -> String {
    let mut buffer = [0u16; 64];
    unsafe {
        let Some(length) = focus_class_len(&mut buffer) else {
            return String::new();
        };
        String::from_utf16_lossy(&buffer[..length])
    }
}

/// 取前台线程焦点窗口的类名（UTF-16），返回有效长度。
unsafe fn focus_class_len(buffer: &mut [u16; 64]) -> Option<usize> {
    let mut info: GUITHREADINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    if GetGUIThreadInfo(0, &mut info).is_err() || info.hwndFocus.0.is_null() {
        return None;
    }
    let length = GetClassNameW(info.hwndFocus, buffer);
    if length <= 0 {
        return None;
    }
    Some(length as usize)
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

    #[test]
    fn class_compare_is_case_insensitive_and_zero_alloc() {
        let class: Vec<u16> = "cabinetwclass".encode_utf16().collect();
        assert!(class_eq_ignore_ascii(&class, "CabinetWClass"));
        assert!(!class_eq_ignore_ascii(&class, "ExploreWClass"));
        let edit: Vec<u16> = "Edit".encode_utf16().collect();
        assert!(class_eq_ignore_ascii(&edit, "edit"));
        // 非 ASCII 类名不匹配任何 ASCII 白名单。
        let unicode: Vec<u16> = "编辑".encode_utf16().collect();
        assert!(!class_eq_ignore_ascii(&unicode, "edit"));
    }
}

