use crate::config::Config;
use crate::history::History;
use crate::hotkey::{display_hotkey, TapKey, WakeHotkey, DOUBLE_TAP_WINDOW};
use crate::model::{Entry, Kind, Scope};
use crate::search::filters::{FileGroup, LocationFilter, MatchToggle, SearchFilters, TimeFilter};
use crate::search::{self, SearchService};
use crate::ui::{self, Theme};
use crate::win::hotkey::{HotkeyEvent, HotkeyService};
use crate::win::hotkey_relay::HotkeyRelay;
use crate::win::tray::{TrayAction, TrayState};
use crate::win::dialog::{self, DialogInfo};
use crate::win::{shell, window};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Id, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 图标缓存：加载在后台线程进行，UI 先画占位图，图标到达后重绘。
/// 之前每次搜索新结果都要在 UI 线程串行跑 SHGetFileInfoW（12 个结果 ≈ 60-120ms 卡顿）。
struct IconStore {
    cache: HashMap<String, Option<egui::TextureHandle>>,
    pending: HashSet<String>,
    tx: std::sync::mpsc::Sender<IconJob>,
    rx: std::sync::mpsc::Receiver<(String, Option<egui::ColorImage>)>,
}

struct IconJob {
    key: String,
    path: String,
    generic: Option<u32>,
}

const GENERIC_FOLDER_ICON: u32 = 0x0000_0010; // FILE_ATTRIBUTE_DIRECTORY
const GENERIC_FILE_ICON: u32 = 0x0000_0080; // FILE_ATTRIBUTE_NORMAL

impl IconStore {
    fn new() -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<IconJob>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("lstr-icons".to_string())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let image = match job.generic {
                        Some(attrs) => crate::win::icons::load_icon_with_attrs(&job.path, 32, attrs),
                        None => crate::win::icons::load_icon(&job.path, 32),
                    };
                    if done_tx.send((job.key, image)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self {
            cache: HashMap::new(),
            pending: HashSet::new(),
            tx: job_tx,
            rx: done_rx,
        }
    }

    /// 把后台完成的图标转成纹理；有新图标时触发重绘。
    fn drain(&mut self, ctx: &egui::Context) {
        let mut loaded = false;
        while let Ok((key, image)) = self.rx.try_recv() {
            self.pending.remove(&key);
            let texture = image.map(|img| {
                ctx.load_texture(key.clone(), img, egui::TextureOptions::LINEAR)
            });
            self.cache.insert(key, texture);
            loaded = true;
        }
        if self.cache.len() > 400 {
            self.cache.clear();
            self.pending.clear();
        }
        if loaded {
            ctx.request_repaint();
        }
    }

    fn request(&mut self, key: String, entry: &Entry) {
        if self.cache.contains_key(&key) || !self.pending.insert(key.clone()) {
            return;
        }
        let generic = match entry.kind {
            // 类型图标（文件夹/按扩展名的文件）不访问磁盘，毫秒级。
            Kind::Folder => Some(GENERIC_FOLDER_ICON),
            Kind::File => Some(GENERIC_FILE_ICON),
            _ => None,
        };
        if self
            .tx
            .send(IconJob {
                key: key.clone(),
                path: entry.full_path(),
                generic,
            })
            .is_err()
        {
            // 后台线程不存在时允许下次重试。
            self.pending.remove(&key);
        }
    }

    fn get(&self, key: &str) -> Option<&egui::TextureHandle> {
        self.cache.get(key).and_then(Option::as_ref)
    }
}

fn icon_key(entry: &Entry) -> String {
    match entry.kind {
        // 文件夹共享一个图标，按扩展名共享文件图标，UWP/.lnk/程序按路径。
        Kind::Folder => "icon:folder".to_string(),
        Kind::File => match entry.name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => {
                format!("icon:file:{}", ext.to_lowercase())
            }
            _ => "icon:file".to_string(),
        },
        Kind::Command => String::new(),
        _ => format!("icon:{}", entry.id),
    }
}

const PAD: f32 = 20.0;
const SEARCH_H: f32 = 53.0;
const TABS_H: f32 = 30.0;
/// 筛选条（位置/时间/类型按钮）高度。
const FILTER_H: f32 = 28.0;
/// 浏览模式的面包屑行（只在 Browse 下占用，紧贴筛选条下方）。
const CRUMB_H: f32 = 22.0;
const LIST_GAP: f32 = 4.0;
const FOOTER_H: f32 = 36.0;
const NEEDLE_H: f32 = 26.0;

pub struct LstrApp {
    cfg: Config,
    theme: Theme,
    history: Arc<Mutex<History>>,
    service: SearchService,
    input: String,
    scope: Scope,
    entries: Vec<Entry>,
    selected: usize,
    total: usize,
    elapsed_ms: f32,
    /// 全链路耗时(请求发出 → 结果回到 UI 帧,含排队/IPC/帧等待)。
    /// 与 elapsed_ms(引擎纯执行)并列显示,定位「引擎快但手感慢」。
    ui_total_ms: Option<f32>,
    error: Option<String>,
    /// 正则模式下的编译错误（不触发搜索，单独提示，不显示“点击启动 Everything”）。
    regex_error: Option<String>,
    /// 自研索引状态行(渐进启动:构建中/未启用提示;错误条优先)。
    index_note: Option<String>,
    /// 状态行首次出现时间(>300ms 才显示,避免闪烁)。
    index_note_since: Option<std::time::Instant>,
    /// 设置页「索引卷」候选盘缓存:(枚举时刻, 升序盘符)。
    /// 枚举一次固定卷要跑 26 组 GetDriveType/GetVolumeInformation,不能每帧调。
    index_drives: Option<(Instant, Vec<char>)>,
    pending_gen: u64,
    /// 诊断用：请求发出时刻（定位「显示 70ms、实际等 2s」的排队来源）。
    search_sent_at: Option<Instant>,
    waiting: bool,
    dirty_at: Option<Instant>,
    hidden: bool,
    shown_at: Instant,
    focus_requested: bool,
    hwnd: Option<isize>,
    effects_applied: bool,
    acrylic_ok: bool,
    hotkey: Option<HotkeyService>,
    /// 提权 worker 的热键中继：管理员窗口在前台时，普通权限的 LL 钩子
    /// 收不到按键（UIPI），由子进程观察后 SetEvent 通知这里。
    hotkey_relay: Option<HotkeyRelay>,
    dialog_watch: Option<crate::win::dialogwatch::DialogWatch>,
    suppressed_dialog: Option<isize>,
    tray: Option<TrayState>,
    toast: Option<(String, Instant)>,
    clear_armed_at: Option<Instant>,
    /// 退出命令二次确认：首次触发记时刻，3 秒内再次触发才真正退出。
    quit_armed_at: Option<Instant>,
    /// 退出原因（写 engine.log 用）：window/tray/cmd:quit/hotkey。
    exit_reason: &'static str,
    /// 下次唤出是否保留输入框内容与结果：Esc/热键收起置真（继续上次搜索），
    /// 执行结果后收起（启动文件/应用）置假（下次唤出从空开始）。
    resume_input: bool,
    last_height: f32,
    needle_y: f32,
    needle_target: f32,
    needle_ready: bool,
    mouse_moved: bool,
    /// 键盘上下键选择后的短时间内不允许 hover 抢选中：鼠标移动事件和按键
    /// 落在同一帧时，光标正好停在第三行会把「下移一格」覆盖成第三行。
    hover_suppress_until: Instant,
    dialog: Option<DialogInfo>,
    settings_open: bool,
    preview_open: bool,
    autostart: bool,
    pending_show: bool,
    pending_dialog: Option<isize>,
    menu_open: bool,
    menu_index: usize,
    manual_size: Option<Vec2>,
    resizing: bool,
    /// 结果列表的滚动偏移（像素）。条目超出可视高度时靠滚轮/键盘跟随浏览。
    scroll_y: f32,
    /// 当前列表是否可滚动（paint_rows 计算，footer 用来提示「滚轮查看更多」）。
    scrollable: bool,
    /// 只在选择真正变化（键盘/hover）时把选中行滚回可视区；
    /// 每帧都做会把滚轮滚动立刻拽回去。
    scroll_to_selection: bool,
    /// 筛选条选择：位置 / 时间 / 类型（按钮操作，不输入 Everything 语法）。
    filters: SearchFilters,
    /// 打开中的筛选下拉菜单。
    filter_menu: Option<FilterMenuKind>,
    /// 对话框事件到达但探测失败时的重试截止时间（子控件可能还没建好）。
    dialog_retry_until: Option<Instant>,
    /// 设置面板当前分页。
    settings_tab: SettingsTab,
    /// 中文输入法是否处于组合态（Preedit 未上屏）：此时不消费任何快捷键，
    /// 回车/Esc/方向键都交给输入法与 TextEdit。
    ime_composing: bool,
    last_width: f32,
    center_frames: u8,
    suppress_hide_until: Instant,
    icons: IconStore,
    /// P0-2：Explorer 内直接键入搜索（默认关闭）。
    typing: Option<crate::win::typing::TypingWatch>,
    /// 设置页「资产」的草稿输入状态。
    assets_ui: crate::assets_ui::AssetUiState,
    /// P0-2：这次唤出锁定的 Explorer 当前目录（None = 普通全局启动器）。
    typing_folder: Option<std::path::PathBuf>,
    /// 目录上下文里临时切到全局搜索（Ctrl+Shift+G）。
    typing_global: bool,
    /// 深路径浏览模式的当前目录：输入框只放过滤词，面包屑占 TABS 行。
    browse_root: Option<std::path::PathBuf>,
    /// Alt+↑ 回退时记住离开的子目录，Alt+↓ 可以再回去（后进先出）。
    browse_forward: Vec<std::path::PathBuf>,
    /// 面包屑中间省略号展开的祖先弹层。
    crumb_more: bool,
    /// 设置页「唤起热键」录制状态（None = 未在录制）。
    hotkey_recording: Option<HotkeyRecording>,
}

