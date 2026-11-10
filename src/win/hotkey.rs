//! 全局唤起热键服务：两条注册路径统一成同一个「唤起事件」出口。
//!
//! - **双击修饰键**（double-ctrl / double-shift / double-alt / legacy double-space）：
//!   `RegisterHotKey` 无法绑裸键，走 `WH_KEYBOARD_LL` 低级钩子**观察**路径
//!   （模式参照 `typing.rs`：专用线程泵消息，退出时 `UnhookWindowsHookEx`）。
//!   回调只更新纯逻辑检测器（[`crate::hotkey::DoubleTapDetector`]），**绝不吞键**——
//!   无条件 `CallNextHookEx`，Ctrl+C / Ctrl+V 等组合完全不受影响。
//! - **组合键**（ctrl+space / alt+q / 裸 F 键）：`RegisterHotKey`（系统级、可靠），
//!   `hWnd = None`，`WM_HOTKEY` 投递到线程消息队列；退出时 `UnregisterHotKey`。
//!
//! 两条路径都把事件发进同一个 channel，[`HotkeyService::poll`] 是唯一出口。
//! Ctrl+G（对话框搜索）的观察也挂在 LL 钩子上，与唤起热键共用一条钩子链。
//!
//! 可靠性约束（低级钩子回调必须在 `LowLevelHooksTimeout` 内返回，否则被系统
//! 静默摘钩）：回调里**无 IO、无锁等待**——状态更新是纯内存操作，触发后只做
//! channel `send` 与 `request_repaint`，唤起逻辑全部 post 到 UI 线程。
//!
//! 更换热键 = 丢弃本服务重新 [`HotkeyService::start`]（Drop 里干净卸载：
//! unhook + unregister + join 线程），无需重启应用。

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use eframe::egui::Context;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP,
    WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::hotkey::{
    parse_hotkey, vk, DoubleTapDetector, Mods, PrimaryKey, WakeHotkey, DEFAULT_HOTKEY,
};

/// `RegisterHotKey` 的应用内 id（本服务同时只注册一个组合键）。
const WAKE_HOTKEY_ID: i32 = 1;

/// 'G' 的虚拟键码（Ctrl+G = 对话框搜索）。
const G_VKEY: u16 = 0x47;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Toggle,
    DialogSearch,
    Quit,
}

/// LL 钩子回调里用的事件接收端：发事件 + 唤醒 UI 线程。
struct HookSink {
    tx: Sender<HotkeyEvent>,
    ctx: Context,
}

/// LL 钩子线程的观察状态（线程本地，回调同步访问，无锁）。
struct HookHandler {
    /// 双击修饰键检测器；组合键模式下为 `None`（不观察双击）。
    tap: Option<DoubleTapDetector>,
    /// Ctrl+G 组合观察（对话框搜索入口）。
    ctrl_g: CtrlGState,
    sink: HookSink,
}

thread_local! {
    static HOOK_HANDLER: RefCell<Option<HookHandler>> = const { RefCell::new(None) };
}

