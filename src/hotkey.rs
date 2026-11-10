//! 唤起热键的纯逻辑定义：配置字符串解析/编码 + 双击检测器状态机。
//!
//! 本模块刻意不依赖 Windows API，全部可单元测试；系统级注册
//! （LL 钩子观察 / `RegisterHotKey`）见 [`crate::win::hotkey`]。
//!
//! 配置字符串沿用 `double-space` 时代的风格：
//! - 双击修饰键：`double-ctrl` / `double-shift` / `double-alt`
//!   （legacy 兼容：`double-space`，双击空格）；
//! - 组合键：`ctrl+space`、`alt+q`、`ctrl+shift+f5`、`f1`（裸 F 键）。
//!   修饰键顺序编码时固定为 ctrl,alt,shift（解析时不限顺序）。
//!
//! 非法输入一律回退 [`DEFAULT_HOTKEY`]（见 [`normalize_hotkey_or_default`]）。

use std::time::{Duration, Instant};

/// 默认唤起热键：双击 Ctrl（Listary 式）。
pub const DEFAULT_HOTKEY: &str = "double-ctrl";

/// 双击检测窗口：两次 keydown 间隔的上限。
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// LL 钩子 / `KBDLLHOOKSTRUCT` 会出现的虚拟键码。
/// 自持常量，避免这个纯逻辑模块依赖 windows crate。
pub mod vk {
    pub const SHIFT: u16 = 0x10;
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const CONTROL: u16 = 0x11;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const MENU: u16 = 0x12;
    pub const LMENU: u16 = 0xA4;
    pub const RMENU: u16 = 0xA5;
    pub const SPACE: u16 = 0x20;
}

/// 双击目标键。修饰键左右变体等价；`Space` 是 legacy `double-space`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapKey {
    Ctrl,
    Shift,
    Alt,
    Space,
}

impl TapKey {
    /// 该键在 LL 钩子里可能出现的 vkey 组：`[通用, 左变体, 右变体]`
    /// （LL 钩子对物理修饰键上报左/右变体，通用码来自注入事件）。
    pub fn vkeys(self) -> [u16; 3] {
        match self {
            TapKey::Ctrl => [vk::CONTROL, vk::LCONTROL, vk::RCONTROL],
            TapKey::Shift => [vk::SHIFT, vk::LSHIFT, vk::RSHIFT],
            TapKey::Alt => [vk::MENU, vk::LMENU, vk::RMENU],
            TapKey::Space => [vk::SPACE, 0, 0],
        }
    }

    fn from_config_token(token: &str) -> Option<TapKey> {
        match token {
            "ctrl" => Some(TapKey::Ctrl),
            "shift" => Some(TapKey::Shift),
            "alt" => Some(TapKey::Alt),
            "space" => Some(TapKey::Space),
            _ => None,
        }
    }

    fn config_name(self) -> &'static str {
        match self {
            TapKey::Ctrl => "ctrl",
            TapKey::Shift => "shift",
            TapKey::Alt => "alt",
            TapKey::Space => "space",
        }
    }

    /// 展示用名（设置页录制提示等）。
    pub fn display_name(self) -> &'static str {
        match self {
            TapKey::Ctrl => "Ctrl",
            TapKey::Shift => "Shift",
            TapKey::Alt => "Alt",
            TapKey::Space => "Space",
        }
    }
}

/// 组合键的主键（普通键）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimaryKey {
    /// `'a'..='z'`（配置/编码用小写，展示用大写）。
    Letter(char),
    /// `0..=9`。
    Digit(u8),
    /// `1..=12`。
    Function(u8),
    Space,
}

impl PrimaryKey {
    fn from_config_token(token: &str) -> Option<PrimaryKey> {
        let bytes = token.as_bytes();
        if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
            return Some(PrimaryKey::Letter(bytes[0] as char));
        }
        if bytes.len() == 1 && bytes[0].is_ascii_digit() {
            return Some(PrimaryKey::Digit(bytes[0] - b'0'));
        }
        if bytes.len() >= 2 && bytes[0] == b'f' && token[1..].bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = token[1..].parse::<u8>() {
                if (1..=12).contains(&n) {
                    return Some(PrimaryKey::Function(n));
                }
            }
        }
        match token {
            "space" => Some(PrimaryKey::Space),
            _ => None,
        }
    }

    fn config_token(self) -> String {
        match self {
            PrimaryKey::Letter(ch) => ch.to_string(),
            PrimaryKey::Digit(d) => d.to_string(),
            PrimaryKey::Function(n) => format!("f{n}"),
            PrimaryKey::Space => "space".to_string(),
        }
    }

    fn display_name(self) -> String {
        match self {
            PrimaryKey::Letter(ch) => ch.to_ascii_uppercase().to_string(),
            PrimaryKey::Digit(d) => d.to_string(),
            PrimaryKey::Function(n) => format!("F{n}"),
            PrimaryKey::Space => "Space".to_string(),
        }
    }
}