impl LstrApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        history: Arc<Mutex<History>>,
        activation_event: isize,
    ) -> Self {
        crate::win::single_instance::start_activation_watcher(
            cc.egui_ctx.clone(),
            activation_event,
        );
        ui::install_fonts(&cc.egui_ctx);
        let theme = Theme::from_config(&cfg);
        cc.egui_ctx.style_mut(|style| {
            style.visuals.panel_fill = Color32::TRANSPARENT;
            style.visuals.window_fill = Color32::TRANSPARENT;
            style.visuals.override_text_color = Some(theme.text);
            style.spacing.item_spacing = Vec2::ZERO;
        });
        ui::apply_text_styles(&cc.egui_ctx);

        let preview_open = cfg.preview;
        // 必须在 SearchService::new 之前创建：worker 启动后会立刻 OpenEvent。
        let hotkey_relay = HotkeyRelay::create();
        let mut service = SearchService::new(cfg.clone(), Arc::clone(&history), cc.egui_ctx.clone());
        let pending_gen = service.search("", Scope::All, false, &SearchFilters::default(), &cfg.rank, cfg.show_history);
        let hotkey = match HotkeyService::start(cc.egui_ctx.clone(), &cfg.hotkey) {
            Ok(service) => Some(service),
            Err(err) => {
                eprintln!("lstr: hotkey 启动失败: {err}");
                None
            }
        };
        let dialog_watch = match crate::win::dialogwatch::DialogWatch::start() {
            Ok(watch) => Some(watch),
            Err(err) => {
                eprintln!("lstr: 对话框监视启动失败: {err}");
                None
            }
        };
        let tray = crate::win::tray::create().ok();
        // P0-2：只有显式开启才安装全局低级键盘钩子。
        let typing = if cfg.explorer_typing {
            match crate::win::typing::TypingWatch::start(cc.egui_ctx.clone()) {
                Ok(service) => Some(service),
                Err(err) => {
                    eprintln!("lstr: typing 启动失败: {err}");
                    None
                }
            }
        } else {
            None
        };
        let manual_size = cfg
            .height
            .map(|height| egui::vec2(cfg.width.clamp(420.0, 1000.0), height.clamp(160.0, 800.0)));

        Self {
            cfg,
            theme,
            history,
            service,
            input: String::new(),
            scope: Scope::All,
            entries: Vec::new(),
            selected: 0,
            total: 0,
            elapsed_ms: 0.0,
            ui_total_ms: None,
            error: None,
            regex_error: None,
            index_note: None,
            index_note_since: None,
            index_drives: None,
            pending_gen,
            search_sent_at: None,
            waiting: true,
            dirty_at: None,
            hidden: true,
            shown_at: Instant::now(),
            focus_requested: true,
            hwnd: None,
            effects_applied: false,
            acrylic_ok: false,
            hotkey,
            hotkey_relay,
            dialog_watch,
            suppressed_dialog: None,
            tray,
            toast: None,
            clear_armed_at: None,
            quit_armed_at: None,
            exit_reason: "window",
            resume_input: false,
            last_height: 0.0,
            needle_y: 0.0,
            needle_target: 0.0,
            needle_ready: false,
            mouse_moved: false,
            hover_suppress_until: Instant::now(),
            dialog: None,
            settings_open: false,
            preview_open,
            autostart: crate::win::autostart::is_enabled(),
            pending_show: false,
            pending_dialog: None,
            menu_open: false,
            menu_index: 0,
            manual_size,
            resizing: false,
            scroll_y: 0.0,
            scrollable: false,
            scroll_to_selection: false,
            filters: SearchFilters::default(),
            filter_menu: None,
            dialog_retry_until: None,
            settings_tab: SettingsTab::Appearance,
            ime_composing: false,
            last_width: 0.0,
            center_frames: 0,
            suppress_hide_until: Instant::now(),
            icons: IconStore::new(),
            typing,
            assets_ui: crate::assets_ui::AssetUiState::default(),
            typing_folder: None,
            browse_root: None,
            browse_forward: Vec::new(),
            crumb_more: false,
            typing_global: false,
            hotkey_recording: None,
        }
    }

    /// 设置页开关：即时安装/卸载全局键盘钩子，不必重启程序。
    fn set_explorer_typing(&mut self, enabled: bool, ctx: &egui::Context) {
        self.cfg.explorer_typing = enabled;
        // 先卸载旧实例（Drop 会反挂钩子并结束线程），再按需安装新的。
        self.typing = None;
        if enabled {
            match crate::win::typing::TypingWatch::start(ctx.clone()) {
                Ok(service) => self.typing = Some(service),
                Err(err) => self.toast(ctx, format!("启动键入搜索失败: {err}")),
            }
        }
        let _ = self.cfg.save();
    }

    /// 应用新的唤起热键（设置页录制完成）：丢弃旧 HotkeyService 重新启动，
    /// 双击修饰键 / 组合键两条注册路径一起热生效，不必重启应用。
    ///
    /// 启动失败（组合键被系统/其他程序占用、钩子装不上）时用旧热键原样拉回，
    /// 返回 `Err` 让 UI 显示失败提示；配置只在成功后落盘，保证「失败保留旧值」。
    fn apply_hotkey(&mut self, ctx: &egui::Context, new_hotkey: String) -> Result<(), String> {
        let old = self.cfg.hotkey.clone();
        // Drop：unhook + unregister + join，干净卸载后再装新的。
        self.hotkey = None;
        match HotkeyService::start(ctx.clone(), &new_hotkey) {
            Ok(service) => {
                self.hotkey = Some(service);
                self.cfg.hotkey = new_hotkey;
                let _ = self.cfg.save();
                Ok(())
            }
            Err(err) => {
                // 旧热键刚被释放，重新注册大概率成功；再失败只能置空并留日志。
                self.hotkey = match HotkeyService::start(ctx.clone(), &old) {
                    Ok(service) => Some(service),
                    Err(restore_err) => {
                        eprintln!("lstr: 热键服务恢复失败({old}): {restore_err}");
                        None
                    }
                };
                Err(err)
            }
        }
    }

    /// P0-2：Explorer 里敲下的第一个字符唤出启动器并写入输入框。
    /// 之后前台不再是 Explorer，钩子自动停止接管，后续输入交回 egui。
    fn poll_typing(&mut self, ctx: &egui::Context) {
        // 一次取走全部字符：快速连打时轮询周期可能跨过多个按键，只保留最后一个会丢字。
        let chars = self
            .typing
            .as_ref()
            .map(|service| service.poll())
            .unwrap_or_default();
        if chars.is_empty() {
            return;
        }
        if self.hidden {
            // 必须在 show() 之前取：钩子吞键时前台还是 Explorer，
            // show() 一旦把窗口显示/激活，GetForegroundWindow 就变成 lstr 自己了。
            let folder = crate::win::explorer::foreground_explorer_folder();
            self.show(ctx);
            self.typing_folder = folder;
            self.typing_global = false;
            self.scope = Scope::All;
        }
        self.input.extend(chars);
        Self::reset_input_cursor(ctx);
        self.selected = 0;
        self.trigger(ctx);
    }

    fn poll_hotkey(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(service) = &self.hotkey {
            while let Some(event) = service.poll() {
                events.push(event);
            }
        }
        // 管理员窗口在前台时，本进程的 LL 钩子被 UIPI 屏蔽；提权 worker
        // 通过命名事件中继同一次双击。若两路同时到达（理论上不会），
        // 只消费一次 toggle，避免开了又关。
        let mut relay_pending = self
            .hotkey_relay
            .as_ref()
            .is_some_and(|relay| relay.take());
        // 设置页录制唤起热键时忽略全局热键触发：否则录 double-ctrl 的瞬间
        // 现役热键会先把窗口收起，录制被打断（事件照常取走，只是不执行）。
        if self.hotkey_recording.is_some() {
            return;
        }
        for event in events {
            match event {
                HotkeyEvent::Toggle => {
                    if relay_pending {
                        relay_pending = false;
                    } else {
                        self.toggle(ctx);
                    }
                }
                HotkeyEvent::DialogSearch => {
                    if let Some(info) = dialog::active_dialog() {
                        // P0-3：有活动 Explorer 目录就零输入直跳，否则退回文件夹搜索。
                        let target = crate::win::explorer::active_explorer_folder();
                        if let Some(folder) = target {
                            match dialog::navigate_to_folder(&info, &folder.to_string_lossy()) {
                                Ok(()) => {
                                    self.suppressed_dialog = Some(info.hwnd);
                                    self.dialog = None;
                                    self.hide(ctx);
                                    dialog::restore_focus(&info);
                                }
                                Err(err) => self.toast(ctx, err),
                            }
                        } else {
                            self.open_dialog_mode(ctx, info);
                        }
                    } else if !self.hidden {
                        self.toast(ctx, "请先打开「打开/保存」对话框，再按 Ctrl+G".to_string());
                    }
                },
            }
        }
        if relay_pending {
            self.toggle(ctx);
        }
    }

    /// 设置页「唤起热键」录制中：消费本帧按键，产出新的热键配置。
    ///
    /// 两类输入（见 [`crate::hotkey`]）：
    /// - 双击修饰键：egui 不派发裸修饰键的 Key 事件，只能逐帧比较
    ///   `modifiers` 检测按下沿；同一修饰键两次按下沿间隔 ≤400ms 即完成
    ///   （录制期间 16ms 高频重绘保证沿不丢）；
    /// - 组合键：`Event::Key` 自带按下时的修饰键快照，修饰键 + 普通键直接编码。
    ///
    /// Esc 随时取消；完成/取消后回到普通按键处理。
    fn poll_hotkey_recording(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let (key_events, mods) = ctx.input(|i| {
            let events: Vec<(egui::Key, egui::Modifiers)> = i
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key { key, modifiers, pressed, repeat, .. }
                        if *pressed && !*repeat =>
                    {
                        Some((*key, *modifiers))
                    }
                    _ => None,
                })
                .collect();
            (events, i.modifiers)
        });

        // take()：录制状态拿走处理，方法中途要调 &mut self（toast/apply）。
        let Some(mut recording) = self.hotkey_recording.take() else {
            return;
        };

        // 1) Esc 取消（无论带不带修饰键）。
        if key_events.iter().any(|(key, _)| *key == egui::Key::Escape) {
            self.toast(ctx, "已取消录制，唤起热键未改动".to_string());
            return; // hotkey_recording 已被 take，保持 None。
        }

        // 2) 普通键按下 → 组合键路径；同时打断双击修饰键序列。
        for (key, modifiers) in &key_events {
            let Some(primary) = primary_key_from_egui(*key) else {
                // 不可录制的键（方向键/标点等）：打断双击修饰键序列（与运行时
                // 检测器「中间杂键重置」语义一致），提示后继续等。
                recording.first_mod = None;
                self.hotkey_recording = Some(recording);
                self.toast(
                    ctx,
                    "该键不支持：请录字母 / 数字 / F1-F12 / 空格，或双击 Ctrl / Alt / Shift"
                        .to_string(),
                );
                return;
            };
            recording.first_mod = None;
            let wake = WakeHotkey::Combo {
                mods: crate::hotkey::Mods {
                    ctrl: modifiers.ctrl,
                    alt: modifiers.alt,
                    shift: modifiers.shift,
                },
                key: primary,
            };
            let encoded = wake.encode();
            match crate::hotkey::parse_hotkey(&encoded) {
                // 裸字母/数字/Shift 单修饰会被 parse 拒绝：全局注册会吞掉正常打字。
                Some(valid) => self.finish_hotkey_recording(ctx, valid),
                None => {
                    // 非法组合：留在录制态，提示后继续等下一个键。
                    self.hotkey_recording = Some(recording);
                    self.toast(
                        ctx,
                        "需要至少一个 Ctrl / Alt 修饰键（Shift 单独修饰会吞掉正常打字）".to_string(),
                    );
                }
            }
            return;
        }

        // 3) 修饰键按下沿 → 双击修饰键检测。
        let current = [mods.ctrl, mods.alt, mods.shift];
        let edges = [
            current[0].then_some(TapKey::Ctrl),
            current[1].then_some(TapKey::Alt),
            current[2].then_some(TapKey::Shift),
        ];
        for (idx, edge) in edges.into_iter().enumerate() {
            let Some(tap) = edge else {
                continue;
            };
            if !recording.prev_mods[idx] {
                match recording.first_mod {
                    Some((same, first_at))
                        if same == tap
                            && now.saturating_duration_since(first_at) <= DOUBLE_TAP_WINDOW =>
                    {
                        self.finish_hotkey_recording(ctx, WakeHotkey::DoubleTap(tap));
                        return;
                    }
                    _ => recording.first_mod = Some((tap, now)),
                }
            }
        }
        recording.prev_mods = current;

        // 未完成：放回录制状态，继续等。
        self.hotkey_recording = Some(recording);
        // 录制期间高频重绘：修饰键按下沿靠帧间比较，丢帧会漏掉双击。
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    /// 录制完成：应用新热键（热生效），失败保留旧值并提示。
    fn finish_hotkey_recording(&mut self, ctx: &egui::Context, wake: WakeHotkey) {
        self.hotkey_recording = None;
        let encoded = wake.encode();
        match self.apply_hotkey(ctx, encoded.clone()) {
            Ok(()) => self.toast(
                ctx,
                format!("唤起热键已改为 {}，立即生效", display_hotkey(&encoded)),
            ),
            Err(err) => self.toast(
                ctx,
                format!(
                    "注册失败（可能被其他程序占用）：{err}；已保留原热键 {}",
                    display_hotkey(&self.cfg.hotkey)
                ),
            ),
        }
    }

    /// 自动唤出：前台出现标准文件对话框时弹到它旁边。
    /// 抑制机制防止循环弹窗——凡是「我们主动把焦点还给对话框」的场景
    /// （填入成功、Esc、失焦隐藏）都记下 hwnd，同一个对话框不再自动弹。
    fn poll_dialog_watch(&mut self, ctx: &egui::Context) {
        let mut latest = None;
        if let Some(watch) = &self.dialog_watch {
            latest = watch.poll();
        }
        // 事件到达 → 开启 2.5s 探测窗口：对话框在「创建激活」时就触发前台事件，
        // 但那时它还不可见、子控件也在逐步构建（实测 600ms 后才齐），
        // 要等它真正显示出来才探测成功。
        if latest.is_some() {
            self.dialog_retry_until = Some(Instant::now() + Duration::from_millis(2500));
        }
        let Some(deadline) = self.dialog_retry_until else {
            return;
        };
        if Instant::now() > deadline {
            self.dialog_retry_until = None;
            return;
        }
        // 只在 lstr 隐藏时自动唤出（可见时用户可能正在打搜索词，不打扰）。
        if !self.cfg.dialog_assist || !self.hidden || self.settings_open {
            self.dialog_retry_until = None;
            return;
        }
        // 严格版探测：事件到达时前台若已切走就不弹（避免枚举兜底误伤后台对话框）。
        let Some(info) = dialog::foreground_dialog() else {
            // 对话框尚未显示/子控件未就绪：留在探测窗口内继续重试。
            ctx.request_repaint_after(Duration::from_millis(60));
            return;
        };
        self.dialog_retry_until = None;
        if self.suppressed_dialog == Some(info.hwnd) {
            return;
        }
        self.suppressed_dialog = None;
        // P0-3：自动跟随 Explorer 当前目录（默认关闭，会改动对话框路径）。
        if self.cfg.dialog_auto_follow {
            if let Some(folder) = crate::win::explorer::active_explorer_folder() {
                if dialog::navigate_to_folder(&info, &folder.to_string_lossy()).is_ok() {
                    self.suppressed_dialog = Some(info.hwnd);
                    return;
                }
            }
        }
        self.open_dialog_mode(ctx, info);
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        let mut action = None;
        if let (Some(tray), Some(hwnd)) = (&self.tray, self.hwnd) {
            action = tray.poll(hwnd);
        }
        match action {
            Some(TrayAction::Show) => {
                if self.hidden {
                    self.show(ctx);
                } else if let Some(hwnd) = self.hwnd {
                    // 已显示时只切回前台，不清空用户正在输入的查询。
                    window::force_foreground(hwnd);
                }
            }
            Some(TrayAction::Settings) => self.open_settings(ctx),
            Some(TrayAction::Quit) => {
                self.exit_reason = "tray";
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
        }
    }

    fn poll_search(&mut self, ctx: &egui::Context) {
        // 正则先在 UI 侧编译：非法表达式不发给 worker，避免“空结果=没找到”的误导。
        if self.filters.matches.regex {
            let pattern = self.input.trim();
            self.regex_error = if pattern.is_empty() {
                None
            } else {
                lstr_index::query::validate_regex(pattern, self.filters.matches.case_sensitive)
                    .err()
                    .map(|err| {
                        // regex 的错误带多行源码指针，这里压成单行，避免撑破 24px 提示条。
                        let detail = err.split_whitespace().collect::<Vec<_>>().join(" ");
                        format!("正则无效: {detail}")
                    })
            };
            if self.regex_error.is_some() {
                self.dirty_at = None;
                self.waiting = false;
                self.entries.clear();
                self.total = 0;
                // 让仍在路上的旧响应失效，避免它覆盖错误态。
                self.pending_gen = 0;
            }
        } else {
            self.regex_error = None;
        }
        if let Some(deadline) = self.dirty_at {
            if Instant::now() >= deadline {
                let base_folder = if self.typing_global {
                    None
                } else {
                    self.typing_folder.clone()
                };
                self.pending_gen = if let Some(root) = self.browse_root.clone() {
                    self.service.search_browse(
                        &self.input,
                        self.scope,
                        self.dialog.is_some(),
                        &self.filters,
                        &self.cfg.rank,
                        self.cfg.show_history,
                        root,
                    )
                } else {
                    self.service.search_in(
                        &self.input,
                        self.scope,
                        self.dialog.is_some(),
                        &self.filters,
                        &self.cfg.rank,
                        self.cfg.show_history,
                        base_folder,
                    )
                };
                self.search_sent_at = Some(Instant::now());
                self.ui_total_ms = None;
                self.waiting = true;
                self.dirty_at = None;
            } else {
                ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
            }
        }
        self.poll_index_status(ctx);
        if let Some(response) = self.service.poll() {
            if response.generation == self.pending_gen {
                // 诊断：总延迟 vs 服务端执行时间。差值 = 排队/唤醒开销。
                if let Some(sent) = self.search_sent_at.take() {
                    let total_ms = sent.elapsed().as_millis();
                    self.ui_total_ms = Some(total_ms as f32);
                    if total_ms > 250 {
                        lstr_index::snapshot::debug_log(&format!(
                            "search: ui total={}ms execute={:.1}ms entries={} total={}",
                            total_ms,
                            response.elapsed_ms,
                            response.entries.len(),
                            response.total
                        ));
                    }
                }
                self.entries = response.entries;
                self.total = response.total;
                self.elapsed_ms = response.elapsed_ms;
                self.error = response.error;
                self.scroll_y = 0.0;
                self.waiting = false;
                self.needle_ready = false;
                if self.selected >= self.entries.len() {
                    self.selected = self.entries.len().saturating_sub(1);
                }
            }
        }
        if self.waiting {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn trigger(&mut self, ctx: &egui::Context) {
        // 一旦输入/筛选变化就进入「搜索中」：防抖的 150ms 里结果还没回来，
        // 空列表不应该先闪一下「没有匹配项」。
        self.waiting = true;
        // 30ms：只用来合并同一帧内的连击/粘贴，不再人为拖慢输入。
        // 请求在 worker 里是 latest-wins 合并的，逐键查询不会堆积。
        // 打字防抖:150ms 内还有击键就只搜最后的状态,避免每个字符都发一次
        // 全量查询(单字符 "a" 在 600 万条索引里是几百毫秒级的扫描)。
        self.dirty_at = Some(Instant::now() + Duration::from_millis(150));
        ctx.request_repaint_after(Duration::from_millis(35));
    }

    fn toggle(&mut self, ctx: &egui::Context) {
        if self.hidden {
            self.show(ctx);
        } else {
            self.hide_keep(ctx);
        }
    }

    /// 外部改写 input 后，TextEdit 的持久光标状态还停在旧位置（通常是 0），
    /// 清掉它，下一帧就会按 cursor_at_end 把光标放到文本末尾。
    fn reset_input_cursor(ctx: &egui::Context) {
        let id = egui::Id::new("lstr-search-input");
        ctx.data_mut(|data| {
            let _ = data.remove::<egui::widgets::text_edit::TextEditState>(id);
        });
    }

    fn show(&mut self, ctx: &egui::Context) {
        // Esc/热键收起后再次唤出：保留输入框内容、选中项与筛选，继续上次搜索；
        // 执行结果后收起则照旧从空开始。
        let keep = std::mem::take(&mut self.resume_input);
        // 诊断：输入保留链路的每一次 show/hide 都落一条（排查「唤出后输入丢失」）。
        lstr_index::snapshot::debug_log(&format!(
            "ui: show keep={keep} input_len={} entries={}",
            self.input.chars().count(),
            self.entries.len()
        ));
        self.dialog = None;
        self.settings_open = false;
        self.menu_open = false;
        self.pending_show = true;
        self.pending_dialog = None;
        self.hidden = false;
        self.shown_at = Instant::now();
        self.center_frames = 3;
        self.suppress_hide_until = Instant::now() + Duration::from_millis(500);
        self.focus_requested = true;
        self.mouse_moved = false;
        self.needle_ready = false;
        if !keep {
            self.input.clear();
            self.selected = 0;
            self.scroll_y = 0.0;
            self.scroll_to_selection = false;
            self.filters = SearchFilters::default();
            self.browse_root = None;
            self.browse_forward.clear();
        }
        Self::reset_input_cursor(ctx);
        self.filter_menu = None;
        // 普通唤出（双击 Space / 托盘）不携带 Explorer 目录上下文。
        self.typing_folder = None;
        self.typing_global = false;
        self.apply_size(ctx);
        if let Some(hwnd) = self.hwnd {
            window::position_center(hwnd);
            window::show(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
        self.trigger(ctx);
    }

    fn open_dialog_mode(&mut self, ctx: &egui::Context, info: DialogInfo) {
        self.dialog = Some(info.clone());
        self.typing_folder = None;
        self.typing_global = false;
        self.browse_root = None;
        self.browse_forward.clear();
        self.menu_open = false;
        self.center_frames = 0;
        self.pending_show = true;
        self.pending_dialog = Some(info.hwnd);
        self.scope = Scope::Folder;
        self.input.clear();
        self.entries.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.filters = SearchFilters::default();
        self.filter_menu = None;
        self.hidden = false;
        self.shown_at = Instant::now();
        // 对话框模式不居中：保持贴着对话框（center_frames 只用于双击 Space 唤出）。
        self.center_frames = 0;
        self.suppress_hide_until = Instant::now() + Duration::from_millis(500);
        self.focus_requested = true;
        self.mouse_moved = false;
        self.needle_ready = false;
        self.apply_size(ctx);
        if let Some(hwnd) = self.hwnd {
            window::position_near_dialog(hwnd, info.hwnd);
            window::show(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
        self.trigger(ctx);
    }

    fn open_settings(&mut self, ctx: &egui::Context) {
        self.settings_open = true;
        self.hidden = false;
        self.shown_at = Instant::now();
        self.center_frames = 3;
        self.suppress_hide_until = Instant::now() + Duration::from_millis(500);
        self.focus_requested = false;
        self.apply_size(ctx);
        if let Some(hwnd) = self.hwnd {
            window::position_center(hwnd);
            window::show(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        // 执行结果后收起：下次唤出从空开始。
        lstr_index::snapshot::debug_log(&format!(
            "ui: hide (fresh) input_len={}",
            self.input.chars().count()
        ));
        self.resume_input = false;
        self.hide_inner(ctx);
    }

    /// Esc / 热键收起：下次唤出保留输入框内容与结果，继续上次搜索。
    fn hide_keep(&mut self, ctx: &egui::Context) {
        lstr_index::snapshot::debug_log(&format!(
            "ui: hide_keep input_len={}",
            self.input.chars().count()
        ));
        self.resume_input = true;
        self.hide_inner(ctx);
    }

    fn hide_inner(&mut self, _ctx: &egui::Context) {
        if let Some(hwnd) = self.hwnd {
            window::hide(hwnd);
        }
        self.hidden = true;
        self.menu_open = false;
        self.center_frames = 0;
        self.focus_requested = false;
        // 下次在 Explorer 里打字会重新读取当时浏览的目录。
        self.typing_folder = None;
        self.typing_global = false;
    }

    fn apply_size(&mut self, ctx: &egui::Context) {
        if self.resizing {
            return;
        }
        let mut width = self
            .manual_size
            .map(|size| size.x)
            .unwrap_or(self.theme.width)
            .clamp(420.0, 1000.0);
        if self.preview_open {
            width = width.max(560.0);
        }
        if self.settings_open {
            width = width.max(780.0);
        }
        let height = if self.settings_open {
            600.0
        } else {
            // 窗口高度按「最大结果数」定：worker 会多送候选（可滚动），
            // 窗口只长到设置的行数，不会因为候选多就撑满屏幕。
            let rows = self.entries.len().min(self.cfg.max_results);
            let mut list_h = if rows == 0 { 120.0 } else { rows as f32 * self.theme.row_h };
            // 空态技巧面板（分类两列约 280px）：仅空输入+空结果时撑高窗口，
            // 一旦有结果或开始输入即恢复按行数计算。
            let tips_active = rows == 0
                && self.input.trim().is_empty()
                && !self.settings_open
                && self.browse_root.is_none();
            if tips_active {
                list_h = list_h.max(300.0);
            }
            let banner_h = if self.error.is_some() || self.regex_error.is_some() || self.index_note.is_some() { 30.0 } else { 0.0 };
            let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
            let computed_h = (SEARCH_H
                + TABS_H
                + FILTER_H
                + if self.browse_root.is_some() { CRUMB_H } else { 0.0 }
                + LIST_GAP
                + list_h
                + LIST_GAP
                + banner_h
                + dialog_h
                + FOOTER_H
                + 8.0)
                .clamp(160.0, 800.0);
            match self.manual_size {
                Some(manual) => computed_h.max(manual.y).clamp(160.0, 800.0),
                None => computed_h,
            }
        };
        if (width - self.last_width).abs() > 0.5 || (height - self.last_height).abs() > 0.5 {
            self.last_width = width;
            self.last_height = height;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, height)));
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // 跟踪 IME 组合状态（跨帧）：中文输入法打出拼音但还没上屏时，
        // 回车应当由输入法上屏字母，不能被我们当成「打开选中结果」吃掉。
        let (ime_active, preedit) = ctx.input(|i| {
            let mut composing = self.ime_composing;
            let mut has_ime_event = false;
            for event in &i.events {
                if let egui::Event::Ime(ime) = event {
                    has_ime_event = true;
                    match ime {
                        egui::ImeEvent::Preedit(text) => composing = !text.is_empty(),
                        egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => composing = false,
                        egui::ImeEvent::Enabled => {}
                    }
                }
            }
            (composing || has_ime_event, composing)
        });
        self.ime_composing = preedit;
        if ime_active {
            return;
        }

        // 设置页「唤起热键」录制中：本帧按键全部进录制器（Esc 取消），
        // 不落入下方任何常规快捷键处理，否则 Esc 会顺手关掉整个设置面板。
        if self.hotkey_recording.is_some() {
            self.poll_hotkey_recording(ctx);
            return;
        }

        // Esc 关闭窗口（不再需要按住），但按层级优先：菜单 → 筛选菜单 →
        // 设置面板 先各自收起，全部收完后再按一次才隐藏窗口。
        // 窗口不会因为失焦自动关闭。
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            if self.crumb_more {
                self.crumb_more = false;
                return;
            }
            if self.menu_open {
                self.menu_open = false;
                return;
            }
            if self.filter_menu.is_some() {
                self.filter_menu = None;
                return;
            }
            if self.settings_open {
                self.settings_open = false;
                return;
            }
            self.menu_open = false;
            self.filter_menu = None;
            self.settings_open = false;
            if self.in_browse() {
                // Esc 分层：先清过滤词，再退出浏览（不直接收窗）。
                if !self.input.is_empty() {
                    self.input.clear();
                    Self::reset_input_cursor(ctx);
                    self.trigger(ctx);
                } else {
                    self.exit_browse(ctx);
                }
                return;
            }
            if let Some(dialog) = self.dialog.take() {
                // 交还焦点：抑制这个对话框的自动唤出。
                self.suppressed_dialog = Some(dialog.hwnd);
                self.hide_keep(ctx);
                dialog::restore_focus(&dialog);
            } else {
                self.hide_keep(ctx);
            }
            return;
        }

        if self.settings_open {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.settings_open = false;
            }
            return;
        }

        // 筛选菜单打开时：Esc / 任意输入先收起菜单，不打断输入框内容。
        if self.filter_menu.is_some() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.filter_menu = None;
                return;
            }
            let typing = ctx.input(|i| {
                i.events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Text(_) | egui::Event::Paste(_)))
            });
            if typing {
                self.filter_menu = None;
            }
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::P)) {
            self.preview_open = !self.preview_open;
            self.cfg.preview = self.preview_open;
            let _ = self.cfg.save();
            return;
        }

        // Ctrl+L：浏览模式下把当前目录放回输入框，切回可编辑完整路径。
        if self.in_browse() && ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::L)) {
            self.edit_browse_path(ctx);
            return;
        }

        // P0-2：目录上下文里一键切到全局搜索 / 切回来。
        if self.typing_folder.is_some()
            && ctx.input_mut(|i| {
                i.consume_key(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::G)
            })
        {
            self.typing_global = !self.typing_global;
            self.selected = 0;
            self.trigger(ctx);
            self.toast(
                ctx,
                if self.typing_global {
                    "已切换到全局搜索"
                } else {
                    "已切回当前目录"
                }
                .to_string(),
            );
            return;
        }

        // Windows 标准上下文菜单键：Shift+F10。egui 不暴露独立的菜单键；
        // Ctrl+K 仍保留。文件夹的 → 继续下钻，两者不冲突。
        if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::F10)) {
            if self.menu_open {
                self.menu_open = false;
            } else {
                self.open_menu();
            }
            return;
        }

        if self.menu_open {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.menu_step(1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.menu_step(-1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                self.menu_activate(ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
                || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowLeft))
            {
                self.menu_open = false;
                return;
            }

            let typing = ctx.input(|i| {
                i.events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Text(_) | egui::Event::Paste(_)))
            });
            if typing {
                self.menu_open = false;
            } else if ctx.input(|i| {
                i.events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
            }) {
                self.menu_open = false;
                return;
            } else {
                return;
            }
        }

        // 路径/浏览模式回退：Alt+↑ 或（过滤词为空时）Backspace 一次退一整级。
        let has_selection = {
            let id = egui::Id::new("lstr-search-input");
            egui::widgets::text_edit::TextEditState::load(ctx, id)
                .and_then(|state| state.cursor.char_range())
                .map(|range| range.primary.index != range.secondary.index)
                .unwrap_or(false)
        };
        if self.in_browse() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowUp)) {
                self.browse_up(ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowDown)) {
                self.browse_down(ctx);
                return;
            }
            if self.input.is_empty()
                && !has_selection
                && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace))
            {
                self.browse_up(ctx);
                return;
            }
        } else if self.path_mode() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowUp)) {
                self.path_up(ctx);
                return;
            }
            if self.input.ends_with(std::path::is_separator)
                && !has_selection
                && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace))
            {
                self.path_up(ctx);
                return;
            }
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::K)) {
            self.open_menu();
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Comma)) {
            self.open_settings(ctx);
            return;
        }

        let input_empty = self.input.is_empty();
        let mut scope_delta = 0;
        if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowLeft)) {
            scope_delta = -1;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowRight)) {
            scope_delta = 1;
        }
        if scope_delta != 0 {
            self.step_scope(ctx, scope_delta);
        }

        let plain_right = ctx.input(|i| {
            !i.modifiers.alt && !i.modifiers.ctrl && !i.modifiers.shift
        });
        if plain_right
            && self.caret_at_end(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowRight))
        {
            // 文件夹优先下钻（Listary 式）；非文件夹退回动作菜单。
            if !self.descend_index(ctx, self.selected) {
                self.open_menu();
            }
            return;
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            self.move_selection(1);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            self.move_selection(-1);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::PageDown)) {
            self.move_selection(6);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::PageUp)) {
            self.move_selection(-6);
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
            self.step_scope(ctx, -1);
        } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            self.step_scope(ctx, 1);
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Enter)) {
            self.admin_selected(ctx);
        } else if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Enter)) {
            self.reveal_selected(ctx);
        } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            self.activate_selected(ctx);
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            if !self.input.is_empty() {
                lstr_index::snapshot::debug_log("ui: esc_clear_input");
                self.input.clear();
                self.trigger(ctx);
            } else if let Some(dialog) = self.dialog.take() {
                // Esc 交还焦点：抑制这个对话框的自动唤出。
                self.suppressed_dialog = Some(dialog.hwnd);
                self.hide(ctx);
                dialog::restore_focus(&dialog);
            } else {
                self.hide(ctx);
            }
        }

        if input_empty && ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::C)) {
            self.copy_selected(ctx);
        }

        const DIGITS: [Key; 9] = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        for (index, key) in DIGITS.iter().enumerate() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, *key)) {
                self.activate_index(ctx, index);
                break;
            }
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as i32;
        self.selected = (((self.selected as i32 + delta) % len + len) % len) as usize;
        // 键盘导航后短暂冻结 hover：同帧的鼠标移动事件不能把选中抢到别的行。
        self.hover_suppress_until = Instant::now() + Duration::from_millis(300);
        // 键盘移动后把选中行滚回可视区（只在选择变化时做，不要每帧做，
        // 否则滚轮滚动会被"保持选中可见"拽回去）。
        self.scroll_to_selection = true;
    }

    fn step_scope(&mut self, ctx: &egui::Context, delta: i32) {
        let effective = search::query::parse(&self.input, self.scope).scope;
        strip_prefix(&mut self.input);
        self.scope = effective.step(delta);
        self.trigger(ctx);
    }

    fn set_scope(&mut self, ctx: &egui::Context, scope: Scope) {
        // 正则是文件名语义，应用/命令页没有可执行的正则匹配。
        if self.filters.matches.regex && matches!(scope, Scope::App | Scope::Command) {
            self.toast(ctx, "正则模式只搜文件 / 文件夹 / 程序".to_string());
            return;
        }
        // 浏览模式只列文件系统条目，应用/命令页没有意义。
        if self.in_browse() && matches!(scope, Scope::App | Scope::Command) {
            self.toast(ctx, "浏览模式只支持文件夹 / 程序 / 文件".to_string());
            return;
        }
        if !self.in_browse() {
            strip_prefix(&mut self.input);
        }
        self.scope = scope;
        self.trigger(ctx);
    }

    fn activate_selected(&mut self, ctx: &egui::Context) {
        self.activate_index(ctx, self.selected);
    }

    fn activate_index(&mut self, ctx: &egui::Context, index: usize) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if let Some(dialog) = self.dialog.clone() {
            if !dialog::is_alive(&dialog) {
                self.dialog = None;
                self.toast(ctx, "对话框已关闭".to_string());
                return;
            }
            if entry.kind == Kind::App || entry.kind == Kind::Command {
                self.toast(ctx, "对话框模式只能选择文件或文件夹".to_string());
                return;
            }
            let path = entry.full_path();
            let result = if entry.kind == Kind::Folder {
                dialog::navigate_to_folder(&dialog, &path)
            } else {
                dialog::fill_path(&dialog, &path)
            };
            match result {
                Ok(()) => {
                    self.service.record_open(&entry);
                    if let Ok(mut history) = self.history.lock() {
                        history.record_query(&self.input, &entry);
                    }
                    // 焦点马上还给对话框：抑制它的自动唤出，不然会循环弹窗。
                    self.suppressed_dialog = Some(dialog.hwnd);
                    self.dialog = None;
                    self.hide_keep(ctx);
                    dialog::restore_focus(&dialog);
                }
                Err(err) => self.toast(ctx, err),
            }
            return;
        }
        // 路径模式：Enter 文件夹 = 继续深入；想在资源管理器打开用 Ctrl+Enter。
        // 普通模式下 Enter 文件夹仍保持打开行为，只有 → 下钻。
        if entry.kind == Kind::Folder && self.path_ui() {
            self.descend_index(ctx, index);
            return;
        }
        if entry.kind == Kind::Command {
            self.run_command(ctx, &entry);
            return;
        }
        // P0-2：从 Explorer 唤起时，文件夹结果让原窗口直接跳过去，
        // 而不是 shell::open_entry 再开一个新资源管理器窗口。
        if entry.kind == Kind::Folder && self.typing_folder.is_some() && !self.typing_global {
            if crate::win::explorer::navigate_active_to(&entry.full_path()).is_ok() {
                self.service.record_open(&entry);
                if let Ok(mut history) = self.history.lock() {
                    // 浏览模式下输入框只是过滤词，不能当成完整查询记录。
                    if self.browse_root.is_none() {
                        history.record_query(&self.input, &entry);
                    }
                }
                self.hide_keep(ctx);
                return;
            }
        }
        match shell::open_entry(&entry) {
            Ok(()) => {
                self.service.record_open(&entry);
                // P1-1：记住「搜这个词时选了它」。
                if let Ok(mut history) = self.history.lock() {
                    // 浏览模式下输入框只是过滤词，不能当成完整查询记录。
                    if self.browse_root.is_none() {
                        history.record_query(&self.input, &entry);
                    }
                }
                self.hide_keep(ctx);
            }
            Err(err) => self.toast(ctx, format!("打开失败: {err}")),
        }
    }

    fn reveal_selected(&mut self, ctx: &egui::Context) {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return;
        };
        if entry.kind == Kind::Command {
            self.toast(ctx, "命令没有所在位置".to_string());
            return;
        }
        match shell::reveal_entry(&entry) {
            // 定位是旁观操作（不消费搜索结果）：收起时保留输入与结果，
            // 下次唤出继续上一次搜索。只有「打开/运行」才算消费。
            Ok(()) => self.hide_keep(ctx),
            Err(err) => self.toast(ctx, err),
        }
    }

    fn admin_selected(&mut self, ctx: &egui::Context) {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return;
        };
        if entry.kind == Kind::Command || entry.kind == Kind::Folder {
            self.toast(ctx, "该结果不支持提权".to_string());
            return;
        }
        match shell::run_as_admin(&entry) {
            // 提权启动也是旁观操作（UAC 可能被取消），不消费搜索结果：
            // 再次唤出仍保留本次输入与结果。
            Ok(()) => self.hide_keep(ctx),
            Err(err) => self.toast(ctx, err),
        }
    }

    fn copy_selected(&mut self, ctx: &egui::Context) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        ctx.copy_text(entry.full_path());
        self.toast(ctx, "已复制路径".to_string());
    }

    fn run_command(&mut self, ctx: &egui::Context, entry: &Entry) {
        // P0-1：自定义命令，{query} = 触发词之后的内容，{current_folder} = 最近浏览目录。
        if let Some(trigger) = entry.id.strip_prefix("usercmd:") {
            let Some(def) = self.cfg.commands.iter().find(|c| c.trigger == trigger).cloned() else {
                return;
            };
            let query = self
                .input
                .split_once(char::is_whitespace)
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            let current_folder = crate::win::explorer::active_explorer_folder()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            match crate::assets::run_command(&def, query, &current_folder) {
                Ok(()) => self.hide(ctx),
                Err(err) => self.toast(ctx, err),
            }
            return;
        }
        if entry.id.starts_with("kwurl:") {
            let Some(path) = &entry.path else { return };
            match shell::open_url(&path.to_string_lossy()) {
                Ok(()) => self.hide(ctx),
                Err(err) => self.toast(ctx, err),
            }
            return;
        }
        if entry.id.starts_with("kwexec:") {
            let Some(path) = &entry.path else { return };
            match shell::open_command_line(&path.to_string_lossy()) {
                Ok(()) => self.hide(ctx),
                Err(err) => self.toast(ctx, err),
            }
            return;
        }
        match entry.id.as_str() {
            "cmd:settings" => self.open_settings(ctx),
            "cmd:reload" => {
                self.cfg = Config::load();
                self.theme = Theme::from_config(&self.cfg);
                self.service.set_limit(self.cfg.max_results);
                self.service.set_keywords(self.cfg.keywords.clone());
                // 应用索引（开始菜单/App Paths/UWP）立刻重建：新装或卸载应用后
                // 执行「重新加载」即可生效，不用重启。
                self.service.invalidate_apps();
                self.preview_open = self.cfg.preview;
                self.trigger(ctx);
                self.toast(ctx, "已重新加载（含应用索引）".to_string());
            }
            "cmd:everything" => match shell::launch_everything() {
                Ok(()) => self.toast(ctx, "Everything 已启动".to_string()),
                Err(err) => self.toast(ctx, err),
            },
            "cmd:quit" => {
                // 二次确认：命令页里的「退出 lstr」太容易误触（点一下整个程序
                // 就消失，体感像崩溃）。首次回车/点击给提示，3 秒内再次触发才退出。
                let armed = self
                    .quit_armed_at
                    .map(|t| t.elapsed() < Duration::from_secs(3))
                    .unwrap_or(false);
                if armed {
                    self.exit_reason = "cmd:quit";
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    self.quit_armed_at = Some(Instant::now());
                    self.toast(ctx, "再按一次回车确认退出（3 秒内有效）".to_string());
                }
            }
            _ => {}
        }
    }

    /// 自研索引状态 → 状态行文案(方案 §5:不闪、不骗人、复用横幅)。
    fn poll_index_status(&mut self, ctx: &egui::Context) {
        let status = self.service.index_status.lock().unwrap().clone();
        let mut note = None;
        if let Some(st) = &status {
            // L2:进度百分比由后台线程推着走,但 UI 只在阶段变化时被唤醒,
            // 不主动重绘的话横幅会停在第一次拿到的数字上。
            if matches!(
                st.phase,
                lstr_index::Phase::Scanning | lstr_index::Phase::Loading
            ) {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            if st.phase == lstr_index::Phase::Scanning {
                let total: u32 = st.volumes.iter().map(|v| v.count).sum();
                let complete = st.volumes.iter().filter(|v| v.complete).count();
                let pcts: Vec<f32> = st.volumes.iter().filter_map(|v| v.pct).collect();
                // 目录枚举模式没有总条数(est=0),只有「已扫条数」:
                // 此时用「已完成卷数/总卷数」表达进度,别拿单卷 100% 平均成 99%。
                let progress = if !pcts.is_empty() && pcts.len() == st.volumes.len() {
                    format!(
                        "{:.0}%",
                        (pcts.iter().sum::<f32>() / pcts.len() as f32 * 100.0).min(99.0)
                    )
                } else {
                    format!("({}/{})", complete, st.volumes.len())
                };
                // Everything 未运行时不要声称「正与 Everything 合并」。
                let suffix = if st.everything_ok {
                    "（结果暂不完整，正与 Everything 合并）"
                } else {
                    "（结果暂不完整）"
                };
                note = Some(format!(
                    "正在构建索引 {} · 已收录 {} 万项{}",
                    progress,
                    total / 10_000,
                    suffix
                ));
            } else if st.phase == lstr_index::Phase::Loading {
                // 主界面不显示任何提示时，用户会以为「卡住了」：等 UAC、等 worker
                // 连管道都可能停在这一阶段（旧 worker 占管道时甚至永久停住）。
                note = Some(if st.everything_ok {
                    "索引准备中：等待管理员授权 / worker 连接（当前由 Everything 出结果）"
                        .to_string()
                } else {
                    "索引准备中：等待管理员授权 / worker 连接（首次构建约 1~3 分钟）"
                        .to_string()
                });
            } else if st.phase == lstr_index::Phase::Fallback {
                // 只有配置期望用内置索引时才提示；backend=everything/off 是用户主动选择。
                let expect_builtin = matches!(
                    self.cfg.index.backend.as_str(),
                    "auto" | "builtin" | "usn"
                );
                if expect_builtin {
                    note = Some(if st.everything_ok {
                        "内置索引不可用（未获管理员授权或 worker 未运行），当前结果来自 Everything"
                            .to_string()
                    } else {
                        "内置索引不可用（未获管理员授权或 worker 未运行），暂时没有搜索结果"
                            .to_string()
                    });
                }
            } else if st.phase == lstr_index::Phase::Ready {
                // usn 后端:USN 层异常状态要浮出来(退避/停用 = 实时增量不可靠,
                // worker 正在自动对账或已停用该卷);正常 Watching 不打扰(安静原则)。
                if self.cfg.index.backend == "usn" {
                    let degraded: Vec<String> = st
                        .volumes
                        .iter()
                        .filter_map(|v| {
                            let s = v.usn_state?;
                            (s != lstr_index::usnwatch::UsnWatchState::Watching)
                                .then(|| format!("{} 卷 USN:{}", v.letter, s.label()))
                        })
                        .collect();
                    if !degraded.is_empty() {
                        note = Some(format!(
                            "USN 实时增量部分降级:{}(索引仍可用,worker 自动对账恢复)",
                            degraded.join(" · ")
                        ));
                    }
                }
            }
        }
        match &note {
            Some(_) => {
                if self.index_note_since.is_none() {
                    self.index_note_since = Some(std::time::Instant::now());
                }
                if let Some(t) = self.index_note_since {
                    if t.elapsed().as_millis() > 300 {
                        self.index_note = note;
                    }
                }
            }
            None => {
                self.index_note = None;
                self.index_note_since = None;
            }
        }
    }

    fn toast(&mut self, ctx: &egui::Context, message: String) {
        self.toast = Some((message, Instant::now() + Duration::from_millis(1300)));
        ctx.request_repaint_after(Duration::from_millis(1330));
    }

    fn animate(&mut self, ctx: &egui::Context) {
        let diff = self.needle_target - self.needle_y;
        if diff.abs() > 0.5 {
            self.needle_y += diff * 0.35;
            ctx.request_repaint();
        } else {
            self.needle_y = self.needle_target;
        }
        if let Some((_, until)) = &self.toast {
            let now = Instant::now();
            if now >= *until {
                self.toast = None;
            } else {
                ctx.request_repaint_after(*until - now);
            }
        }
    }

    fn paint(&mut self, ctx: &egui::Context) {
        let theme = self.theme;
        let entries = self.entries.clone();
        let query = search::query::parse(&self.input, self.scope);
        let row_h = theme.row_h;
        let list_h = if entries.is_empty() {
            120.0
        } else {
            entries.len() as f32 * row_h
        };
        let banner_h = if self.error.is_some() || self.regex_error.is_some() || self.index_note.is_some() { 30.0 } else { 0.0 };
        let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
        let preview_w = if self.preview_open { 216.0 } else { 0.0 };
        let height = if self.settings_open {
            600.0
        } else {
            (SEARCH_H
                + TABS_H
                + FILTER_H
                + if self.browse_root.is_some() { CRUMB_H } else { 0.0 }
                + LIST_GAP
                + list_h
                + LIST_GAP
                + banner_h
                + dialog_h
                + FOOTER_H
                + 8.0)
                .clamp(150.0, 540.0)
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let mut rect = ui.max_rect();
                if rect.height() < height - 2.0 {
                    rect.set_height(height);
                }
                let painter = ui.painter().clone();
                let bg = if self.acrylic_ok {
                    theme.bg
                } else {
                    Color32::from_rgba_unmultiplied(24, 28, 34, 246)
                };
                painter.rect_filled(rect, CornerRadius::same(theme.radius), bg);
                painter.rect_stroke(
                    rect.shrink(0.5),
                    CornerRadius::same(theme.radius),
                    Stroke::new(1.0_f32, theme.hair2),
                    StrokeKind::Inside,
                );
                if !theme.light {
                    let y = rect.top() + 0.5;
                    painter.line_segment(
                        [
                            egui::pos2(rect.left() + theme.radius as f32 + 2.0, y),
                            egui::pos2(rect.right() - theme.radius as f32 - 2.0, y),
                        ],
                        Stroke::new(1.0_f32, Color32::from_white_alpha(16)),
                    );
                }

                if self.settings_open {
                    // 设置面板也能拖：标题整条 44px 都可拖，关闭按钮后注册会优先命中。
                    self.paint_drag_strip(ui, &painter, rect, 44.0);
                    self.paint_settings(ui, ctx, rect);
                    return;
                }

                // 右侧图标组（清除 ×/历史/设置）从 right-PAD-72 排到 right-PAD，
                // 文本区必须留出这段再加 8px 呼吸位，否则长输入会画到图标下面
                // （TextEdit 只在控件矩形内裁剪，rect 给满宽就必然叠字）。
                const ICON_AREA: f32 = 80.0;
                let search_rect = Rect::from_min_size(
                    egui::pos2(rect.left() + PAD, rect.top() + 14.0),
                    egui::vec2(
                        (rect.width() - 2.0 * PAD - ICON_AREA).max(120.0),
                        28.0,
                    ),
                );
                let hint = if self.in_browse() {
                    "在当前目录过滤…".to_string()
                } else if self.filters.matches.regex {
                    "输入正则表达式…".to_string()
                } else if self.dialog.is_some() {
                    dialog_hint(query.scope).to_string()
                } else if let Some(folder) = &self.typing_folder {
                    if self.typing_global {
                        "全局搜索中（Ctrl+Shift+G 回到当前目录）…".to_string()
                    } else {
                        let name = folder
                            .file_name()
                            .map(|part| part.to_string_lossy().into_owned())
                            .unwrap_or_else(|| folder.to_string_lossy().into_owned());
                        format!("在 {name} 中搜索（Ctrl+Shift+G 全局）…")
                    }
                } else {
                    hint_for(query.scope).to_string()
                };
                let response = ui.put(
                    search_rect,
                    egui::TextEdit::singleline(&mut self.input)
                        .frame(false)
                        .margin(egui::Margin::same(0))
                        .font(ui::text_font(22.0))
                        .text_color(theme.text)
                        .hint_text(hint.as_str())
                        .desired_width(f32::INFINITY)
                        .id(egui::Id::new("lstr-search-input")),
                );
                if response.changed() {
                    self.menu_open = false;
                    self.filter_menu = None;
                    self.trigger(ctx);
                }
                let window_focused = ctx.input(|i| i.focused);
                if self.focus_requested || (window_focused && !response.has_focus()) {
                    response.request_focus();
                    self.focus_requested = false;
                }
                let focus_color = if response.has_focus() {
                    theme.amber
                } else {
                    theme.hair
                };
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + PAD, rect.top() + SEARCH_H),
                        egui::pos2(rect.right() - PAD, rect.top() + SEARCH_H),
                    ],
                    Stroke::new(1.0_f32, focus_color),
                );

                // 右侧图标统一垂直居中于搜索文本行（文本区 top+14 高 28 → 中线 top+28）。
                let icon_top = rect.top() + 18.0;
                let gear_rect = Rect::from_min_size(
                    egui::pos2(rect.right() - PAD - 20.0, icon_top),
                    egui::vec2(20.0, 20.0),
                );
                let gear = ui.interact(gear_rect, Id::new("lstr-gear"), Sense::click());
                if gear.hovered() {
                    painter.rect_filled(
                        gear_rect.shrink(1.0),
                        CornerRadius::same(6),
                        theme.surface_hi,
                    );
                    painter.rect_stroke(
                        gear_rect.shrink(1.0),
                        CornerRadius::same(6),
                        Stroke::new(1.0_f32, theme.hair2),
                        StrokeKind::Inside,
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let gear_color = if gear.hovered() { theme.text } else { theme.sub };
                painter.text(
                    gear_rect.center(),
                    Align2::CENTER_CENTER,
                    "⚙",
                    ui::icon_font(14.0),
                    gear_color,
                );
                if gear.clicked() {
                    self.open_settings(ctx);
                }

                // 历史开关：双击 Space 唤出时是否展示「最近使用」。
                let history_rect = Rect::from_min_size(
                    egui::pos2(rect.right() - PAD - 46.0, icon_top),
                    egui::vec2(20.0, 20.0),
                );
                let history =
                    ui.interact(history_rect, Id::new("lstr-history-toggle"), Sense::click());
                let history_on = self.cfg.show_history;
                // 开启态复用筛选条的语言：amber_soft 底 + 琥珀色图标；关闭态只剩
                // 灰色图标。不再画斜线——那读起来像「禁止」，而且比齿轮丑。
                if history_on || history.hovered() {
                    painter.rect_filled(
                        history_rect.shrink(1.0),
                        CornerRadius::same(6),
                        if history_on {
                            theme.amber_soft
                        } else {
                            theme.surface_hi
                        },
                    );
                    painter.rect_stroke(
                        history_rect.shrink(1.0),
                        CornerRadius::same(6),
                        Stroke::new(
                            1.0_f32,
                            if history_on {
                                theme.amber.gamma_multiply(0.6)
                            } else {
                                theme.hair2
                            },
                        ),
                        StrokeKind::Inside,
                    );
                }
                if history.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let history_color = if history_on {
                    theme.amber
                } else if history.hovered() {
                    theme.sub
                } else {
                    theme.dim
                };
                // 时钟字形：和齿轮同一字体族、同一字号，竖向墨迹才能对齐。
                painter.text(
                    history_rect.center(),
                    Align2::CENTER_CENTER,
                    "\u{1F553}",
                    ui::icon_font(14.0),
                    history_color,
                );
                if history.hovered() {
                    history.clone().on_hover_text(if self.cfg.show_history {
                        "历史记录：开（点击关闭）"
                    } else {
                        "历史记录：关（点击开启）"
                    });
                }
                if history.clicked() {
                    self.cfg.show_history = !self.cfg.show_history;
                    let _ = self.cfg.save();
                    self.selected = 0;
                    self.trigger(ctx);
                    self.toast(
                        ctx,
                        if self.cfg.show_history {
                            "历史记录已开启"
                        } else {
                            "历史记录已关闭"
                        }
                        .to_string(),
                    );
                }

                if !self.input.is_empty() {
                    let clear_rect = Rect::from_min_size(
                        egui::pos2(rect.right() - PAD - 72.0, icon_top),
                        egui::vec2(20.0, 20.0),
                    );
                    let clear =
                        ui.interact(clear_rect, Id::new("lstr-clear"), Sense::click());
                    if clear.hovered() {
                        painter.rect_filled(
                            clear_rect.shrink(1.0),
                            CornerRadius::same(6),
                            theme.surface_hi,
                        );
                        painter.rect_stroke(
                            clear_rect.shrink(1.0),
                            CornerRadius::same(6),
                            Stroke::new(1.0_f32, theme.hair2),
                            StrokeKind::Inside,
                        );
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    painter.text(
                        clear_rect.center(),
                        Align2::CENTER_CENTER,
                        "×",
                        ui::text_font(14.0),
                        if clear.hovered() { theme.text } else { theme.dim },
                    );
                    if clear.clicked() {
                        lstr_index::snapshot::debug_log("ui: clear_button");
                        if self.in_browse() && self.input.is_empty() {
                            // 浏览模式过滤词已空：× 退出浏览。
                            self.exit_browse(ctx);
                        } else {
                            self.input.clear();
                            self.selected = 0;
                            self.menu_open = false;
                            self.focus_requested = true;
                            self.trigger(ctx);
                        }
                    }
                }

                self.paint_tabs(ui, &painter, rect, &query);
                self.paint_filters(ui, &painter, rect);
                let crumb_h = if let Some(root) = self.browse_root.clone() {
                    self.paint_breadcrumb(ui, &painter, rect, &root);
                    CRUMB_H
                } else {
                    0.0
                };
                let list_top =
                    rect.top() + SEARCH_H + TABS_H + FILTER_H + crumb_h + LIST_GAP;
                let list_rect = Rect::from_min_max(
                    rect.left_top(),
                    egui::pos2(rect.right() - PAD - preview_w, rect.bottom()),
                );
                // 列表可视高度：扣掉底部提示条、错误条/对话框横幅。条目超出时
                // 窗口高度封顶 800，多的部分靠滚轮滚动查看。
                let view_bottom = rect.bottom() - FOOTER_H - banner_h - dialog_h - LIST_GAP;
                let view_h = (view_bottom - list_top).max(theme.row_h);
                // 滚动条贴着窗口右缘（无预览时）；开预览时贴列表右边界。
                let scroll_right = if preview_w > 0.0 {
                    list_rect.right()
                } else {
                    rect.right()
                };
                self.paint_rows(
                    ui, &painter, list_rect, list_top, view_h, scroll_right, &entries, &query,
                );

                let banner_top = list_top + list_h.min(view_h) + LIST_GAP;
                let mut extra_top = banner_top;
                if self.error.is_none() && self.regex_error.is_none() {
                    if let Some(note) = self.index_note.clone() {
                        let banner = Rect::from_min_size(
                            egui::pos2(rect.left() + PAD, extra_top),
                            egui::vec2(rect.width() - 2.0 * PAD, 24.0),
                        );
                        painter.rect_filled(banner, CornerRadius::same(7), theme.amber_soft);
                        painter.circle_filled(
                            egui::pos2(banner.left() + 11.0, banner.center().y),
                            3.0,
                            theme.sub,
                        );
                        painter.text(
                            egui::pos2(banner.left() + 21.0, banner.center().y),
                            Align2::LEFT_CENTER,
                            note,
                            ui::text_font(11.5),
                            theme.sub,
                        );
                        extra_top += 30.0;
                    }
                }
                if let Some(error) = self.error.clone() {
                    let banner = Rect::from_min_size(
                        egui::pos2(rect.left() + PAD, extra_top),
                        egui::vec2(rect.width() - 2.0 * PAD, 24.0),
                    );
                    let response =
                        ui.interact(banner, Id::new("lstr-error-banner"), Sense::click());
                    painter.rect_filled(banner, CornerRadius::same(7), theme.amber_soft);
                    painter.rect_stroke(
                        banner,
                        CornerRadius::same(7),
                        Stroke::new(
                            1.0_f32,
                            if response.hovered() {
                                theme.amber.gamma_multiply(0.6)
                            } else {
                                theme.amber.gamma_multiply(0.35)
                            },
                        ),
                        StrokeKind::Inside,
                    );
                    painter.circle_filled(
                        egui::pos2(banner.left() + 11.0, banner.center().y),
                        3.0,
                        theme.amber,
                    );
                    painter.text(
                        egui::pos2(banner.left() + 21.0, banner.center().y),
                        Align2::LEFT_CENTER,
                        format!("{error} · 点击启动 Everything"),
                        ui::text_font(11.5),
                        theme.text,
                    );
                    if response.clicked() {
                        match shell::launch_everything() {
                            Ok(()) => {
                                self.error = None;
                                self.toast(ctx, "Everything 已启动，正在连接…".to_string());
                                self.trigger(ctx);
                            }
                            Err(err) => self.toast(ctx, err),
                        }
                    }
                    extra_top += 30.0;
                }
                if let Some(error) = self.regex_error.clone() {
                    let banner = Rect::from_min_size(
                        egui::pos2(rect.left() + PAD, extra_top),
                        egui::vec2(rect.width() - 2.0 * PAD, 24.0),
                    );
                    painter.rect_filled(banner, CornerRadius::same(7), theme.amber_soft);
                    painter.circle_filled(
                        egui::pos2(banner.left() + 11.0, banner.center().y),
                        3.0,
                        theme.amber,
                    );
                    painter.text(
                        egui::pos2(banner.left() + 21.0, banner.center().y),
                        Align2::LEFT_CENTER,
                        error,
                        ui::text_font(11.5),
                        theme.text,
                    );
                    extra_top += 30.0;
                }
                if preview_w > 0.0 {
                    let footer_top = rect.bottom() - FOOTER_H;
                    painter.line_segment(
                        [
                            egui::pos2(list_rect.right() + 3.0, rect.top() + SEARCH_H),
                            egui::pos2(list_rect.right() + 3.0, footer_top),
                        ],
                        Stroke::new(1.0_f32, theme.hair),
                    );
                    let pane = Rect::from_min_max(
                        egui::pos2(list_rect.right() + 9.0, list_top),
                        egui::pos2(rect.right() - PAD, footer_top - 6.0),
                    );
                    if let Some(entry) = entries.get(self.selected) {
                        self.paint_preview(ui, &painter, pane, entry, &query.text);
                    }
                }
                if let Some(dialog) = &self.dialog {
                    let strip = Rect::from_min_size(
                        egui::pos2(rect.left() + PAD, extra_top),
                        egui::vec2(rect.width() - 2.0 * PAD, 24.0),
                    );
                    painter.rect_filled(strip, CornerRadius::same(7), theme.amber_soft);
                    painter.rect_stroke(
                        strip,
                        CornerRadius::same(7),
                        Stroke::new(1.0_f32, theme.amber.gamma_multiply(0.35)),
                        StrokeKind::Inside,
                    );
                    painter.circle_filled(
                        egui::pos2(strip.left() + 11.0, strip.center().y),
                        3.0,
                        theme.amber,
                    );
                    let label = if dialog.is_save {
                        "保存对话框 · Enter 填入文件名"
                    } else {
                        "文件对话框 · Enter 跳转 / 填入"
                    };
                    painter.text(
                        egui::pos2(strip.left() + 21.0, strip.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        ui::text_font(11.5),
                        theme.text,
                    );
                }

                self.paint_footer(&painter, rect, &entries, &query);
                self.paint_menu(ctx, list_rect);
                self.paint_drag_resize(ui, &painter, rect);
            });
    }

    fn paint_tabs(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        query: &crate::model::Query,
    ) {
        let theme = self.theme;
        let y = rect.top() + SEARCH_H + 6.0;
        let mut x = rect.left() + PAD;
        let mut clicked = None;
        for scope in Scope::ALL {
            let active = query.scope == scope;
            let color = if active { theme.text } else { theme.sub };
            let galley = painter.layout_no_wrap(
                scope.label().to_string(),
                ui::text_font(12.5),
                color,
            );
            let width = galley.size().x;
            let tab_rect = Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, 20.0));
            let response = ui.interact(
                tab_rect,
                Id::new(("lstr-tab", scope.index())),
                Sense::click(),
            );
            if response.hovered() && !active {
                painter.galley(
                    egui::pos2(x, y),
                    painter.layout_no_wrap(
                        scope.label().to_string(),
                        ui::text_font(12.5),
                        theme.text,
                    ),
                    theme.text,
                );
            } else {
                painter.galley(egui::pos2(x, y), galley, color);
            }
            if active {
                painter.rect_filled(
                    Rect::from_min_size(egui::pos2(x, y + 20.0), egui::vec2(width, 2.0)),
                    CornerRadius::same(1),
                    theme.amber,
                );
            }
            if response.clicked() {
                clicked = Some(scope);
            }
            x += width + 18.0;
        }
        painter.text(
            egui::pos2(rect.right() - PAD, y + 4.0),
            Align2::RIGHT_TOP,
            "Tab · Alt ←→",
            ui::mono_font(10.0),
            theme.dim,
        );
        if let Some(scope) = clicked {
            self.set_scope(ui.ctx(), scope);
        }
    }

    /// 浏览模式的面包屑：占用原类别 Tab 那一行（高 20px），窗口高度不变。
    /// 祖先可点击跳转；中间放不下时折叠成 …，点开列出隐藏的祖先。
    fn paint_breadcrumb(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        root: &std::path::Path,
    ) {
        let theme = self.theme;
        let font = ui::text_font(12.5);
        // 面包屑在筛选条下方独占一行；TABS/FILTER 两行保留给主界面选项。
        let y = rect.top() + SEARCH_H + TABS_H + FILTER_H + 1.0;
        let height = 20.0;
        let hint = "Ctrl+L 编辑 · Alt↑ 上级";
        let hint_galley = painter.layout_no_wrap(hint.to_string(), ui::mono_font(10.0), theme.dim);
        let hint_w = hint_galley.size().x + 16.0;
        let max_right = rect.right() - PAD - hint_w;
        let items = breadcrumb_items(root);
        let widths: Vec<f32> = items
            .iter()
            .map(|(label, _)| {
                painter
                    .layout_no_wrap(label.clone(), font.clone(), theme.sub)
                    .size()
                    .x
            })
            .collect();
        let sep_w = 14.0;
        let total: f32 = widths.iter().sum::<f32>()
            + sep_w * items.len().saturating_sub(1) as f32;
        let avail = max_right - (rect.left() + PAD);

        // 可见项：全放不下时保留“此电脑”+ 末尾若干项，中间用 … 折叠。
        let mut visible: Vec<usize> = (0..items.len()).collect();
        let mut hidden: Vec<usize> = Vec::new();
        if total > avail && items.len() > 2 {
            let ell_w = 18.0;
            let mut used = widths[0];
            let mut tail: Vec<usize> = Vec::new();
            for idx in (1..items.len()).rev() {
                let cost = sep_w + widths[idx];
                if used + ell_w + sep_w + cost > avail {
                    break;
                }
                tail.push(idx);
                used += cost;
            }
            if !tail.is_empty() {
                tail.reverse();
                hidden = (1..items.len()).filter(|i| !tail.contains(i)).collect();
                visible = std::iter::once(0).chain(tail).collect();
            }
        }

        enum Part {
            Item(usize),
            Ellipsis,
        }
        let mut parts: Vec<Part> = Vec::new();
        if hidden.is_empty() {
            parts.extend(visible.iter().copied().map(Part::Item));
        } else {
            parts.push(Part::Item(visible[0]));
            parts.push(Part::Ellipsis);
            parts.extend(visible[1..].iter().copied().map(Part::Item));
        }

        let mut x = rect.left() + PAD;
        let mut clicked: Option<usize> = None;
        let mut more_clicked = false;
        for (n, part) in parts.iter().enumerate() {
            if n > 0 {
                painter.text(
                    egui::pos2(x, y + height * 0.5),
                    Align2::LEFT_CENTER,
                    "›",
                    ui::text_font(11.0),
                    theme.dim,
                );
                x += sep_w;
            }
            match part {
                Part::Item(idx) => {
                    let (label, path) = &items[*idx];
                    let w = widths[*idx];
                    let crumb = Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, height));
                    let response =
                        ui.interact(crumb, Id::new(("lstr-crumb", *idx)), Sense::click());
                    let active = *idx == items.len() - 1;
                    let color = if active {
                        theme.text
                    } else if response.hovered() {
                        theme.amber
                    } else {
                        theme.sub
                    };
                    painter.galley(
                        egui::pos2(x, y),
                        painter.layout_no_wrap(label.clone(), font.clone(), color),
                        color,
                    );
                    if response.hovered() {
                        response.clone().on_hover_text(path.display().to_string());
                    }
                    if response.clicked() {
                        clicked = Some(*idx);
                    }
                    x += w;
                }
                Part::Ellipsis => {
                    let r = Rect::from_min_size(egui::pos2(x, y), egui::vec2(18.0, height));
                    let response = ui.interact(r, Id::new("lstr-crumb-more"), Sense::click());
                    let color = if response.hovered() {
                        theme.amber
                    } else {
                        theme.dim
                    };
                    painter.galley(
                        egui::pos2(x + 2.0, y),
                        painter.layout_no_wrap("…".to_string(), font.clone(), color),
                        color,
                    );
                    if response.clicked() {
                        more_clicked = true;
                    }
                    x += 18.0;
                }
            }
        }
        painter.galley(
            egui::pos2(rect.right() - PAD - hint_galley.size().x, y),
            hint_galley,
            theme.dim,
        );

        if more_clicked {
            self.crumb_more = true;
        }
        if let Some(idx) = clicked {
            self.crumb_more = false;
            let path = items[idx].1.clone();
            if path.as_os_str().is_empty() {
                self.exit_browse(ui.ctx());
            } else {
                self.browse_root = Some(path);
                self.browse_forward.clear();
                self.input.clear();
                self.selected = 0;
                self.scroll_y = 0.0;
                self.menu_open = false;
                self.filter_menu = None;
                Self::reset_input_cursor(ui.ctx());
                self.focus_requested = true;
                self.trigger(ui.ctx());
            }
            return;
        }

        if self.crumb_more && !hidden.is_empty() {
            let mut more_clicked_item: Option<usize> = None;
            let area = egui::Area::new(Id::new("lstr-crumb-more-menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(rect.left() + PAD, y + 24.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame {
                        fill: theme.menu,
                        stroke: Stroke::new(1.0_f32, theme.hair2),
                        corner_radius: CornerRadius::same(10),
                        inner_margin: egui::Margin::same(4),
                        ..Default::default()
                    }
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        for idx in &hidden {
                            let (row, response) = ui.allocate_exact_size(
                                egui::vec2(260.0, 26.0),
                                Sense::click(),
                            );
                            if response.hovered() {
                                ui.painter().rect_filled(
                                    row.shrink(2.0),
                                    CornerRadius::same(6),
                                    theme.surface_hi,
                                );
                            }
                            ui.painter().text(
                                egui::pos2(row.left() + 8.0, row.center().y),
                                Align2::LEFT_CENTER,
                                &items[*idx].0,
                                ui::text_font(12.0),
                                theme.text,
                            );
                            if response.clicked() {
                                more_clicked_item = Some(*idx);
                            }
                        }
                    });
                });
            if let Some(idx) = more_clicked_item {
                self.crumb_more = false;
                let path = items[idx].1.clone();
                if path.as_os_str().is_empty() {
                    self.exit_browse(ui.ctx());
                } else {
                    self.browse_root = Some(path);
                self.browse_forward.clear();
                    self.input.clear();
                    self.selected = 0;
                    self.scroll_y = 0.0;
                    Self::reset_input_cursor(ui.ctx());
                    self.focus_requested = true;
                    self.trigger(ui.ctx());
                }
            } else if ui.ctx().input(|i| i.pointer.any_click()) {
                if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    if !area.response.rect.contains(pos) {
                        self.crumb_more = false;
                    }
                }
            }
        }
    }

    /// 筛选条：位置 / 时间 / 类型三个按钮。用户点选即可，内部自动翻译成
    /// Everything 的 path:/dm:/ext: 查询段——不需要记语法。
    /// 匹配 chip 的标签要能看出当前选了什么，而不是永远只写「匹配」。
    fn match_chip_label(&self) -> String {
        let matches = self.filters.matches;
        let mut parts: Vec<&str> = Vec::new();
        if matches.regex {
            parts.push("正则");
        } else if matches.whole_word {
            parts.push("全词");
        }
        if matches.case_sensitive {
            parts.push("大小写");
        }
        if parts.is_empty() {
            "匹配".to_string()
        } else {
            format!("匹配 · {}", parts.join("+"))
        }
    }

    fn clear_filter_kind(&mut self, kind: FilterMenuKind) {
        match kind {
            FilterMenuKind::Location => self.filters.location = None,
            FilterMenuKind::Time => self.filters.time = None,
            FilterMenuKind::Group => self.filters.group = None,
            FilterMenuKind::Match => self.filters.matches = Default::default(),
        }
        self.filter_menu = None;
    }

    fn paint_filters(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let theme = self.theme;
        let y = rect.top() + SEARCH_H + TABS_H + 2.0;
        // PathTyping 下筛选不生效；Browse 下位置由面包屑决定，其余筛选可用。
        let typing = self.path_mode();
        let browse = self.in_browse();
        let mut x = rect.left() + PAD;

        let chips: [(FilterMenuKind, String, bool); 4] = [
            (
                FilterMenuKind::Location,
                if browse {
                    "位置 · 当前目录".to_string()
                } else {
                    format!(
                        "位置 · {}",
                        self.filters
                            .location
                            .as_ref()
                            .map(|location| location.label())
                            .unwrap_or_else(|| "全部".to_string())
                    )
                },
                !browse && self.filters.location.is_some(),
            ),
            (
                FilterMenuKind::Time,
                format!(
                    "时间 · {}",
                    self.filters
                        .time
                        .map(|time| time.label())
                        .unwrap_or("不限")
                ),
                self.filters.time.is_some(),
            ),
            (
                FilterMenuKind::Group,
                format!(
                    "类型 · {}",
                    self.filters
                        .group
                        .map(|group| group.label())
                        .unwrap_or("全部")
                ),
                self.filters.group.is_some(),
            ),
            (
                FilterMenuKind::Match,
                self.match_chip_label(),
                !self.filters.matches.is_default(),
            ),
        ];

        let mut clicked = None;
        for (kind, label, active) in chips {
            // PathTyping 全部禁用；Browse 只禁用「位置」（由面包屑决定）。
            let disabled = typing || (browse && kind == FilterMenuKind::Location);
            let color = if disabled {
                theme.dim
            } else if active {
                theme.amber
            } else {
                theme.sub
            };
            let galley = painter.layout_no_wrap(label, ui::text_font(11.0), color);
            // 激活的 chip 右侧留出单独的 × 命中区：只清这一项，不影响其它条件。
            let clear_w = if active && !disabled { 14.0 } else { 0.0 };
            let width = galley.size().x + 28.0 + clear_w;
            let chip = Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, 22.0));
            let response = ui.interact(
                chip,
                Id::new(("lstr-filter-chip", kind.index())),
                if disabled { Sense::hover() } else { Sense::click() },
            );
            if !disabled {
                if active {
                    painter.rect_filled(chip, CornerRadius::same(6), theme.amber_soft);
                } else if response.hovered() {
                    painter.rect_filled(chip, CornerRadius::same(6), theme.hover);
                }
                painter.rect_stroke(
                    chip,
                    CornerRadius::same(6),
                    Stroke::new(
                        1.0_f32,
                        if active {
                            theme.amber.gamma_multiply(0.6)
                        } else {
                            theme.hair
                        },
                    ),
                    StrokeKind::Inside,
                );
            }
            painter.galley(
                egui::pos2(chip.left() + 9.0, chip.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
            // 下拉三角用绘制而非字符：▾ 这类符号在 SysFont 里可能缺字形（显示成方框）。
            let cx = chip.right() - 10.0 - clear_w;
            let cy = chip.center().y + 1.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(cx - 3.5, cy - 2.0),
                    egui::pos2(cx + 3.5, cy - 2.0),
                    egui::pos2(cx, cy + 2.5),
                ],
                color,
                Stroke::NONE,
            ));
            let clear_rect = if active && !disabled {
                Some(Rect::from_min_size(
                    egui::pos2(chip.right() - 15.0, chip.top() + 4.0),
                    egui::vec2(12.0, 14.0),
                ))
            } else {
                None
            };
            if let Some(clear_rect) = clear_rect {
                painter.text(
                    clear_rect.center(),
                    Align2::CENTER_CENTER,
                    "×",
                    ui::text_font(11.0),
                    theme.amber,
                );
            }
            if response.clicked() && !disabled {
                // 用指针位置区分「点 ×」和「点 chip 打开菜单」；两个独立
                // interact 重叠时父级会吞掉子级的 click，这里手工命中测试。
                let clicked_clear = clear_rect.is_some_and(|rect| {
                    ui.ctx()
                        .input(|i| i.pointer.interact_pos())
                        .is_some_and(|pos| rect.contains(pos))
                });
                if clicked_clear {
                    self.clear_filter_kind(kind);
                    self.trigger(ui.ctx());
                } else {
                    clicked = Some(kind);
                }
            }
            x += width + 8.0;
        }

        // 有筛选或匹配选项时才出现清除入口。
        if (!self.filters.is_empty() || !self.filters.matches.is_default()) && !typing {
            // 窄窗口下四个 chip 已接近占满，清除退化为单字符 ×，避免压到 chip。
            let label = if x + 84.0 > rect.right() - PAD {
                "×"
            } else {
                "× 清除全部"
            };
            let galley = painter.layout_no_wrap(label.to_string(), ui::text_font(10.5), theme.dim);
            let clear = Rect::from_min_size(
                egui::pos2(x + 4.0, y + 3.0),
                egui::vec2(galley.size().x + 8.0, 16.0),
            );
            let response = ui.interact(clear, Id::new("lstr-filter-clear"), Sense::click());
            painter.galley(
                egui::pos2(clear.left(), clear.center().y - galley.size().y / 2.0),
                galley,
                if response.hovered() { theme.text } else { theme.dim },
            );
            if response.clicked() {
                self.filters = SearchFilters::default();
                self.filter_menu = None;
                self.trigger(ui.ctx());
            }
            x = clear.right();
        }

        if typing {
            painter.text(
                egui::pos2(x + 8.0, y + 11.0),
                Align2::LEFT_CENTER,
                "路径模式中筛选不生效",
                ui::mono_font(10.0),
                theme.dim,
            );
        }

        if let Some(kind) = clicked {
            self.filter_menu = (self.filter_menu != Some(kind)).then_some(kind);
        }

        self.paint_filter_menu(ui, rect);
    }

    fn filter_items(&self, kind: FilterMenuKind) -> Vec<FilterItem> {
        let mut items = Vec::new();
        let active = match kind {
            FilterMenuKind::Location => self.filters.location.is_some(),
            FilterMenuKind::Time => self.filters.time.is_some(),
            FilterMenuKind::Group => self.filters.group.is_some(),
            FilterMenuKind::Match => !self.filters.matches.is_default(),
        };
        if active {
            items.push(FilterItem {
                label: match kind {
                    FilterMenuKind::Location => "清除位置",
                    FilterMenuKind::Time => "清除时间",
                    FilterMenuKind::Group => "清除类型",
                    FilterMenuKind::Match => "清除匹配选项",
                }
                .to_string(),
                selected: false,
                choice: FilterChoice::Clear(kind),
            });
        }
        match kind {
            FilterMenuKind::Location => {
                items.push(FilterItem {
                    label: "全部位置".to_string(),
                    selected: self.filters.location.is_none(),
                    choice: FilterChoice::Location(None),
                });
                for location in crate::search::filters::user_locations() {
                    items.push(FilterItem {
                        label: location.label(),
                        selected: self.filters.location.as_ref() == Some(&location),
                        choice: FilterChoice::Location(Some(location)),
                    });
                }
                for drive in crate::search::filters::available_drives() {
                    let location = LocationFilter::Drive(drive);
                    items.push(FilterItem {
                        label: format!("{drive}:"),
                        selected: self.filters.location.as_ref() == Some(&location),
                        choice: FilterChoice::Location(Some(location)),
                    });
                }
            }
            FilterMenuKind::Time => {
                items.push(FilterItem {
                    label: "不限时间".to_string(),
                    selected: self.filters.time.is_none(),
                    choice: FilterChoice::Time(None),
                });
                for time in [
                    TimeFilter::Today,
                    TimeFilter::Week,
                    TimeFilter::Month,
                    TimeFilter::Year,
                ] {
                    items.push(FilterItem {
                        label: time.label().to_string(),
                        selected: self.filters.time == Some(time),
                        choice: FilterChoice::Time(Some(time)),
                    });
                }
            }
            FilterMenuKind::Group => {
                items.push(FilterItem {
                    label: "全部类型".to_string(),
                    selected: self.filters.group.is_none(),
                    choice: FilterChoice::Group(None),
                });
                for group in FileGroup::ALL {
                    items.push(FilterItem {
                        label: group.label().to_string(),
                        selected: self.filters.group == Some(group),
                        choice: FilterChoice::Group(Some(group)),
                    });
                }
            }
            FilterMenuKind::Match => {
                for (label, toggle) in [
                    ("区分大小写", MatchToggle::CaseSensitive),
                    ("全词匹配", MatchToggle::WholeWord),
                    ("正则表达式", MatchToggle::Regex),
                ] {
                    items.push(FilterItem {
                        label: label.to_string(),
                        selected: self.filters.matches.get(toggle),
                        choice: FilterChoice::Match(toggle),
                    });
                }
            }
        }
        items
    }

    fn paint_filter_menu(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let Some(kind) = self.filter_menu else {
            return;
        };
        let theme = self.theme;
        let items = self.filter_items(kind);
        let item_h = 28.0;
        let width = 200.0;
        let top = rect.top() + SEARCH_H + TABS_H + FILTER_H;
        let left = rect.left() + PAD;
        let mut clicked = None;

        let area = egui::Area::new(Id::new(("lstr-filter-menu", kind.index())))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(left, top))
            .show(ui.ctx(), |ui| {
                let frame = egui::Frame {
                    fill: theme.menu,
                    stroke: Stroke::new(1.0_f32, theme.hair2),
                    corner_radius: CornerRadius::same(10),
                    inner_margin: egui::Margin::same(4),
                    shadow: egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 30,
                        spread: 0,
                        color: Color32::from_black_alpha(110),
                    },
                    ..Default::default()
                };
                frame.show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    for (index, item) in items.iter().enumerate() {
                        let (row, response) =
                            ui.allocate_exact_size(egui::vec2(width - 8.0, item_h), Sense::click());
                        if response.hovered() {
                            ui.painter().rect_filled(
                                row.shrink(2.0),
                                CornerRadius::same(6),
                                theme.surface_hi,
                            );
                        }
                        let color = if item.selected { theme.amber } else { theme.text };
                        ui.painter().text(
                            egui::pos2(row.left() + 8.0, row.center().y),
                            Align2::LEFT_CENTER,
                            &item.label,
                            ui::text_font(12.5),
                            color,
                        );
                        if item.selected {
                            // ✓ 同样可能缺字形，用两段线画一个对勾。
                            let tip = egui::pos2(row.right() - 14.0, row.center().y + 1.0);
                            let stroke = Stroke::new(1.6_f32, theme.amber);
                            ui.painter().line_segment(
                                [tip + egui::vec2(-5.0, 0.0), tip + egui::vec2(-1.5, 3.5)],
                                stroke,
                            );
                            ui.painter().line_segment(
                                [tip + egui::vec2(-1.5, 3.5), tip + egui::vec2(4.5, -4.5)],
                                stroke,
                            );
                        }
                        if response.clicked() {
                            clicked = Some(index);
                        }
                    }
                });
            });

        if let Some(index) = clicked {
            if let Some(item) = items.get(index) {
                match &item.choice {
                    // 匹配选项是多选：点一下切换，菜单保持打开，方便连续勾选。
                    FilterChoice::Match(toggle) => {
                        let next = !self.filters.matches.get(*toggle);
                        self.filters.matches.set(*toggle, next);
                        if self.filters.matches.regex
                            && matches!(self.scope, Scope::App | Scope::Command)
                        {
                            self.scope = Scope::All;
                        }
                        self.trigger(ui.ctx());
                        return;
                    }
                    FilterChoice::Clear(clear_kind) => {
                        let clear_kind = *clear_kind;
                        self.clear_filter_kind(clear_kind);
                        self.trigger(ui.ctx());
                        return;
                    }
                    FilterChoice::Location(value) => self.filters.location = value.clone(),
                    FilterChoice::Time(value) => self.filters.time = *value,
                    FilterChoice::Group(value) => self.filters.group = *value,
                }
                self.filter_menu = None;
                self.trigger(ui.ctx());
            }
            return;
        }

        // 点击菜单外、且不在筛选条上时收起（点筛选条本身用于切换/关闭）。
        let chip_row = Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + SEARCH_H + TABS_H),
            egui::pos2(
                rect.right(),
                rect.top() + SEARCH_H + TABS_H + FILTER_H,
            ),
        );
        if ui.ctx().input(|i| i.pointer.any_click()) {
            if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                if !area.response.rect.contains(pos) && !chip_row.contains(pos) {
                    self.filter_menu = None;
                }
            }
        }
    }

    fn paint_rows(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        list_top: f32,
        view_h: f32,
        scroll_right: f32,
        entries: &[Entry],
        query: &crate::model::Query,
    ) {
        let theme = self.theme;
        if entries.is_empty() {
            // 空查询（还没开始搜索）：分类两列的搜索技巧面板。
            if query.is_empty() && self.browse_root.is_none() {
                self.needle_target = 0.0;
                self.scrollable = false;
                // 左列「搜索语法」：位置 / 筛选 / 范围；右列「输入与快捷键」。
                // 分类标题（sub 色）+ 键名（mono sub）+ 说明（mono dim），
                // 窗口窄时退化为单列顺排。
                let syntax: [(&str, &[(&str, &str)]); 3] = [
                    (
                        "位置",
                        &[
                            ("c:  d:", "只搜这些盘，可多个"),
                            ("-c:", "排除某盘，如 ap -c:"),
                            ("path:D:\\目录", "限定到某目录"),
                        ],
                    ),
                    (
                        "筛选",
                        &[
                            ("ext:rs", "按扩展名"),
                            ("时间 · 类型", "用下方筛选条切换"),
                            ("匹配", "大小写 / 全词 / 正则"),
                        ],
                    ),
                    (
                        "范围",
                        &[("a 微信", "搜应用"), ("f 文件", "只搜文件"), ("> 命令", "命令页")],
                    ),
                ];
                let misc: [(&str, &[(&str, &str)]); 2] = [
                    (
                        "输入",
                        &[
                            ("拼音", "全拼或首字母，wx → 微信"),
                            ("Tab", "切换范围"),
                            ("Alt+←", "返回上级范围"),
                        ],
                    ),
                    (
                        "快捷键",
                        &[
                            ("Esc", "收起（再唤出保留输入）"),
                            ("Ctrl+G", "对话框搜索并填入"),
                            ("Shift+F10", "动作菜单（文件夹也可）"),
                            ("双击 Ctrl", "唤出"),
                        ],
                    ),
                ];
                let x0 = rect.left() + 26.0;
                let two_col = rect.width() >= 560.0;
                let col_w = if two_col {
                    (rect.width() - 64.0) / 2.0
                } else {
                    rect.width() - 52.0
                };
                let key_w = 96.0;
                let draw_group = |painter: &egui::Painter, x: f32, y: &mut f32, (title, rows): (&str, &[(&str, &str)])| {
                    painter.text(
                        egui::pos2(x, *y),
                        Align2::LEFT_TOP,
                        title,
                        ui::text_font(11.5),
                        theme.sub,
                    );
                    *y += 21.0;
                    for (key, desc) in rows {
                        painter.text(
                            egui::pos2(x + 6.0, *y),
                            Align2::LEFT_TOP,
                            *key,
                            ui::mono_font(11.0),
                            theme.sub,
                        );
                        painter.text(
                            egui::pos2(x + 6.0 + key_w, *y),
                            Align2::LEFT_TOP,
                            *desc,
                            ui::mono_font(11.0),
                            theme.dim,
                        );
                        *y += 19.0;
                    }
                    *y += 8.0;
                };
                let mut y = list_top + 14.0;
                painter.text(
                    egui::pos2(x0, y),
                    Align2::LEFT_TOP,
                    "搜索技巧",
                    ui::text_font(12.0),
                    theme.sub,
                );
                y += 24.0;
                for group in syntax {
                    draw_group(painter, x0, &mut y, group);
                }
                if two_col {
                    let mut ry = list_top + 14.0 + 24.0;
                    for group in misc {
                        draw_group(painter, x0 + col_w + 12.0, &mut ry, group);
                    }
                } else {
                    for group in misc {
                        draw_group(painter, x0, &mut y, group);
                    }
                }
                return;
            }
            // 查询在途（防抖/排队/执行中）时，不要立刻显示「没有匹配项」：
            // 打字瞬间结果还没回来，先显示「搜索中…」，避免一帧空列表闪成
            // 「没有匹配项」再被结果替换。
            if self.waiting {
                painter.text(
                    egui::pos2(rect.center().x, list_top + 44.0),
                    Align2::CENTER_CENTER,
                    "搜索中…",
                    ui::text_font(15.0),
                    theme.dim,
                );
                self.needle_target = 0.0;
                self.scrollable = false;
                return;
            }
            painter.text(
                egui::pos2(rect.center().x, list_top + 44.0),
                Align2::CENTER_CENTER,
                "没有匹配项",
                ui::text_font(15.0),
                theme.text,
            );
            let empty_hint = if self.in_browse() {
                if query.scope != Scope::All {
                    "当前类别没有匹配项"
                } else if self.input.trim().is_empty() {
                    "目录为空、不存在或无权限"
                } else {
                    "没有符合过滤词的文件"
                }
            } else if self.path_ui() {
                "目录为空、不存在或无权限"
            } else if self.dialog.is_some() {
                match query.scope {
                    Scope::Folder => "输入文件夹名，Enter 跳转",
                    _ => "输入文件名，Enter 填入",
                }
            } else if self.typing_folder.is_some() && !self.typing_global {
                "当前目录内无匹配 · Ctrl+Shift+G 全局搜索"
            } else {
                "试试 ext:rs、path: 或 a 微信"
            };
            painter.text(
                egui::pos2(rect.center().x, list_top + 68.0),
                Align2::CENTER_CENTER,
                empty_hint,
                ui::mono_font(11.5),
                theme.dim,
            );
            self.needle_target = 0.0;
            self.scrollable = false;
            return;
        }

        let list_area = Rect::from_min_max(
            egui::pos2(rect.left(), list_top),
            egui::pos2(rect.right(), list_top + view_h),
        );
        let total_h = entries.len() as f32 * theme.row_h;
        let max_scroll = (total_h - view_h).max(0.0);
        // 鼠标滚轮：指针在列表区域内才滚，滚完立即重绘。
        if ui
            .ctx()
            .input(|i| i.pointer.hover_pos().is_some_and(|pos| list_area.contains(pos)))
        {
            let delta = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if delta != 0.0 {
                self.scroll_y -= delta;
                ui.ctx().request_repaint();
            }
        }
        // 键盘/hover 改变选择后才把选中行滚回可视区。若每帧都做，
        // 滚轮刚滚下去就会被"保持选中可见"立刻拽回原位。
        if self.scroll_to_selection {
            let selected_top = self.selected as f32 * theme.row_h;
            if selected_top < self.scroll_y {
                self.scroll_y = selected_top;
            } else if selected_top + theme.row_h > self.scroll_y + view_h {
                self.scroll_y = selected_top + theme.row_h - view_h;
            }
            self.scroll_to_selection = false;
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
        self.scrollable = max_scroll > 0.0;
        // 只绘制可视区域，滚动时未显示的行不画也不接收鼠标事件。
        // 滚动条在窗口右缘（可能超出列表裁剪区），用未裁剪的 painter 画。
        let raw_painter = painter.clone();
        let painter = painter.with_clip_rect(list_area);
        let painter = &painter;

        // 微小抖动（<2px）不算“真的在动”：Windows 高 DPI 下指针可能每帧
        // 报 0.5~1px 的噪声，足以在按 ↓ 的同一帧把选中抢到第三行。
        let pointer_moved = ui.ctx().input(|i| i.pointer.delta().length_sq() > 4.0);
        if pointer_moved {
            self.mouse_moved = true;
        }
        let mut hovered = None;
        let mut clicked = None;
        let mut drag_started = None;
        let q = query.text.to_lowercase();
        // Browse 下不是“浏览目录条目”而是递归搜索结果，关键词照常高亮。
        let path_mode = self.path_mode();
        // 副标题（等宽字体）的字符预算：用实际字符宽度换算，尽量把行宽用满。
        let mono_char_w = ui::mono_font(11.5);
        let char_w = painter
            .layout_no_wrap("x".to_string(), mono_char_w, self.theme.sub)
            .size()
            .x
            .max(1.0);
        let sub_budget = (((rect.width() - 16.0 - 64.0) / char_w).floor() as usize).max(16);
        // 两行内容（标题 + 副标题）的墨迹几何（含关键词下划线）。每帧量一次，
        // 所有行共用同一组基线：按墨迹而不是 galley 盒居中——盒子上下留白
        // 不对称、下划线还画在盒底之外，按盒居中会让降部探出高亮底边。
        // 代表串带大写 + 降部，覆盖最高的上沿和最低的下沿。
        let title_ink = ui::LineInk::of(&painter.layout_job(ui::text_job(
            "Wg",
            "Wg",
            &ui::text_font(14.5),
            theme.text,
            theme.amber,
        )));
        let sub_ink = ui::LineInk::of(&painter.layout_job(ui::text_job(
            "Wg",
            "Wg",
            &ui::mono_font(11.5),
            theme.sub,
            theme.amber,
        )));
        // 下划线画在行盒底、比字形降部低 ~1px：量一个不带下划线的降部，
        // 把这段溢出补偿一半（光学居中），否则字形本体看起来整体偏上。
        let sub_plain = ui::LineInk::of(&painter.layout_job(ui::text_job(
            "Wg",
            "",
            &ui::mono_font(11.5),
            theme.sub,
            theme.amber,
        )));
        let optical_bias = ((sub_ink.down - sub_plain.down) / 2.0).max(0.0);
        for (index, entry) in entries.iter().enumerate() {
            let row_top = list_top + index as f32 * theme.row_h - self.scroll_y;
            if row_top + theme.row_h <= list_top || row_top >= list_top + view_h {
                continue;
            }
            let row_rect = Rect::from_min_size(
                egui::pos2(rect.left() + 8.0, row_top),
                egui::vec2(rect.width() - 16.0, theme.row_h),
            );
            let response = ui.interact(
                row_rect,
                Id::new(("lstr-row", index)),
                Sense::click_and_drag(),
            );
            // hover 抢选中只在鼠标「真的在动」时生效。否则滚轮滚动后指针停在
            // 某行上，每一帧都把键盘选中的行抢回去 → 上下键看起来失灵。
            if response.hovered()
                && self.mouse_moved
                && pointer_moved
                && self.shown_at.elapsed() > Duration::from_millis(60)
                && Instant::now() >= self.hover_suppress_until
            {
                hovered = Some(index);
            }
            if response.clicked() && self.shown_at.elapsed() > Duration::from_millis(120) {
                clicked = Some(index);
            }
            // 拖出到其他应用（微信/邮件/IDE）：OLE 接管鼠标后这里拿到 drag_started。
            if response.drag_started() {
                drag_started = Some(index);
            }
            if response.secondary_clicked() {
                self.selected = index;
                self.menu_open = true;
                self.menu_index = 0;
            }
            let selected = index == self.selected;
            if selected {
                painter.rect_filled(
                    row_rect.shrink2(egui::vec2(0.0, 1.0)),
                    CornerRadius::same(8),
                    theme.sel,
                );
            } else if response.hovered() {
                painter.rect_filled(
                    row_rect.shrink2(egui::vec2(0.0, 1.0)),
                    CornerRadius::same(8),
                    theme.hover,
                );
            }

            let icon_rect = Rect::from_center_size(
                egui::pos2(row_rect.left() + 24.0, row_rect.center().y),
                egui::vec2(20.0, 20.0),
            );
            let texture = if entry.kind == Kind::Command {
                None
            } else {
                let key = icon_key(entry);
                self.icons.request(key.clone(), entry);
                self.icons.get(&key)
            };
            if let Some(texture) = texture {
                painter.image(
                    texture.id(),
                    icon_rect,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            } else if entry.kind == Kind::App {
                ui::paint_app_tile(painter, icon_rect, entry);
            } else {
                ui::paint_file_icon(painter, icon_rect, entry.kind, theme.sub);
            }

            let pinyin_hit = entry.kind == Kind::App
                && self.filters.matches.is_default()
                && !q.is_empty()
                && !entry.name.to_lowercase().contains(&q)
                && entry
                    .pinyin
                    .as_deref()
                    .map(|pinyin| pinyin.contains(&q))
                    .unwrap_or(false);
            // 子序列模糊命中（vsc → Visual Studio Code）：命中的字母逐个下划线。
            let fuzzy_hit = if !pinyin_hit
                && !q.is_empty()
                && matches!(entry.kind, Kind::App | Kind::Command | Kind::Program)
                && !entry.name.to_lowercase().contains(&q)
            {
                crate::search::fuzzy::subsequence(&entry.name, &q).map(|(_, pos)| pos)
            } else {
                None
            };
            // 路径模式是「浏览目录」而非搜索，此时不做关键词高亮；
            // 普通搜索结果（文件夹/文件）都高亮。
            let title_q = if pinyin_hit || path_mode {
                ""
            } else {
                &query.text
            };
            // 路径行判定与 meta（修改时间；选中且开启时并列大小）先在标题前算好：
            // meta 占右侧固定宽度，标题/路径的可用宽度都必须扣掉它，否则长路径
            // 会画到日期文字下面（UI 冲突的根因）。
            let is_path_row = matches!(entry.kind, Kind::File | Kind::Folder | Kind::Program);
            let full_path = entry.full_path();
            // 路径行的拼音命中标注:应用行副标题有「应用 · 拼音 xx」,文件行
            // 也要让用户看出结果不是名字字面命中(如 weixin → 微信资料)。
            // 判定只看名字/查询 token,不额外做 IO;path 模式是浏览目录,不标。
            let pinyin_path_hit =
                is_path_row && !path_mode && is_pinyin_path_hit(&entry.name, &query.text);
            // 「已打开」(对话框 Quick Switch 的近期目录)与「拼音」是两种
            // 不同来源提示:同时满足时保留两者,读作「已打开 · 拼音 · …」,
            // 先交代条目已在打开列表里,再说明本次为何命中。
            // 拼音标注只显示本次查询词（与应用行「应用 · 拼音 weixin」一致）：
            // 目的只是标明「这条是拼音命中」，不列名字的完整拼音。
            let lead = match (entry.id.starts_with("qs:"), pinyin_path_hit) {
                (true, true) => format!("已打开 · 拼音 {} · ", query.text),
                (true, false) => "已打开 · ".to_string(),
                (false, true) => format!("拼音 {} · ", query.text),
                (false, false) => String::new(),
            };
            let time_text = if is_path_row {
                entry
                    .modified
                    .map(ui::format_list_time)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            // 大小/类型常显（受「显示元数据」开关控制）：用户要横向对比体积
            // 再决定打开哪个，不能只在选中行才给。文件夹显示「文件夹」。
            let size_text = if self.cfg.show_meta {
                match entry.kind {
                    Kind::Folder => "文件夹".to_string(),
                    Kind::Command => String::new(),
                    _ => entry.size.map(ui::format_size).unwrap_or_default(),
                }
            } else {
                String::new()
            };
            // 右侧两列，各自右对齐锚点（跨行对齐的前提）：
            //   时间列锚点 = 右缘 - 10；大小列锚点 = 时间列左侧 8px。
            // 预留宽度按列固定值计算（时间列 88 / 大小列 64，字体缩放已计入）。
            const TIME_COL_W: f32 = 88.0;
            const SIZE_COL_W: f32 = 64.0;
            const META_GAP: f32 = 8.0;
            const META_MARGIN: f32 = 10.0;
            let has_size = !size_text.is_empty();
            let has_time = !time_text.is_empty();
            let meta_w = match (has_size, has_time) {
                (false, false) => 0.0,
                (true, false) => META_MARGIN + SIZE_COL_W,
                (false, true) => META_MARGIN + TIME_COL_W,
                (true, true) => META_MARGIN + TIME_COL_W + META_GAP + SIZE_COL_W,
            };
            let mut title_job = match &fuzzy_hit {
                Some(positions) => ui::text_job_positions(
                    &entry.name,
                    positions,
                    &ui::text_font(14.5),
                    theme.text,
                    theme.amber,
                ),
                None => ui::text_job_case(
                    &entry.name,
                    title_q,
                    &ui::text_font(14.5),
                    theme.text,
                    theme.amber,
                    self.filters.matches.case_sensitive,
                ),
            };
            title_job.wrap.max_width = (row_rect.width() - 54.0 - meta_w - 6.0).max(24.0);
            title_job.wrap.max_rows = 1;
            title_job.wrap.overflow_character = Some('…');
            // egui 默认优先在连字符/标点处断行：长文件名 "lstr-9cc85..." 只剩
            // 一个连字符在第 5 个字符，会被截成 "lstr-…"。break_anywhere 让
            // 它在真实宽度边缘截断，窗口拉宽就能多看几个字。
            title_job.wrap.break_anywhere = true;
            let title_galley = painter.layout_job(title_job);
            let title_width = title_galley.size().x;
            // 两行内容按墨迹在高亮底内居中（见 ui::two_line_offsets）。高亮底是
            // row_rect.shrink2(0, 1.0)，所以顶边 +1、可用高度 row_h-2。
            let (mut y_title, y_sub) = ui::two_line_offsets(
                row_rect.top() + 1.0,
                theme.row_h - 2.0,
                &title_ink,
                &sub_ink,
                optical_bias,
            );
            // 不同字体 ascent 不同（Segoe 19 / 微软雅黑 18），按 galley 顶放置会让
            // 中文标题整体偏高 1px；这里按该行真实基线对齐，回到同一条网格线。
            let title_base = title_galley
                .rows
                .first()
                .and_then(|r| r.row.glyphs.first())
                .map(|g| g.pos.y)
                .unwrap_or(title_ink.base);
            y_title += title_ink.base - title_base;
            painter.galley(
                egui::pos2(row_rect.left() + 54.0, y_title),
                title_galley,
                theme.text,
            );

            // 长路径不再中段省略：从第一行「文件名之后」开始接排，
            // 放不下的部分折到第二行行首继续，两行合起来是完整路径，行高不变。
            // （is_path_row/full_path/lead/meta/meta_w 已在标题前算好）
            // 是否折两行按「实际渲染宽度」判断而不是字符数：中文字符宽约为
            // ASCII 的两倍，按字符数判断会让视觉上已超宽的路径仍被单行省略。
            let mono = ui::mono_font(11.5);
            let gap = 10.0;
            let head_max = (row_rect.width() - 54.0 - meta_w - title_width - gap).max(24.0);
            let mut path_text = String::new();
            let mut two_line_path = false;
            if is_path_row {
                path_text = format!("{lead}{full_path}");
                let path_w = painter
                    .layout_no_wrap(path_text.clone(), mono.clone(), theme.sub)
                    .size()
                    .x;
                two_line_path = path_w > head_max;
            }

            if two_line_path {
                let needle = if path_mode { "" } else { &query.text };
                let cut = split_path_head(
                    |candidate| {
                        painter
                            .layout_no_wrap(candidate.to_string(), mono.clone(), theme.sub)
                            .size()
                            .x
                    },
                    &path_text,
                    head_max,
                );
                let (head, tail) = split_at_chars(&path_text, cut);

                if !head.is_empty() {
                    let mut head_job = ui::text_job_case(
                        &head,
                        needle,
                        &mono,
                        theme.sub,
                        theme.amber,
                        self.filters.matches.case_sensitive,
                    );
                    // 切分按纯文本测量，高亮分段可能带来极小宽度差，留几像素防裁字。
                    head_job.wrap.max_width = head_max + 6.0;
                    head_job.wrap.max_rows = 1;
                    head_job.wrap.break_anywhere = true;
                    let head_galley = painter.layout_job(head_job);
                    // 首行接排的路径片段按基线对齐标题（此前按盒子对齐，
                    // 等宽与标题升部不同会差 1~2px）。
                    let y = y_title + (title_ink.base - sub_ink.base).max(0.0);
                    painter.galley(
                        egui::pos2(row_rect.left() + 54.0 + title_width + gap, y),
                        head_galley,
                        theme.sub,
                    );
                }
                if !tail.is_empty() {
                    let mut tail_job = ui::text_job_case(
                        &tail,
                        needle,
                        &mono,
                        theme.sub,
                        theme.amber,
                        self.filters.matches.case_sensitive,
                    );
                    tail_job.wrap.max_width = (row_rect.width() - 54.0 - meta_w - 10.0).max(24.0);
                    tail_job.wrap.max_rows = 1;
                    tail_job.wrap.break_anywhere = true;
                    // 极端超长路径的兜底：正常窗口宽度两行足够，不会走到省略。
                    tail_job.wrap.overflow_character = Some('…');
                    painter.galley(
                        egui::pos2(row_rect.left() + 54.0, y_sub),
                        painter.layout_job(tail_job),
                        theme.sub,
                    );
                }
            } else {
                let subtitle = match entry.kind {
                    Kind::App => {
                        if pinyin_hit {
                            format!("应用 · 拼音 {}", query.text)
                        } else if fuzzy_hit.is_some() {
                            format!("应用 · 模糊 {}", query.text)
                        } else if entry.id.starts_with("sys:") {
                            format!("系统功能 · {}", ui::display_path(entry))
                        } else {
                            "应用".to_string()
                        }
                    }
                    Kind::Command => {
                        if entry.id.starts_with("kwurl:") {
                            let host = entry
                                .path
                                .as_ref()
                                .map(|p| ui::url_host(&p.to_string_lossy()).to_string())
                                .unwrap_or_default();
                            format!("网络搜索 · {host}")
                        } else if entry.id.starts_with("kwexec:") {
                            "关键字 · Enter 运行".to_string()
                        } else {
                            "命令 · Enter 执行".to_string()
                        }
                    }
                    _ => {
                        let path_text = ui::middle_ellipsis(&full_path, sub_budget);
                        if lead.is_empty() {
                            path_text
                        } else {
                            format!("{lead}{path_text}")
                        }
                    }
                };
                let mut sub_job = ui::text_job_case(
                    &subtitle,
                    if path_mode { "" } else { &query.text },
                    &ui::mono_font(11.5),
                    theme.sub,
                    theme.amber,
                    self.filters.matches.case_sensitive,
                );
                sub_job.wrap.max_width = (row_rect.width() - 54.0 - meta_w - 10.0).max(24.0);
                sub_job.wrap.max_rows = 1;
                sub_job.wrap.overflow_character = Some('…');
                sub_job.wrap.break_anywhere = true;
                painter.galley(
                    egui::pos2(row_rect.left() + 54.0, y_sub),
                    painter.layout_job(sub_job),
                    theme.sub,
                );
            }

            // 两列各自右对齐：时间贴右缘，大小在时间列左侧一列——
            // 跨行对齐不随文字宽度漂移（相对时间「N 天前」已被定宽日期取代）。
            if !time_text.is_empty() {
                painter.text(
                    egui::pos2(row_rect.right() - META_MARGIN, row_rect.center().y),
                    Align2::RIGHT_CENTER,
                    time_text.as_str(),
                    ui::mono_font(11.0),
                    theme.dim,
                );
            }
            if !size_text.is_empty() {
                let x = if time_text.is_empty() {
                    row_rect.right() - META_MARGIN
                } else {
                    row_rect.right() - META_MARGIN - TIME_COL_W - META_GAP
                };
                painter.text(
                    egui::pos2(x, row_rect.center().y),
                    Align2::RIGHT_CENTER,
                    size_text.as_str(),
                    ui::mono_font(11.0),
                    theme.dim,
                );
            }

            if selected {
                self.needle_target = row_rect.top() + (theme.row_h - NEEDLE_H) / 2.0;
            }
        }

        // 选中行理论上始终可见（上面已自动滚动），这里再兜底算一次，
        // 避免裁剪边界的那一行没被绘制时指针停在旧位置。
        self.needle_target =
            list_top + self.selected as f32 * theme.row_h - self.scroll_y
                + (theme.row_h - NEEDLE_H) / 2.0;
        if !self.needle_ready {
            self.needle_y = self.needle_target;
            self.needle_ready = true;
        }
        let needle_rect = Rect::from_min_size(
            egui::pos2(rect.left() + 8.0, self.needle_y),
            egui::vec2(2.0, NEEDLE_H),
        );
        painter.rect_filled(
            needle_rect.expand(4.0),
            CornerRadius::same(4),
            theme.amber_soft,
        );
        painter.rect_filled(needle_rect, CornerRadius::same(1), theme.amber);

        // 滚动条：贴着窗口右缘的 3px 细条（可拖拽）。
        if max_scroll > 0.0 {
            let track = Rect::from_min_max(
                egui::pos2(scroll_right - 5.0, list_top + 2.0),
                egui::pos2(scroll_right - 2.0, list_top + view_h - 2.0),
            );
            // 命中区比视觉条宽很多；右端停在 -3，避开窗口右缘 3px 的缩放手势。
            let hit = Rect::from_min_max(
                egui::pos2(scroll_right - 12.0, track.top() - 2.0),
                egui::pos2(scroll_right - 3.0, track.bottom() + 2.0),
            );
            let response = ui.interact(hit, Id::new("lstr-scrollbar"), Sense::click_and_drag());
            if (response.dragged() || response.clicked()) && track.height() > 0.0 {
                if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    let ratio = ((pos.y - track.top()) / track.height()).clamp(0.0, 1.0);
                    self.scroll_y = ratio * max_scroll;
                    ui.ctx().request_repaint();
                }
            }
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            // 轨道几乎不可见（只是位置提示），滑块轻量着色。
            raw_painter.rect_filled(track, CornerRadius::same(2), theme.hair);
            let thumb_h = (view_h / total_h * track.height())
                .max(18.0)
                .min(track.height());
            let travel = (track.height() - thumb_h).max(0.0);
            let ratio = if max_scroll > 0.0 {
                self.scroll_y / max_scroll
            } else {
                0.0
            };
            let thumb = Rect::from_min_size(
                egui::pos2(track.left(), track.top() + ratio * travel),
                egui::vec2(track.width(), thumb_h),
            );
            raw_painter.rect_filled(
                thumb,
                CornerRadius::same(2),
                if response.dragged() || response.hovered() {
                    theme.sub
                } else {
                    theme.sub.gamma_multiply(0.7)
                },
            );
        }

        if let Some(index) = hovered {
            // 菜单打开时冻结选中行：菜单锚定在选中行下方，鼠标移向菜单途中
            // 经过别的行会改选中 → 菜单每帧跟着跳走，永远追不上。
            if !self.menu_open {
                self.selected = index;
            }
        }
        if let Some(index) = clicked {
            self.activate_index(ui.ctx(), index);
        }
        if let Some(index) = drag_started {
            self.start_file_drag(ui.ctx(), index);
        }
    }

    /// 把选中结果的实体文件拖出到其他应用。模态 OLE 循环结束后才返回；
    /// 落下成功按「使用」计入历史并隐藏窗口，取消则停留在原界面。
    fn start_file_drag(&mut self, ctx: &egui::Context, index: usize) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        let Some(path) = entry.path.clone() else {
            return;
        };
        // 命令/网络搜索/UWP 没有实体文件可拖（URL 与 shell: 路径 exists 为 false）。
        if !path.exists() {
            return;
        }
        // 模态循环期间主线程阻塞，失焦隐藏不会触发；无需额外抑制。
        match crate::win::dragdrop::drag_files(vec![path]) {
            Ok(crate::win::dragdrop::DragOutcome::Dropped) => {
                self.service.record_open(&entry);
                self.hide(ctx);
            }
            Ok(crate::win::dragdrop::DragOutcome::Cancelled) => {}
            Err(err) => self.toast(ctx, err),
        }
        // 注意：OLE 吃掉了结束拖拽的那次鼠标抬起消息，egui 的 pointer
        // 按下态可能残留，下一次真实输入事件会自愈（egui 原生 StartDrag 同款情况）。
    }

    fn paint_footer(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        entries: &[Entry],
        query: &crate::model::Query,
    ) {
        let theme = self.theme;
        let footer_top = rect.bottom() - FOOTER_H;
        painter.line_segment(
            [
                egui::pos2(rect.left() + PAD, footer_top),
                egui::pos2(rect.right() - PAD, footer_top),
            ],
            Stroke::new(1.0_f32, theme.hair),
        );

        let mut default_text = if query.is_empty() {
            format!(
                "{} 项{}",
                entries.len(),
                if query.scope == Scope::All && self.cfg.show_history {
                    " · 最近使用"
                } else {
                    ""
                }
            )
        } else {
            format!(
                // total 计数在 10 万处截断(见 query::TOTAL_CAP),显示为近似值。
                // 总=按键请求发出到结果上屏(含排队/IPC/帧等待);引擎=worker 内纯执行。
                "{} 个结果 · 总 {:.0}ms · 引擎 {:.0}ms{}",
                if self.total >= 100_000 {
                    "10万+".to_string()
                } else {
                    self.total.to_string()
                },
                self.ui_total_ms.unwrap_or(self.elapsed_ms),
                self.elapsed_ms,
                if self.scrollable { " · 滚轮查看更多" } else { "" }
            )
        };
        if let Some(root) = &self.browse_root {
            let name = root
                .file_name()
                .map(|part| part.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned());
            default_text = format!("{} 项 · 目录: {name}", entries.len());
        }
        if let Some(folder) = &self.typing_folder {
            if self.typing_global {
                default_text.push_str(" · 全局");
            } else {
                let name = folder
                    .file_name()
                    .map(|part| part.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.to_string_lossy().into_owned());
                default_text.push_str(&format!(" · 目录: {name}"));
            }
        }
        let now = Instant::now();
        let active_toast = match &self.toast {
            Some((message, until)) if now < *until => Some(message.clone()),
            _ => None,
        };
        let left_text = active_toast.clone().unwrap_or(default_text);
        let left_color = if active_toast.is_some() {
            theme.amber
        } else {
            theme.dim
        };

        let path_mode = self.path_ui();
        let selected_is_folder = self
            .entries
            .get(self.selected)
            .map(|entry| entry.kind == Kind::Folder)
            .unwrap_or(false);
        let hints: [(&str, &str); 4] = if self.in_browse() {
            [
                ("↑↓", "选择"),
                ("↵", "进入"),
                ("Alt↑↓", "上/下级"),
                ("Esc", "返回"),
            ]
        } else if path_mode {
            [
                ("↑↓", "选择"),
                ("→", "进入"),
                ("Alt↑", "上一级"),
                ("Esc", "清空"),
            ]
        } else if query.is_empty() {
            [
                ("↑↓", "选择"),
                ("↵", "打开"),
                ("Tab", "类别"),
                ("Esc", "关闭"),
            ]
        } else {
            [
                ("↑↓", "选择"),
                ("↵", "打开"),
                ("→", if selected_is_folder { "进入" } else { "动作" }),
                ("Esc", "关闭"),
            ]
        };
        let mut right = rect.right() - PAD;
        for (key, label) in hints.iter().rev() {
            let key_galley = painter.layout_no_wrap(
                key.to_string(),
                ui::mono_font(10.0),
                theme.dim,
            );
            let label_galley = painter.layout_no_wrap(
                label.to_string(),
                ui::text_font(11.0),
                theme.dim,
            );
            let key_w = key_galley.size().x + 8.0;
            let total = key_w + 4.0 + label_galley.size().x;
            let x = right - total;
            // 键帽框围着键文字的实际 galley 画（+2px 内边距）：↑↓ 箭头来自
            // CJK 回退字体，行高 >16px，固定高度的框会被字形撑出错位。
            // 框与键文字共享同一几何(bounds+padding)，从构造上对齐；
            // 标签文字用 LEFT_CENTER 锚到同一中心线(footer_top+18)。
            let key_h = key_galley.size().y;
            let key_rect = Rect::from_min_size(
                egui::pos2(x, footer_top + 18.0 - key_h * 0.5 - 2.0),
                egui::vec2(key_w, key_h + 4.0),
            );
            painter.rect_stroke(
                key_rect,
                CornerRadius::same(4),
                Stroke::new(1.0_f32, theme.hair),
                StrokeKind::Inside,
            );
            painter.galley(
                egui::pos2(x + 4.0, footer_top + 18.0 - key_h * 0.5),
                key_galley,
                theme.dim,
            );
            painter.text(
                egui::pos2(x + key_w + 4.0, footer_top + 18.0),
                Align2::LEFT_CENTER,
                label.to_string(),
                ui::text_font(11.0),
                theme.dim,
            );
            right = x - 12.0;
        }

        // 左侧文案按右侧提示剩余宽度截断（toast 可能很长，以前会盖住快捷键提示）。
        let max_w = (right + 12.0 - (rect.left() + PAD)).max(80.0);
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &left_text,
            0.0,
            egui::TextFormat {
                font_id: ui::mono_font(11.0),
                color: left_color,
                ..Default::default()
            },
        );
        job.wrap.max_width = max_w;
        job.wrap.max_rows = 1;
        job.wrap.overflow_character = Some('…');
        job.wrap.break_anywhere = true;
        let galley = painter.layout_job(job);
        painter.galley(
            egui::pos2(rect.left() + PAD, footer_top + 14.0 - galley.size().y / 2.0),
            galley,
            left_color,
        );
    }

    fn caret_at_end(&self, ctx: &egui::Context) -> bool {
        let chars = self.input.chars().count();
        let id = egui::Id::new("lstr-search-input");
        if let Some(state) = egui::widgets::text_edit::TextEditState::load(ctx, id) {
            if let Some(range) = state.cursor.char_range() {
                return range.primary.index >= chars && range.secondary.index >= chars;
            }
        }
        true
    }

    /// 是否处于 PathTyping（手输完整路径）。正则模式下 `D:\...` 也按正则处理。
    fn path_mode(&self) -> bool {
        !self.filters.matches.regex && crate::search::browse::parse(&self.input).is_some()
    }

    /// 深路径浏览：输入框只放过滤词，当前目录由 browse_root 承担。
    fn in_browse(&self) -> bool {
        self.browse_root.is_some()
    }

    /// 路径 UI（PathTyping 或 Browse）：筛选条/高亮/footer 用。
    fn path_ui(&self) -> bool {
        self.in_browse() || self.path_mode()
    }

    /// 文件夹结果：进入深路径浏览模式。输入框清空为过滤词，当前目录写进
    /// browse_root（不再把整条路径塞进输入框）。返回 false 表示不是文件夹。
    fn descend_index(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        if entry.kind != Kind::Folder {
            return false;
        }
        self.browse_root = Some(std::path::PathBuf::from(entry.full_path()));
        self.browse_forward.clear();
        // 位置由面包屑决定；其它筛选（时间/类型/匹配）保留并继续生效。
        self.filters.location = None;
        self.input.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.scroll_to_selection = false;
        self.menu_open = false;
        self.filter_menu = None;
        Self::reset_input_cursor(ctx);
        self.move_caret_to_end(ctx);
        self.apply_size(ctx);
        self.trigger(ctx);
        true
    }

    /// 路径模式回退一级：把输入截到最后一个分隔符（含）。
    fn path_up(&mut self, ctx: &egui::Context) {
        let trimmed = self.input.trim_end_matches(std::path::is_separator);
        let Some(pos) = trimmed.rfind(std::path::is_separator) else {
            return;
        };
        let next: String = trimmed[..=pos].to_string();
        if next == self.input {
            return;
        }
        self.input = next;
        self.selected = 0;
        self.menu_open = false;
        self.move_caret_to_end(ctx);
        self.trigger(ctx);
    }

    /// 浏览模式回退一级；盘符根/UNC 根再回退就退出浏览。
    fn browse_up(&mut self, ctx: &egui::Context) {
        let Some(root) = self.browse_root.clone() else {
            return;
        };
        // 记住当前目录：Alt+↓ 可以再回到下一级。
        self.browse_forward.push(root.clone());
        match root.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                self.browse_root = Some(parent.to_path_buf());
                self.input.clear();
                self.selected = 0;
                self.scroll_y = 0.0;
                self.menu_open = false;
                self.filter_menu = None;
                Self::reset_input_cursor(ctx);
                self.apply_size(ctx);
                self.trigger(ctx);
            }
            _ => self.exit_browse(ctx),
        }
    }

    /// Alt+↓：回到 Alt+↑ 之前离开的子目录（后进先出）。
    fn browse_down(&mut self, ctx: &egui::Context) {
        let Some(next) = self.browse_forward.pop() else {
            return;
        };
        self.browse_root = Some(next);
        self.input.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.scroll_to_selection = false;
        self.menu_open = false;
        self.filter_menu = None;
        Self::reset_input_cursor(ctx);
        self.apply_size(ctx);
        self.trigger(ctx);
    }

    /// 退出浏览模式，回到全局搜索（输入与结果清空）。
    fn exit_browse(&mut self, ctx: &egui::Context) {
        if self.browse_root.take().is_none() {
            return;
        }
        self.browse_forward.clear();
        self.input.clear();
        self.entries.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.menu_open = false;
        self.filter_menu = None;
        Self::reset_input_cursor(ctx);
        self.apply_size(ctx);
        self.trigger(ctx);
    }

    /// Ctrl+L：把当前浏览目录放回输入框，切回可编辑完整路径的 PathTyping。
    fn edit_browse_path(&mut self, ctx: &egui::Context) {
        let Some(root) = self.browse_root.take() else {
            return;
        };
        self.browse_forward.clear();
        let mut path = root.to_string_lossy().into_owned();
        if !path.ends_with(['\\', '/']) {
            path.push('\\');
        }
        self.input = path;
        self.selected = 0;
        self.menu_open = false;
        self.filter_menu = None;
        Self::reset_input_cursor(ctx);
        self.move_caret_to_end(ctx);
        self.apply_size(ctx);
        self.trigger(ctx);
    }

    fn move_caret_to_end(&self, ctx: &egui::Context) {
        let id = egui::Id::new("lstr-search-input");
        if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ctx, id) {
            let end = self.input.chars().count();
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(end),
            )));
            state.store(ctx, id);
        }
    }

    fn open_menu(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.menu_open = true;
        self.menu_index = 0;
    }

    fn menu_step(&mut self, delta: i32) {
        let Some(entry) = self.entries.get(self.selected) else {
            self.menu_open = false;
            return;
        };
        let actions = self.cfg.actions.clone();
        let items = menu_items(entry, &actions);
        if items.is_empty() {
            self.menu_open = false;
            return;
        }
        let len = items.len() as i32;
        let mut next = self.menu_index as i32;
        for _ in 0..len {
            next = ((next + delta) % len + len) % len;
            if items[next as usize].enabled {
                break;
            }
        }
        self.menu_index = next as usize;
    }

    fn menu_activate(&mut self, ctx: &egui::Context) {
        let Some(entry) = self.entries.get(self.selected) else {
            self.menu_open = false;
            return;
        };
        let actions = self.cfg.actions.clone();
        let items = menu_items(entry, &actions);
        if let Some(item) = items.get(self.menu_index) {
            if item.enabled {
                let action = item.action;
                self.run_menu_action(ctx, action);
                return;
            }
        }
        self.menu_open = false;
    }

    fn run_menu_action(&mut self, ctx: &egui::Context, action: MenuAction) {
        self.menu_open = false;
        match action {
            MenuAction::AddFavorite => {
                let Some(entry) = self.entries.get(self.selected).cloned() else {
                    return;
                };
                if entry.kind == Kind::Command {
                    self.toast(ctx, "命令不能收藏".to_string());
                    return;
                }
                match crate::assets::add_favorite(&mut self.cfg, &entry.name, &entry.full_path()) {
                    Ok(true) => {
                        self.service.set_favorites(self.cfg.favorites.clone());
                        self.toast(ctx, "已加入收藏".to_string());
                    }
                    Ok(false) => self.toast(ctx, "已在收藏中".to_string()),
                    Err(err) => self.toast(ctx, err),
                }
                self.trigger(ctx);
            }
            MenuAction::CustomAction(index) => {
                let Some(entry) = self.entries.get(self.selected).cloned() else {
                    return;
                };
                let Some(def) = self.cfg.actions.get(index).cloned() else {
                    return;
                };
                let current_folder = crate::win::explorer::active_explorer_folder()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match crate::assets::run_action(&def, &entry, &self.input, &current_folder) {
                    Ok(()) => self.hide(ctx),
                    Err(err) => self.toast(ctx, err),
                }
            }
            MenuAction::Open => self.activate_selected(ctx),
            MenuAction::Reveal => self.reveal_selected(ctx),
            MenuAction::CopyPath => {
                if let Some(entry) = self.entries.get(self.selected) {
                    ctx.copy_text(entry.full_path());
                    self.toast(ctx, "已复制路径".to_string());
                }
            }
            MenuAction::CopyName => {
                if let Some(entry) = self.entries.get(self.selected) {
                    ctx.copy_text(entry.name.clone());
                    self.toast(ctx, "已复制文件名".to_string());
                }
            }
            MenuAction::Admin => self.admin_selected(ctx),
            MenuAction::OpenWith => {
                let Some(entry) = self.entries.get(self.selected).cloned() else {
                    return;
                };
                match shell::open_with(&entry) {
                    // 打开方式只是弹系统对话框，不消费搜索结果：保留输入，
                    // 用户关掉对话框后再唤出仍能继续上一次搜索。
                    Ok(()) => self.hide_keep(ctx),
                    Err(err) => self.toast(ctx, err),
                }
            }
            MenuAction::OpenTerminal => {
                let Some(entry) = self.entries.get(self.selected).cloned() else {
                    return;
                };
                match shell::open_terminal(&entry) {
                    // 终端是旁观操作（不改动搜索结果），同样保留输入与结果。
                    Ok(()) => self.hide_keep(ctx),
                    Err(err) => self.toast(ctx, err),
                }
            }
            MenuAction::RemoveHistory => {
                let Some(entry) = self.entries.get(self.selected).cloned() else {
                    return;
                };
                if let Ok(mut history) = self.history.lock() {
                    history.remove(&entry.id);
                }
                self.trigger(ctx);
                self.toast(ctx, "已从最近使用中移除".to_string());
            }
        }
    }

    fn paint_menu(&mut self, ctx: &egui::Context, rect: Rect) {
        if !self.menu_open {
            return;
        }
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            self.menu_open = false;
            return;
        };
        let items = menu_items(&entry, &self.cfg.actions);
        let theme = self.theme;
        let item_h = 30.0;
        let width = 236.0;
        let height = items.len() as f32 * item_h + 8.0;
        let list_top = rect.top()
            + SEARCH_H
            + TABS_H
            + FILTER_H
            + if self.browse_root.is_some() { CRUMB_H } else { 0.0 }
            + LIST_GAP;
        let row_top = list_top + self.selected as f32 * theme.row_h;
        let mut top = row_top + theme.row_h + 2.0;
        if top + height > rect.bottom() - FOOTER_H - 4.0 {
            top = row_top - height - 2.0;
        }
        top = top.max(rect.top() + 8.0);
        let left = (rect.left() + 44.0).min(rect.right() - width - 8.0);
        let selected_index = self.menu_index;
        // hover 抢选中只在鼠标真的移动时生效：菜单常出现在光标附近，
        // 否则每一帧都把键盘选中的项拉回鼠标下那一行，方向键看起来失灵。
        let pointer_moved = ctx.input(|i| i.pointer.delta() != Vec2::ZERO);
        let mut clicked = None;
        let mut hovered = None;

        egui::Area::new(egui::Id::new("lstr-action-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(left, top))
            .show(ctx, |ui| {
                let frame = egui::Frame {
                    fill: theme.menu,
                    stroke: Stroke::new(1.0_f32, theme.hair2),
                    corner_radius: CornerRadius::same(10),
                    inner_margin: egui::Margin::same(4),
                    shadow: egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 30,
                        spread: 0,
                        color: Color32::from_black_alpha(110),
                    },
                    ..Default::default()
                };
                frame.show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    for (index, item) in items.iter().enumerate() {
                        let (row_rect, response) = ui.allocate_exact_size(
                            egui::vec2(width - 8.0, item_h),
                            Sense::click(),
                        );
                        if index == selected_index || (response.hovered() && pointer_moved) {
                            ui.painter().rect_filled(
                                row_rect.shrink(2.0),
                                CornerRadius::same(6),
                                theme.surface_hi,
                            );
                        }
                        if response.hovered() && pointer_moved {
                            hovered = Some(index);
                        }
                        let color = if item.enabled { theme.text } else { theme.dim };
                        ui.painter().text(
                            egui::pos2(row_rect.left() + 8.0, row_rect.center().y),
                            Align2::LEFT_CENTER,
                            item.label.as_str(),
                            ui::text_font(13.0),
                            color,
                        );
                        ui.painter().text(
                            egui::pos2(row_rect.right() - 8.0, row_rect.center().y),
                            Align2::RIGHT_CENTER,
                            item.shortcut,
                            ui::mono_font(10.0),
                            theme.dim,
                        );
                        if response.clicked() && item.enabled {
                            clicked = Some(index);
                        }
                    }
                });
            });

        if let Some(index) = hovered {
            self.menu_index = index;
        }
        if let Some(index) = clicked {
            self.menu_index = index;
            let action = items[index].action;
            self.run_menu_action(ctx, action);
        }
    }

    fn paint_preview(
        &mut self,
        _ui: &mut egui::Ui,
        painter: &egui::Painter,
        pane: Rect,
        entry: &Entry,
        q: &str,
    ) {
        let theme = self.theme;
        let mut y = pane.top() + 6.0;

        // 和结果行共用同一份纹理，UWP 图标只解析一次。
        let texture = if entry.kind == Kind::Command {
            None
        } else {
            let key = icon_key(entry);
            self.icons.request(key.clone(), entry);
            self.icons.get(&key)
        };
        let icon_rect =
            Rect::from_min_size(egui::pos2(pane.left() + 8.0, y), egui::vec2(32.0, 32.0));
        if let Some(texture) = texture {
            painter.image(
                texture.id(),
                icon_rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else if entry.kind == Kind::App {
            ui::paint_app_tile(painter, icon_rect, entry);
        } else {
            ui::paint_file_icon(painter, icon_rect, entry.kind, theme.sub);
        }
        y += 42.0;

        let kind_label = match entry.kind {
            Kind::File => "文件",
            Kind::Folder => "文件夹",
            Kind::Program => "程序",
            Kind::App => "应用",
            Kind::Command => "命令",
        };
        let mut name_job = ui::text_job_case(
            &entry.name,
            q,
            &ui::text_font(14.5),
            theme.text,
            theme.amber,
            self.filters.matches.case_sensitive,
        );
        name_job.wrap.max_width = pane.width() - 16.0;
        name_job.wrap.max_rows = 2;
        name_job.wrap.overflow_character = Some('…');
        name_job.wrap.break_anywhere = true;
        let name_galley = painter.layout_job(name_job);
        painter.galley(
            egui::pos2(pane.left() + 8.0, y),
            name_galley.clone(),
            theme.text,
        );
        y += name_galley.size().y + 6.0;

        painter.text(
            egui::pos2(pane.left() + 8.0, y),
            Align2::LEFT_TOP,
            kind_label,
            ui::mono_font(11.0),
            theme.sub,
        );
        y += 18.0;

        let full = ui::display_path(entry);
        let mut path_job = ui::text_job_case(
            &full,
            q,
            &ui::mono_font(11.0),
            theme.sub,
            theme.amber,
            self.filters.matches.case_sensitive,
        );
        path_job.wrap.max_width = pane.width() - 16.0;
        path_job.wrap.break_anywhere = true;
        let path_galley = painter.layout_job(path_job);
        painter.galley(
            egui::pos2(pane.left() + 8.0, y),
            path_galley.clone(),
            theme.sub,
        );
        y += path_galley.size().y + 10.0;

        if let Some(size) = entry.size {
            painter.text(
                egui::pos2(pane.left() + 8.0, y),
                Align2::LEFT_TOP,
                format!("大小   {}", ui::format_size(size)),
                ui::mono_font(11.0),
                theme.sub,
            );
            y += 16.0;
        }
        if let Some(line) = preview_modified_line(entry) {
            painter.text(
                egui::pos2(pane.left() + 8.0, y),
                Align2::LEFT_TOP,
                line,
                ui::mono_font(11.0),
                theme.sub,
            );
        }
    }

    /// 自绘复选框：egui 默认 Checkbox 在浅色自定义背景上几乎不可见。
    fn settings_checkbox(
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        theme: &Theme,
        id: Id,
        rect: Rect,
        checked: &mut bool,
    ) -> bool {
        let response = ui.interact(rect, id, Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let box_rect = Rect::from_center_size(
            egui::pos2(rect.left() + 11.0, rect.center().y),
            egui::vec2(18.0, 18.0),
        );
        painter.rect_filled(
            box_rect,
            CornerRadius::same(4),
            if *checked { theme.amber } else { theme.hover },
        );
        painter.rect_stroke(
            box_rect,
            CornerRadius::same(4),
            Stroke::new(
                1.0_f32,
                if *checked {
                    theme.amber
                } else if response.hovered() {
                    theme.sub
                } else {
                    theme.hair2
                },
            ),
            StrokeKind::Inside,
        );
        if *checked {
            let tip = egui::pos2(box_rect.center().x, box_rect.center().y + 1.0);
            let mark = Stroke::new(1.8_f32, Color32::WHITE);
            painter.line_segment(
                [tip + egui::vec2(-4.0, 0.0), tip + egui::vec2(-1.0, 3.0)],
                mark,
            );
            painter.line_segment(
                [tip + egui::vec2(-1.0, 3.0), tip + egui::vec2(4.5, -4.0)],
                mark,
            );
        }
        if response.clicked() {
            *checked = !*checked;
            true
        } else {
            false
        }
    }

    fn settings_label(painter: &egui::Painter, theme: &Theme, x: f32, y: f32, text: &str) {
        painter.text(
            egui::pos2(x, y + 14.0),
            Align2::LEFT_CENTER,
            text,
            ui::text_font(12.5),
            theme.sub,
        );
    }

    fn settings_heading(
        painter: &egui::Painter,
        theme: &Theme,
        area: Rect,
        title: &str,
        hint: &str,
    ) {
        painter.text(
            egui::pos2(area.left(), area.top() + 4.0),
            Align2::LEFT_TOP,
            title,
            ui::text_font(16.0),
            theme.text,
        );
        if !hint.is_empty() {
            painter.text(
                egui::pos2(area.left(), area.top() + 30.0),
                Align2::LEFT_TOP,
                hint,
                ui::mono_font(10.5),
                theme.dim,
            );
        }
    }

    fn paint_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rect: Rect) {
        let theme = self.theme;
        let painter = ui.painter().clone();

        // 关闭按钮
        let close_rect = Rect::from_min_size(
            egui::pos2(rect.right() - PAD - 22.0, rect.top() + 10.0),
            egui::vec2(22.0, 22.0),
        );
        let close = ui.interact(close_rect, Id::new("lstr-settings-close"), Sense::click());
        if close.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        painter.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            ui::text_font(14.0),
            if close.hovered() { theme.text } else { theme.sub },
        );
        if close.clicked() {
            self.settings_open = false;
            // 关掉设置面板时退出热键录制态，避免录制提示残留到下次打开。
            self.hotkey_recording = None;
        }

        painter.text(
            egui::pos2(rect.left() + 24.0, rect.top() + 16.0),
            Align2::LEFT_TOP,
            "设置",
            ui::text_font(19.0),
            theme.text,
        );

        // 左侧导航
        const SIDEBAR_W: f32 = 150.0;
        let tabs = [
            (SettingsTab::Appearance, "外观"),
            (SettingsTab::Search, "搜索"),
            (SettingsTab::Behavior, "行为"),
            (SettingsTab::Ranking, "排序规则"),
            (SettingsTab::Noise, "噪声规则"),
            (SettingsTab::Maintenance, "维护"),
            (SettingsTab::Assets, "资产"),
        ];
        let mut y = rect.top() + 60.0;
        for (tab, label) in tabs {
            let item = Rect::from_min_size(
                egui::pos2(rect.left() + 16.0, y),
                egui::vec2(SIDEBAR_W - 32.0, 30.0),
            );
            let response = ui.interact(
                item,
                Id::new(("lstr-settings-nav", tab as u8)),
                Sense::click(),
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            let active = self.settings_tab == tab;
            if active {
                painter.rect_filled(item, CornerRadius::same(7), theme.amber_soft);
                painter.rect_filled(
                    Rect::from_min_size(
                        egui::pos2(item.left(), item.center().y - 7.0),
                        egui::vec2(2.0, 14.0),
                    ),
                    CornerRadius::same(1),
                    theme.amber,
                );
            } else if response.hovered() {
                painter.rect_filled(item, CornerRadius::same(7), theme.hover);
            }
            painter.text(
                egui::pos2(item.left() + 12.0, item.center().y),
                Align2::LEFT_CENTER,
                label,
                ui::text_font(12.5),
                if active { theme.text } else { theme.sub },
            );
            if response.clicked() {
                self.settings_tab = tab;
            }
            y += 34.0;
        }

        painter.line_segment(
            [
                egui::pos2(rect.left() + SIDEBAR_W, rect.top() + 56.0),
                egui::pos2(rect.left() + SIDEBAR_W, rect.bottom() - 16.0),
            ],
            Stroke::new(1.0_f32, theme.hair),
        );

        let area = Rect::from_min_max(
            egui::pos2(rect.left() + SIDEBAR_W + 24.0, rect.top() + 52.0),
            egui::pos2(rect.right() - 24.0, rect.bottom() - 16.0),
        );
        match self.settings_tab {
            SettingsTab::Appearance => self.paint_settings_appearance(ui, &painter, area),
            SettingsTab::Search => self.paint_settings_search(ui, ctx, &painter, area),
            SettingsTab::Behavior => self.paint_settings_behavior(ui, ctx, &painter, area),
            SettingsTab::Ranking => self.paint_settings_ranking(ui, ctx, &painter, area),
            SettingsTab::Noise => self.paint_settings_noise(ui, ctx, &painter, area),
            SettingsTab::Maintenance => self.paint_settings_maintenance(ui, ctx, &painter, area),
            SettingsTab::Assets => self.paint_settings_assets(ui, ctx, &painter, area),
        }
    }

    fn paint_settings_appearance(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(painter, &theme, area, "外观", "主题、强调色与窗口质感，改动即时生效");
        let mut y = area.top() + 62.0;
        const ROW: f32 = 36.0;
        let ctrl_x = area.left() + 130.0;
        let ctrl_w = (area.width() - 140.0).min(260.0);

        Self::settings_label(painter, &theme, area.left(), y, "主题");
        for (index, (value, label)) in [("obsidian", "Obsidian"), ("frost", "Frost")]
            .iter()
            .enumerate()
        {
            let button = Rect::from_min_size(
                egui::pos2(ctrl_x + index as f32 * 108.0, y),
                egui::vec2(100.0, 26.0),
            );
            let selected = self.cfg.theme.mode == *value;
            let response = ui.interact(button, Id::new(("lstr-theme", index)), Sense::click());
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            painter.rect_filled(
                button,
                CornerRadius::same(6),
                if selected { theme.surface_hi } else { theme.hover },
            );
            painter.rect_stroke(
                button,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, if selected { theme.amber } else { theme.hair }),
                StrokeKind::Inside,
            );
            painter.text(
                button.center(),
                Align2::CENTER_CENTER,
                *label,
                ui::text_font(12.5),
                if selected { theme.text } else { theme.sub },
            );
            if response.clicked() && !selected {
                self.cfg.theme.mode = value.to_string();
                self.theme = Theme::from_config(&self.cfg);
                let _ = self.cfg.save();
                if let Some(hwnd) = self.hwnd {
                    window::apply_effects(hwnd, self.theme.light, self.cfg.theme.opacity);
                }
            }
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "强调色");
        let accents = [
            ("#E0A040", "琥珀"),
            ("#6C9BFF", "湖蓝"),
            ("#4FC1A6", "青绿"),
            ("#E07790", "玫瑰"),
            ("#A78BFA", "紫罗兰"),
        ];
        for (index, (hex, name)) in accents.iter().enumerate() {
            let swatch = Rect::from_min_size(
                egui::pos2(ctrl_x + index as f32 * 34.0, y + 2.0),
                egui::vec2(22.0, 22.0),
            );
            let selected = self.cfg.theme.accent.eq_ignore_ascii_case(hex);
            let response = ui.interact(swatch, Id::new(("lstr-accent", index)), Sense::click());
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            let rgb = crate::config::parse_hex(hex).unwrap_or([224, 160, 64]);
            painter.circle_filled(
                swatch.center(),
                8.0,
                Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
            );
            if selected {
                painter.circle_stroke(swatch.center(), 10.0, Stroke::new(2.0_f32, theme.text));
            } else if response.hovered() {
                painter.circle_stroke(swatch.center(), 10.0, Stroke::new(1.5_f32, theme.sub));
            }
            if response.hovered() {
                response.clone().on_hover_text(*name);
            }
            if response.clicked() && !selected {
                self.cfg.theme.accent = hex.to_string();
                self.theme = Theme::from_config(&self.cfg);
                let _ = self.cfg.save();
                if let Some(hwnd) = self.hwnd {
                    window::apply_effects(hwnd, self.theme.light, self.cfg.theme.opacity);
                }
            }
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "不透明度");
        let mut opacity = self.cfg.theme.opacity;
        let response = ui.put(
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0)),
            egui::Slider::new(&mut opacity, 0.72..=0.98),
        );
        if response.changed() {
            self.cfg.theme.opacity = opacity;
            let _ = self.cfg.save();
            if let Some(hwnd) = self.hwnd {
                window::apply_effects(hwnd, self.theme.light, self.cfg.theme.opacity);
            }
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "圆角");
        let mut radius = self.cfg.theme.radius;
        let response = ui.put(
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0)),
            egui::Slider::new(&mut radius, 8.0..=24.0),
        );
        if response.changed() {
            self.cfg.theme.radius = radius;
            self.theme = Theme::from_config(&self.cfg);
            let _ = self.cfg.save();
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "行高");
        let mut row_height = self.cfg.theme.row_height;
        let response = ui.put(
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0)),
            egui::Slider::new(&mut row_height, 36.0..=52.0),
        );
        if response.changed() {
            self.cfg.theme.row_height = row_height;
            self.theme = Theme::from_config(&self.cfg);
            let _ = self.cfg.save();
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "预览面板");
        let mut preview = self.cfg.preview;
        if Self::settings_checkbox(
            ui,
            painter,
            &theme,
            Id::new("lstr-preview-check"),
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(60.0, 22.0)),
            &mut preview,
        ) {
            self.cfg.preview = preview;
            self.preview_open = preview;
            let _ = self.cfg.save();
        }
    }

    fn paint_settings_behavior(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(painter, &theme, area, "行为", "唤起热键、结果数量、对话框联动与开机启动");
        let mut y = area.top() + 62.0;
        const ROW: f32 = 36.0;
        let ctrl_x = area.left() + 130.0;
        let ctrl_w = (area.width() - 140.0).min(240.0);

        // —— 唤起热键：显示当前值 + 录制（双击修饰键 / 组合键），完成后热生效 ——
        Self::settings_label(painter, &theme, area.left(), y, "唤起热键");
        let recording = self.hotkey_recording.is_some();
        let hotkey_label = if recording {
            match self.hotkey_recording.as_ref().and_then(|state| state.first_mod) {
                // 双击修饰键进行中：提示第二落。
                Some((tap, _)) => {
                    format!("已按下 {}：再按一下完成双击，或加普通键（Esc 取消）", tap.display_name())
                }
                None => "按下一个键…（Esc 取消）".to_string(),
            }
        } else {
            display_hotkey(&self.cfg.hotkey)
        };
        painter.text(
            egui::pos2(ctrl_x, y + 14.0),
            Align2::LEFT_CENTER,
            &hotkey_label,
            ui::text_font(12.0),
            if recording { theme.amber } else { theme.text },
        );
        let button = Rect::from_min_size(
            egui::pos2(ctrl_x + 186.0, y + 2.0),
            egui::vec2(60.0, 24.0),
        );
        let record = ui.interact(button, Id::new("lstr-hotkey-record"), Sense::click());
        if record.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        painter.rect_filled(
            button,
            CornerRadius::same(6),
            if recording { theme.amber_soft } else { theme.hover },
        );
        let button_stroke_w: f32 = 1.0;
        painter.rect_stroke(
            button,
            CornerRadius::same(6),
            Stroke::new(
                button_stroke_w,
                if recording {
                    theme.amber
                } else if record.hovered() {
                    theme.sub
                } else {
                    theme.hair2
                },
            ),
            StrokeKind::Inside,
        );
        painter.text(
            button.center(),
            Align2::CENTER_CENTER,
            if recording { "取消" } else { "录制" },
            ui::text_font(11.5),
            if recording { theme.text } else { theme.sub },
        );
        if record.clicked() {
            // 再点一次（或 Esc）退出录制；这里只切换状态，按键消费在 handle_keys。
            self.hotkey_recording = if recording {
                None
            } else {
                Some(HotkeyRecording::default())
            };
        }
        if !recording {
            painter.text(
                egui::pos2(button.right() + 10.0, y + 14.0),
                Align2::LEFT_CENTER,
                "双击修饰键或组合键",
                ui::mono_font(10.5),
                theme.dim,
            );
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "最大结果数");
        let mut max_results = self.cfg.max_results as f32;
        let response = ui.put(
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0)),
            egui::Slider::new(&mut max_results, 4.0..=20.0).integer(),
        );
        if response.changed() {
            self.cfg.max_results = max_results.round() as usize;
            self.service.set_limit(self.cfg.max_results);
            let _ = self.cfg.save();
            self.trigger(ctx);
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "对话框自动唤出");
        let mut dialog_assist = self.cfg.dialog_assist;
        if Self::settings_checkbox(
            ui,
            painter,
            &theme,
            Id::new("lstr-dialog-check"),
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(60.0, 22.0)),
            &mut dialog_assist,
        ) {
            self.cfg.dialog_assist = dialog_assist;
            let _ = self.cfg.save();
            if dialog_assist {
                self.toast(ctx, "打开/保存对话框出现时将自动唤出".to_string());
            }
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "开机自启");
        let mut autostart = self.autostart;
        if Self::settings_checkbox(
            ui,
            painter,
            &theme,
            Id::new("lstr-autostart-check"),
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(60.0, 22.0)),
            &mut autostart,
        ) {
            match crate::win::autostart::set_enabled(autostart) {
                Ok(()) => {
                    self.autostart = autostart;
                    let _ = self.cfg.save();
                }
                Err(err) => {
                    self.toast(ctx, err);
                    self.autostart = crate::win::autostart::is_enabled();
                }
            }
        }

        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "资源管理器键入即搜");
        let mut explorer_typing = self.cfg.explorer_typing;
        if Self::settings_checkbox(
            ui,
            painter,
            &theme,
            Id::new("lstr-typing-check"),
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(60.0, 22.0)),
            &mut explorer_typing,
        ) {
            self.set_explorer_typing(explorer_typing, ctx);
            self.toast(
                ctx,
                if explorer_typing {
                    "已启用：在资源管理器里直接打字即可搜索".to_string()
                } else {
                    "已停用键入搜索".to_string()
                },
            );
        }
        // 钩子实际状态可见：避免「开关开着但钩子没起来」变成黑盒。
        painter.text(
            egui::pos2(ctrl_x + 70.0, y + 14.0),
            Align2::LEFT_CENTER,
            if self.typing.is_some() {
                "钩子运行中"
            } else {
                "未启动（见 typing.log）"
            },
            ui::mono_font(10.5),
            if self.typing.is_some() { theme.sub } else { theme.dim },
        );
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "对话框自动跟随目录");
        let mut dialog_auto_follow = self.cfg.dialog_auto_follow;
        if Self::settings_checkbox(
            ui,
            painter,
            &theme,
            Id::new("lstr-follow-check"),
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(60.0, 22.0)),
            &mut dialog_auto_follow,
        ) {
            self.cfg.dialog_auto_follow = dialog_auto_follow;
            let _ = self.cfg.save();
        }

        painter.text(
            egui::pos2(area.left(), area.bottom() - 40.0),
            Align2::LEFT_TOP,
            "键入即搜：资源管理器内直接打字；自动跟随：对话框跳到刚浏览的目录",
            ui::mono_font(10.0),
            theme.dim,
        );
        painter.text(
            egui::pos2(area.left(), area.bottom() - 24.0),
            Align2::LEFT_TOP,
            "收藏 / 动作 / 命令见 config.toml 的 [[favorite]] [[action]] [[command]]",
            ui::mono_font(10.0),
            theme.dim,
        );
    }

    /// 设置页「资产」：收藏 / 动作 / 命令的可视化增删改。
    /// 渲染放在独立模块里，这里只负责分页区域、保存配置与同步搜索服务。
    fn paint_settings_assets(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(painter, &theme, area, "资产", "收藏、自定义动作与命令");
        let inner = Rect::from_min_max(
            egui::pos2(area.left(), area.top() + 46.0),
            egui::pos2(area.right(), area.bottom() - 6.0),
        );
        let cfg = &mut self.cfg;
        let state = &mut self.assets_ui;
        let mut changed = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            changed = crate::assets_ui::paint(ui, painter, &theme, inner, cfg, state);
        });
        if changed {
            let _ = self.cfg.save();
            self.service.set_favorites(self.cfg.favorites.clone());
            self.trigger(ctx);
        }
    }

    fn paint_settings_ranking(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(
            painter,
            &theme,
            area,
            "排序规则",
            "分数越高越优先；修改立即生效。词首 +30 / 位置越前 +10 / 文件夹 +5 为内置微调",
        );
        let mut rank = self.cfg.rank.clone();
        let mut changed = false;
        let mut y = area.top() + 58.0;
        const ROW: f32 = 30.0;
        let ctrl_x = area.left() + 128.0;
        let ctrl_w = (area.width() - 138.0).min(230.0);

        macro_rules! slider_row {
            ($field:ident, $label:expr, $range:expr) => {{
                Self::settings_label(painter, &theme, area.left(), y, $label);
                let mut value = rank.$field as f32;
                let rect = Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0));
                if ui
                    .put(rect, egui::Slider::new(&mut value, $range).integer())
                    .changed()
                {
                    rank.$field = value.round() as i32;
                    changed = true;
                }
                y += ROW;
            }};
        }

        slider_row!(exact, "名称完全一致", 0.0..=1200.0);
        slider_row!(prefix, "名称前缀", 0.0..=1200.0);
        slider_row!(substring, "名称子串", 0.0..=1200.0);
        slider_row!(tokens, "多词全含", 0.0..=1200.0);
        slider_row!(normalized, "分隔符不敏感", 0.0..=1200.0);
        slider_row!(pinyin_prefix, "拼音前缀", 0.0..=1200.0);
        slider_row!(pinyin_substring, "拼音子串", 0.0..=1200.0);
        slider_row!(fuzzy, "模糊匹配", 0.0..=1200.0);
        slider_row!(path_only, "仅路径包含", 0.0..=1200.0);

        painter.text(
            egui::pos2(area.left(), y + 2.0),
            Align2::LEFT_TOP,
            "加成 / 降权",
            ui::text_font(11.5),
            theme.text,
        );
        y += 24.0;
        slider_row!(history_cap, "历史使用上限", 0.0..=300.0);
        slider_row!(recent_boost, "最近修改加成", 0.0..=50.0);
        slider_row!(noise_strength, "噪声降权强度(%)", 0.0..=200.0);

        // 一眼看明白当前顺序（分两行，避免溢出到按钮下方）
        let ladder: Vec<(String, i32)> = rank
            .ladder()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
        painter.text(
            egui::pos2(area.left(), y + 2.0),
            Align2::LEFT_TOP,
            "当前顺序",
            ui::mono_font(10.0),
            theme.dim,
        );
        for (line_index, chunk) in ladder.chunks(5).enumerate() {
            let text = chunk
                .iter()
                .map(|(name, value)| format!("{name} {value}"))
                .collect::<Vec<_>>()
                .join("  >  ");
            painter.text(
                egui::pos2(area.left(), y + 18.0 + line_index as f32 * 15.0),
                Align2::LEFT_TOP,
                text,
                ui::mono_font(9.5),
                theme.sub,
            );
        }

        // 恢复默认（右下角，避免压住顺序文字）
        let reset = Rect::from_min_size(
            egui::pos2(area.right() - 96.0, area.bottom() - 30.0),
            egui::vec2(92.0, 24.0),
        );
        let response = ui.interact(reset, Id::new("lstr-rank-reset"), Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        painter.rect_filled(reset, CornerRadius::same(6), theme.hover);
        painter.rect_stroke(
            reset,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, theme.hair),
            StrokeKind::Inside,
        );
        painter.text(
            reset.center(),
            Align2::CENTER_CENTER,
            "恢复默认",
            ui::text_font(11.5),
            theme.text,
        );
        if response.clicked() {
            rank = crate::config::RankConfig::default();
            changed = true;
        }

        if changed {
            self.cfg.rank = rank;
            let _ = self.cfg.save();
            self.trigger(ctx);
        }
    }

    fn paint_settings_noise(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(
            painter,
            &theme,
            area,
            "噪声规则",
            "路径命中片段即降权（正斜杠、不区分大小写）；搜索词本身命中规则时不降权",
        );
        let mut rules = self.cfg.rank.noise_rules.clone();
        let mut changed = false;
        let mut remove = None;
        let list_top = area.top() + 56.0;
        let list_rect = Rect::from_min_max(
            egui::pos2(area.left(), list_top),
            egui::pos2(area.right(), (area.bottom() - 40.0).max(list_top + 60.0)),
        );

        // 滚动区必须锚定在内容区里（否则会从窗口左上角开始压住侧栏和标题）。
        ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("lstr-noise-rules")
            .max_height(list_rect.height())
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(list_rect.width());
                for (index, rule) in rules.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let edit_w = (list_rect.width() - 220.0).max(160.0);
                        let response = ui.add_sized(
                            [edit_w, 22.0],
                            egui::TextEdit::singleline(&mut rule.pattern)
                                .hint_text("路径片段，如 node_modules/"),
                        );
                        if response.changed() {
                            changed = true;
                        }
                        let mut penalty = rule.penalty as f32;
                        let slider = ui.add_sized(
                            [96.0, 22.0],
                            egui::Slider::new(&mut penalty, 0.0..=300.0).integer(),
                        );
                        if slider.changed() {
                            rule.penalty = penalty.round() as i32;
                            changed = true;
                        }
                        if ui.button("×").clicked() {
                            remove = Some(index);
                        }
                    });
                }
            });
        });

        if let Some(index) = remove {
            if index < rules.len() {
                rules.remove(index);
                changed = true;
            }
        }

        // 底部：添加规则 / 恢复默认
        let add = Rect::from_min_size(
            egui::pos2(area.left(), area.bottom() - 28.0),
            egui::vec2(104.0, 24.0),
        );
        let response = ui.interact(add, Id::new("lstr-noise-add"), Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        painter.rect_filled(add, CornerRadius::same(6), theme.hover);
        painter.rect_stroke(
            add,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, theme.hair),
            StrokeKind::Inside,
        );
        painter.text(
            add.center(),
            Align2::CENTER_CENTER,
            "＋ 添加规则",
            ui::text_font(11.5),
            theme.text,
        );
        if response.clicked() {
            rules.push(crate::config::NoiseRule {
                pattern: String::new(),
                penalty: 100,
            });
            changed = true;
        }

        let reset = Rect::from_min_size(
            egui::pos2(area.right() - 96.0, area.bottom() - 28.0),
            egui::vec2(92.0, 24.0),
        );
        let response = ui.interact(reset, Id::new("lstr-noise-reset"), Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        painter.rect_filled(reset, CornerRadius::same(6), theme.hover);
        painter.rect_stroke(
            reset,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, theme.hair),
            StrokeKind::Inside,
        );
        painter.text(
            reset.center(),
            Align2::CENTER_CENTER,
            "恢复默认",
            ui::text_font(11.5),
            theme.text,
        );
        if response.clicked() {
            rules = crate::config::default_noise_rules();
            changed = true;
        }

        if changed {
            self.cfg.rank.noise_rules = rules;
            let _ = self.cfg.save();
            self.trigger(ctx);
        }
    }

    /// 「搜索」设置页:索引后端、噪声排除、状态(方案 §7 P3)。
    fn paint_settings_search(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(painter, &theme, area, "搜索", "文件索引引擎与候选策略");
        let mut y = area.top() + 62.0;
        const ROW: f32 = 38.0;

        // —— 状态行 ——
        let status = self.service.index_status.lock().unwrap().clone();
        let status_text = match &status {
            Some(st) => {
                let base = match st.phase {
                    lstr_index::Phase::Ready => "内置索引:就绪".to_string(),
                    lstr_index::Phase::Scanning => {
                        let total: u32 = st.volumes.iter().map(|v| v.count).sum();
                        let done = st.volumes.iter().filter(|v| v.complete).count();
                        format!(
                            "内置索引:构建中({}/{}) · 已收录 {} 万项",
                            done,
                            st.volumes.len(),
                            total / 10_000
                        )
                    }
                    lstr_index::Phase::Loading => "内置索引:等待 worker 连接…".to_string(),
                    lstr_index::Phase::Fallback => "内置索引:未启用(需管理员授权)".to_string(),
                };
                // L4:运行期后端与 Everything 可用性以前只赋值、没人读。显示出来
                // 才能解释「自动」模式此刻是谁在出结果、时间筛选是否可用。
                format!(
                    "{base} · 后端:{} · Everything:{}",
                    match st.backend {
                        crate::search::builtin::BackendKind::Builtin => "内置索引",
                    },
                    if st.everything_ok { "在线" } else { "离线" }
                )
            }
            None => "内置索引:未启用".to_string(),
        };
        Self::settings_label(painter, &theme, area.left(), y, "索引状态");
        painter.text(
            egui::pos2(area.left() + 130.0, y + 13.0),
            Align2::LEFT_CENTER,
            status_text,
            ui::text_font(11.5),
            theme.text,
        );
        if let Some(st) = &status {
            let mut y2 = y + 24.0;
            for v in &st.volumes {
                let mark = if v.complete { "✓" } else { "…" };
                let pct = v.pct.map(|p| format!(" {:.0}%", p * 100.0)).unwrap_or_default();
                let mut line = format!("{} 卷 {}:{}  {} 万项", mark, v.letter, pct, v.count / 10_000);
                // USN 实时增量层状态(usn 后端 worker 经 UsnWatch 帧上报;
                // 非 Off 才显示,旧后端零噪音)。游标 age 说明增量新鲜度。
                if let Some(usn) = v.usn_state {
                    match v.cursor_age_secs {
                        Some(age) => {
                            line.push_str(&format!(" · USN:{}(游标 {age}s 前)", usn.label()))
                        }
                        None => line.push_str(&format!(" · USN:{}", usn.label())),
                    }
                }
                painter.text(
                    egui::pos2(area.left() + 130.0, y2 + 6.0),
                    Align2::LEFT_CENTER,
                    line,
                    ui::text_font(11.0),
                    if v.complete { theme.sub } else { theme.text },
                );
                y2 += 20.0;
            }
            if y2 > y {
                y = y2 + 8.0;
            }
        }
        y += ROW;

        // —— 索引卷勾选 ——
        // 只有勾选的盘进内置索引;盘符升序(C D E F),点击立即生效(worker 起停扫描)。
        // 候选盘 = 固定 NTFS 盘 ∪ 配置里已有的盘 ∪ worker 上报的盘,
        // 这样「已选但当前枚举不到」的盘也能取消勾选,否则用户没法撤销。
        Self::settings_label(painter, &theme, area.left(), y, "索引卷");
        let known = self.index_drive_letters();
        let selected: Vec<char> = known
            .iter()
            .copied()
            .filter(|letter| self.cfg.index.volume_enabled(*letter))
            .collect();
        const CHIP_W: f32 = 44.0;
        const CHIP_H: f32 = 26.0;
        const CHIP_GAP: f32 = 6.0;
        let mut chip_x = area.left() + 130.0;
        let mut row_top = y;
        let mut toggled: Option<char> = None;
        for letter in known.iter().copied() {
            // 盘多时自动换行,不横向溢出(最小窗口宽度 780 也放得下 4 个盘)。
            if chip_x + CHIP_W > area.right() {
                chip_x = area.left() + 130.0;
                row_top += CHIP_H + 4.0;
            }
            let rect =
                Rect::from_min_size(egui::pos2(chip_x, row_top), egui::vec2(CHIP_W, CHIP_H));
            let on = self.cfg.index.volume_enabled(letter);
            let response = ui.interact(rect, Id::new(("lstr-idx-volume", letter)), Sense::click());
            if response.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            painter.rect_filled(
                rect,
                CornerRadius::same(6),
                if on { theme.amber_soft } else { theme.hover },
            );
            painter.rect_stroke(
                rect,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, if on { theme.amber } else { theme.hair }),
                StrokeKind::Inside,
            );
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                format!("{letter}:"),
                ui::text_font(11.0),
                if on { theme.amber } else { theme.text },
            );
            if response.clicked() {
                toggled = Some(letter);
            }
            chip_x += CHIP_W + CHIP_GAP;
        }
        // 只剩一个卷时给提示:再点它会被拒绝(配置里空列表表示「全部」,不能真全不选)。
        let mut hint_h = 0.0;
        if selected.len() <= 1 {
            painter.text(
                egui::pos2(area.left() + 130.0, row_top + CHIP_H + 11.0),
                Align2::LEFT_CENTER,
                "至少保留一个卷",
                ui::text_font(10.0),
                theme.dim,
            );
            hint_h = 19.0;
        }
        y = row_top + ROW + hint_h;
        if let Some(letter) = toggled {
            let mut next = selected.clone();
            if next.contains(&letter) {
                next.retain(|c| *c != letter);
            } else {
                next.push(letter);
            }
            if self.cfg.index.set_selected_volumes(&next, &known) {
                let _ = self.cfg.save();
                let mask = self.cfg.index.volume_mask();
                self.service.set_index_volumes(mask);
                self.toast(
                    ctx,
                    format!("索引卷:{} · 立即生效", lstr_index::volmask::describe(mask)),
                );
            } else {
                // 设置页打开时 footer 不绘制,toast 只作为关闭设置后的兜底;
                // 页面内的可见提示是上面那行「至少保留一个卷」。
                self.toast(ctx, "至少保留一个盘".to_string());
            }
        }

        // —— 后端选择 ——
        // 两列等宽网格:旧版写死 118px 宽/124px 步进,「仅内置索引(不碰 Everything)」
        // 文字溢出按钮、第 4 个按钮又被窗口右边裁掉;这里按可见宽度均分。
        Self::settings_label(painter, &theme, area.left(), y, "索引后端");
        // usn = 实验后端:目录扫描首扫 + USN journal 实时增量(带熔断/对账护栏);
        // 文案风格与其余四项一致(短句括注)。
        let backends = [("auto", "自动(内置优先+兜底)"), ("builtin", "仅内置索引(不碰 Everything)"), ("usn", "实验:目录扫描+USN 实时增量"), ("everything", "仅 Everything"), ("off", "关闭内置索引")];
        let current = self.cfg_index_backend();
        const BACKEND_CHIP_H: f32 = 26.0;
        const BACKEND_CHIP_GAP: f32 = 8.0;
        const BACKEND_COLS: usize = 2;
        let font = ui::text_font(10.5);
        let start_x = area.left() + 130.0;
        let avail = (area.right() - start_x).max(160.0);
        let chip_w = ((avail - BACKEND_CHIP_GAP * (BACKEND_COLS - 1) as f32) / BACKEND_COLS as f32).floor();
        for (i, (key, label)) in backends.iter().enumerate() {
            let col = i % BACKEND_COLS;
            let row = i / BACKEND_COLS;
            let rect = Rect::from_min_size(
                egui::pos2(
                    start_x + col as f32 * (chip_w + BACKEND_CHIP_GAP),
                    y + row as f32 * (BACKEND_CHIP_H + BACKEND_CHIP_GAP),
                ),
                egui::vec2(chip_w, BACKEND_CHIP_H),
            );
            let accent = *key == current;
            let response = ui.interact(rect, Id::new(("lstr-idx-backend", *key)), Sense::click());
            if response.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            painter.rect_filled(rect, CornerRadius::same(6), if accent { theme.amber_soft } else { theme.hover });
            painter.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0_f32, if accent { theme.amber } else { theme.hair }), StrokeKind::Inside);
            painter.text(rect.center(), Align2::CENTER_CENTER, *label, font.clone(), if accent { theme.amber } else { theme.text });
            if response.clicked() {
                self.set_cfg_index_backend(key);
            }
        }
        let rows = (backends.len() + BACKEND_COLS - 1) / BACKEND_COLS;
        y += rows as f32 * BACKEND_CHIP_H + (rows.saturating_sub(1)) as f32 * BACKEND_CHIP_GAP + 12.0;

        // —— 噪声排除开关 ——
        Self::settings_label(painter, &theme, area.left(), y, "噪声目录");
        let noise_on = self.cfg_index_noise();
        let rect = Rect::from_min_size(egui::pos2(area.left() + 130.0, y), egui::vec2(180.0, 26.0));
        let response = ui.interact(rect, Id::new("lstr-idx-noise"), Sense::click());
        painter.rect_filled(rect, CornerRadius::same(6), if noise_on { theme.amber_soft } else { theme.hover });
        painter.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0_f32, if noise_on { theme.amber } else { theme.hair }), StrokeKind::Inside);
        painter.text(rect.center(), Align2::CENTER_CENTER, if noise_on { "已排除(node_modules 等)" } else { "不过滤(内存占用更高)" }, ui::text_font(10.5), if noise_on { theme.amber } else { theme.text });
        if response.clicked() {
            self.set_cfg_index_noise(!noise_on);
        }
        y += ROW;

        // 帮助:按实际生效时机分开写——切卷立即生效,后端/噪声要重启。
        // 这里用 layout+wrap 按 area 宽度自动折行:文案再长也不会跑出右边界
        // (painter.text 是 no-wrap,旧版单行长段落其实早就被窗口右侧裁掉了)。
        let help = "自动:内置索引未就绪时用 Everything 兜底合并;仅内置:完全不读取 Everything;仅 Everything:只用 Everything 后端。\n\
实验 usn:目录扫描首扫 + USN journal 实时增量(带熔断/自动对账护栏;journal 重置或连续出错时该卷自动退回目录扫描对账)。\n\
索引卷勾选立即生效(worker 自动起停该盘扫描);索引后端与噪声目录仍需重启 lstr 生效。\n\
时间筛选在内置索引(目录扫描)模式下可用。\n\
内置索引只覆盖本机固定 NTFS 卷;网络盘/FAT32/exFAT 等需 Everything。";
        let galley = painter.layout(
            help.to_string(),
            ui::text_font(10.5),
            theme.sub,
            area.width(),
        );
        painter.galley(egui::pos2(area.left(), y + 8.0), galley, theme.sub);
    }

    fn cfg_index_backend(&self) -> &'static str {
        match self.cfg.index.backend.as_str() {
            "builtin" => "builtin",
            "usn" => "usn",
            "everything" => "everything",
            "off" => "off",
            _ => "auto",
        }
    }

    fn set_cfg_index_backend(&mut self, key: &str) {
        self.cfg.index.backend = key.to_string();
        let _ = self.cfg.save();
    }

    fn cfg_index_noise(&self) -> bool {
        self.cfg.index.exclude_noise
    }

    fn set_cfg_index_noise(&mut self, on: bool) {
        self.cfg.index.exclude_noise = on;
        // 噪声位在扫描/compact 期写进 attrs,改开关只能重建快照。快照现在带
        // exclude_noise 指纹,加载时不匹配会自行拒绝;这里主动删掉旧快照,
        // 让用户在下次启动时就得到重建,而不是等到下次扫描。
        // 只删 vol_*.bin:不动 index 目录里的日志等文件,也避免与引擎落盘
        // 竞争(整目录 remove_dir_all 可能撞上正在写的临时文件)。
        let dir = crate::config::attach_dir().join("index");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let looks_like_snapshot = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("vol_") && n.ends_with(".bin"))
                    .unwrap_or(false);
                if looks_like_snapshot {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let _ = self.cfg.save();
    }

    /// 设置页「索引卷」的候选盘:固定 NTFS 盘 ∪ 配置已选盘 ∪ worker 上报盘,
    /// 升序去重(visual 上必须 C D E F 这样排,便于和资源管理器对照)。
    ///
    /// `fixed_ntfs_volumes()` 一次要跑 26 组 GetDriveType/GetVolumeInformation,
    /// 不能每帧调用:结果缓存在 [`Self::index_drives`],5 秒过期
    /// (热插拔后最多 5 秒就能在设置页看到新盘)。
    fn index_drive_letters(&mut self) -> Vec<char> {
        const TTL: Duration = Duration::from_secs(5);
        if let Some((at, letters)) = &self.index_drives {
            if at.elapsed() < TTL {
                return letters.clone();
            }
        }
        let mut letters: Vec<char> = lstr_index::win::fixed_ntfs_volumes()
            .into_iter()
            .map(|(letter, _)| letter)
            .collect();
        // 配置里已选、但此刻枚举不到的盘(比如移动硬盘没插)也要显示,否则没法取消勾选。
        letters.extend(self.cfg.index.volume_letters());
        if let Some(st) = self.service.index_status.lock().unwrap().as_ref() {
            letters.extend(st.volumes.iter().map(|v| v.letter));
        }
        letters.sort_unstable();
        letters.dedup();
        // 枚举异常(权限等)时至少给一个 C:,否则这一行是空的、用户没法操作。
        if letters.is_empty() {
            letters.push('C');
        }
        self.index_drives = Some((Instant::now(), letters.clone()));
        letters
    }

    fn paint_settings_maintenance(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        area: Rect,
    ) {
        let theme = self.theme;
        Self::settings_heading(painter, &theme, area, "维护", "配置文件、窗口与历史数据");
        let mut y = area.top() + 62.0;
        const ROW: f32 = 38.0;

        let button = |ui: &mut egui::Ui,
                      painter: &egui::Painter,
                      theme: &Theme,
                      rect: Rect,
                      label: &str,
                      accent: bool|
         -> bool {
            let response = ui.interact(rect, Id::new(("lstr-maint", label)), Sense::click());
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            painter.rect_filled(rect, CornerRadius::same(6), theme.hover);
            painter.rect_stroke(
                rect,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, if accent { theme.amber } else { theme.hair }),
                StrokeKind::Inside,
            );
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                label,
                ui::text_font(11.5),
                if accent { theme.amber } else { theme.text },
            );
            response.clicked()
        };

        Self::settings_label(painter, &theme, area.left(), y, "配置文件");
        if button(
            ui,
            painter,
            &theme,
            Rect::from_min_size(egui::pos2(area.left() + 130.0, y), egui::vec2(140.0, 26.0)),
            "打开配置文件",
            false,
        ) {
            let _ = shell::open_config();
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "窗口");
        if button(
            ui,
            painter,
            &theme,
            Rect::from_min_size(egui::pos2(area.left() + 130.0, y), egui::vec2(140.0, 26.0)),
            "重置窗口大小",
            false,
        ) {
            self.cfg.width = 640.0;
            self.cfg.height = None;
            self.manual_size = None;
            self.last_width = 0.0;
            self.last_height = 0.0;
            let _ = self.cfg.save();
            self.apply_size(ctx);
            self.toast(ctx, "窗口大小已重置".to_string());
        }
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "使用历史");
        let armed = self
            .clear_armed_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3));
        if button(
            ui,
            painter,
            &theme,
            Rect::from_min_size(egui::pos2(area.left() + 130.0, y), egui::vec2(140.0, 26.0)),
            if armed { "确认清空？" } else { "清空历史" },
            armed,
        ) {
            if armed {
                if let Ok(mut history) = self.history.lock() {
                    history.clear();
                }
                self.clear_armed_at = None;
                self.selected = 0;
                self.trigger(ctx);
                self.toast(ctx, "已清空使用历史".to_string());
            } else {
                self.clear_armed_at = Some(Instant::now());
                self.toast(ctx, "再点一次「确认清空？」才会删除".to_string());
            }
        }

        painter.text(
            egui::pos2(area.left(), area.bottom() - 46.0),
            Align2::LEFT_TOP,
            "配置文件：%APPDATA%\\lstr\\config.toml",
            ui::mono_font(10.0),
            theme.dim,
        );
        painter.text(
            egui::pos2(area.left(), area.bottom() - 28.0),
            Align2::LEFT_TOP,
            format!(
                "lstr {} · 排序规则可在「排序规则」页调整",
                env!("CARGO_PKG_VERSION")
            ),
            ui::mono_font(10.0),
            theme.dim,
        );
    }

    /// 顶部拖动条：主界面 12px，设置面板放到 44px（标题整条都能拖）。
    fn paint_drag_strip(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        drag_h: f32,
    ) {
        let theme = self.theme;
        let corner = 12.0;
        let drag_rect = Rect::from_min_max(
            egui::pos2(rect.left() + corner, rect.top()),
            egui::pos2(rect.right() - corner, rect.top() + drag_h),
        );
        let drag = ui.interact(drag_rect, Id::new("lstr-drag"), Sense::drag());
        if drag.drag_started() {
            self.suppress_hide_until = Instant::now() + Duration::from_millis(600);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.hovered() || drag.dragged() {
            ui.ctx()
                .set_cursor_icon(if drag.dragged() {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
        }
        let handle = Rect::from_center_size(
            egui::pos2(rect.center().x, rect.top() + 5.0),
            egui::vec2(22.0, 2.0),
        );
        let handle_color = if drag.hovered() || drag.dragged() {
            theme.sub
        } else {
            theme.hair2
        };
        painter.rect_filled(handle, CornerRadius::same(1), handle_color);
    }

    fn paint_drag_resize(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let drag_h = 12.0;
        // 右缘只留 3px 给窗口缩放，其余让给滚动条拖拽。
        let edge = 3.0;
        let corner = 12.0;
        self.paint_drag_strip(ui, painter, rect, drag_h);

        let handles = [
            (
                Rect::from_min_max(
                    egui::pos2(rect.right() - edge, rect.top() + drag_h),
                    rect.right_bottom(),
                ),
                egui::ResizeDirection::East,
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left(), rect.top() + drag_h),
                    egui::pos2(rect.left() + edge, rect.bottom()),
                ),
                egui::ResizeDirection::West,
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left(), rect.bottom() - edge),
                    rect.right_bottom(),
                ),
                egui::ResizeDirection::South,
                egui::CursorIcon::ResizeVertical,
            ),
            (
                Rect::from_min_max(
                    rect.right_bottom() - egui::vec2(corner, corner),
                    rect.right_bottom(),
                ),
                egui::ResizeDirection::SouthEast,
                egui::CursorIcon::ResizeNwSe,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left(), rect.bottom() - corner),
                    egui::pos2(rect.left() + corner, rect.bottom()),
                ),
                egui::ResizeDirection::SouthWest,
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.right() - corner, rect.top()),
                    egui::pos2(rect.right(), rect.top() + corner),
                ),
                egui::ResizeDirection::NorthEast,
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                Rect::from_min_max(
                    rect.left_top(),
                    egui::pos2(rect.left() + corner, rect.top() + corner),
                ),
                egui::ResizeDirection::NorthWest,
                egui::CursorIcon::ResizeNwSe,
            ),
        ];
        for (index, (handle_rect, direction, cursor)) in handles.iter().enumerate() {
            let response = ui.interact(
                *handle_rect,
                Id::new(("lstr-resize", index)),
                Sense::drag(),
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(*cursor);
            }
            if response.drag_started() {
                self.resizing = true;
                self.suppress_hide_until = Instant::now() + Duration::from_millis(600);
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::BeginResize(*direction));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Open,
    Reveal,
    CopyPath,
    CopyName,
    Admin,
    OpenWith,
    OpenTerminal,
    RemoveHistory,
    AddFavorite,
    CustomAction(usize),
}