/// 一个后台服务线程：id 用于投递 WM_QUIT，join 用于退出时等待清理完成。
///
/// Drop 时投递 WM_QUIT 并 join：无论正常卸载还是启动中途失败
/// （组合键注册失败要撤掉已装好的钩子线程），清理路径都走这一条。
struct ServiceThread {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl Drop for ServiceThread {
    fn drop(&mut self) {
        // WM_QUIT 唤醒消息循环；LL 钩子线程退出时 UnhookWindowsHookEx，
        // 组合键线程退出时 UnregisterHotKey，都发生在自己的线程上。
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A running hotkey listener.
///
/// Dropping the service unhooks / unregisters everything and joins the threads.
pub struct HotkeyService {
    rx: Receiver<HotkeyEvent>,
    threads: Vec<ServiceThread>,
}

impl HotkeyService {
    /// Starts the listener threads for `hotkey` (配置字符串，见 [`crate::hotkey`])
    /// and waits until they are ready.
    ///
    /// 组合键注册失败（被系统/其他程序占用）返回 `Err`，调用方保留旧值。
    pub fn start(ctx: Context, hotkey: &str) -> Result<Self, String> {
        // 防御：config 加载时已归一化，这里再兜底一次，坏值走默认而不是失效。
        let wake = parse_hotkey(hotkey)
            .or_else(|| parse_hotkey(DEFAULT_HOTKEY))
            .ok_or_else(|| format!("无法解析唤起热键: {hotkey}"))?;

        let (tx, rx) = mpsc::channel();
        let mut threads: Vec<ServiceThread> = Vec::with_capacity(2);

        // —— LL 钩子线程：Ctrl+G 观察始终在，双击检测按需挂上 ——
        let tap = match wake {
            WakeHotkey::DoubleTap(key) => Some(DoubleTapDetector::new(key.vkeys())),
            WakeHotkey::Combo { .. } => None,
        };
        threads.push(spawn_hook_thread(ctx.clone(), tx.clone(), tap)?);

        // —— RegisterHotKey 线程：仅组合键模式 ——
        if let WakeHotkey::Combo { mods, key } = wake {
            threads.push(spawn_combo_thread(ctx, tx, mods, key)?);
        }

        Ok(Self { rx, threads })
    }

    /// Returns a pending hotkey event without blocking.
    pub fn poll(&self) -> Option<HotkeyEvent> {
        match self.rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        // 逐个停线程（ServiceThread::drop：WM_QUIT + join）；
        // rx 先于此处销毁，卸载瞬间到达的触发不会投给已销毁的 UI。
        self.threads.clear();
    }
}

fn spawn_hook_thread(
    ctx: Context,
    tx: Sender<HotkeyEvent>,
    tap: Option<DoubleTapDetector>,
) -> Result<ServiceThread, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = thread::Builder::new()
        .name("lstr-hotkey-hook".to_string())
        .spawn(move || run_hook(ctx, tx, tap, ready_tx))
        .map_err(|err| format!("failed to spawn hotkey hook thread: {err}"))?;

    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(ServiceThread {
            thread_id,
            join: Some(join),
        }),
        Ok(Err(err)) => {
            let _ = join.join();
            Err(err)
        }
        Err(_) => {
            let _ = join.join();
            Err("hotkey hook thread exited before it was ready".to_string())
        }
    }
}

/// LL 钩子线程主体：安装钩子 → 泵消息 → 退出时反挂钩子。
/// 低级钩子要求安装线程自己泵消息，回调才会被调用。
fn run_hook(ctx: Context, tx: Sender<HotkeyEvent>, tap: Option<DoubleTapDetector>, ready: Sender<Result<u32, String>>) {
    let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
        Ok(hook) => hook,
        Err(err) => {
            let _ = ready.send(Err(format!("SetWindowsHookExW 失败: {err}")));
            return;
        }
    };

    HOOK_HANDLER.with(|cell| {
        *cell.borrow_mut() = Some(HookHandler {
            tap,
            ctrl_g: CtrlGState::default(),
            sink: HookSink { tx, ctx },
        });
    });

    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe { DispatchMessageW(&message) };
    }

    // 先拆状态再反钩子：反钩子之后不会再有回调，但顺序上保持干净。
    HOOK_HANDLER.with(|cell| {
        cell.borrow_mut().take();
    });
    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
}

/// LL 钩子回调：只观察、不吞键。
///
/// 任何路径都 `CallNextHookEx` 放行；唤起/搜索事件发到 channel 由 UI 线程处理。
/// 热路径只有一次 thread_local 借用 + 纯内存状态更新，无 IO、无锁等待。
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 回调穿过 FFI 边界，panic 必须就地兜住（fast-fail 会整个进程闪退）；
    // 无论结果如何都放行按键。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if code < 0 || lparam.0 == 0 {
            return;
        }
        let message = wparam.0 as u32;
        let is_down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let is_up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
        if !is_down && !is_up {
            return;
        }
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        // 0x00 / 0xFF 标记伪键（如 overrun），不是真实按键。
        if kb.vkCode == 0 || kb.vkCode > u16::MAX as u32 {
            return;
        }
        let vkey = kb.vkCode as u16;
        let now = Instant::now();

        let event = HOOK_HANDLER.with(|cell| {
            let mut guard = cell.borrow_mut();
            let Some(handler) = guard.as_mut() else {
                return None;
            };
            if let Some(tap) = handler.tap.as_mut() {
                if tap.on_key(vkey, is_down, now) {
                    return Some(HotkeyEvent::Toggle);
                }
            }
            if is_down {
                handler
                    .ctrl_g
                    .on_key_down(vkey)
                    .then_some(HotkeyEvent::DialogSearch)
            } else {
                handler.ctrl_g.on_key_up(vkey);
                None
            }
        });

        if let Some(event) = event {
            HOOK_HANDLER.with(|cell| {
                if let Some(handler) = cell.borrow().as_ref() {
                    let _ = handler.sink.tx.send(event);
                    handler.sink.ctx.request_repaint();
                }
            });
        }
    }));
    CallNextHookEx(None, code, wparam, lparam)
}