/// 组合键的修饰键集合（Win 键不收录：系统保留太多，误录会全局打架）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// 结构化的唤起热键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeHotkey {
    /// 双击同一修饰键（或 legacy 双击空格）；LL 钩子观察路径。
    DoubleTap(TapKey),
    /// 修饰键 + 普通键（或裸 F 键）；`RegisterHotKey` 路径。
    Combo { mods: Mods, key: PrimaryKey },
}

impl WakeHotkey {
    /// 编码为配置字符串（规范形式）。
    pub fn encode(self) -> String {
        match self {
            WakeHotkey::DoubleTap(key) => format!("double-{}", key.config_name()),
            WakeHotkey::Combo { mods, key } => {
                let mut text = String::with_capacity(16);
                if mods.ctrl {
                    text.push_str("ctrl+");
                }
                if mods.alt {
                    text.push_str("alt+");
                }
                if mods.shift {
                    text.push_str("shift+");
                }
                text.push_str(&key.config_token());
                text
            }
        }
    }
}

/// 解析配置字符串；不认识的结构返回 `None`（由调用方决定回退）。
pub fn parse_hotkey(raw: &str) -> Option<WakeHotkey> {
    let text = raw.trim().to_ascii_lowercase();
    if let Some(rest) = text.strip_prefix("double-") {
        return TapKey::from_config_token(rest.trim()).map(WakeHotkey::DoubleTap);
    }

    let mut mods = Mods::default();
    let mut primary: Option<PrimaryKey> = None;
    for token in text.split('+') {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        if primary.is_some() {
            // 主键后面还跟 token（"ctrl+q+alt"）：结构非法。
            return None;
        }
        match token {
            "ctrl" | "control" => {
                if mods.ctrl {
                    return None; // 重复修饰键
                }
                mods.ctrl = true;
            }
            "alt" => {
                if mods.alt {
                    return None;
                }
                mods.alt = true;
            }
            "shift" => {
                if mods.shift {
                    return None;
                }
                mods.shift = true;
            }
            _ => primary = Some(PrimaryKey::from_config_token(token)?),
        }
    }

    let key = primary?;
    // 校验：字母/数字/空格必须有 ctrl 或 alt 修饰。shift 单独修饰就是打
    // 大写字母/符号本身，全局注册会把正常打字整个吞掉；裸 F 键允许。
    if matches!(key, PrimaryKey::Function(_))
        || mods.ctrl
        || mods.alt
    {
        Some(WakeHotkey::Combo { mods, key })
    } else {
        None
    }
}

/// 规范化：合法输入返回规范编码字符串，非法返回 `None`。
pub fn normalize_hotkey(raw: &str) -> Option<String> {
    parse_hotkey(raw).map(|wake| wake.encode())
}

/// 非法输入回退默认热键（config 加载路径使用）。
pub fn normalize_hotkey_or_default(raw: &str) -> String {
    normalize_hotkey(raw).unwrap_or_else(|| DEFAULT_HOTKEY.to_string())
}

/// 设置页展示文案：`双击 Ctrl` / `Ctrl+Q` / `F1`。
/// 无法解析的串按原样展示（不应发生：load 时已归一化）。
pub fn display_hotkey(raw: &str) -> String {
    match parse_hotkey(raw) {
        Some(WakeHotkey::DoubleTap(key)) => format!("双击 {}", key.display_name()),
        Some(WakeHotkey::Combo { mods, key }) => {
            let mut parts: Vec<String> = Vec::with_capacity(4);
            if mods.ctrl {
                parts.push("Ctrl".to_string());
            }
            if mods.alt {
                parts.push("Alt".to_string());
            }
            if mods.shift {
                parts.push("Shift".to_string());
            }
            parts.push(key.display_name());
            parts.join("+")
        }
        None => raw.to_string(),
    }
}