/// 设置面板的分页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab {
    Appearance = 0,
    Search = 1,
    Behavior = 2,
    Ranking = 3,
    Noise = 4,
    Maintenance = 5,
    Assets = 6,
}

/// 设置页「唤起热键」录制状态。
#[derive(Debug, Default)]
struct HotkeyRecording {
    /// 双击修饰键：第一个修饰键的按下沿（键 + 时刻）；None = 尚未按下。
    first_mod: Option<(TapKey, Instant)>,
    /// 上一帧的修饰键状态 [ctrl, alt, shift]，用于检测按下沿。
    prev_mods: [bool; 3],
}

/// egui 按键 → 组合键主键。只收录可安全全局注册的键：
/// 字母 / 数字 / F1-F12 / 空格；方向键、标点等一律不支持录制。
fn primary_key_from_egui(key: egui::Key) -> Option<crate::hotkey::PrimaryKey> {
    use crate::hotkey::PrimaryKey as P;
    use egui::Key as K;
    Some(match key {
        K::A => P::Letter('a'),
        K::B => P::Letter('b'),
        K::C => P::Letter('c'),
        K::D => P::Letter('d'),
        K::E => P::Letter('e'),
        K::F => P::Letter('f'),
        K::G => P::Letter('g'),
        K::H => P::Letter('h'),
        K::I => P::Letter('i'),
        K::J => P::Letter('j'),
        K::K => P::Letter('k'),
        K::L => P::Letter('l'),
        K::M => P::Letter('m'),
        K::N => P::Letter('n'),
        K::O => P::Letter('o'),
        K::P => P::Letter('p'),
        K::Q => P::Letter('q'),
        K::R => P::Letter('r'),
        K::S => P::Letter('s'),
        K::T => P::Letter('t'),
        K::U => P::Letter('u'),
        K::V => P::Letter('v'),
        K::W => P::Letter('w'),
        K::X => P::Letter('x'),
        K::Y => P::Letter('y'),
        K::Z => P::Letter('z'),
        K::Num0 => P::Digit(0),
        K::Num1 => P::Digit(1),
        K::Num2 => P::Digit(2),
        K::Num3 => P::Digit(3),
        K::Num4 => P::Digit(4),
        K::Num5 => P::Digit(5),
        K::Num6 => P::Digit(6),
        K::Num7 => P::Digit(7),
        K::Num8 => P::Digit(8),
        K::Num9 => P::Digit(9),
        K::F1 => P::Function(1),
        K::F2 => P::Function(2),
        K::F3 => P::Function(3),
        K::F4 => P::Function(4),
        K::F5 => P::Function(5),
        K::F6 => P::Function(6),
        K::F7 => P::Function(7),
        K::F8 => P::Function(8),
        K::F9 => P::Function(9),
        K::F10 => P::Function(10),
        K::F11 => P::Function(11),
        K::F12 => P::Function(12),
        K::Space => P::Space,
        _ => return None,
    })
}