fn spawn_combo_thread(
    ctx: Context,
    tx: Sender<HotkeyEvent>,
    mods: Mods,
    key: PrimaryKey,
) -> Result<ServiceThread, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = thread::Builder::new()
        .name("lstr-hotkey-combo".to_string())
        .spawn(move || run_combo(ctx, tx, mods, key, ready_tx))
        .map_err(|err| format!("failed to spawn hotkey combo thread: {err}"))?;

    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(ServiceThread {
            thread_id,
            join: Some(join),
        }),
        Ok(Err(err)) => {
            let _ = join.join();
            Err(err)
        }
        Err(_) => {
            let _ = join.join();
            Err("hotkey combo thread exited before it was ready".to_string())
        }
    }
}

/// `RegisterHotKey` 线程主体：注册 → 泵消息收 `WM_HOTKEY` → 退出时反注册。
/// `hWnd = None` 时 `WM_HOTKEY` 直接投递到本线程消息队列，无需隐藏窗口。
fn run_combo(
    ctx: Context,
    tx: Sender<HotkeyEvent>,
    mods: Mods,
    key: PrimaryKey,
    ready: Sender<Result<u32, String>>,
) {
    let flags = combo_modifiers(mods) | MOD_NOREPEAT;
    let vkey = combo_vkey(key);
    // 注册失败最常见的原因：组合键已被系统或其他程序占用（如 IME、录屏软件）。
    if let Err(err) = unsafe { RegisterHotKey(None, WAKE_HOTKEY_ID, flags, vkey) } {
        let _ = ready.send(Err(format!("RegisterHotKey({vkey:#x}) 失败: {err}")));
        return;
    }

    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe { DispatchMessageW(&message) };
        if message.message == WM_HOTKEY && message.wParam.0 as i32 == WAKE_HOTKEY_ID {
            let _ = tx.send(HotkeyEvent::Toggle);
            ctx.request_repaint();
        }
    }

    unsafe {
        let _ = UnregisterHotKey(None, WAKE_HOTKEY_ID);
    }
}

fn combo_modifiers(mods: Mods) -> HOT_KEY_MODIFIERS {
    let mut flags = HOT_KEY_MODIFIERS(0);
    if mods.ctrl {
        flags |= MOD_CONTROL;
    }
    if mods.alt {
        flags |= MOD_ALT;
    }
    if mods.shift {
        flags |= MOD_SHIFT;
    }
    flags
}

/// [`PrimaryKey`] → Win32 虚拟键码（与解析规则一一对应）。
fn combo_vkey(key: PrimaryKey) -> u32 {
    match key {
        PrimaryKey::Letter(ch) => u32::from(ch.to_ascii_uppercase() as u8), // 'A'..='Z'
        PrimaryKey::Digit(d) => u32::from(b'0' + d.clamp(0, 9)),            // 0x30..=0x39
        PrimaryKey::Function(n) => 0x70 + u32::from(n.clamp(1, 12)) - 1,    // VK_F1..VK_F12
        PrimaryKey::Space => u32::from(vk::SPACE),
    }
}

/// Tracks the Ctrl+G chord.
///
/// Left and right Ctrl are tracked independently; Ctrl counts as held while
/// either one is down. `fired` latches after the chord fires, so the
/// auto-repeat key-downs Windows generates while G is held emit no further
/// events. The latch clears on G key-up or when the last held Ctrl is
/// released, so a fresh chord can fire again.
///
/// This is deliberately free of Windows APIs so it can be unit tested.
#[derive(Debug, Default)]
struct CtrlGState {
    left_ctrl_down: bool,
    right_ctrl_down: bool,
    fired: bool,
    generic_ctrl_down: bool,
}

impl CtrlGState {
    fn ctrl_down(&self) -> bool {
        self.left_ctrl_down || self.right_ctrl_down || self.generic_ctrl_down
    }