/// 双击检测器（Listary 式）状态机：输入按键事件序列，输出是否触发唤起。
///
/// 规则（纯逻辑，无 Windows API）：
/// - 同一目标键两次 keydown 间隔 ≤ [`DOUBLE_TAP_WINDOW`] 触发，**第二次按下即触发**；
/// - 第一次 keydown 必须先 keyup：按住不松的第二次 keydown（长按 / auto-repeat）不触发，
///   防止「按住 Ctrl 再按别的」类技巧键误触；
/// - 两次之间任何其他键的 keydown / keyup 都重置计时（Ctrl+C / Ctrl+V 等组合完全不受影响）；
/// - 目标键左右变体等价（[`TapKey::vkeys`]）；
/// - 间隔超窗的第二落不触发，但会作为新手势的第一次落下重新计时。
///
/// `CallNextHookEx` 由调用方负责：本检测器只观察，不吞键。
#[derive(Debug)]
pub struct DoubleTapDetector {
    targets: [u16; 3],
    window: Duration,
    /// 第一次 keydown 的时刻；`None` = 空闲。
    first_down: Option<Instant>,
    /// 第一次 keydown 之后是否已经 keyup（armed：等第二次按下）。
    released: bool,
}

impl DoubleTapDetector {
    pub fn new(targets: [u16; 3]) -> Self {
        Self {
            targets,
            window: DOUBLE_TAP_WINDOW,
            first_down: None,
            released: false,
        }
    }

    pub fn reset(&mut self) {
        self.first_down = None;
        self.released = false;
    }

    fn is_target(&self, vkey: u16) -> bool {
        self.targets.contains(&vkey)
    }