/// 筛选条上的四个按钮。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilterMenuKind {
    Location,
    Time,
    Group,
    Match,
}

impl FilterMenuKind {
    fn index(self) -> u8 {
        match self {
            FilterMenuKind::Location => 0,
            FilterMenuKind::Time => 1,
            FilterMenuKind::Group => 2,
            FilterMenuKind::Match => 3,
        }
    }
}

/// 下拉菜单的一项：显示文本 + 是否选中 + 选中后写入哪个筛选字段。
struct FilterItem {
    label: String,
    selected: bool,
    choice: FilterChoice,
}

enum FilterChoice {
    Location(Option<LocationFilter>),
    Time(Option<TimeFilter>),
    Group(Option<FileGroup>),
    /// 匹配选项是多选：点击后菜单保持打开。
    Match(MatchToggle),
    /// 只清除当前这一项筛选（菜单顶部的「清除该项」）。
    Clear(FilterMenuKind),
}

#[derive(Clone)]
struct MenuItem {
    label: String,
    shortcut: &'static str,
    enabled: bool,
    action: MenuAction,
}

fn menu_items(entry: &Entry, actions: &[crate::assets::ActionDef]) -> Vec<MenuItem> {
    let is_command = entry.kind == Kind::Command;
    let is_program = entry.kind == Kind::Program;
    let is_file = entry.kind == Kind::File;
    // 应用（开始菜单 .lnk / App Paths / UWP）也允许尝试提权：普通 .lnk 与
    // 系统工具（appwiz.cpl、taskmgr）都能提权；UWP 由执行层给出明确提示。
    let can_admin = is_program || is_file || (entry.kind == Kind::App && entry.path.is_some());
    // 「在终端打开」只对文件系统条目有意义（文件夹自身 / 文件所在目录）。
    let can_terminal = entry.path.is_some()
        && matches!(entry.kind, Kind::Folder | Kind::File | Kind::Program);
    let mut items = vec![
        MenuItem {
            label: "打开".to_string(),
            shortcut: "↵",
            enabled: true,
            action: MenuAction::Open,
        },
        MenuItem {
            label: "在资源管理器中定位".to_string(),
            shortcut: "Ctrl ↵",
            enabled: !is_command,
            action: MenuAction::Reveal,
        },
        MenuItem {
            label: "复制路径".to_string(),
            shortcut: "Ctrl C",
            enabled: !is_command,
            action: MenuAction::CopyPath,
        },
        MenuItem {
            label: "复制文件名".to_string(),
            shortcut: "",
            enabled: true,
            action: MenuAction::CopyName,
        },
        MenuItem {
            label: "以管理员身份运行".to_string(),
            shortcut: "Ctrl ⇧ ↵",
            enabled: can_admin,
            action: MenuAction::Admin,
        },
        MenuItem {
            label: "打开方式…".to_string(),
            shortcut: "",
            enabled: is_file || is_program,
            action: MenuAction::OpenWith,
        },
        MenuItem {
            label: "在终端打开".to_string(),
            shortcut: "",
            enabled: can_terminal,
            action: MenuAction::OpenTerminal,
        },
        MenuItem {
            label: "从最近使用中移除".to_string(),
            shortcut: "",
            enabled: !is_command,
            action: MenuAction::RemoveHistory,
        },
        MenuItem {
            label: "加入收藏".to_string(),
            shortcut: "",
            enabled: !is_command,
            action: MenuAction::AddFavorite,
        },
    ];
    // P0-1：自定义动作作用于选中的结果，模板可引用 {action_path}。
    for (index, action) in actions.iter().enumerate() {
        items.push(MenuItem {
            label: action.title.clone(),
            shortcut: "",
            enabled: !is_command,
            action: MenuAction::CustomAction(index),
        });
    }
    items
}

