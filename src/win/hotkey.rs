//! Global hotkeys built on the Raw Input API: double-Space toggles the
//! launcher, Ctrl+G opens dialog search.
//!
//! `RegisterHotKey` cannot bind a bare key (it always requires modifiers), so
//! this service instead creates a hidden top-level window and registers for
//! keyboard raw input with `RIDEV_INPUTSINK`. A dedicated thread owns the
//! window and runs its message loop.
//!
//! Raw Input only *observes* input: WM_INPUT is forwarded to `DefWindowProcW`
//! and `RIDEV_NOLEGACY` is not set, so every key still reaches the focused
//! application. In particular Ctrl+G is not consumed here: the foreground
//! window (including a focused search dialog) receives it as well.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::Context;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_G, VK_LCONTROL, VK_RCONTROL, VK_SPACE};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RAWKEYBOARD, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
    PostThreadMessageW, RegisterClassW, TranslateMessage, UnregisterClassW, HCURSOR, HICON, MSG,
    RI_KEY_BREAK, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_INPUT, WNDCLASSW, WNDCLASS_STYLES,
    WS_POPUP,
};

const WINDOW_CLASS: PCWSTR = w!("lstr.RawInputHotkey");

/// Maximum delay between the two Space key releases of a double tap.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(350);

/// Virtual-key code for Space (matches `VK_SPACE`).
const SPACE_VKEY: u16 = VK_SPACE.0;

/// Virtual-key codes for the Ctrl+G chord.
const CTRL_VKEY: u16 = VK_CONTROL.0;
const LCTRL_VKEY: u16 = VK_LCONTROL.0;
const RCTRL_VKEY: u16 = VK_RCONTROL.0;
const G_VKEY: u16 = VK_G.0;

/// Usage page 1 / usage 6 is the HID "generic desktop: keyboard" collection.
const HID_USAGE_PAGE_GENERIC: u16 = 1;
const HID_USAGE_GENERIC_KEYBOARD: u16 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Toggle,
    DialogSearch,
    Quit,
}

/// A running Raw Input hotkey listener.
///
/// Dropping the service stops the listener thread and joins it.
pub struct HotkeyService {
    rx: Receiver<HotkeyEvent>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HotkeyService {
    /// Starts the listener thread and waits until its window is ready.
    pub fn start(ctx: eframe::egui::Context) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let join = thread::Builder::new()
            .name("lstr-hotkey".to_string())
            .spawn(move || run(ctx, tx, ready_tx))
            .map_err(|err| format!("failed to spawn hotkey thread: {err}"))?;

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
                Err("hotkey thread exited before it was ready".to_string())
            }
        }
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
        // Wake the message loop; it tears down its window and class on exit.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Tracks the previous Space release for the double-tap gesture.
///
/// This is deliberately free of Windows APIs so it can be unit tested.
#[derive(Debug, Default)]
struct DoubleTapState {
    first_release: Option<Instant>,
}

impl DoubleTapState {
    /// A non-Space key press cancels a pending first tap. Space key-downs are
    /// ignored so auto-repeat does not break an in-progress gesture.
    fn on_key_down(&mut self, vkey: u16) {
        if vkey != SPACE_VKEY {
            self.first_release = None;
        }
    }