    /// Updates modifier state and reports whether this key-down completes a
    /// fresh Ctrl+G chord.
    fn on_key_down(&mut self, vkey: u16) -> bool {
        match vkey {
            vk::CONTROL => self.generic_ctrl_down = true,
            vk::LCONTROL => self.left_ctrl_down = true,
            vk::RCONTROL => self.right_ctrl_down = true,
            G_VKEY => {
                if self.ctrl_down() && !self.fired {
                    self.fired = true;
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// Releases a tracked key. Releasing G or the last held Ctrl re-arms the
    /// chord; key-ups for untracked keys change nothing.
    fn on_key_up(&mut self, vkey: u16) {
        match vkey {
            vk::CONTROL => {
                self.generic_ctrl_down = false;
                if !self.ctrl_down() {
                    self.fired = false;
                }
            }
            vk::LCONTROL => {
                self.left_ctrl_down = false;
                if !self.ctrl_down() {
                    self.fired = false;
                }
            }
            vk::RCONTROL => {
                self.right_ctrl_down = false;
                if !self.ctrl_down() {
                    self.fired = false;
                }
            }
            G_VKEY => self.fired = false,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combo_vkeys_match_parse_rules() {
        assert_eq!(combo_vkey(PrimaryKey::Letter('q')), 0x51); // 'Q'
        assert_eq!(combo_vkey(PrimaryKey::Letter('a')), 0x41);
        assert_eq!(combo_vkey(PrimaryKey::Digit(0)), 0x30);
        assert_eq!(combo_vkey(PrimaryKey::Digit(9)), 0x39);
        assert_eq!(combo_vkey(PrimaryKey::Function(1)), 0x70); // VK_F1
        assert_eq!(combo_vkey(PrimaryKey::Function(12)), 0x7B); // VK_F12
        assert_eq!(combo_vkey(PrimaryKey::Space), 0x20); // VK_SPACE
    }

    #[test]
    fn combo_modifiers_map_to_win32_flags() {
        let flags = combo_modifiers(Mods { ctrl: true, alt: true, shift: true });
        assert_eq!(flags, MOD_CONTROL | MOD_ALT | MOD_SHIFT);
        assert_eq!(combo_modifiers(Mods::default()), HOT_KEY_MODIFIERS(0));
    }

    // 注意：HotkeyService::start 会安装真实的系统级钩子/热键，单测绝不直接
    // 调用它（约束：不运行真实 hook）；字符串解析与状态机行为全部在
    // crate::hotkey 与下方纯结构测试里覆盖。

    // —— CtrlGState（原 Raw Input 版测试原样保留，语义不变） ——

    #[test]
    fn ctrl_g_fires_once() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(vk::LCONTROL));
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn ctrl_g_auto_repeat_does_not_fire_again() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(vk::RCONTROL));
        assert!(state.on_key_down(G_VKEY));
        // Holding G generates repeated key-downs until it is released.
        assert!(!state.on_key_down(G_VKEY));
        assert!(!state.on_key_down(G_VKEY));
    }

    #[test]
    fn g_release_rearms_ctrl_g() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(vk::LCONTROL));
        assert!(state.on_key_down(G_VKEY));
        state.on_key_up(G_VKEY);
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn ctrl_release_rearms_ctrl_g() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(vk::LCONTROL));
        assert!(state.on_key_down(G_VKEY));
        state.on_key_up(vk::LCONTROL);

        // Ctrl pressed again while G is still held; the next auto-repeat
        // key-down counts as a fresh chord.
        assert!(!state.on_key_down(vk::LCONTROL));
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn g_without_ctrl_does_not_fire() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(G_VKEY));
        state.on_key_up(G_VKEY);
        assert!(!state.on_key_down(G_VKEY));
    }

    #[test]
    fn generic_ctrl_g_fires_once() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(vk::CONTROL));
        assert!(state.on_key_down(G_VKEY));
        assert!(!state.on_key_down(G_VKEY));
        state.on_key_up(G_VKEY);
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn one_ctrl_release_keeps_chord_latched_while_other_ctrl_held() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(vk::LCONTROL));
        assert!(!state.on_key_down(vk::RCONTROL));
        assert!(state.on_key_down(G_VKEY));

        // The chord never stopped being active, so auto-repeat stays ignored.
        state.on_key_up(vk::LCONTROL);
        assert!(!state.on_key_down(G_VKEY));
    }
}