impl eframe::App for LstrApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    /// 缺陷修复:GUI 优雅退出(WM_CLOSE/托盘退出/cmd:quit 都汇到这里)必须
    /// 收提权 index worker —— 此前 run_native 返回即进程退出,后台线程被硬杀,
    /// Shutdown 帧发不出去,提权 worker 残留占住唯一管道实例,下次启动撞上
    /// 旧 worker(实测多次出现怪状态)。这里同步发关闭指令并等其退出(有
    /// 超时);worker 收到 Shutdown 后走 write_clean_exit_flag 优雅退出。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 退出原因落 engine.log：下次出现「窗口突然消失」可直接定性，
        // 不用再靠时间线推断（window=关窗/Esc 等未标记路径，tray=托盘退出）。
        lstr_index::snapshot::debug_log(&format!("ui exit reason={}", self.exit_reason));
        self.service
            .graceful_shutdown(std::time::Duration::from_secs(6));
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !self.effects_applied {
            if let Some(hwnd) = window::hwnd_of_frame(frame) {
                self.hwnd = Some(hwnd);
                // 拿到窗口后立刻注销 winit 的 OLE 拖放接收器（结果拖出会闪退，见
                // dragdrop.rs::revoke_window_drop_target）。只需一次。
                crate::win::dragdrop::revoke_window_drop_target(hwnd);
                self.acrylic_ok = window::apply_effects(hwnd, self.theme.light, self.cfg.theme.opacity);
                if self.hidden {
                    window::hide(hwnd);
                } else {
                    window::position_center(hwnd);
                }
                self.effects_applied = true;
            }
        }

        // 再次双击 exe / 从快捷方式启动时，第二个实例会通知这里唤出窗口。
        if crate::win::single_instance::activation_requested() {
            if self.hidden {
                self.show(ctx);
            } else if let Some(hwnd) = self.hwnd {
                window::force_foreground(hwnd);
            }
        }

        crate::win::explorer::note_foreground_explorer();
        self.poll_typing(ctx);
        self.poll_hotkey(ctx);
        self.poll_dialog_watch(ctx);
        self.poll_tray(ctx);
        self.poll_search(ctx);
        self.icons.drain(ctx);

        // 对话框模式：目标对话框被关掉就退出对话框模式（正在显示则一并隐藏）。
        if let Some(info) = &self.dialog {
            if !dialog::is_alive(info) {
                self.dialog = None;
                if !self.hidden {
                    self.hide(ctx);
                }
            }
        }

        if self.hidden {
            // 键入即搜安装后降到 60ms 兜底节拍（钩子本身已 request_repaint，
            // 这里只是防止某些窗口过程丢通知），否则维持 250ms 省电节拍。
            let idle_ms = if self.typing.is_some() { 60 } else { 250 };
            ctx.request_repaint_after(Duration::from_millis(idle_ms));
            return;
        }

        self.apply_size(ctx);
        if self.center_frames > 0 {
            if let Some(hwnd) = self.hwnd {
                window::position_center(hwnd);
            }
            self.center_frames -= 1;
        }
        if self.pending_show {
            if let Some(hwnd) = self.hwnd {
                if let Some(dialog) = self.pending_dialog {
                    window::position_near_dialog(hwnd, dialog);
                } else {
                    window::position_center(hwnd);
                }
            }
            self.pending_show = false;
        }
        self.handle_keys(ctx);
        if self.hidden {
            return;
        }
        self.paint(ctx);
        self.animate(ctx);
        if self.resizing {
            let down = ctx.input(|i| i.pointer.any_down());
            if !down {
                self.resizing = false;
                if let Some(size) = ctx.input(|i| i.viewport().inner_rect.map(|rect| rect.size())) {
                    self.manual_size = Some(size);
                    // 记住用户拖出来的尺寸，下次启动/唤出沿用。
                    self.cfg.width = size.x.clamp(420.0, 1000.0);
                    self.cfg.height = Some(size.y.clamp(160.0, 800.0));
                    let _ = self.cfg.save();
                }
            }
        }
    }
}

