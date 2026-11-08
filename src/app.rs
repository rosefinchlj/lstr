use crate::config::Config;
use crate::history::History;
use crate::model::{Entry, Kind, Scope};
use crate::search::filters::{FileGroup, LocationFilter, SearchFilters, TimeFilter};
use crate::search::{self, SearchService};
use crate::ui::{self, Theme};
use crate::win::hotkey::{HotkeyEvent, HotkeyService};
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
    error: Option<String>,
    pending_gen: u64,
    waiting: bool,
    dirty_at: Option<Instant>,
    hidden: bool,
    shown_at: Instant,
    focus_requested: bool,
    hwnd: Option<isize>,
    effects_applied: bool,
    acrylic_ok: bool,
    hotkey: Option<HotkeyService>,
    dialog_watch: Option<crate::win::dialogwatch::DialogWatch>,
    suppressed_dialog: Option<isize>,
    tray: Option<TrayState>,
    toast: Option<(String, Instant)>,
    clear_armed_at: Option<Instant>,
    last_height: f32,
    needle_y: f32,
    needle_target: f32,
    needle_ready: bool,
    mouse_moved: bool,
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
        let mut service = SearchService::new(cfg.clone(), Arc::clone(&history), cc.egui_ctx.clone());
        let pending_gen = service.search("", Scope::All, false, &SearchFilters::default(), &cfg.rank, cfg.show_history);
        let hotkey = match HotkeyService::start(cc.egui_ctx.clone()) {
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
            match crate::win::typing::TypingWatch::start() {
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
            error: None,
            pending_gen,
            waiting: true,
            dirty_at: None,
            hidden: true,
            shown_at: Instant::now(),
            focus_requested: true,
            hwnd: None,
            effects_applied: false,
            acrylic_ok: false,
            hotkey,
            dialog_watch,
            suppressed_dialog: None,
            tray,
            toast: None,
            clear_armed_at: None,
            last_height: 0.0,
            needle_y: 0.0,
            needle_target: 0.0,
            needle_ready: false,
            mouse_moved: false,
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
        }
    }

    /// 设置页开关：即时安装/卸载全局键盘钩子，不必重启程序。
    fn set_explorer_typing(&mut self, enabled: bool, ctx: &egui::Context) {
        self.cfg.explorer_typing = enabled;
        // 先卸载旧实例（Drop 会反挂钩子并结束线程），再按需安装新的。
        self.typing = None;
        if enabled {
            match crate::win::typing::TypingWatch::start() {
                Ok(service) => self.typing = Some(service),
                Err(err) => self.toast(ctx, format!("启动键入搜索失败: {err}")),
            }
        }
        let _ = self.cfg.save();
    }

    /// P0-2：Explorer 里敲下的第一个字符唤出启动器并写入输入框。
    /// 之后前台不再是 Explorer，钩子自动停止接管，后续输入交回 egui。
    fn poll_typing(&mut self, ctx: &egui::Context) {
        let ch = self.typing.as_ref().and_then(|service| service.poll());
        let Some(ch) = ch else {
            return;
        };
        if self.hidden {
            self.show(ctx);
        }
        self.input.push(ch);
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
        for event in events {
            match event {
                HotkeyEvent::Toggle => {
                    self.toggle(ctx);
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
                HotkeyEvent::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
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
            Some(TrayAction::Quit) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            None => {}
        }
    }

    fn poll_search(&mut self, ctx: &egui::Context) {
        if let Some(deadline) = self.dirty_at {
            if Instant::now() >= deadline {
                self.pending_gen = self.service.search(&self.input, self.scope, self.dialog.is_some(), &self.filters, &self.cfg.rank, self.cfg.show_history);
                self.waiting = true;
                self.dirty_at = None;
            } else {
                ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
            }
        }
        if let Some(response) = self.service.poll() {
            if response.generation == self.pending_gen {
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
        // 30ms：只用来合并同一帧内的连击/粘贴，不再人为拖慢输入。
        // 请求在 worker 里是 latest-wins 合并的，逐键查询不会堆积。
        self.dirty_at = Some(Instant::now() + Duration::from_millis(30));
        ctx.request_repaint_after(Duration::from_millis(35));
    }

    fn toggle(&mut self, ctx: &egui::Context) {
        if self.hidden {
            self.show(ctx);
        } else {
            self.hide(ctx);
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
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
        self.input.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.scroll_to_selection = false;
        self.filters = SearchFilters::default();
        self.filter_menu = None;
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

    fn hide(&mut self, _ctx: &egui::Context) {
        if let Some(hwnd) = self.hwnd {
            window::hide(hwnd);
        }
        self.hidden = true;
        self.menu_open = false;
        self.center_frames = 0;
        self.focus_requested = false;
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
            let list_h = if rows == 0 { 120.0 } else { rows as f32 * self.theme.row_h };
            let banner_h = if self.error.is_some() { 30.0 } else { 0.0 };
            let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
            let computed_h = (SEARCH_H
                + TABS_H
                + FILTER_H
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

        // 路径模式回退：Alt+↑ 或（筛选为空时）Backspace 一次退一整级。
        if crate::search::browse::parse(&self.input).is_some() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowUp)) {
                self.path_up(ctx);
                return;
            }
            let filter_empty = self.input.ends_with(std::path::is_separator);
            // 有选中文本（如 Ctrl+A 全选）时让 TextEdit 正常删除，不触发整级回退。
            let has_selection = {
                let id = egui::Id::new("lstr-search-input");
                egui::widgets::text_edit::TextEditState::load(ctx, id)
                    .and_then(|state| state.cursor.char_range())
                    .map(|range| range.primary.index != range.secondary.index)
                    .unwrap_or(false)
            };
            if filter_empty
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
        strip_prefix(&mut self.input);
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
                    self.hide(ctx);
                    dialog::restore_focus(&dialog);
                }
                Err(err) => self.toast(ctx, err),
            }
            return;
        }
        // 路径模式：Enter 文件夹 = 继续深入；想在资源管理器打开用 Ctrl+Enter。
        // 普通模式下 Enter 文件夹仍保持打开行为，只有 → 下钻。
        if entry.kind == Kind::Folder && crate::search::browse::parse(&self.input).is_some() {
            self.descend_index(ctx, index);
            return;
        }
        if entry.kind == Kind::Command {
            self.run_command(ctx, &entry);
            return;
        }
        match shell::open_entry(&entry) {
            Ok(()) => {
                self.service.record_open(&entry);
                // P1-1：记住「搜这个词时选了它」。
                if let Ok(mut history) = self.history.lock() {
                    history.record_query(&self.input, &entry);
                }
                self.hide(ctx);
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
            Ok(()) => self.hide(ctx),
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
            Ok(()) => self.hide(ctx),
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
            "cmd:quit" => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
    }

    fn toast(&mut self, ctx: &egui::Context, message: String) {
        self.toast = Some((message, Instant::now() + Duration::from_millis(1300)));
        ctx.request_repaint_after(Duration::from_millis(1330));
    }

    fn update_focus(&mut self, ctx: &egui::Context) {
        if self.hidden
            || self.shown_at.elapsed() < Duration::from_millis(350)
            || Instant::now() < self.suppress_hide_until
            || ctx.input(|i| i.pointer.any_down())
        {
            return;
        }
        if let Some(hwnd) = self.hwnd {
            if !window::is_foreground(hwnd) {
                // 失焦隐藏时如果处于对话框模式，同样抑制该对话框的自动唤出，
                // 否则用户点回对话框会触发一次无意义的再弹出。
                if let Some(dialog) = &self.dialog {
                    self.suppressed_dialog = Some(dialog.hwnd);
                }
                self.hide(ctx);
            }
        }
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
        let banner_h = if self.error.is_some() { 30.0 } else { 0.0 };
        let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
        let preview_w = if self.preview_open { 216.0 } else { 0.0 };
        let height = if self.settings_open {
            600.0
        } else {
            (SEARCH_H
                + TABS_H
                + FILTER_H
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
                    Stroke::new(1.0, theme.hair2),
                    StrokeKind::Inside,
                );
                if !theme.light {
                    let y = rect.top() + 0.5;
                    painter.line_segment(
                        [
                            egui::pos2(rect.left() + theme.radius as f32 + 2.0, y),
                            egui::pos2(rect.right() - theme.radius as f32 - 2.0, y),
                        ],
                        Stroke::new(1.0, Color32::from_white_alpha(16)),
                    );
                }

                if self.settings_open {
                    // 设置面板也能拖：标题整条 44px 都可拖，关闭按钮后注册会优先命中。
                    self.paint_drag_strip(ui, &painter, rect, 44.0);
                    self.paint_settings(ui, ctx, rect);
                    return;
                }

                let search_rect = Rect::from_min_size(
                    egui::pos2(rect.left() + PAD, rect.top() + 14.0),
                    egui::vec2(rect.width() - 2.0 * PAD, 28.0),
                );
                let hint = if self.dialog.is_some() {
                    dialog_hint(query.scope)
                } else {
                    hint_for(query.scope)
                };
                let response = ui.put(
                    search_rect,
                    egui::TextEdit::singleline(&mut self.input)
                        .frame(false)
                        .margin(egui::Margin::same(0))
                        .font(ui::text_font(22.0))
                        .text_color(theme.text)
                        .hint_text(hint)
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
                    Stroke::new(1.0, focus_color),
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
                        Stroke::new(1.0, theme.hair2),
                        StrokeKind::Inside,
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let gear_color = if gear.hovered() { theme.text } else { theme.sub };
                painter.text(
                    gear_rect.center(),
                    Align2::CENTER_CENTER,
                    "⚙",
                    ui::text_font(14.0),
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
                if history.hovered() {
                    painter.rect_filled(
                        history_rect.shrink(1.0),
                        CornerRadius::same(6),
                        theme.surface_hi,
                    );
                    painter.rect_stroke(
                        history_rect.shrink(1.0),
                        CornerRadius::same(6),
                        Stroke::new(1.0, theme.hair2),
                        StrokeKind::Inside,
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let history_color = if self.cfg.show_history {
                    if history.hovered() { theme.amber } else { theme.sub }
                } else if history.hovered() {
                    theme.sub
                } else {
                    theme.dim
                };
                // 画一个时钟图标（字体缺 U+23F2 之类的 glyph）。
                let center = history_rect.center();
                let clock = Stroke::new(1.3, history_color);
                painter.circle_stroke(center, 7.0, clock);
                painter.line_segment(
                    [center, center + egui::vec2(0.0, -4.0)],
                    clock,
                );
                painter.line_segment(
                    [center, center + egui::vec2(3.2, 1.6)],
                    clock,
                );
                if !self.cfg.show_history {
                    // 关闭态：斜线划掉。
                    painter.line_segment(
                        [
                            center + egui::vec2(-6.5, 6.5),
                            center + egui::vec2(6.5, -6.5),
                        ],
                        Stroke::new(1.6, history_color),
                    );
                }
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
                            Stroke::new(1.0, theme.hair2),
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
                        self.input.clear();
                        self.selected = 0;
                        self.menu_open = false;
                        self.focus_requested = true;
                        self.trigger(ctx);
                    }
                }

                self.paint_tabs(ui, &painter, rect, &query);
                self.paint_filters(ui, &painter, rect);
                let list_top = rect.top() + SEARCH_H + TABS_H + FILTER_H + LIST_GAP;
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
                            1.0,
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
                if preview_w > 0.0 {
                    let footer_top = rect.bottom() - FOOTER_H;
                    painter.line_segment(
                        [
                            egui::pos2(list_rect.right() + 3.0, rect.top() + SEARCH_H),
                            egui::pos2(list_rect.right() + 3.0, footer_top),
                        ],
                        Stroke::new(1.0, theme.hair),
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
                        Stroke::new(1.0, theme.amber.gamma_multiply(0.35)),
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

    /// 筛选条：位置 / 时间 / 类型三个按钮。用户点选即可，内部自动翻译成
    /// Everything 的 path:/dm:/ext: 查询段——不需要记语法。
    fn paint_filters(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let theme = self.theme;
        // 路径模式本身就是位置筛选，按钮仅在普通搜索下生效。
        let path_mode = crate::search::browse::parse(&self.input).is_some();
        let y = rect.top() + SEARCH_H + TABS_H + 2.0;
        let mut x = rect.left() + PAD;

        let chips: [(FilterMenuKind, String, bool); 3] = [
            (
                FilterMenuKind::Location,
                format!(
                    "位置 · {}",
                    self.filters
                        .location
                        .as_ref()
                        .map(|location| location.label())
                        .unwrap_or_else(|| "全部".to_string())
                ),
                self.filters.location.is_some(),
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
        ];

        let mut clicked = None;
        for (kind, label, active) in chips {
            let color = if path_mode {
                theme.dim
            } else if active {
                theme.amber
            } else {
                theme.sub
            };
            let galley = painter.layout_no_wrap(label, ui::text_font(11.0), color);
            let width = galley.size().x + 28.0;
            let chip = Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, 22.0));
            let response = ui.interact(
                chip,
                Id::new(("lstr-filter-chip", kind.index())),
                if path_mode { Sense::hover() } else { Sense::click() },
            );
            if !path_mode {
                if active {
                    painter.rect_filled(chip, CornerRadius::same(6), theme.amber_soft);
                } else if response.hovered() {
                    painter.rect_filled(chip, CornerRadius::same(6), theme.hover);
                }
                painter.rect_stroke(
                    chip,
                    CornerRadius::same(6),
                    Stroke::new(
                        1.0,
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
            let cx = chip.right() - 10.0;
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
            if response.clicked() && !path_mode {
                clicked = Some(kind);
            }
            x += width + 8.0;
        }

        // 有筛选时才出现清除入口。
        if !self.filters.is_empty() && !path_mode {
            // ×（U+00D7）在 Segoe UI 里一定有字形，比 ✕ 安全。
            let galley = painter.layout_no_wrap("× 清除".to_string(), ui::text_font(10.5), theme.dim);
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

        if path_mode {
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
                    stroke: Stroke::new(1.0, theme.hair2),
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
                            let stroke = Stroke::new(1.6, theme.amber);
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
            // 空查询（还没开始搜索）不显示任何文案——「暂无记录」会被误解成搜不到。
            if query.is_empty() {
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
            let empty_hint = if crate::search::browse::parse(&self.input).is_some() {
                "目录为空、不存在或无权限"
            } else if self.dialog.is_some() {
                match query.scope {
                    Scope::Folder => "输入文件夹名，Enter 跳转",
                    _ => "输入文件名，Enter 填入",
                }
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

        let pointer_moved = ui.ctx().input(|i| i.pointer.delta() != Vec2::ZERO);
        if pointer_moved {
            self.mouse_moved = true;
        }
        let mut hovered = None;
        let mut clicked = None;
        let mut drag_started = None;
        let q = query.text.to_lowercase();
        let path_mode = crate::search::browse::parse(&self.input).is_some();
        // 副标题（等宽字体）的字符预算：用实际字符宽度换算，尽量把行宽用满。
        let mono_char_w = ui::mono_font(11.5);
        let char_w = painter
            .layout_no_wrap("x".to_string(), mono_char_w, self.theme.sub)
            .size()
            .x
            .max(1.0);
        let sub_budget = (((rect.width() - 16.0 - 64.0) / char_w).floor() as usize).max(16);
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
                    row_rect.shrink2(egui::vec2(0.0, 2.0)),
                    CornerRadius::same(8),
                    theme.sel,
                );
            } else if response.hovered() {
                painter.rect_filled(
                    row_rect.shrink2(egui::vec2(0.0, 2.0)),
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
            let mut title_job = match &fuzzy_hit {
                Some(positions) => ui::text_job_positions(
                    &entry.name,
                    positions,
                    &ui::text_font(14.5),
                    theme.text,
                    theme.amber,
                ),
                None => ui::text_job(
                    &entry.name,
                    title_q,
                    &ui::text_font(14.5),
                    theme.text,
                    theme.amber,
                ),
            };
            title_job.wrap.max_width = row_rect.width() - 54.0 - 76.0;
            title_job.wrap.max_rows = 1;
            title_job.wrap.overflow_character = Some('…');
            // egui 默认优先在连字符/标点处断行：长文件名 "lstr-9cc85..." 只剩
            // 一个连字符在第 5 个字符，会被截成 "lstr-…"。break_anywhere 让
            // 它在真实宽度边缘截断，窗口拉宽就能多看几个字。
            title_job.wrap.break_anywhere = true;
            painter.galley(
                egui::pos2(row_rect.left() + 54.0, row_rect.top() + 5.0),
                painter.layout_job(title_job),
                theme.text,
            );

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
                    // 按实际可用像素换算字符预算：以前固定 72 字符 + 只留两段尾，
                    // 两条不同的深路径会显示成同一串，同名结果无法区分。
                    let path_text = ui::middle_ellipsis(&entry.full_path(), sub_budget);
                    if entry.id.starts_with("qs:") {
                        format!("已打开 · {path_text}")
                    } else {
                        path_text
                    }
                }
            };
            let mut sub_job = ui::text_job(
                &subtitle,
                if path_mode { "" } else { &query.text },
                &ui::mono_font(11.5),
                theme.sub,
                theme.amber,
            );
            sub_job.wrap.max_width = row_rect.width() - 54.0 - 10.0;
            sub_job.wrap.max_rows = 1;
            sub_job.wrap.overflow_character = Some('…');
            // 同上：路径里大量 "\\" / "." 会被当断行点，加 break_anywhere 才会
            // 在宽度边缘截断。
            sub_job.wrap.break_anywhere = true;
            painter.galley(
                egui::pos2(row_rect.left() + 54.0, row_rect.top() + 25.0),
                painter.layout_job(sub_job),
                theme.sub,
            );

            if selected && self.cfg.show_meta {
                let meta = match entry.kind {
                    Kind::Folder => "文件夹".to_string(),
                    Kind::Command => String::new(),
                    _ => entry.size.map(ui::format_size).unwrap_or_default(),
                };
                if !meta.is_empty() {
                    painter.text(
                        egui::pos2(row_rect.right() - 10.0, row_rect.center().y),
                        Align2::RIGHT_CENTER,
                        meta,
                        ui::mono_font(11.0),
                        theme.dim,
                    );
                }
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
            self.selected = index;
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
            Stroke::new(1.0, theme.hair),
        );

        let default_text = if query.is_empty() {
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
                "{} 个结果 · {:.2}s{}",
                self.total,
                self.elapsed_ms / 1000.0,
                if self.scrollable { " · 滚轮查看更多" } else { "" }
            )
        };
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

        let path_mode = crate::search::browse::parse(&self.input).is_some();
        let selected_is_folder = self
            .entries
            .get(self.selected)
            .map(|entry| entry.kind == Kind::Folder)
            .unwrap_or(false);
        let hints: [(&str, &str); 4] = if path_mode {
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
            let key_rect = Rect::from_min_size(
                egui::pos2(x, footer_top + 10.0),
                egui::vec2(key_w, 16.0),
            );
            painter.rect_stroke(
                key_rect,
                CornerRadius::same(4),
                Stroke::new(1.0, theme.hair),
                StrokeKind::Inside,
            );
            painter.galley(
                egui::pos2(x + 4.0, footer_top + 12.5),
                key_galley,
                theme.dim,
            );
            painter.galley(
                egui::pos2(x + key_w + 4.0, footer_top + 12.5),
                label_galley,
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

    /// 文件夹结果：就地展开（Listary 式下钻）。输入框替换为 `路径\` 进入路径模式，
    /// 返回 false 表示选中项不是文件夹，调用方退回原行为（动作菜单/打开）。
    fn descend_index(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        if entry.kind != Kind::Folder {
            return false;
        }
        let mut path = entry.full_path();
        if !path.ends_with(['\\', '/']) {
            path.push('\\');
        }
        self.input = path;
        self.selected = 0;
        self.menu_open = false;
        self.move_caret_to_end(ctx);
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
                    Ok(()) => self.hide(ctx),
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
        let list_top = rect.top() + SEARCH_H + TABS_H + FILTER_H + LIST_GAP;
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
                    stroke: Stroke::new(1.0, theme.hair2),
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
        ui: &mut egui::Ui,
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
        let mut name_job = ui::text_job(
            &entry.name,
            q,
            &ui::text_font(14.5),
            theme.text,
            theme.amber,
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
        let mut path_job = ui::text_job(
            &full,
            q,
            &ui::mono_font(11.0),
            theme.sub,
            theme.amber,
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
        if let Some(modified) = entry.modified {
            painter.text(
                egui::pos2(pane.left() + 8.0, y),
                Align2::LEFT_TOP,
                format!("修改   {}", ui::format_relative_time(modified)),
                ui::mono_font(11.0),
                theme.sub,
            );
            y += 16.0;
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
                1.0,
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
            let mark = Stroke::new(1.8, Color32::WHITE);
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
            Stroke::new(1.0, theme.hair),
        );

        let area = Rect::from_min_max(
            egui::pos2(rect.left() + SIDEBAR_W + 24.0, rect.top() + 52.0),
            egui::pos2(rect.right() - 24.0, rect.bottom() - 16.0),
        );
        match self.settings_tab {
            SettingsTab::Appearance => self.paint_settings_appearance(ui, &painter, area),
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
                Stroke::new(1.0, if selected { theme.amber } else { theme.hair }),
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
                painter.circle_stroke(swatch.center(), 10.0, Stroke::new(2.0, theme.text));
            } else if response.hovered() {
                painter.circle_stroke(swatch.center(), 10.0, Stroke::new(1.5, theme.sub));
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
        Self::settings_heading(painter, &theme, area, "行为", "结果数量、对话框联动与开机启动");
        let mut y = area.top() + 62.0;
        const ROW: f32 = 36.0;
        let ctrl_x = area.left() + 130.0;
        let ctrl_w = (area.width() - 140.0).min(240.0);

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
            Stroke::new(1.0, theme.hair),
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
            Stroke::new(1.0, theme.hair),
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
            Stroke::new(1.0, theme.hair),
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
                Stroke::new(1.0, if accent { theme.amber } else { theme.hair }),
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
    RemoveHistory,
    AddFavorite,
    CustomAction(usize),
}

/// 设置面板的分页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab {
    Appearance = 0,
    Behavior = 1,
    Ranking = 2,
    Noise = 3,
    Maintenance = 4,
    Assets = 5,
}

/// 筛选条上的三个按钮。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilterMenuKind {
    Location,
    Time,
    Group,
}

impl FilterMenuKind {
    fn index(self) -> u8 {
        match self {
            FilterMenuKind::Location => 0,
            FilterMenuKind::Time => 1,
            FilterMenuKind::Group => 2,
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
            enabled: is_program || is_file,
            action: MenuAction::Admin,
        },
        MenuItem {
            label: "打开方式…".to_string(),
            shortcut: "",
            enabled: is_file || is_program,
            action: MenuAction::OpenWith,
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
            ctx.request_repaint_after(Duration::from_millis(250));
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
        self.update_focus(ctx);
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