    /// Records a key release and reports whether it completes a double tap.
    fn on_key_up(&mut self, vkey: u16, now: Instant) -> bool {
        if vkey != SPACE_VKEY {
            self.first_release = None;
            return false;
        }

        if let Some(first) = self.first_release.take() {
            if now.saturating_duration_since(first) <= DOUBLE_TAP_WINDOW {
                return true;
            }
        }

        // First tap, or a too-slow second tap that starts a new gesture.
        self.first_release = Some(now);
        false
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
            CTRL_VKEY => self.generic_ctrl_down = true,
            LCTRL_VKEY => self.left_ctrl_down = true,
            RCTRL_VKEY => self.right_ctrl_down = true,
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
            CTRL_VKEY => {
                self.generic_ctrl_down = false;
                if !self.ctrl_down() {
                    self.fired = false;
                }
            }
            LCTRL_VKEY => {
                self.left_ctrl_down = false;
                if !self.ctrl_down() {
                    self.fired = false;
                }
            }
            RCTRL_VKEY => {
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

/// State shared with the window procedure on the listener thread.
struct Handler {
    tap: DoubleTapState,
    ctrl_g: CtrlGState,
    tx: Sender<HotkeyEvent>,
    ctx: Context,
}

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

struct WindowSetup {
    hwnd: HWND,
    hinstance: HINSTANCE,
}

fn run(ctx: Context, tx: Sender<HotkeyEvent>, ready: Sender<Result<u32, String>>) {
    let thread_id = unsafe { GetCurrentThreadId() };

    let setup = match unsafe { create_hidden_window() } {
        Ok(setup) => setup,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    HANDLER.with(|cell| {
        *cell.borrow_mut() = Some(Handler {
            tap: DoubleTapState::default(),
            ctrl_g: CtrlGState::default(),
            tx,
            ctx,
        });
    });

    if let Err(err) = unsafe { register_keyboard(setup.hwnd) } {
        HANDLER.with(|cell| {
            cell.borrow_mut().take();
        });
        unsafe { cleanup_window(&setup) };
        let _ = ready.send(Err(err));
        return;
    }

    let _ = ready.send(Ok(thread_id));

    unsafe { message_loop() };

    HANDLER.with(|cell| {
        cell.borrow_mut().take();
    });
    unsafe { cleanup_window(&setup) };
}

/// Registers the window class and creates the hidden top-level window.
///
/// The window is a `WS_POPUP` window without `WS_VISIBLE`, not an
/// `HWND_MESSAGE` window: message-only windows do not receive raw input.
unsafe fn create_hidden_window() -> Result<WindowSetup, String> {
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|err| format!("GetModuleHandleW failed: {err}"))?;
    let hinstance = HINSTANCE::from(module);

    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: WINDOW_CLASS,
    };

    if unsafe { RegisterClassW(&wc) } == 0 {
        let error = unsafe { GetLastError() };
        return Err(format!("RegisterClassW failed: {}", error.0));
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            WINDOW_CLASS,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
    };

    match hwnd {
        Ok(hwnd) => Ok(WindowSetup { hwnd, hinstance }),
        Err(err) => {
            unsafe {
                let _ = UnregisterClassW(WINDOW_CLASS, Some(hinstance));
            }
            Err(format!("CreateWindowExW failed: {err}"))
        }
    }
}

/// Subscribes the window to keyboard raw input even while it is in the
/// background. `RIDEV_INPUTSINK` requires a target window handle.
unsafe fn register_keyboard(hwnd: HWND) -> Result<(), String> {
    let device = RAWINPUTDEVICE {
        usUsagePage: HID_USAGE_PAGE_GENERIC,
        usUsage: HID_USAGE_GENERIC_KEYBOARD,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };

    unsafe {
        RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32)
            .map_err(|err| format!("RegisterRawInputDevices failed: {err}"))
    }
}

unsafe fn message_loop() {
    loop {
        let mut msg = MSG::default();
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 == 0 {
            // WM_QUIT: the window was destroyed and the thread should exit.
            break;
        }
        if result.0 == -1 {
            // GetMessageW failure; nothing sensible left to do.
            break;
        }
        // WM_CLOSE sent with PostThreadMessageW has no window handle.
        if msg.hwnd.is_invalid() && msg.message == WM_CLOSE {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn cleanup_window(setup: &WindowSetup) {
    unsafe {
        let _ = DestroyWindow(setup.hwnd);
        let _ = UnregisterClassW(WINDOW_CLASS, Some(setup.hinstance));
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // panic 穿透 extern "system" 边界会触发 fast-fail abort（0xc0000409），
    // 整个程序闪退。这里吞掉并交给 panic hook 记录，窗口继续存活。
    let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match msg {
            WM_INPUT => {
                // Raw Input observes but does not consume the key: the event is
                // still delivered to the focused application, and DefWindowProcW
                // must run so the system can clean up the raw input buffer.
                unsafe { handle_raw_input(lparam) };
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }));
    match handled {
        Ok(result) => result,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn handle_raw_input(lparam: LPARAM) {
    let Some((vkey, is_key_up)) = (unsafe { read_keyboard_event(lparam) }) else {
        return;
    };

    let event = HANDLER.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(handler) = guard.as_mut() else {
            return None;
        };
        if is_key_up {
            let toggled = handler.tap.on_key_up(vkey, Instant::now());
            handler.ctrl_g.on_key_up(vkey);
            toggled.then_some(HotkeyEvent::Toggle)
        } else {
            handler.tap.on_key_down(vkey);
            handler
                .ctrl_g
                .on_key_down(vkey)
                .then_some(HotkeyEvent::DialogSearch)
        }
    });

    if let Some(event) = event {
        HANDLER.with(|cell| {
            if let Some(handler) = cell.borrow().as_ref() {
                let _ = handler.tx.send(event);
                handler.ctx.request_repaint();
            }
        });
    }
}

/// Reads the virtual key and up/down flag from a `WM_INPUT` message.
unsafe fn read_keyboard_event(lparam: LPARAM) -> Option<(u16, bool)> {
    let hrawinput = HRAWINPUT(lparam.0 as *mut c_void);
    let header_size = size_of::<RAWINPUTHEADER>() as u32;

    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    let copied = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            Some(&mut raw as *mut RAWINPUT as *mut c_void),
            &mut size,
            header_size,
        )
    };
    if copied == 0 || copied == u32::MAX {
        return None;
    }

    // A keyboard RAWINPUT is smaller than RAWINPUT, so use the returned size
    // to make sure the keyboard payload was actually copied.
    if raw.header.dwType != RIM_TYPEKEYBOARD.0
        || (size as usize) < offset_of!(RAWINPUT, data) + size_of::<RAWKEYBOARD>()
    {
        return None;
    }

    let keyboard = unsafe { raw.data.keyboard };
    // 0x00 / 0xFF mark pseudo keys (e.g. overrun errors), not real input.
    if keyboard.VKey == 0 || keyboard.VKey == 0xFF {
        return None;
    }

    let is_key_up = u32::from(keyboard.Flags) & RI_KEY_BREAK != 0;
    Some((keyboard.VKey, is_key_up))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHER_VKEY: u16 = 0x41; // 'A'

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn double_tap_within_window_toggles_once() {
        let mut tap = DoubleTapState::default();
        let t0 = Instant::now();

        assert!(!tap.on_key_up(SPACE_VKEY, t0));
        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 349)));