/// 按像素宽度二分出「第一行能放下的字符数」：中文/西文混排也准，
/// 不用假设等宽字体的字符宽度。`measure` 注入测量函数（像素宽度），
/// 便于在无 `Painter` 的单测里验证切分逻辑。
fn split_path_head(
    measure: impl Fn(&str) -> f32,
    text: &str,
    max_width: f32,
) -> usize {
    let chars: Vec<char> = text.chars().collect();
    // 整串放得下就不必二分（常见短路径一帧只做这一次测量）。
    if measure(text) <= max_width {
        return chars.len();
    }
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let candidate: String = chars[..mid].iter().collect();
        if measure(&candidate) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// 面包屑分段：首项「此电脑」（空路径 = 退出浏览），随后盘符/UNC 根与各级目录。
fn breadcrumb_items(root: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
    use std::path::Component;
    let mut items: Vec<(String, std::path::PathBuf)> =
        vec![("此电脑".to_string(), std::path::PathBuf::new())];
    let mut current = std::path::PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
        match component {
            Component::Prefix(prefix) => {
                let mut label = prefix.as_os_str().to_string_lossy().into_owned();
                while label.ends_with(['\\', '/']) {
                    label.pop();
                }
                // Prefix 的 as_os_str 是 "D:"（不含根分隔符）；这里补回根，
                // 后续 Normal 组件才能真正拼成 "D:\a" 而不是盘符相对路径 "D:a"。
                let root = std::path::PathBuf::from(format!("{label}\\"));
                current = root.clone();
                items.push((label, root));
            }
            Component::RootDir => {}
            Component::Normal(name) => {
                items.push((name.to_string_lossy().into_owned(), current.clone()));
            }
            _ => {}
        }
    }
    items
}