    /// 输入一个按键事件；返回 `true` 表示双击触发（调用方发送唤起事件）。
    pub fn on_key(&mut self, vkey: u16, is_down: bool, now: Instant) -> bool {
        if !self.is_target(vkey) {
            // 其他任何键（按下或抬起）都打断手势：组合键中间夹了别的键不算双击。
            self.reset();
            return false;
        }

        if is_down {
            match self.first_down {
                None => {
                    self.first_down = Some(now);
                    self.released = false;
                }
                Some(first) if self.released => {
                    if now.saturating_duration_since(first) <= self.window {
                        // 第二次按下：触发并复位，连续三击不会立刻再触发。
                        self.reset();
                        return true;
                    }
                    // 太慢：这次落下是新手势的第一落。
                    self.first_down = Some(now);
                    self.released = false;
                }
                Some(_) => {
                    // 仍按着（长按 / auto-repeat 的重复 keydown）：既不触发也不重置。
                }
            }
            return false;
        }

        // 目标键 keyup：窗口内抬起 → armed 等第二落；拖太久 → 整个手势过期。
        if let Some(first) = self.first_down {
            if now.saturating_duration_since(first) <= self.window {
                self.released = true;
            } else {
                self.reset();
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn detector(tap: TapKey) -> DoubleTapDetector {
        DoubleTapDetector::new(tap.vkeys())
    }

    /// 快捷构造：向检测器喂一次 keydown/keyup。
    fn tap(det: &mut DoubleTapDetector, vkey: u16, down: bool, t: Instant) -> bool {
        det.on_key(vkey, down, t)
    }

    // —— 双击检测器：间隔边界 ——

    #[test]
    fn double_ctrl_within_window_triggers_on_second_down() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 40)));
        // 第二次按下即触发（不等到 keyup）。
        assert!(tap(&mut det, vk::RCONTROL, true, at(t0, 399)));
    }

    #[test]
    fn double_ctrl_at_window_boundary_triggers() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 50)));
        assert!(tap(&mut det, vk::RCONTROL, true, at(t0, 400)));
    }

    #[test]
    fn double_ctrl_past_window_does_not_trigger_and_restarts_gesture() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 50)));
        // 401ms：超过窗口，不触发；但这落成为新手势的第一落。
        assert!(!tap(&mut det, vk::RCONTROL, true, at(t0, 401)));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 450)));
        assert!(tap(&mut det, vk::RCONTROL, true, at(t0, 700)));
    }

    // —— 双击检测器：长按 / auto-repeat ——

    #[test]
    fn long_press_second_down_does_not_trigger() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        // 第一次 keydown 未 keyup 就来了第二次 keydown（换手指/长按）：不触发。
        assert!(!tap(&mut det, vk::RCONTROL, true, at(t0, 100)));
        assert!(!tap(&mut det, vk::RCONTROL, false, at(t0, 150)));
        // 抬起后才来的第二落，间隔仍在窗口内：触发。
        assert!(tap(&mut det, vk::LCONTROL, true, at(t0, 200)));
    }

    #[test]
    fn auto_repeat_key_downs_do_not_trigger_or_reset() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        // 按住不放，Windows 会持续发重复 keydown。
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 40)));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 80)));
        // 松开时已超窗 → 手势过期，不给 armed 留机会。
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 500)));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 520)));
        // 新手势正常工作。
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 560)));
        assert!(tap(&mut det, vk::LCONTROL, true, at(t0, 600)));
    }

    #[test]
    fn long_press_past_window_then_tap_is_fresh_gesture() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 300))); // 长按中的重复
        // 500ms 才松开：超窗，keyup 直接过期。
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 500)));
        // 700ms 的落下是新手势的第一落，不触发；补完一落一抬才触发。
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 700)));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 730)));
        assert!(tap(&mut det, vk::LCONTROL, true, at(t0, 760)));
    }

    // —— 双击检测器：中间杂键 ——

    #[test]
    fn other_key_down_between_taps_resets() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 40)));
        // 中间按了 'C'（Ctrl+C 复制）：重置。
        assert!(!tap(&mut det, 0x43, true, at(t0, 80)));
        assert!(!tap(&mut det, 0x43, false, at(t0, 90)));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 120)));
    }

    #[test]
    fn other_modifier_down_between_taps_resets() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 40)));
        // 中间夹 Shift（Ctrl+Shift 组合的前半）：重置。
        assert!(!tap(&mut det, vk::LSHIFT, true, at(t0, 80)));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 120)));
    }

    #[test]
    fn stray_other_key_up_also_resets() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 40)));
        // 钩子启动前就按住的键此刻才松开：一样重置（与旧 double-space 行为一致）。
        assert!(!tap(&mut det, 0x41, false, at(t0, 80)));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 120)));
    }

    // —— 双击检测器：左右变体等价 ——

    #[test]
    fn left_and_right_ctrl_are_interchangeable() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::RCONTROL, true, t0));
        assert!(!tap(&mut det, vk::RCONTROL, false, at(t0, 40)));
        assert!(tap(&mut det, vk::LCONTROL, true, at(t0, 100)));
    }

    #[test]
    fn generic_ctrl_vk_also_counts() {
        // 注入事件（SendInput）常带通用 VK_CONTROL(0x11)。
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::CONTROL, true, t0));
        assert!(!tap(&mut det, vk::CONTROL, false, at(t0, 40)));
        assert!(tap(&mut det, vk::CONTROL, true, at(t0, 100)));
    }

    // —— 双击检测器：触发后状态 ——

    #[test]
    fn trigger_resets_state_so_next_pair_starts_fresh() {
        let mut det = detector(TapKey::Ctrl);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 40)));
        assert!(tap(&mut det, vk::LCONTROL, true, at(t0, 80)));
        // 三连击：第三次按下是新手势的第一落，不再触发。
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 100)));
        assert!(!tap(&mut det, vk::LCONTROL, true, at(t0, 130)));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 150)));
        assert!(tap(&mut det, vk::LCONTROL, true, at(t0, 180)));
    }

    // —— 双击检测器：其他 TapKey ——

    #[test]
    fn double_shift_works_same_way() {
        let mut det = detector(TapKey::Shift);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::LSHIFT, true, t0));
        assert!(!tap(&mut det, vk::LSHIFT, false, at(t0, 40)));
        assert!(tap(&mut det, vk::RSHIFT, true, at(t0, 100)));

        // Ctrl 不该触发 double-shift。
        let mut det = detector(TapKey::Shift);
        assert!(!tap(&mut det, vk::LCONTROL, true, t0));
        assert!(!tap(&mut det, vk::LCONTROL, false, at(t0, 40)));
        assert!(!tap(&mut det, vk::LSHIFT, true, at(t0, 100)));
    }

    #[test]
    fn double_space_legacy_still_works() {
        let mut det = detector(TapKey::Space);
        let t0 = Instant::now();

        assert!(!tap(&mut det, vk::SPACE, true, t0));
        assert!(!tap(&mut det, vk::SPACE, false, at(t0, 40)));
        assert!(tap(&mut det, vk::SPACE, true, at(t0, 100)));
    }

    // —— 配置字符串：解析 / 编码 / roundtrip ——

    #[test]
    fn parses_double_modifiers() {
        assert_eq!(
            parse_hotkey("double-ctrl"),
            Some(WakeHotkey::DoubleTap(TapKey::Ctrl))
        );
        assert_eq!(
            parse_hotkey("double-space"),
            Some(WakeHotkey::DoubleTap(TapKey::Space))
        );
        assert_eq!(parse_hotkey(" Double-ALT "), parse_hotkey("double-alt"));
    }

    #[test]
    fn parses_combos_case_insensitive_any_mod_order() {
        let ctrl_space = parse_hotkey("ctrl+space").unwrap();
        assert_eq!(
            ctrl_space,
            WakeHotkey::Combo {
                mods: Mods { ctrl: true, ..Mods::default() },
                key: PrimaryKey::Space
            }
        );
        assert_eq!(parse_hotkey("CTRL+SPACE"), Some(ctrl_space));
        // 修饰键顺序无关，编码归一为 ctrl+alt+shift。
        assert_eq!(parse_hotkey("shift+alt+ctrl+q"), parse_hotkey("ctrl+alt+shift+q"));
        // "control" 别名。
        assert_eq!(parse_hotkey("control+q"), parse_hotkey("ctrl+q"));
    }

    #[test]
    fn parses_bare_function_keys() {
        assert_eq!(
            parse_hotkey("f1"),
            Some(WakeHotkey::Combo {
                mods: Mods::default(),
                key: PrimaryKey::Function(1)
            })
        );
        assert_eq!(parse_hotkey("F12"), parse_hotkey("f12"));
    }

    #[test]
    fn rejects_invalid_hotkey_strings() {
        // 裸字母/数字/空格：会全局吞掉正常打字。
        assert_eq!(parse_hotkey("q"), None);
        assert_eq!(parse_hotkey("5"), None);
        assert_eq!(parse_hotkey("space"), None);
        // shift 单独修饰字母 = 打大写本身。
        assert_eq!(parse_hotkey("shift+q"), None);
        // 结构错误。
        assert_eq!(parse_hotkey("double"), None);
        assert_eq!(parse_hotkey("double-f1"), None);
        assert_eq!(parse_hotkey("double-ctrl+q"), None);
        assert_eq!(parse_hotkey("ctrl+"), None);
        assert_eq!(parse_hotkey("ctrl++q"), None);
        assert_eq!(parse_hotkey("ctrl+q+alt"), None);
        assert_eq!(parse_hotkey("ctrl+ctrl+q"), None);
        assert_eq!(parse_hotkey(""), None);
        assert_eq!(parse_hotkey("   "), None);
        assert_eq!(parse_hotkey("f13"), None);
        assert_eq!(parse_hotkey("ctrl+f13"), None);
        assert_eq!(parse_hotkey("win+q"), None);
    }

    #[test]
    fn encode_produces_canonical_form() {
        let wake = parse_hotkey("shift+ALT+ctrl+q").unwrap();
        assert_eq!(wake.encode(), "ctrl+alt+shift+q");
        assert_eq!(
            WakeHotkey::DoubleTap(TapKey::Ctrl).encode(),
            "double-ctrl"
        );
    }

    #[test]
    fn roundtrip_parse_encode_parse_is_stable() {
        for raw in [
            "double-ctrl",
            "double-shift",
            "double-alt",
            "double-space",
            "ctrl+space",
            "alt+q",
            "ctrl+alt+shift+f5",
            "f1",
            "ctrl+5",
            "alt+0",
            "ctrl+shift+z",
        ] {
            let wake = parse_hotkey(raw).expect(raw);
            let encoded = wake.encode();
            let reparsed = parse_hotkey(&encoded).expect(&encoded);
            assert_eq!(wake, reparsed, "roundtrip 失败: {raw} -> {encoded}");
            assert_eq!(encoded, reparsed.encode(), "编码应已是规范形: {raw}");
        }
    }

    #[test]
    fn normalize_returns_none_for_garbage_and_default_for_fallback() {
        assert_eq!(normalize_hotkey(" alt+q "), Some("alt+q".to_string()));
        assert_eq!(normalize_hotkey("bogus"), None);
        assert_eq!(normalize_hotkey("q"), None);
        assert_eq!(normalize_hotkey_or_default("ctrl+space"), "ctrl+space");
        assert_eq!(normalize_hotkey_or_default("bogus"), DEFAULT_HOTKEY);
        assert_eq!(normalize_hotkey_or_default(""), DEFAULT_HOTKEY);
    }

    #[test]
    fn display_text_is_human_readable() {
        assert_eq!(display_hotkey("double-ctrl"), "双击 Ctrl");
        assert_eq!(display_hotkey("double-space"), "双击 Space");
        assert_eq!(display_hotkey("ctrl+space"), "Ctrl+Space");
        assert_eq!(display_hotkey("alt+q"), "Alt+Q");
        assert_eq!(display_hotkey("ctrl+alt+shift+f5"), "Ctrl+Alt+Shift+F5");
        assert_eq!(display_hotkey("f1"), "F1");
        // 非法串原样回显（load 已归一化，正常不会出现）。
        assert_eq!(display_hotkey("???"), "???");
    }
}