        // The pair is consumed: the next release starts a fresh tap.
        assert!(!tap.on_key_up(SPACE_VKEY, at(t0, 400)));
        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 600)));
    }

    #[test]
    fn double_tap_at_window_boundary_toggles() {
        let mut tap = DoubleTapState::default();
        let t0 = Instant::now();

        assert!(!tap.on_key_up(SPACE_VKEY, t0));
        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 350)));
    }

    #[test]
    fn slow_second_tap_does_not_toggle() {
        let mut tap = DoubleTapState::default();
        let t0 = Instant::now();

        assert!(!tap.on_key_up(SPACE_VKEY, t0));
        assert!(!tap.on_key_up(SPACE_VKEY, at(t0, 351)));

        // The slow release becomes the start of a new double tap.
        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 500)));
    }

    #[test]
    fn other_key_down_resets_pending_tap() {
        let mut tap = DoubleTapState::default();
        let t0 = Instant::now();

        assert!(!tap.on_key_up(SPACE_VKEY, t0));
        tap.on_key_down(OTHER_VKEY);

        assert!(!tap.on_key_up(SPACE_VKEY, at(t0, 150)));
        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 250)));
    }

    #[test]
    fn other_key_up_resets_pending_tap() {
        let mut tap = DoubleTapState::default();
        let t0 = Instant::now();

        assert!(!tap.on_key_up(SPACE_VKEY, t0));
        assert!(!tap.on_key_up(OTHER_VKEY, at(t0, 100)));

        assert!(!tap.on_key_up(SPACE_VKEY, at(t0, 150)));
        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 250)));
    }

    #[test]
    fn space_auto_repeat_key_down_does_not_reset() {
        let mut tap = DoubleTapState::default();
        let t0 = Instant::now();

        assert!(!tap.on_key_up(SPACE_VKEY, t0));
        // Holding Space generates repeated key-downs before the second release.
        tap.on_key_down(SPACE_VKEY);
        tap.on_key_down(SPACE_VKEY);
        tap.on_key_down(SPACE_VKEY);

        assert!(tap.on_key_up(SPACE_VKEY, at(t0, 120)));
    }

    #[test]
    fn ctrl_g_fires_once() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(LCTRL_VKEY));
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn ctrl_g_auto_repeat_does_not_fire_again() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(RCTRL_VKEY));
        assert!(state.on_key_down(G_VKEY));
        // Holding G generates repeated key-downs until it is released.
        assert!(!state.on_key_down(G_VKEY));
        assert!(!state.on_key_down(G_VKEY));
    }

    #[test]
    fn g_release_rearms_ctrl_g() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(LCTRL_VKEY));
        assert!(state.on_key_down(G_VKEY));
        state.on_key_up(G_VKEY);
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn ctrl_release_rearms_ctrl_g() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(LCTRL_VKEY));
        assert!(state.on_key_down(G_VKEY));
        state.on_key_up(LCTRL_VKEY);

        // Ctrl pressed again while G is still held; the next auto-repeat
        // key-down counts as a fresh chord.
        assert!(!state.on_key_down(LCTRL_VKEY));
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

        assert!(!state.on_key_down(CTRL_VKEY));
        assert!(state.on_key_down(G_VKEY));
        assert!(!state.on_key_down(G_VKEY));
        state.on_key_up(G_VKEY);
        assert!(state.on_key_down(G_VKEY));
    }

    #[test]
    fn one_ctrl_release_keeps_chord_latched_while_other_ctrl_held() {
        let mut state = CtrlGState::default();

        assert!(!state.on_key_down(LCTRL_VKEY));
        assert!(!state.on_key_down(RCTRL_VKEY));
        assert!(state.on_key_down(G_VKEY));

        // The chord never stopped being active, so auto-repeat stays ignored.
        state.on_key_up(LCTRL_VKEY);
        assert!(!state.on_key_down(G_VKEY));
    }
}