/// 按字符数把完整路径切成「第一行接排段」与「第二行续段」。
fn split_at_chars(text: &str, count: usize) -> (String, String) {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(count).collect();
    let tail: String = chars.collect();
    (head, tail)
}

/// 路径行是否属于「拼音命中」。
///
/// 条件(全部满足):
/// - 查询非空;
/// - **每个** token 都不被名字(小写)包含 —— 任一 token 字面命中就说明
///   结果不是靠拼音进来的(token 级判断,多词查询「微信 资料」不会误标);
/// - 至少一个 token 是纯 ASCII(中文 token 本身就不可能是拼音输入);
/// - 名字含非 ASCII 字符(纯 ASCII 名字的拼音 key 只是名字小写,不存在
///   「拼音命中」语义)。
fn is_pinyin_path_hit(name: &str, query_text: &str) -> bool {
    let name_lc = name.to_lowercase();
    let mut any_token = false;
    let mut any_ascii = false;
    for token in query_text.split_whitespace() {
        if token.is_empty() {
            continue;
        }
        let token_lc = token.to_lowercase();
        if name_lc.contains(&token_lc) {
            return false;
        }
        any_token = true;
        any_ascii |= token_lc.is_ascii();
    }
    any_token && any_ascii && !name.is_ascii()
}

/// 预览面板的「修改」行。None = mtime 未知(索引 0 且 FS 查询失败),
/// 面板不显示该行;有值时走与结果列表同一套相对时间格式化。
fn preview_modified_line(entry: &Entry) -> Option<String> {
    entry
        .modified
        .map(|ts| format!("修改   {}", ui::format_relative_time(ts)))
}

fn dialog_hint(scope: Scope) -> &'static str {
    match scope {
        Scope::Folder => "搜索文件夹并跳转…",
        Scope::File | Scope::Program => "搜索文件并填入…",
        Scope::All => "搜索并填入当前对话框…",
        Scope::App | Scope::Command => "对话框模式…",
    }
}

fn hint_for(scope: Scope) -> &'static str {
    match scope {
        Scope::All => "搜索文件、应用或命令…",
        Scope::Folder => "搜索文件夹…",
        Scope::Program => "搜索可执行程序…",
        Scope::File => "搜索文件…",
        Scope::App => "搜索应用…",
        Scope::Command => "搜索命令…",
    }
}

fn strip_prefix(input: &mut String) {
    let value = input.trim_start().to_string();
    if let Some(rest) = value.strip_prefix('>') {
        *input = rest.trim_start().to_string();
        return;
    }
    let lower = value.to_lowercase();
    if lower.starts_with("a ") || lower.starts_with("f ") {
        *input = value[2..].trim_start().to_string();
    }
}

#[cfg(test)]
mod path_layout_tests {
    use super::*;

    #[test]
    fn split_at_chars_keeps_the_full_path() {
        let (head, tail) = split_at_chars("abcdef", 2);
        assert_eq!(head, "ab");
        assert_eq!(tail, "cdef");
        assert_eq!(format!("{head}{tail}"), "abcdef");
    }

    #[test]
    fn split_at_chars_is_unicode_safe() {
        let (head, tail) = split_at_chars("中文路径", 2);
        assert_eq!(head, "中文");
        assert_eq!(tail, "路径");
        assert_eq!(format!("{head}{tail}"), "中文路径");
    }

    #[test]
    fn split_at_chars_handles_out_of_range_counts() {
        assert_eq!(split_at_chars("abc", 0), (String::new(), "abc".to_string()));
        assert_eq!(split_at_chars("abc", 99), ("abc".to_string(), String::new()));
    }

    /// 等宽字体的近似宽度：ASCII 10px，中文/全角 20px。
    fn fake_measure(text: &str) -> f32 {
        text.chars()
            .map(|c| if c.is_ascii() { 10.0 } else { 20.0 })
            .sum()
    }

    #[test]
    fn split_path_head_returns_all_chars_when_text_fits() {
        assert_eq!(split_path_head(fake_measure, "abcdef", 60.0), 6);
        assert_eq!(split_path_head(fake_measure, "中文路径", 80.0), 4);
    }

    #[test]
    fn split_path_head_respects_pixels_not_chars() {
        // 同样的字符数，中文占两倍宽度：40px 只放得下 2 个汉字，或 4 个 ASCII。
        assert_eq!(split_path_head(fake_measure, "中文路径abcd", 40.0), 2);
        assert_eq!(split_path_head(fake_measure, "abcd中文路径", 40.0), 4);
        // 35px：2 个 ASCII(20) 放得下，3 个 ASCII(30) 放不下但 1 汉字(20)+1 ASCII(10) 放得下。
        assert_eq!(split_path_head(fake_measure, "中a中a", 35.0), 2);
    }

    #[test]
    fn split_path_head_handles_zero_width() {
        assert_eq!(split_path_head(fake_measure, "abc", 0.0), 0);
        // 测出来恒为 0 时视为全部放得下，避免除零/死循环。
        assert_eq!(split_path_head(|_| 0.0, "abc", 5.0), 3);
    }

    /// 问题 2：文件结果靠拼音命中时副标题要标「拼音 · 」。
    #[test]
    fn pinyin_path_hit_marks_ascii_query_on_cjk_name() {
        assert!(is_pinyin_path_hit("LstrAcc-微信资料", "weixin"));
        assert!(is_pinyin_path_hit("LstrAcc-微信资料", "wxzl"));
        // 多 token 都是 ASCII 且都不在名字里：仍是拼音命中。
        assert!(is_pinyin_path_hit("LstrAcc-微信资料", "weixin ziliao"));
    }

    /// token 级判断：任一 token 直接出现在名字里就不算拼音命中，
    /// 避免「微信 资料」「lstracc weixin」这类混合查询误标。
    #[test]
    fn pinyin_path_hit_rejects_literal_token_match() {
        assert!(!is_pinyin_path_hit("LstrAcc-微信资料", "微信"));
        assert!(!is_pinyin_path_hit("LstrAcc-微信资料", "微信 资料"));
        assert!(!is_pinyin_path_hit("LstrAcc-微信资料", "lstracc weixin"));
        // 大小写不敏感：字面命中同样拒绝。
        assert!(!is_pinyin_path_hit("LstrAcc-微信资料", "LSTRACC"));
    }

    /// 拼音标注只对「非 ASCII 名字 + 纯 ASCII token」成立。
    #[test]
    fn pinyin_path_hit_requires_cjk_name_and_ascii_token() {
        // 纯 ASCII 名字没有拼音语义。
        assert!(!is_pinyin_path_hit("report.txt", "weixin"));
        // 非 ASCII token 不是拼音输入。
        assert!(!is_pinyin_path_hit("微信资料", "资料拼音"));
        // 空查询不标。
        assert!(!is_pinyin_path_hit("微信资料", ""));
        assert!(!is_pinyin_path_hit("微信资料", "   "));
    }

    /// 预览面板的修改时间行:Entry.modified 有值(候选用索引 mtime 填充,
    /// finalize 再用 FS mtime 刷新)时必须可达,未知时不显示。
    #[test]
    fn preview_shows_modified_line_when_entry_has_mtime() {
        let mut entry = Entry {
            id: r"C:\tools\notes.md".to_string(),
            kind: Kind::File,
            name: "notes.md".to_string(),
            path: Some(std::path::PathBuf::from(r"C:\tools\notes.md")),
            size: Some(3),
            modified: Some(1_704_067_200), // 2024-01-01
            pinyin: None,
            initial: "N".to_string(),
            tone: 0,
        };
        let line = preview_modified_line(&entry).expect("有 mtime 必须显示修改行");
        assert!(line.starts_with("修改   "), "格式不对: {line}");
        assert!(line.len() > "修改   ".len(), "修改行不能为空: {line}");

        entry.modified = None;
        assert!(preview_modified_line(&entry).is_none(), "未知 mtime 不显示");
    }

    fn menu_entry(kind: Kind, path: Option<&str>) -> Entry {
        Entry {
            id: "test".to_string(),
            kind,
            name: "test".to_string(),
            path: path.map(std::path::PathBuf::from),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        }
    }

    fn item(items: &[MenuItem], action: MenuAction) -> MenuItem {
        items
            .iter()
            .find(|item| item.action == action)
            .cloned()
            .unwrap_or_else(|| panic!("菜单缺少 {action:?}"))
    }

    /// 应用条目（开始菜单 .lnk / 系统工具）也要能尝试管理员启动；
    /// 只有没有路径的应用保持禁用。
    #[test]
    fn admin_menu_is_enabled_for_apps_with_paths() {
        let app = menu_items(
            &menu_entry(Kind::App, Some(r"C:\Start Menu\Terminal.lnk")),
            &[],
        );
        assert!(item(&app, MenuAction::Admin).enabled);
        let uwp = menu_items(
            &menu_entry(
                Kind::App,
                Some(r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"),
            ),
            &[],
        );
        assert!(
            item(&uwp, MenuAction::Admin).enabled,
            "打包应用也应可尝试，执行层会给出明确提示"
        );
        let no_path = menu_items(&menu_entry(Kind::App, None), &[]);
        assert!(!item(&no_path, MenuAction::Admin).enabled);
        let folder = menu_items(&menu_entry(Kind::Folder, Some(r"C:\work")), &[]);
        assert!(!item(&folder, MenuAction::Admin).enabled, "文件夹不支持提权");
    }

    /// 「在终端打开」：文件夹 / 文件 / 程序可用，应用与命令不可用。
    #[test]
    fn terminal_menu_is_enabled_for_file_system_entries() {
        for (kind, path) in [
            (Kind::Folder, r"C:\work"),
            (Kind::File, r"C:\work\notes.txt"),
            (Kind::Program, r"C:\Tools\app.exe"),
        ] {
            let items = menu_items(&menu_entry(kind, Some(path)), &[]);
            assert!(item(&items, MenuAction::OpenTerminal).enabled, "{kind:?} 应可终端打开");
        }
        for (kind, path) in [
            (Kind::App, Some(r"C:\Start Menu\Terminal.lnk")),
            (Kind::Command, None),
        ] {
            let items = menu_items(&menu_entry(kind, path), &[]);
            assert!(!item(&items, MenuAction::OpenTerminal).enabled, "{kind:?} 不应终端打开");
        }
    }

    #[test]
    fn breadcrumb_items_split_drive_and_segments() {
        let items = breadcrumb_items(std::path::Path::new(r"D:\a\b"));
        let labels: Vec<&str> = items.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["此电脑", "D:", "a", "b"]);
        assert_eq!(items[0].1, std::path::PathBuf::new(), "首项用于退出浏览");
        assert_eq!(items[1].1, std::path::PathBuf::from(r"D:\"));
        assert_eq!(items[3].1, std::path::PathBuf::from(r"D:\a\b"));
    }
}
