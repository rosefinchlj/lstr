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

/// 图标要走的加载路径。显式枚举（而不是路径/URL 两个 Option）是为了让
/// 「应用条目误用书签回退图标」这种错误在类型层面无处可藏。
enum IconSource {
    /// 类型图标（文件夹 / 按扩展名的文件），不访问磁盘。
    Generic(String, u32),
    /// 书签：网站 favicon；取不到回退 Edge 图标。
    Favicon(String),
    /// 应用 / 程序等普通条目：从真实路径取 shell 图标。
    Shell(String),
}

struct IconJob {
    key: String,
    source: IconSource,
}

/// 条目 → 图标来源。书签只用 favicon（失败回退 Edge）；其它类型一律走
/// 自己的路径，绝不能借用浏览器图标。
fn icon_source(entry: &Entry) -> IconSource {
    match entry.kind {
        Kind::Folder => IconSource::Generic(entry.full_path(), GENERIC_FOLDER_ICON),
        Kind::File => IconSource::Generic(entry.full_path(), GENERIC_FILE_ICON),
        Kind::Bookmark => IconSource::Favicon(entry.full_path()),
        _ => IconSource::Shell(entry.full_path()),
    }
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
                    let image = match job.source {
                        IconSource::Generic(path, attrs) => {
                            crate::win::icons::load_icon_with_attrs(&path, 32, attrs)
                        }
                        IconSource::Favicon(url) => crate::win::icons::load_favicon(&url)
                            .or_else(crate::win::icons::edge_icon),
                        IconSource::Shell(path) => {
                            crate::win::icons::load_icon(&path, 32)
                        }
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
        // 书签页一次可能有上百个图标同时到货：每帧最多上传 16 张纹理，
        // 剩下的下一帧继续（已有 request_repaint），避免一次性上传造成卡顿。
        const ICONS_PER_FRAME: usize = 16;
        let mut loaded = false;
        let mut uploaded = 0;
        while uploaded < ICONS_PER_FRAME {
            let Ok((key, image)) = self.rx.try_recv() else {
                break;
            };
            self.pending.remove(&key);
            let texture = image.map(|img| {
                ctx.load_texture(key.clone(), img, egui::TextureOptions::LINEAR)
            });
            self.cache.insert(key, texture);
            loaded = true;
            uploaded += 1;
        }
        // 每份书签一张 favicon（32×32 ≈ 4KB 纹理），400 条的上限会被
        // 「书签」页滚动刷穿、反复清空重取；放宽到 1024 仍只有几 MB。
        if self.cache.len() > 1024 {
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
        let source = icon_source(entry);
        if self
            .tx
            .send(IconJob {
                key: key.clone(),
                source,
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

/// 预览内容缓存(图片缩略图/文本头部),后台线程加载,镜像 IconStore。
struct PreviewStore {
    cache: HashMap<String, Option<PreviewContent>>,
    pending: HashSet<String>,
    tx: std::sync::mpsc::Sender<PreviewJob>,
    rx: std::sync::mpsc::Receiver<(String, Option<crate::preview::PreviewData>)>,
}

enum PreviewContent {
    Image(egui::TextureHandle),
    Text(String),
}

struct PreviewJob {
    key: String,
    entry: Entry,
}

impl PreviewStore {
    fn new() -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<PreviewJob>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("lstr-preview".to_string())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let data = crate::preview::load(&job.entry);
                    if done_tx.send((job.key, data)).is_err() {
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

    /// 后台完成的预览转纹理/文本;有新内容时重绘。
    fn drain(&mut self, ctx: &egui::Context) {
        let mut loaded = false;
        while let Ok((key, data)) = self.rx.try_recv() {
            self.pending.remove(&key);
            let content = data.map(|d| match d {
                crate::preview::PreviewData::Image(img) => PreviewContent::Image(
                    ctx.load_texture(key.clone(), img, egui::TextureOptions::LINEAR),
                ),
                crate::preview::PreviewData::Text(t) => PreviewContent::Text(t),
            });
            self.cache.insert(key, content);
            loaded = true;
        }
        if self.cache.len() > 120 {
            self.cache.clear();
            self.pending.clear();
        }
        if loaded {
            ctx.request_repaint();
        }
    }

    fn request(&mut self, key: String, entry: &Entry) {
        if crate::preview::kind_of(entry) == crate::preview::PreviewKind::None {
            return;
        }
        if self.cache.contains_key(&key) || !self.pending.insert(key.clone()) {
            return;
        }
        if self
            .tx
            .send(PreviewJob { key: key.clone(), entry: entry.clone() })
            .is_err()
        {
            self.pending.remove(&key);
        }
    }

    fn get(&self, key: &str) -> Option<&PreviewContent> {
        self.cache.get(key).and_then(Option::as_ref)
    }
}

fn preview_key(entry: &Entry) -> String {
    format!("preview:{}", entry.full_path().to_lowercase())
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
        // 每份书签按 URL 单独缓存网站 favicon（取不到时后台回退 Edge 图标）。
        Kind::Bookmark => format!("icon:{}", entry.id),
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
/// 「搜索技巧」面板的排版尺寸。绘制与窗口定高共用同一组常量，
/// 避免面板高度和底部横幅定位各算各的。
const HELP_TOP_PAD: f32 = 14.0;
const HELP_TITLE_H: f32 = 24.0;
const HELP_GROUP_HEAD_H: f32 = 21.0;
const HELP_ROW_H: f32 = 19.0;
const HELP_GROUP_GAP: f32 = 8.0;

/// 搜索框左侧条件 chip 的宽度（不绘制）。`label` 为空 = 紧凑模式（只画值）。
/// `dense` = 多个条件时的缩小版（字号/内边距/高度都更小）。
fn condition_chip_width(painter: &egui::Painter, label: &str, value: &str, dense: bool) -> f32 {
    let (label_size, value_size, pad, gap, clear) = if dense {
        (11.0, 11.0, 8.0, 5.0, 14.0)
    } else {
        (12.0, 12.0, 10.0, 6.0, 16.0)
    };
    let label_w = if label.is_empty() {
        0.0
    } else {
        painter
            .layout_no_wrap(label.to_string(), ui::text_font(label_size), Color32::WHITE)
            .size()
            .x
            + gap
            + 1.0
            + gap
    };
    let value_w = painter
        .layout_no_wrap(value.to_string(), ui::mono_font(value_size), Color32::WHITE)
        .size()
        .x;
    pad + label_w + value_w + pad + clear
}

/// chip 在当前类别下是否只是置灰展示（点击/编辑/删除都不响应）。
/// 范围/搜索 chip 本身是「当前模式」，不随条件失效，任何时候都能操作。
fn chip_inactive(kind: ConditionChipKind, conditions_apply: bool) -> bool {
    !conditions_apply
        && !matches!(
            kind,
            ConditionChipKind::Scope | ConditionChipKind::Keyword
        )
}

/// 画一个条件 chip（标签比例字体 + 值等宽字体 + × 命中区），返回
/// `(宽度, 是否点了 ×, 是否点了本体, 是否双击本体)`。日期/大小/盘符共用；
/// 单击定位虚拟光标，双击进入编辑。
///
/// 当前类别下不生效的条件只做置灰展示（`interactive = false`）：不响应
/// 点击/双击/×，hover 也不再高亮，避免「灰着的条件还能被编辑/删掉」。
fn paint_condition_chip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    left: f32,
    top: f32,
    label: &str,
    value: &str,
    color: Color32,
    bg: Color32,
    id: &str,
    tooltip: &str,
    dense: bool,
    interactive: bool,
) -> (f32, bool, bool, bool) {
    let (label_size, value_size, pad, gap, clear_w, chip_h) = if dense {
        (11.0, 11.0, 8.0, 5.0, 14.0, 22.0)
    } else {
        (12.0, 12.0, 10.0, 6.0, 16.0, 24.0)
    };
    let label_galley = (!label.is_empty())
        .then(|| painter.layout_no_wrap(label.to_string(), ui::text_font(label_size), color));
    let value_galley = painter.layout_no_wrap(value.to_string(), ui::mono_font(value_size), color);
    let has_label = label_galley.is_some();
    let label_w = label_galley.as_ref().map(|g| g.size().x).unwrap_or(0.0);
    let chip_w = pad
        + if has_label {
            label_w + gap + 1.0 + gap
        } else {
            0.0
        }
        + value_galley.size().x
        + pad
        + clear_w;
    let chip = Rect::from_min_size(egui::pos2(left, top), egui::vec2(chip_w, chip_h));
    let clear = Rect::from_min_size(
        egui::pos2(chip.right() - clear_w - 4.0, chip.top() + 3.0),
        egui::vec2(clear_w, chip_h - 6.0),
    );
    let body_rect = Rect::from_min_max(chip.min, egui::pos2(clear.left() - 2.0, chip.max.y));
    let pointer = ui.ctx().pointer_hover_pos();
    let body_hover = interactive && pointer.is_some_and(|pos| body_rect.contains(pos));
    let clear_hover = interactive && pointer.is_some_and(|pos| clear.contains(pos));

    // 背景：类型色低透明度；hover 略微加深。边框很淡，靠留白和字色分层。
    let fill = if body_hover {
        Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), bg.a().saturating_add(12))
    } else {
        bg
    };
    painter.rect_filled(chip, CornerRadius::same(8), fill);
    painter.rect_stroke(
        chip,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, color.gamma_multiply(0.22)),
        StrokeKind::Inside,
    );

    let mut x = chip.left() + pad;
    if let Some(label_galley) = &label_galley {
        // 标签弱化（同色相、更低明度），值保持全色，形成主次。
        painter.galley(
            egui::pos2(x, chip.center().y - label_galley.size().y / 2.0),
            label_galley.clone(),
            color.gamma_multiply(0.62),
        );
        x += label_w + gap;
        let divider_h = if dense { 10.0 } else { 12.0 };
        let divider = Rect::from_min_size(
            egui::pos2(x, chip.center().y - divider_h / 2.0),
            egui::vec2(1.0, divider_h),
        );
        painter.rect_filled(divider, CornerRadius::same(1), color.gamma_multiply(0.28));
        x += 1.0 + gap;
    }
    painter.galley(
        egui::pos2(x, chip.center().y - value_galley.size().y / 2.0),
        value_galley,
        color,
    );

    // ×：默认弱化，hover 出现圆形底并变亮。
    if clear_hover {
        painter.circle_filled(clear.center(), 7.0, color.gamma_multiply(0.10));
    }
    painter.text(
        clear.center(),
        Align2::CENTER_CENTER,
        "×",
        ui::mono_font(if dense { 10.0 } else { 11.0 }),
        if clear_hover {
            color
        } else {
            color.gamma_multiply(0.55)
        },
    );

    let sense = if interactive { Sense::click() } else { Sense::hover() };
    let body = ui
        .interact(body_rect, Id::new(("lstr-cond-chip-body", id)), sense)
        .on_hover_text(tooltip.to_string());
    let response = ui
        .interact(clear, Id::new(("lstr-cond-chip-clear", id)), sense)
        .on_hover_text(tooltip.to_string());
    if interactive && (body_hover || clear_hover) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    (
        chip_w,
        response.clicked(),
        body.clicked(),
        body.double_clicked(),
    )
}

/// 条件 chip 的数据源（按键编辑/删除时用来定位 `filters` 里的数据）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConditionChipKind {
    /// 当前类别（`self.scope`），与 Tab 是同一状态的两个视图。
    Scope,
    /// 网络搜索模式（`filters.keyword`，`b ` → Bing）。
    Keyword,
    Date,
    Size,
    /// `filters.path`（`path:D:\work` 提交的目录 chip）。
    Path,
    /// `filters.drives` 的下标。
    Drive(usize),
}

struct ConditionChipInfo {
    kind: ConditionChipKind,
    label: &'static str,
    value: String,
    color: Color32,
    bg: Color32,
    id: String,
    tooltip: String,
    /// 还原到输入框的可编辑文本；None = 区间异常无法还原。
    edit_text: Option<String>,
    /// 还原后需要 `condition_hold`（盘符 `C:` / 范围前缀 / 路径都会立即再次提交）。
    hold_after_edit: bool,
    /// 还原时是否插到输入文本前面（范围前缀必须在前；其它 chip 追加在末尾）。
    prepend_edit: bool,
}

/// 光标左侧最近的 chip 下标。虚拟光标优先；没有虚拟光标时，文本光标在
/// 开头（或输入为空）则 chip 全在文本左侧，最右 chip 就是左邻。
fn left_chip_index(caret: Option<usize>, count: usize, text_at_start: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match caret {
        Some(index) => index.checked_sub(1).filter(|&index| index < count),
        None if text_at_start => Some(count - 1),
        None => None,
    }
}

/// chip 边界光标的 ← 移动：`None`（文本区）= 从最右进入，落在 `count`
/// （最后一个 chip 右侧）；一路左移，最左停在 1（边界 0 没有左邻 chip）。
fn chip_caret_left(caret: Option<usize>, count: usize) -> Option<usize> {
    match caret {
        Some(index) => Some(index.saturating_sub(1).max(1)),
        None => Some(count),
    }
}

/// chip 边界光标的 → 移动：右移一格；已经在 `count` 就回到文本区。
fn chip_caret_right(caret: Option<usize>, count: usize) -> Option<usize> {
    caret.and_then(|index| (index < count).then_some(index + 1))
}

/// 删除第 `removed` 个 chip 后，虚拟光标应落在的位置（chip 数变为 `new_count`）。
/// 边界下标合法范围是 `0..=new_count`（`new_count` = 最后一个 chip 右侧）。
fn caret_after_remove(caret: Option<usize>, removed: usize, new_count: usize) -> Option<usize> {
    if new_count == 0 {
        return None;
    }
    let index = caret?;
    let next = if index > removed { index - 1 } else { index };
    // 边界 0（第一个 chip 左侧）没有可编辑/删除的左邻，光标至少停在 1。
    Some(next.min(new_count).max(1))
}

/// 编辑 chip 后回车要提交的文本：只差终结符的路径条件补一个冒号。
///
/// 用解析器本身判断（补冒号后能否多解析出一条路径条件），这样裸路径、
/// 引号路径、以及「查询词 + 路径 chip」都能覆盖；盘符 `C:` 已带终结符、
/// 范围 `a ` 没有路径条件，都原样返回。`path:` 这种还没给值的也不会
/// 被补成 `path::`（补了同样解析不出来）。
fn condition_enter_text(input: &str) -> String {
    let trimmed = input.trim_end();
    if trimmed.ends_with(':') || trimmed.ends_with('：') {
        return input.to_string();
    }
    let probe = format!("{trimmed}:");
    let before = crate::search::conditions::extract(trimmed).paths.len();
    let after = crate::search::conditions::extract(&probe).paths.len();
    if after > before {
        probe
    } else {
        input.to_string()
    }
}

/// 范围 chip 还原成文本时的前缀（`a ` / `f ` / `>`）；「全部」没有文本形式。
fn scope_prefix_text(scope: Scope) -> &'static str {
    match scope {
        Scope::App => "a ",
        Scope::File => "f ",
        Scope::Command => ">",
        _ => "",
    }
}

/// 路径 chip 值的最大字符数：超长路径只留末尾（tooltip 才显示完整路径，
/// 否则深路径会把输入行挤满）。
const PATH_CHIP_MAX_CHARS: usize = 26;

/// 路径 chip 的显示值：从末尾按字符取预算，尽量在分隔符处下刀。
fn shorten_path_for_chip(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars {
        return path.to_string();
    }
    let tail: String = path.chars().skip(count - max_chars).collect();
    let display = match tail.find(['\\', '/']) {
        Some(pos) if pos + 1 < tail.len() => tail[pos + 1..].to_string(),
        _ => tail,
    };
    format!("…\\{display}")
}

/// 路径 chip 还原成可编辑文本：带空格的路径加引号，保证重新解析一致。
fn path_edit_text(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if text.contains([' ', '\t']) {
        format!("path:\"{text}\"")
    } else {
        format!("path:{text}")
    }
}

/// 去掉条件文本末尾的终结冒号（进入编辑态时用，避免立刻又被收成 chip）。
fn strip_condition_terminator(text: &str) -> &str {
    crate::search::conditions::strip_terminator(text)
}

/// 日期分段输入遮罩（date 条件编辑态）：比较符 + 年/月/日三段，
/// 未输入的段用激活时的「今天」补足并以暗色显示；Tab/←→ 跳段、
/// ← 到比较符段（↑↓ 或直接打 `>`/`<`/`=` 改比较符）、
/// ↑↓ 加减、数字从左覆盖、打满自动跳下一段、回车提交。
#[derive(Clone, Debug)]
struct DateMask {
    /// 字段名（`date` / `日期` / `dm`…），提交时重建条件文本。
    field: String,
    /// 操作符文本（`>` / `>=` / `=`…；空 = 裸值，默认等于）。
    op: String,
    /// 激活时用户输入的原样前缀里比较符之前的部分（`date ` / `date: `）。
    head: String,
    /// 前缀里比较符之后的原样空白（`date > ` 的那个空格）。
    tail: String,
    /// 两个日期的段输入；第二个仅闭区间（`A..B`）使用。
    dates: [DateMaskSegs; 2],
    two: bool,
    /// 该侧是否只有年份（`date>2026`：年过滤，只编辑年段）。
    year_only: [bool; 2],
    /// 从 chip 进入编辑（而非输入框自动激活）：输入非条件字符 = 改搜查询。
    from_chip_edit: bool,
    /// 焦点段下一次数字输入是否整体覆盖旧值（chip 带来的旧值可直接改写）。
    replace_digit: bool,
    /// 焦点：(日期下标, 段下标 0=年 1=月 2=日)。
    date_idx: usize,
    part: usize,
    /// 焦点在比较符段（第一个值段再往左一步）。
    op_focused: bool,
    /// 连续输入的比较符字符（`>` + `=` → `>=`）；其它按键清零。
    op_typing: String,
    /// 默认日期（激活时的今天）。
    default: (i32, u32, u32),
}

#[derive(Clone, Default, Debug)]
struct DateMaskSegs {
    y: String,
    m: String,
    d: String,
}

fn date_mask_part_width(part: usize) -> usize {
    if part == 0 {
        4
    } else {
        2
    }
}

/// 是否是比较符字符（条件字段后还没输入值时，允许先打比较符）。
fn is_condition_op_text(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| matches!(c, '>' | '<' | '=' | '≥' | '≤' | '＞' | '＜' | '＝'))
}

/// 把「字段 + 比较符」的原样前缀拆成比较符前/后两段：比较符永远贴着
/// 前缀末尾（后面只剩空白），拆开后遮罩才能单独绘制并编辑比较符。
fn split_op_prefix(prefix: &str, op: &str) -> (String, String) {
    if op.is_empty() {
        return (prefix.to_string(), String::new());
    }
    match prefix.trim_end().strip_suffix(op) {
        Some(head) => (
            head.to_string(),
            prefix[head.len() + op.len()..].to_string(),
        ),
        None => (prefix.to_string(), String::new()),
    }
}

/// 遮罩值段的统一高度（下划线/点击热区用）。
const MASK_SEG_H: f32 = 24.0;

/// 遮罩前缀绘制：字段原文 + 比较符 + 比较符后的空白。比较符是可选中的
/// 一段（← 到达后 ↑↓ 换档，或直接打 `>`/`<`/`=`），聚焦时琥珀色下划线。
/// 返回 (前缀结束的 x, 比较符是否被点击)。
fn paint_condition_prefix(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    center_y: f32,
    start_x: f32,
    head: &str,
    op: &str,
    tail: &str,
    op_focused: bool,
    id: egui::Id,
    font: &egui::FontId,
    theme: Theme,
) -> (f32, bool) {
    let galley_w = |text: &str| {
        if text.is_empty() {
            return 0.0;
        }
        painter
            .layout_no_wrap(text.to_string(), font.clone(), theme.text)
            .size()
            .x
    };
    let mut x = start_x;
    if !head.is_empty() {
        painter.text(
            egui::pos2(x, center_y),
            Align2::LEFT_CENTER,
            head,
            font.clone(),
            theme.text,
        );
        x += galley_w(head);
    }
    let mut clicked = false;
    if !op.is_empty() {
        let op_w = galley_w(op);
        painter.text(
            egui::pos2(x, center_y),
            Align2::LEFT_CENTER,
            op,
            font.clone(),
            if op_focused { theme.amber } else { theme.text },
        );
        if op_focused {
            let underline_y = center_y + MASK_SEG_H / 2.0 + 2.0;
            painter.line_segment(
                [
                    egui::pos2(x - 2.0, underline_y),
                    egui::pos2(x + op_w + 2.0, underline_y),
                ],
                Stroke::new(1.5_f32, theme.amber),
            );
        }
        clicked = ui
            .interact(
                Rect::from_min_size(
                    egui::pos2(x, center_y - MASK_SEG_H / 2.0),
                    egui::vec2(op_w, MASK_SEG_H),
                ),
                id,
                Sense::click(),
            )
            .clicked();
        x += op_w;
    }
    if !tail.is_empty() {
        painter.text(
            egui::pos2(x, center_y),
            Align2::LEFT_CENTER,
            tail,
            font.clone(),
            theme.text,
        );
        x += galley_w(tail);
    }
    (x, clicked)
}

impl DateMask {
    fn new(field: String, op: String, prefix_display: String) -> Self {
        let (head, tail) = split_op_prefix(&prefix_display, &op);
        Self {
            field,
            op,
            head,
            tail,
            dates: Default::default(),
            two: false,
            year_only: [false; 2],
            from_chip_edit: false,
            replace_digit: true,
            date_idx: 0,
            part: 0,
            op_focused: false,
            op_typing: String::new(),
            default: crate::search::conditions::today_ymd(),
        }
    }

    /// 原始前缀（字段 + 比较符 + 比较符后的空白），比较符改动后跟着变。
    fn prefix(&self) -> String {
        format!("{}{}{}", self.head, self.op, self.tail)
    }

    /// 这一侧要渲染/跳段的段数：年份条件只有年段。
    fn part_count(&self, date_idx: usize) -> usize {
        if self.year_only[date_idx] {
            1
        } else {
            3
        }
    }

    /// 是否还没有任何段输入（自动遮罩刚激活、值还是空的）。
    fn all_typed_empty(&self) -> bool {
        self.dates
            .iter()
            .all(|segs| segs.y.is_empty() && segs.m.is_empty() && segs.d.is_empty())
    }

    fn from_ymd((y, m, d): (i32, u32, u32)) -> DateMaskSegs {
        DateMaskSegs {
            y: format!("{y:04}"),
            m: format!("{m:02}"),
            d: format!("{d:02}"),
        }
    }

    fn typed_str(&self, date_idx: usize, part: usize) -> &String {
        let segs = &self.dates[date_idx];
        match part {
            0 => &segs.y,
            1 => &segs.m,
            _ => &segs.d,
        }
    }

    fn typed_mut(&mut self) -> &mut String {
        let (date_idx, part) = (self.date_idx, self.part);
        match part {
            0 => &mut self.dates[date_idx].y,
            1 => &mut self.dates[date_idx].m,
            _ => &mut self.dates[date_idx].d,
        }
    }

    fn default_digits(&self, part: usize) -> String {
        match part {
            0 => format!("{:04}", self.default.0),
            1 => format!("{:02}", self.default.1),
            _ => format!("{:02}", self.default.2),
        }
    }

    fn part_valid(part: usize, digits: &str) -> bool {
        let Ok(v) = digits.parse::<u32>() else {
            return false;
        };
        match part {
            0 => (1970..=9999).contains(&v),
            1 => (1..=12).contains(&v),
            _ => (1..=31).contains(&v),
        }
    }

    /// 段显示文本：已输入部分（正常色）+ 默认值补尾（暗色）。
    /// 补尾先用默认值对应位，非法时退化为补 0，仍非法（瞬态）用默认整段。
    fn display_part(&self, date_idx: usize, part: usize) -> String {
        let typed = self.typed_str(date_idx, part);
        if typed.is_empty() {
            return self.default_digits(part);
        }
        let width = date_mask_part_width(part);
        if typed.len() >= width {
            return typed.clone();
        }
        let def = self.default_digits(part);
        let mut display = format!("{typed}{}", &def[typed.len()..]);
        if !Self::part_valid(part, &display) {
            display = format!("{typed}{}", &"0".repeat(width - typed.len()));
        }
        if !Self::part_valid(part, &display) {
            display = def;
        }
        display
    }

    /// 段有效值（显示文本解析并夹紧到合法范围）。
    fn effective_part(&self, date_idx: usize, part: usize) -> u32 {
        let v: u32 = self.display_part(date_idx, part).parse().unwrap_or(0);
        match part {
            0 => v.clamp(1970, 9999),
            1 => v.clamp(1, 12),
            _ => v.clamp(1, 31),
        }
    }

    fn effective_date(&self, date_idx: usize) -> (i32, u32, u32) {
        let year = self.effective_part(date_idx, 0) as i32;
        let month = self.effective_part(date_idx, 1);
        let day = self
            .effective_part(date_idx, 2)
            .min(crate::search::conditions::days_in_month_of(year, month));
        (year, month, day)
    }

    /// 输入一位数字：年累加到 4 位自动跳段；月/日首位 ≥2/≥4 直接补 0 跳段，
    /// 第二位非法则拒绝（保持等待）。chip 带入的旧值在焦点段第一次
    /// 输入数字时整体覆盖，不用先退格。
    fn feed_digit(&mut self, c: char) {
        self.op_focused = false;
        self.op_typing.clear();
        let part = self.part;
        let width = date_mask_part_width(part);
        if self.replace_digit {
            self.typed_mut().clear();
            self.replace_digit = false;
        }
        if self.typed_str(self.date_idx, part).len() >= width {
            return;
        }
        match part {
            0 => {
                self.typed_mut().push(c);
                if !self.year_only[self.date_idx] && self.typed_str(self.date_idx, 0).len() == 4 {
                    self.advance();
                }
            }
            _ => {
                if self.typed_str(self.date_idx, part).is_empty() {
                    let d = c.to_digit(10).unwrap_or(0);
                    // 月 2..9 / 日 4..9：只可能是 0X，补 0 立即跳段。
                    let lone_advance = match part {
                        1 => d >= 2,
                        _ => d >= 4,
                    };
                    if lone_advance {
                        *self.typed_mut() = format!("0{c}");
                        self.advance();
                        return;
                    }
                    self.typed_mut().push(c);
                } else {
                    let candidate = format!("{}{}", self.typed_str(self.date_idx, part), c);
                    if Self::part_valid(part, &candidate) {
                        *self.typed_mut() = candidate;
                        self.advance();
                    }
                    // 非法第二位：拒绝，保持等待
                }
            }
        }
    }

    /// 遮罩内直接打比较符（`<` / `=` / `≥`…）：不离开遮罩，就地改比较符。
    /// 闭区间（`A..B` 没有比较符）不接受，调用方按普通查询处理。
    fn feed_op_char(&mut self, c: char) -> bool {
        if self.two {
            return false;
        }
        match crate::search::conditions::extend_op_text(&self.op_typing, c) {
            Some(op) => {
                self.op = op.clone();
                self.op_typing = op;
                true
            }
            None => false,
        }
    }

    fn feed_backspace(&mut self) {
        self.op_typing.clear();
        if self.op_focused {
            // 比较符段没有可删的内容：吞掉按键，不动值。
            return;
        }
        if !self.typed_str(self.date_idx, self.part).is_empty() {
            self.typed_mut().pop();
            return;
        }
        self.retreat();
    }

    fn advance(&mut self) {
        self.op_typing.clear();
        if self.op_focused {
            // 比较符 → 第一个值段。
            self.op_focused = false;
            self.date_idx = 0;
            self.part = 0;
            return;
        }
        let last_date = if self.two { 1 } else { 0 };
        if self.part + 1 < self.part_count(self.date_idx) {
            self.part += 1;
        } else if self.date_idx < last_date {
            self.date_idx += 1;
            self.part = 0;
        } else {
            return;
        }
        self.replace_digit = self.from_chip_edit;
    }

    fn retreat(&mut self) {
        self.op_typing.clear();
        if self.op_focused {
            return;
        }
        // 第一个值段再往左 = 比较符段（闭区间没有比较符，跳过）。
        if self.date_idx == 0 && self.part == 0 && !self.two && !self.op.is_empty() {
            self.op_focused = true;
            return;
        }
        if self.part > 0 {
            self.part -= 1;
        } else if self.date_idx > 0 {
            self.date_idx -= 1;
            self.part = self.part_count(self.date_idx) - 1;
        } else {
            return;
        }
        self.replace_digit = self.from_chip_edit;
    }

    /// ↑↓：段值 ±1（年夹紧、月回绕、日夹紧到当月天数）；比较符段上
    /// 则是切换比较符。
    fn spin(&mut self, delta: i32) {
        if self.op_focused {
            self.op = crate::search::conditions::cycle_op(&self.op, delta).to_string();
            self.op_typing.clear();
            return;
        }
        self.op_typing.clear();
        let (date_idx, part) = (self.date_idx, self.part);
        let current = self.effective_part(date_idx, part) as i32;
        let next = match part {
            0 => (current + delta).clamp(1970, 9999),
            1 => (current - 1 + delta).rem_euclid(12) + 1,
            _ => {
                let (year, month, _) = self.effective_date(date_idx);
                let dim = crate::search::conditions::days_in_month_of(year, month) as i32;
                (current + delta).clamp(1, dim)
            }
        };
        let width = date_mask_part_width(part);
        *self.typed_mut() = format!("{:0width$}", next, width = width);
        self.replace_digit = true;
    }

    fn fmt_date(&self, date_idx: usize) -> String {
        if self.year_only[date_idx] {
            return self.display_part(date_idx, 0);
        }
        format!(
            "{}-{}-{}",
            self.display_part(date_idx, 0),
            self.display_part(date_idx, 1),
            self.display_part(date_idx, 2)
        )
    }

    /// 镜像到输入框的文本（与遮罩显示一致，无终结冒号 → 查询保持暂停）。
    fn mirror_text(&self) -> String {
        let value = if self.two {
            format!("{}..{}", self.fmt_date(0), self.fmt_date(1))
        } else {
            self.fmt_date(0)
        };
        format!("{}{}", self.prefix(), value)
    }

    /// 回退自由文本：只保留用户真正输入过的段（默认补尾不带入）。
    fn partial_text(&self) -> String {
        let fmt = |date_idx: usize| {
            let segs = &self.dates[date_idx];
            let mut out = String::new();
            if !segs.y.is_empty() {
                out.push_str(&segs.y);
                if !segs.m.is_empty() {
                    out.push('-');
                    out.push_str(&segs.m);
                    if !segs.d.is_empty() {
                        out.push('-');
                        out.push_str(&segs.d);
                    }
                }
            } else if !segs.m.is_empty() {
                out.push_str(&segs.m);
            }
            out
        };
        let value = if self.two {
            format!("{}..{}", fmt(0), fmt(1))
        } else {
            fmt(0)
        };
        format!("{}{}", self.prefix(), value)
    }

    /// 提交文本：未输完的段先夹紧固化，再拼规范条件（终结冒号由调用方
    /// 通过 commit_conditions 消费）。年份条件提交成年份文本（不是整日）。
    fn commit_text(&self) -> String {
        let fmt = |date_idx: usize| {
            if self.year_only[date_idx] {
                format!("{:04}", self.effective_part(date_idx, 0))
            } else {
                date_mask_fmt(self.effective_date(date_idx))
            }
        };
        let value = if self.two {
            format!("{}..{}", fmt(0), fmt(1))
        } else {
            fmt(0)
        };
        if self.op.is_empty() {
            format!("{} {}:", self.field, value)
        } else {
            format!("{}{}{}:", self.field, self.op, value)
        }
    }
}

fn date_mask_fmt((y, m, d): (i32, u32, u32)) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

/// 把日期 chip 编辑文本里的一个值填进遮罩某一侧：完整日期 → 三段；
/// 年份条件 → 只留年段（整年语义，不能展开成具体某天）。
fn apply_date_edit_value(
    mask: &mut DateMask,
    date_idx: usize,
    value: crate::search::conditions::DateEditValue,
) {
    use crate::search::conditions::DateEditValue;
    match value {
        DateEditValue::Day(ymd) => mask.dates[date_idx] = DateMask::from_ymd(ymd),
        DateEditValue::Year(year) => {
            mask.dates[date_idx] = DateMaskSegs {
                y: format!("{year:04}"),
                m: String::new(),
                d: String::new(),
            };
            mask.year_only[date_idx] = true;
        }
    }
}

/// 大小分段输入遮罩：比较符 + 数字段 + 单位段（B/KB/MB/GB/TB），
/// 交互与日期遮罩一致：Tab/←→ 跳段、← 到比较符段（↑↓ 或直接打
/// `>`/`<`/`=` 改比较符）、↑↓ 加减（数字 ±1、单位换档）、数字从左覆盖
/// 默认值、回车提交。
#[derive(Clone, Debug)]
struct SizeMask {
    field: String,
    op: String,
    /// 激活时用户输入的原样前缀里比较符之前的部分（`size` / `size:`）。
    head: String,
    /// 前缀里比较符之后的原样空白（`size > ` 的那个空格）。
    tail: String,
    sizes: [SizeSegs; 2],
    two: bool,
    idx: usize,
    /// 0 = 数字段，1 = 单位段。
    part: usize,
    /// 从 chip 进入编辑（而非输入框自动激活）：输入非条件字符 = 改搜查询。
    from_chip_edit: bool,
    /// 数字段下一次数字输入是否整体覆盖旧值（chip 带来的旧值可直接改写）。
    replace_digit: bool,
    /// 连续输入的单位字母（`G` → GB、`GB` → GB）。
    unit_typing: String,
    /// 焦点在比较符段（第一个数字段再往左一步）。
    op_focused: bool,
    /// 连续输入的比较符字符（`>` + `=` → `>=`）；其它按键清零。
    op_typing: String,
}

#[derive(Clone, Debug)]
struct SizeSegs {
    number: String,
    /// `SIZE_UNIT_LABELS` 下标，默认 MB(2)。
    unit: usize,
}

fn default_size_segs() -> SizeSegs {
    SizeSegs {
        number: String::new(),
        unit: 2,
    }
}

impl SizeMask {
    fn new(field: String, op: String, prefix_display: String) -> Self {
        let (head, tail) = split_op_prefix(&prefix_display, &op);
        Self {
            field,
            op,
            head,
            tail,
            sizes: [default_size_segs(), default_size_segs()],
            two: false,
            idx: 0,
            part: 0,
            from_chip_edit: false,
            replace_digit: true,
            unit_typing: String::new(),
            op_focused: false,
            op_typing: String::new(),
        }
    }

    /// 原始前缀（字段 + 比较符 + 比较符后的空白），比较符改动后跟着变。
    fn prefix(&self) -> String {
        format!("{}{}{}", self.head, self.op, self.tail)
    }

    fn from_parts((number, unit): (String, usize)) -> SizeSegs {
        SizeSegs {
            number,
            unit: unit.min(4),
        }
    }

    fn number(&self) -> &String {
        &self.sizes[self.idx].number
    }

    fn number_mut(&mut self) -> &mut String {
        let idx = self.idx;
        &mut self.sizes[idx].number
    }

    fn display_number(&self, idx: usize) -> String {
        if self.sizes[idx].number.is_empty() {
            "10".to_string()
        } else {
            self.sizes[idx].number.clone()
        }
    }

    fn unit_label(&self, idx: usize) -> &'static str {
        crate::search::conditions::SIZE_UNIT_LABELS[self.sizes[idx].unit.min(4)]
    }

    fn effective_number(&self, idx: usize) -> f64 {
        self.display_number(idx).parse().unwrap_or(10.0)
    }

    fn feed_digit(&mut self, c: char) {
        self.op_focused = false;
        self.unit_typing.clear();
        self.op_typing.clear();
        if self.part == 1 {
            // 焦点在单位时输入数字：自动回到数字段。
            self.part = 0;
        }
        if self.replace_digit {
            self.number_mut().clear();
            self.replace_digit = false;
        }
        if c == '.' && (self.number().is_empty() || self.number().contains('.')) {
            return;
        }
        if self.number().len() < 12 {
            self.number_mut().push(c);
        }
    }

    /// 遮罩内直接打比较符（`<` / `=` / `≥`…）：不离开遮罩，就地改比较符。
    /// 闭区间（`A..B` 没有比较符）不接受，调用方按普通查询处理。
    fn feed_op_char(&mut self, c: char) -> bool {
        if self.two {
            return false;
        }
        match crate::search::conditions::extend_op_text(&self.op_typing, c) {
            Some(op) => {
                self.op = op.clone();
                self.op_typing = op;
                true
            }
            None => false,
        }
    }

    /// 单位段输入字母：`G` → GB、`GB` → GB；不认识的字母返回 false，
    /// 由调用方转成普通查询（编辑态不接受任意字符）。
    fn feed_unit_letter(&mut self, c: char) -> bool {
        self.op_focused = false;
        self.op_typing.clear();
        let lower = c.to_ascii_lowercase();
        let candidate = format!("{}{}", self.unit_typing, lower);
        if let Some(unit) = crate::search::conditions::size_unit_index(&candidate) {
            self.unit_typing = candidate;
            self.sizes[self.idx].unit = unit;
            self.part = 1;
            return true;
        }
        if let Some(unit) = crate::search::conditions::size_unit_index(&lower.to_string()) {
            self.unit_typing = lower.to_string();
            self.sizes[self.idx].unit = unit;
            self.part = 1;
            return true;
        }
        false
    }

    fn feed_backspace(&mut self) {
        self.unit_typing.clear();
        self.op_typing.clear();
        if self.op_focused {
            // 比较符段没有可删的内容：吞掉按键，不动值。
            return;
        }
        if !self.number().is_empty() {
            self.number_mut().pop();
            return;
        }
        self.retreat();
    }

    fn advance(&mut self) {
        self.op_typing.clear();
        if self.op_focused {
            // 比较符 → 数字段。
            self.op_focused = false;
            self.idx = 0;
            self.part = 0;
            return;
        }
        let last = if self.two { 3 } else { 1 };
        let pos = self.idx * 2 + self.part;
        if pos < last {
            let next = pos + 1;
            self.idx = next / 2;
            self.part = next % 2;
            self.replace_digit = self.from_chip_edit;
            self.unit_typing.clear();
        }
    }

    fn retreat(&mut self) {
        self.op_typing.clear();
        if self.op_focused {
            return;
        }
        // 第一个数字段再往左 = 比较符段（闭区间没有比较符，跳过）。
        if self.idx == 0 && self.part == 0 && !self.two && !self.op.is_empty() {
            self.op_focused = true;
            return;
        }
        let pos = self.idx * 2 + self.part;
        if pos > 0 {
            let prev = pos - 1;
            self.idx = prev / 2;
            self.part = prev % 2;
            self.replace_digit = self.from_chip_edit;
            self.unit_typing.clear();
        }
    }

    fn spin(&mut self, delta: i32) {
        self.unit_typing.clear();
        self.op_typing.clear();
        if self.op_focused {
            self.op = crate::search::conditions::cycle_op(&self.op, delta).to_string();
            return;
        }
        self.replace_digit = true;
        if self.part == 1 {
            let unit = self.sizes[self.idx].unit as i32 + delta;
            self.sizes[self.idx].unit = unit.clamp(0, 4) as usize;
            return;
        }
        let mut value = self.effective_number(self.idx) + delta as f64;
        if value < 0.0 {
            value = 0.0;
        }
        let text = if (value.fract()).abs() < 1e-9 {
            format!("{}", value as u64)
        } else {
            format!("{value:.2}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        };
        self.sizes[self.idx].number = text;
    }

    fn part_text(&self, idx: usize) -> String {
        format!("{}{}", self.display_number(idx), self.unit_label(idx))
    }

    fn mirror_text(&self) -> String {
        if self.two {
            format!("{}{}..{}", self.prefix(), self.part_text(0), self.part_text(1))
        } else {
            format!("{}{}", self.prefix(), self.part_text(0))
        }
    }

    /// 退出遮罩转自由文本时的已输入部分。用户继续输入单位字母（`10M`）时
    /// 不要带默认单位，否则会拼成 `10MBM`；其它字符带上当前单位。
    fn partial_text_for_exit(&self, exit_text: &str) -> String {
        let include_unit = !exit_text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
        self.partial_text_with_unit(include_unit)
    }

    fn partial_text_with_unit(&self, include_unit: bool) -> String {
        let fmt = |idx: usize| {
            if self.sizes[idx].number.is_empty() {
                String::new()
            } else if include_unit {
                format!("{}{}", self.sizes[idx].number, self.unit_label(idx))
            } else {
                self.sizes[idx].number.clone()
            }
        };
        if self.two {
            format!("{} {}..{}", self.field, fmt(0), fmt(1))
        } else {
            // 用激活时的原样前缀（`size:` / `size >= `），比较符改动和
            // 紧凑写法的冒号都不丢。
            format!("{}{}", self.prefix(), fmt(0))
        }
    }

    /// 是否还没有任何数字输入（自动遮罩刚激活）。
    fn typed_empty(&self) -> bool {
        self.sizes.iter().all(|segs| segs.number.is_empty())
    }

    fn commit_text(&self) -> String {
        format!("{}:", self.mirror_text())
    }
}

/// 条件操作撤销/重做用的一份状态快照。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConditionSnapshot {
    filters: SearchFilters,
    input: String,
    chip_caret: Option<usize>,
    /// 范围也是 chip 的一种（可删/可改），撤回时要一起还原。
    scope: Scope,
}

/// 条件操作的 undo/redo 栈：提交/删除/编辑 chip、盘符还原路径、清空筛选
/// 都会先记录一份快照。栈满 64 条，连续相同状态不重复入栈。
#[derive(Default)]
struct ConditionHistory {
    undo: Vec<ConditionSnapshot>,
    redo: Vec<ConditionSnapshot>,
}

impl ConditionHistory {
    const LIMIT: usize = 64;

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn record(&mut self, snapshot: ConditionSnapshot) {
        // 新操作让 redo 失效；重复状态（例如同一帧多次提交）不重复入栈。
        self.redo.clear();
        if self.undo.last() == Some(&snapshot) {
            return;
        }
        if self.undo.len() >= Self::LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(snapshot);
    }

    fn undo(&mut self, current: ConditionSnapshot) -> Option<ConditionSnapshot> {
        let previous = self.undo.pop()?;
        if self.redo.len() >= Self::LIMIT {
            self.redo.remove(0);
        }
        self.redo.push(current);
        Some(previous)
    }

    fn redo(&mut self, current: ConditionSnapshot) -> Option<ConditionSnapshot> {
        let next = self.redo.pop()?;
        if self.undo.len() >= Self::LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(current);
        Some(next)
    }

    /// 用户主动取消编辑时，把刚压入的「进入编辑」快照撤掉，
    /// 避免 Ctrl+Z 又回到同一个编辑态。
    fn forget_last(&mut self, filters: &SearchFilters, input: &str, scope: Scope) {
        if self.undo.last().is_some_and(|snapshot| {
            &snapshot.filters == filters && snapshot.input == input && snapshot.scope == scope
        }) {
            self.undo.pop();
        }
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// 空查询帮助面板的内容高度（相对列表顶部）：宽窗口两列，以较高的左列
/// 为准；窄窗口退化为单列顺排，两列内容相加。
fn help_panel_height(width: f32) -> f32 {
    let group = |rows: f32| HELP_GROUP_HEAD_H + rows * HELP_ROW_H + HELP_GROUP_GAP;
    // 左列：位置 5 行（盘符 2 + 路径 2 + 分段）/ 筛选 6 行（日期、大小、
    // 条件编辑、撤回…）/ 范围与搜索 4 行。行数必须与 `syntax` 表一致，否则末行被裁。
    let left = group(5.0) + group(6.0) + group(4.0);
    let head = HELP_TOP_PAD + HELP_TITLE_H;
    if width >= 560.0 {
        head + left
    } else {
        head + left + group(4.0) + group(5.0)
    }
}

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
    /// 正则模式下的编译错误（不触发搜索，单独提示）。
    regex_error: Option<String>,
    /// 输入框日期/大小条件解析错误（非法日期/单位/空区间等，保留原文不提交）。
    condition_error: Option<String>,
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
    /// 热键服务为空时下次重试的时刻（钩子线程死掉/安装失败后自愈用）。
    hotkey_retry_at: Option<Instant>,
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
    /// 「加载更多」请求在途：响应到达时保留当前滚动位置与选中项。
    more_pending: bool,
    /// 本次查询已加载到后端给不出更多（或到上限），滚到底不再空转。
    more_exhausted: bool,
    /// 筛选条选择：位置 / 时间 / 类型（按钮操作，不输入查询语法）。
    filters: SearchFilters,
    /// 打开中的筛选下拉菜单。
    filter_menu: Option<FilterMenuKind>,
    /// 输入框条件 chip 的虚拟光标：`Some(i)` = 光标在第 i 个 chip 左侧
    /// （0..chip 数）；None = 光标在文本里。
    chip_caret: Option<usize>,
    /// 编辑盘符 chip 时暂停查询：还原出的 `C:` 不能立刻又被收成 chip。
    condition_hold: bool,
    /// 用户 Esc 取消过的联想输入串；以该串开头时不再自动弹。
    suggest_dismissed: Option<String>,
    /// 联想候选的键盘选中项。
    suggest_index: usize,
    /// 条件编辑前的 filters/输入/范围快照：Esc 取消时恢复。
    condition_edit_backup: Option<(SearchFilters, String, Scope)>,
    /// 条件操作的撤销/重做栈（Ctrl+Z / Ctrl+Y / Ctrl+Shift+Z）。
    condition_history: ConditionHistory,
    /// 日期分段输入遮罩（date 条件的年/月/日结构化编辑）。
    date_mask: Option<DateMask>,
    /// 大小分段输入遮罩（size 条件的数字/单位结构化编辑）。
    size_mask: Option<SizeMask>,
    /// 输入历史召回：最近输入串缓存（最新在前）+ 当前游标 + 召回前草稿。
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    /// 删除确认（结果菜单「删除到回收站」）。
    pending_delete: Option<PendingDelete>,
    /// 上一次删除（可一键撤回）。
    last_deleted: Option<DeletedItem>,
    /// 当前打开的菜单里，选中条目是否在「最近使用」历史中。
    menu_in_history: bool,
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
    /// 预览面板内容缓存(图片缩略图/文本头部)。
    previews: PreviewStore,
    /// 学习徽标缓存(按当前查询词),让自学习在结果行可见。
    learn_for: String,
    learn_badges: HashMap<String, crate::history::LearnBadge>,
    /// 书签 id → 收藏夹层级（「收藏夹栏 › 工作 › 需求」），随每次响应刷新。
    bookmark_folders: HashMap<String, String>,
    /// 产生当前结果列表的查询词（请求发出时的输入，条件已剥离）。
    /// 结果行的「拼音 xx / 模糊 xx / 关键词高亮」必须用它：条件编辑/联想
    /// 暂停期间输入框里是半截条件，拿它当查询词会把 `size >= …` 标成拼音命中。
    results_query_text: String,
    /// 学习数据页视图:false=按词学习,true=常用排行。
    learning_view_frecency: bool,
    /// 已生效的排除规则表(「立即重建」时更新;与当前配置比对决定是否提示重建)。
    noise_exclude_applied: Vec<String>,
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
        // 已生效排除表:与当前配置比对决定「立即重建」按钮是否出现。
        let noise_exclude_applied = cfg
            .rank
            .noise_rules
            .iter()
            .filter(|r| r.exclude)
            .map(|r| r.pattern.clone())
            .collect();

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
            condition_error: None,
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
            hotkey_retry_at: None,
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
            more_pending: false,
            more_exhausted: false,
            filters: SearchFilters::default(),
            filter_menu: None,
            chip_caret: None,
            condition_hold: false,
            suggest_dismissed: None,
            suggest_index: 0,
            condition_edit_backup: None,
            condition_history: ConditionHistory::default(),
            date_mask: None,
            size_mask: None,
            input_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            pending_delete: None,
            last_deleted: None,
            menu_in_history: false,
            dialog_retry_until: None,
            settings_tab: SettingsTab::Appearance,
            ime_composing: false,
            last_width: 0.0,
            center_frames: 0,
            suppress_hide_until: Instant::now(),
            icons: IconStore::new(),
            previews: PreviewStore::new(),
            learn_for: String::new(),
            learn_badges: HashMap::new(),
            bookmark_folders: HashMap::new(),
            results_query_text: String::new(),
            learning_view_frecency: false,
            noise_exclude_applied,
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
        // 钩子线程意外退出后服务永久哑火（只有重启应用能恢复），这里就地重装。
        self.revive_hotkey_if_dead(ctx);
        for event in events {
            match event {
                HotkeyEvent::Toggle => {
                    if relay_pending {
                        relay_pending = false;
                    } else {
                        lstr_index::snapshot::debug_log(&format!(
                            "hotkey: toggle hidden={}",
                            self.hidden
                        ));
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

    /// 热键服务自愈：钩子线程一旦退出，channel 永久断开，双击 Ctrl 再也
    /// 收不到。发现即重装；装不上（极少见，比如瞬间资源紧张）就 5s 后再试，
    /// 不让「一次失败」变成「重启应用才恢复」。
    fn revive_hotkey_if_dead(&mut self, ctx: &egui::Context) {
        if self.hotkey.as_ref().is_some_and(HotkeyService::is_dead) {
            lstr_index::snapshot::debug_log("hotkey: service dead, restarting");
            // 先丢掉死服务再装新的：Drop 会 join 已退出的线程，不会挂住。
            self.hotkey = None;
            self.hotkey_retry_at = Some(Instant::now());
        }
        if self.hotkey.is_some() {
            self.hotkey_retry_at = None;
            return;
        }
        let retry_due = self
            .hotkey_retry_at
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(5));
        if !retry_due {
            return;
        }
        match HotkeyService::start(ctx.clone(), &self.cfg.hotkey) {
            Ok(service) => {
                lstr_index::snapshot::debug_log("hotkey: service restarted");
                self.hotkey = Some(service);
                self.hotkey_retry_at = None;
            }
            Err(err) => {
                lstr_index::snapshot::debug_log(&format!("hotkey: restart failed: {err}"));
                self.hotkey_retry_at = Some(Instant::now());
            }
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
        // 切到正则/PathTyping/应用页时日期条件不生效，残留的错误条也不能留着。
        if !self.conditions_apply(self.scope) {
            self.condition_error = None;
        }
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
        // 日期条件已写错（例如 2026-13-01）：不把半截条件当文本搜，
        // 结果区只留错误条；用户改对/删掉后由下一次 trigger 恢复。
        if self.condition_error.is_some() {
            self.dirty_at = None;
            self.waiting = false;
            self.entries.clear();
            self.total = 0;
            self.pending_gen = 0;
        }
        if let Some(deadline) = self.dirty_at {
            if Instant::now() >= deadline {
                let base_folder = if self.typing_global {
                    None
                } else {
                    self.typing_folder.clone()
                };
                // 这批结果对应的查询词（条件已剥离）。结果行标注/高亮用它，
                // 避免条件编辑期间把 `size >= …` 半截条件当拼音查询词。
                // 相对路径分段查询（`目录\名字`）用名字锚点做高亮。
                self.results_query_text = crate::search::query::parse(&self.input, self.scope)
                    .path_anchor()
                    .map(str::to_string)
                    .unwrap_or_else(|| self.input.clone());
                // 输入历史：只记真正发出去的查询（浏览目录/路径模式不算）。
                if self.browse_root.is_none() && !self.path_mode() {
                    if let Ok(mut history) = self.history.lock() {
                        history.record_input(&self.input);
                    }
                }
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
                let prev_len = self.entries.len();
                let was_more = self.more_pending;
                self.more_pending = false;
                self.entries = response.entries;
                self.bookmark_folders = self.service.bookmark_folders();
                self.total = response.total;
                self.elapsed_ms = response.elapsed_ms;
                self.error = response.error;
                if was_more {
                    // 加载更多：保留滚动位置与选中项，让用户接着往下扫。
                    // 后端给不出新条目（去重后数量没涨）= 已经到底，停止续拉。
                    lstr_index::snapshot::debug_log(&format!(
                        "ui: load more gen={} entries={} total={}",
                        response.generation,
                        self.entries.len(),
                        self.total
                    ));
                    if self.entries.len() <= prev_len {
                        self.more_exhausted = true;
                    }
                } else {
                    self.scroll_y = 0.0;
                }
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

    /// 日期/大小/路径等条件是否在当前上下文解析：正则、PathTyping、应用/命令/
    /// 书签页、以及网络搜索模式都不解析；浏览模式生效（与时间/类型筛选一致）。
    fn conditions_apply(&self, scope: Scope) -> bool {
        !self.filters.matches.regex
            && !self.path_mode()
            // 搜索模式：输入框里的一切都是搜索词（包括 `date>…`、`a ` 这些）。
            && self.filters.keyword.is_none()
            && !matches!(scope, Scope::App | Scope::Bookmark | Scope::Command)
    }

    /// 输入框条件 chip 的描述列表（绘制与按键处理共用同一顺序）：
    /// 时间 → 大小 → 盘符（按加入顺序）。
    fn condition_chips(&self) -> Vec<ConditionChipInfo> {
        let theme = self.theme;
        let scope_color = if theme.light {
            Color32::from_rgb(86, 96, 150)
        } else {
            Color32::from_rgb(168, 180, 232)
        };
        let size_color = if theme.light {
            Color32::from_rgb(46, 112, 142)
        } else {
            Color32::from_rgb(118, 188, 210)
        };
        let path_color = if theme.light {
            Color32::from_rgb(108, 84, 160)
        } else {
            Color32::from_rgb(196, 172, 240)
        };
        let drive_color = if theme.light {
            Color32::from_rgb(46, 122, 82)
        } else {
            Color32::from_rgb(120, 196, 154)
        };
        let mut chips = Vec::new();
        // 范围 chip：与 Tab 是同一状态（self.scope）的两个视图；「全部」不显示。
        if self.scope != Scope::All {
            chips.push(ConditionChipInfo {
                kind: ConditionChipKind::Scope,
                label: "范围",
                value: self.scope.label().to_string(),
                color: scope_color,
                bg: Color32::from_rgba_unmultiplied(
                    scope_color.r(),
                    scope_color.g(),
                    scope_color.b(),
                    34,
                ),
                id: "scope".into(),
                tooltip: format!(
                    "当前类别：{}；双击改回文本，点 × 回到全部",
                    self.scope.label()
                ),
                edit_text: Some(scope_prefix_text(self.scope).to_string()),
                hold_after_edit: true,
                prepend_edit: true,
            });
        }
        // 搜索 chip：网络搜索关键字（`b ` → Bing）。颜色沿用关键字结果行的橙色。
        if let Some(trigger) = &self.filters.keyword {
            let name = self
                .cfg
                .keywords
                .iter()
                .find(|kw| kw.trigger.eq_ignore_ascii_case(trigger))
                .map(|kw| kw.name.clone())
                .unwrap_or_else(|| trigger.clone());
            let keyword_color = if theme.light {
                Color32::from_rgb(150, 92, 20)
            } else {
                Color32::from_rgb(230, 170, 90)
            };
            chips.push(ConditionChipInfo {
                kind: ConditionChipKind::Keyword,
                label: "搜索",
                value: name.clone(),
                color: keyword_color,
                bg: Color32::from_rgba_unmultiplied(
                    keyword_color.r(),
                    keyword_color.g(),
                    keyword_color.b(),
                    34,
                ),
                id: "keyword".into(),
                tooltip: format!(
                    "网络搜索：{name}（关键字 {trigger}）；双击改回文本，点 × 移除"
                ),
                edit_text: Some(format!("{trigger} ")),
                hold_after_edit: true,
                prepend_edit: true,
            });
        }
        if let Some(range) = self.filters.modified.as_ref().filter(|r| !r.is_empty()) {
            // tooltip 附带换算后的绝对日期：别名条件（今天/本周/近7天）
            // 能直接看到真实区间。
            let tooltip = match crate::search::conditions::date_range_text(range) {
                Some(text) => format!("修改时间 {text}（本地时间）；双击编辑，点 × 移除"),
                None => "修改时间条件（本地时间）；双击编辑，点 × 移除".into(),
            };
            chips.push(ConditionChipInfo {
                kind: ConditionChipKind::Date,
                label: "时间",
                value: range.label(),
                color: theme.amber,
                bg: Color32::from_rgba_unmultiplied(
                    theme.amber.r(),
                    theme.amber.g(),
                    theme.amber.b(),
                    34,
                ),
                id: "date".into(),
                tooltip,
                edit_text: crate::search::conditions::source_edit_text(&range.sources)
                    .or_else(|| {
                        crate::search::conditions::date_edit_text(range)
                            .map(|text| strip_condition_terminator(&text).to_string())
                    }),
                hold_after_edit: false,
                prepend_edit: false,
            });
        }
        if let Some(range) = self.filters.size.as_ref().filter(|r| !r.is_empty()) {
            chips.push(ConditionChipInfo {
                kind: ConditionChipKind::Size,
                label: "大小",
                value: range.label(),
                color: size_color,
                bg: Color32::from_rgba_unmultiplied(size_color.r(), size_color.g(), size_color.b(), 34),
                id: "size".into(),
                tooltip: "文件大小条件；双击编辑，点 × 移除".into(),
                edit_text: crate::search::conditions::source_edit_text(&range.sources)
                    .or_else(|| {
                        crate::search::conditions::size_edit_text(range)
                            .map(|text| strip_condition_terminator(&text).to_string())
                    }),
                hold_after_edit: false,
                prepend_edit: false,
            });
        }
        // 路径 chip：输入框提交的目录条件（引擎递归搜子树）。
        if let Some(path) = &self.filters.path {
            let full = path.display().to_string();
            chips.push(ConditionChipInfo {
                kind: ConditionChipKind::Path,
                label: "路径",
                value: shorten_path_for_chip(&full, PATH_CHIP_MAX_CHARS),
                color: path_color,
                bg: Color32::from_rgba_unmultiplied(
                    path_color.r(),
                    path_color.g(),
                    path_color.b(),
                    34,
                ),
                id: "path".into(),
                tooltip: format!("目录：{full}；双击编辑，点 × 移除"),
                edit_text: Some(path_edit_text(path)),
                hold_after_edit: true,
                prepend_edit: false,
            });
        }
        for (index, drive) in self.filters.drives.iter().enumerate() {
            chips.push(ConditionChipInfo {
                kind: ConditionChipKind::Drive(index),
                // 盘符 chip 直接显示「C盘 / 排除C盘」，不再带「盘符」标签。
                label: "",
                value: drive.label(),
                color: drive_color,
                bg: Color32::from_rgba_unmultiplied(
                    drive_color.r(),
                    drive_color.g(),
                    drive_color.b(),
                    34,
                ),
                id: format!("drive-{index}"),
                tooltip: if drive.exclude {
                    format!("排除 {} 盘；双击编辑，点 × 移除", drive.letter)
                } else {
                    format!("只搜 {} 盘；双击编辑，点 × 移除", drive.letter)
                },
                edit_text: Some(drive.source()),
                hold_after_edit: true,
                prepend_edit: false,
            });
        }
        chips
    }

    /// 输入框里是否有任何 chip（范围也算；← 进 chip 区、页脚提示都用它）。
    fn has_input_chips(&self) -> bool {
        self.scope != Scope::All || self.filters.has_input_conditions()
    }

    /// 当前应该弹出的条件联想候选；Esc 取消过的前缀不再弹。
    fn condition_suggestions(&self) -> Vec<crate::search::conditions::Suggestion> {
        if !self.conditions_apply(self.scope)
            || self.ime_composing
            || self.chip_caret.is_some()
            || self.settings_open
        {
            return Vec::new();
        }
        let trimmed = self.input.trim();
        if let Some(dismissed) = &self.suggest_dismissed {
            if !dismissed.is_empty() && trimmed.starts_with(dismissed.as_str()) {
                return Vec::new();
            }
        }
        crate::search::conditions::suggestions(&self.input)
    }

    /// 接受联想候选：把输入换成 `size>` / `date>`，光标到末尾并继续暂停查询。
    fn accept_suggestion(&mut self, ctx: &egui::Context) {
        let suggestions = self.condition_suggestions();
        if suggestions.is_empty() {
            return;
        }
        let index = self.suggest_index.min(suggestions.len() - 1);
        let fill = suggestions[index].fill.clone();
        self.input = fill;
        self.chip_caret = None;
        self.condition_hold = false;
        self.date_mask = None;
        self.size_mask = None;
        self.suggest_dismissed = None;
        self.suggest_index = 0;
        self.reset_history_recall();
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
        self.trigger(ctx);
    }

    /// Esc 取消联想：当前串不再弹，立即恢复搜索。
    fn dismiss_suggestion(&mut self, ctx: &egui::Context) {
        let trimmed = self.input.trim().to_string();
        if !trimmed.is_empty() {
            self.suggest_dismissed = Some(trimmed);
        }
        self.suggest_index = 0;
        self.trigger(ctx);
    }

    /// 文本光标是否停在输入框最开头（输入为空也算）。
    fn text_at_start(&self, ctx: &egui::Context) -> bool {
        if self.input.is_empty() {
            return true;
        }
        let id = egui::Id::new("lstr-search-input");
        egui::widgets::text_edit::TextEditState::load(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .is_some_and(|range| range.primary.index == 0 && range.secondary.index == 0)
    }

    /// chip 在当前类别下只是置灰展示（条件不生效）：不可点、不可编辑/删除。
    /// 范围/搜索 chip 本身是模式，任何时候都能操作。
    fn chip_inactive(&self, index: usize) -> bool {
        let Some(chip) = self.condition_chips().into_iter().nth(index) else {
            return true;
        };
        chip_inactive(chip.kind, self.conditions_apply(self.scope))
    }

    /// 光标左侧最近的条件 chip 下标（编辑用：虚拟光标优先，否则最右 chip）。
    fn condition_edit_target(&self) -> Option<usize> {
        let count = self.condition_chips().len();
        left_chip_index(self.chip_caret, count, true).filter(|&index| !self.chip_inactive(index))
    }

    /// Ctrl+Backspace 的删除目标：虚拟光标左侧 chip；否则文本在开头时删最右 chip。
    fn condition_delete_target(&self, ctx: &egui::Context) -> Option<usize> {
        let count = self.condition_chips().len();
        left_chip_index(self.chip_caret, count, self.text_at_start(ctx))
            .filter(|&index| !self.chip_inactive(index))
    }

    /// 删除第 `index` 个条件 chip，并修正虚拟光标。
    fn remove_chip(&mut self, index: usize) -> bool {
        let kind = self.condition_chips().get(index).map(|chip| chip.kind);
        match kind {
            // 移除范围 chip = 回到「全部」页。
            Some(ConditionChipKind::Scope) => self.scope = Scope::All,
            // 移除搜索 chip = 退出网络搜索模式。
            Some(ConditionChipKind::Keyword) => self.filters.keyword = None,
            Some(ConditionChipKind::Date) => self.filters.modified = None,
            Some(ConditionChipKind::Size) => self.filters.size = None,
            Some(ConditionChipKind::Path) => self.filters.path = None,
            Some(ConditionChipKind::Drive(drive)) => {
                if drive >= self.filters.drives.len() {
                    return false;
                }
                self.filters.drives.remove(drive);
            }
            None => return false,
        }
        let new_count = self.condition_chips().len();
        self.chip_caret = caret_after_remove(self.chip_caret, index, new_count);
        true
    }

    /// 把第 `index` 个 chip 还原成可编辑文本，进入编辑态（Esc 可撤销）。
    /// 日期 chip 走分段遮罩（年/月/日，输入框为空时）；其它 chip 还原为
    /// 等价文本自由编辑。
    fn edit_chip(&mut self, index: usize, ctx: &egui::Context) -> bool {
        let Some(chip) = self.condition_chips().into_iter().nth(index) else {
            return false;
        };
        let Some(text) = chip.edit_text else {
            return false;
        };
        if self.condition_edit_backup.is_none() {
            self.condition_edit_backup =
                Some((self.filters.clone(), self.input.clone(), self.scope));
        }
        self.record_condition_undo();
        if !self.remove_chip(index) {
            return false;
        }
        // 日期 chip 且输入框没有其它文本：进入分段遮罩（年份条件只编辑年段）。
        if matches!(chip.kind, ConditionChipKind::Date) && self.input.trim().is_empty() {
            if let Some((field, op, first, second)) =
                crate::search::conditions::parse_date_edit_parts(&text)
            {
                let prefix = if op.is_empty() {
                    format!("{field} ")
                } else {
                    format!("{field} {op} ")
                };
                let mut mask = DateMask::new(field, op, prefix);
                mask.from_chip_edit = true;
                apply_date_edit_value(&mut mask, 0, first);
                if let Some(second) = second {
                    apply_date_edit_value(&mut mask, 1, second);
                    mask.two = true;
                }
                self.date_mask = Some(mask);
                self.size_mask = None;
                self.chip_caret = None;
                self.condition_hold = false;
                self.suggest_dismissed = None;
                Self::reset_input_cursor(ctx);
                self.focus_requested = true;
                self.sync_date_mask(ctx);
                return true;
            }
        }
        // 大小 chip 且输入框没有其它文本：数字/单位分段遮罩。
        if matches!(chip.kind, ConditionChipKind::Size) && self.input.trim().is_empty() {
            if let Some((op, first, second)) =
                crate::search::conditions::parse_size_edit_parts(&text)
            {
                let prefix = if op.is_empty() {
                    "size ".to_string()
                } else {
                    format!("size{op}")
                };
                let mut mask = SizeMask::new("size".into(), op, prefix);
                mask.from_chip_edit = true;
                mask.sizes[0] = SizeMask::from_parts(first);
                if let Some(second) = second {
                    mask.sizes[1] = SizeMask::from_parts(second);
                    mask.two = true;
                }
                self.size_mask = Some(mask);
                self.date_mask = None;
                self.chip_caret = None;
                self.condition_hold = false;
                self.suggest_dismissed = None;
                Self::reset_input_cursor(ctx);
                self.focus_requested = true;
                self.sync_size_mask(ctx);
                return true;
            }
        }
        self.date_mask = None;
        self.size_mask = None;
        let trimmed = self.input.trim_end();
        self.input = if trimmed.is_empty() {
            text
        } else if chip.prepend_edit {
            // 范围前缀必须回到最前面，否则 `f ` 会被当成普通词。
            format!("{text}{trimmed}")
        } else {
            format!("{trimmed} {text}")
        };
        self.chip_caret = None;
        self.condition_hold = chip.hold_after_edit;
        self.suggest_dismissed = None;
        self.reset_history_recall();
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
        self.trigger(ctx);
        true
    }

    /// Esc 取消条件编辑：恢复编辑前的 chip 和输入。
    fn cancel_condition_edit(&mut self, ctx: &egui::Context) -> bool {
        let Some((filters, input, scope)) = self.condition_edit_backup.take() else {
            return false;
        };
        // 进入编辑时压过一条 undo 快照；用户主动取消 = 撤销这次编辑。
        self.condition_history.forget_last(&filters, &input, scope);
        self.filters = filters;
        self.input = input;
        self.scope = scope;
        self.condition_hold = false;
        self.chip_caret = None;
        self.date_mask = None;
        self.size_mask = None;
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
        self.trigger(ctx);
        true
    }

    /// 当前条件状态的快照（filters + 输入 + 虚拟光标 + 范围）。
    fn condition_snapshot(&self) -> ConditionSnapshot {
        ConditionSnapshot {
            filters: self.filters.clone(),
            input: self.input.clone(),
            chip_caret: self.chip_caret,
            scope: self.scope,
        }
    }

    /// 遮罩内容镜像回输入框并保持查询暂停。
    fn sync_date_mask(&mut self, ctx: &egui::Context) {
        let text = self
            .date_mask
            .as_ref()
            .map(|mask| mask.mirror_text())
            .unwrap_or_default();
        if self.input != text {
            self.input = text;
        }
        self.chip_caret = None;
        self.trigger(ctx);
    }

    /// 遮罩回车：固化各段（未输完的夹紧），拼规范条件文本提交成 chip。
    fn commit_date_mask(&mut self, ctx: &egui::Context) {
        let Some(mask) = self.date_mask.take() else {
            return;
        };
        self.input = mask.commit_text();
        self.date_mask = None;
        self.size_mask = None;
        self.condition_edit_backup = None;
        self.condition_hold = false;
        self.focus_requested = true;
        Self::reset_input_cursor(ctx);
        self.commit_conditions(ctx);
        self.trigger(ctx);
    }

    /// 遮罩内输入了不属于条件的字符。默认按「改搜普通查询」处理：条件
    /// 文本整体丢掉，只保留用户这次输入的字符（chip 编辑态条件已移出
    /// filters，自动激活的遮罩也一样，不能再留下半截条件）。只有两种
    /// 情况保留前缀继续按条件编辑：字段后还没输入值时打比较符
    ///（`date:` + `>`），以及还没输入任何段时打别名（`date >` + `今天`）。
    fn exit_date_mask_to_free_text(&mut self, ctx: &egui::Context, text: &str) {
        let keep_prefix = self.date_mask.as_ref().is_some_and(|mask| {
            !mask.from_chip_edit
                && mask.all_typed_empty()
                && ((mask.op.is_empty() && is_condition_op_text(text))
                    || crate::search::conditions::is_composing(&format!(
                        "{}{}",
                        mask.prefix(),
                        text
                    )))
        });
        let partial = self
            .date_mask
            .as_ref()
            .map(|mask| mask.partial_text())
            .unwrap_or_default();
        self.date_mask = None;
        self.size_mask = None;
        if keep_prefix {
            self.input = format!("{partial}{text}");
        } else {
            self.condition_edit_backup = None;
            self.condition_hold = false;
            self.input = text.to_string();
        }
        self.reset_history_recall();
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
        self.trigger(ctx);
    }

    /// 大小遮罩内容镜像回输入框并保持查询暂停。
    fn sync_size_mask(&mut self, ctx: &egui::Context) {
        let text = self
            .size_mask
            .as_ref()
            .map(|mask| mask.mirror_text())
            .unwrap_or_default();
        if self.input != text {
            self.input = text;
        }
        self.chip_caret = None;
        self.trigger(ctx);
    }

    /// 大小遮罩回车：拼规范条件文本（默认 10 MB）提交成 chip。
    fn commit_size_mask(&mut self, ctx: &egui::Context) {
        let Some(mask) = self.size_mask.take() else {
            return;
        };
        self.input = mask.commit_text();
        self.condition_edit_backup = None;
        self.condition_hold = false;
        self.focus_requested = true;
        Self::reset_input_cursor(ctx);
        self.commit_conditions(ctx);
        self.trigger(ctx);
    }

    /// 大小遮罩内输入了不属于条件的字符（非数字、非单位字母、非比较符）：
    /// 按「改搜普通查询」处理——条件文本整体丢弃，只留这次输入的字符，
    /// 不再留下 `size>10yyyy` 这种半截条件。字段后还没输入值时打的比较符
    ///（`size:` + `>`）保留前缀继续按条件编辑。
    fn exit_size_mask_to_free_text(&mut self, ctx: &egui::Context, text: &str) {
        let keep_prefix = self.size_mask.as_ref().is_some_and(|mask| {
            !mask.from_chip_edit
                && mask.typed_empty()
                && mask.op.is_empty()
                && is_condition_op_text(text)
        });
        let partial = self
            .size_mask
            .as_ref()
            .map(|mask| mask.partial_text_for_exit(text))
            .unwrap_or_default();
        self.size_mask = None;
        if keep_prefix {
            self.input = format!("{partial}{text}");
        } else {
            self.condition_edit_backup = None;
            self.condition_hold = false;
            self.input = text.to_string();
        }
        self.reset_history_recall();
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
        self.trigger(ctx);
    }

    /// 大小遮罩按键处理。返回 true = 本帧按键已被遮罩消费。
    fn handle_size_mask_keys(&mut self, ctx: &egui::Context) -> bool {
        if self.size_mask.is_none() {
            return false;
        }
        // 数字/小数点进段；`:` 提交；字母当单位输入（`G`→GB、`GB`→GB）；
        // 比较符（`>`/`<`/`=`…）就地改比较符、不离开遮罩；空格不属于条件，
        // 直接忽略；其它字符 = 用户改搜普通查询。
        // 从 chip 进入编辑时条件整体丢弃；自动激活时保留已输入片段继续打条件。
        let mut feed: Vec<char> = Vec::new();
        let mut units: Vec<char> = Vec::new();
        let mut ops: Vec<char> = Vec::new();
        let mut exit_text: Option<String> = None;
        let mut commit = false;
        ctx.input_mut(|i| {
            i.events.retain(|event| match event {
                egui::Event::Text(text) => {
                    if text.is_empty() {
                        return true;
                    }
                    if text.chars().all(char::is_whitespace) {
                        // 条件里没有空白：空格不进输入框，避免把条件拆坏。
                        return false;
                    }
                    if text.chars().all(|c| c.is_ascii_digit() || c == '.') {
                        feed.extend(text.chars());
                    } else if text.chars().all(crate::search::conditions::is_op_char) {
                        ops.extend(text.chars());
                    } else if matches!(text.as_str(), ":" | "：") {
                        commit = true;
                    } else if text.chars().count() == 1
                        && text.chars().all(|c| c.is_ascii_alphabetic())
                    {
                        units.extend(text.chars());
                    } else {
                        exit_text
                            .get_or_insert_with(String::new)
                            .push_str(text);
                    }
                    false
                }
                _ => true,
            })
        });
        if let Some(text) = exit_text {
            self.exit_size_mask_to_free_text(ctx, &text);
            return true;
        }
        if commit {
            self.commit_size_mask(ctx);
            return true;
        }
        let fed = !feed.is_empty() || !units.is_empty() || !ops.is_empty();
        for c in feed {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.feed_digit(c);
            }
        }
        for c in ops {
            let accepted = self
                .size_mask
                .as_mut()
                .is_some_and(|mask| mask.feed_op_char(c));
            if !accepted {
                // 闭区间没有比较符可改：比较符字符就是查询内容。
                self.exit_size_mask_to_free_text(ctx, &c.to_string());
                return true;
            }
        }
        for c in units {
            let accepted = self
                .size_mask
                .as_mut()
                .is_some_and(|mask| mask.feed_unit_letter(c));
            if !accepted {
                // 不是合法单位字母：字母本身就是查询内容，条件整体丢弃。
                self.exit_size_mask_to_free_text(ctx, &c.to_string());
                return true;
            }
        }
        let mut handled = fed;
        // 数字段为空时 Ctrl+Backspace 不消费，留给「删左邻 chip」。
        let ctrl_bksp = ctx.input(|i| i.modifiers.ctrl)
            && ctx.input(|i| i.key_pressed(Key::Backspace));
        let number_empty = self
            .size_mask
            .as_ref()
            .is_some_and(|mask| mask.number().is_empty());
        if ctrl_bksp && !number_empty {
            if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Backspace)) {
                if let Some(mask) = self.size_mask.as_mut() {
                    mask.feed_backspace();
                }
                handled = true;
            }
        } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace))
            || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete))
        {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.feed_backspace();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.advance();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.retreat();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowRight)) {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.advance();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowLeft)) {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.retreat();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.spin(1);
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.spin(-1);
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            self.commit_size_mask(ctx);
            return true;
        }
        if handled {
            self.sync_size_mask(ctx);
        }
        handled
    }

    /// 遮罩按键处理。返回 true = 本帧按键已被遮罩消费。
    fn handle_date_mask_keys(&mut self, ctx: &egui::Context) -> bool {
        if self.date_mask.is_none() {
            return false;
        }
        // 文本事件：数字进段；`-`/`/`/`.` 当分隔符（当前段已有数字才跳段，
        // 否则忽略——避免 `2026` 自动跳段后再打 `-` 把月跳过去）；
        // 比较符（`>`/`<`/`=`…）就地改比较符、不离开遮罩；
        // `:` 提交；空格不属于条件，直接忽略；其它字符 = 改搜普通查询
        //（从 chip 进入编辑时条件整体丢弃，自动激活时保留前缀继续打别名）。
        let mut feed: Vec<char> = Vec::new();
        let mut ops: Vec<char> = Vec::new();
        let mut exit_text: Option<String> = None;
        let mut commit = false;
        ctx.input_mut(|i| {
            i.events.retain(|event| match event {
                egui::Event::Text(text) => {
                    if text.is_empty() {
                        return true;
                    }
                    if text.chars().all(char::is_whitespace) {
                        // 条件里没有空白：空格不进输入框，避免把条件拆坏。
                        return false;
                    }
                    if text
                        .chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, '-' | '/' | '.'))
                    {
                        feed.extend(text.chars());
                    } else if text.chars().all(crate::search::conditions::is_op_char) {
                        ops.extend(text.chars());
                    } else if matches!(text.as_str(), ":" | "：") {
                        commit = true;
                    } else {
                        exit_text
                            .get_or_insert_with(String::new)
                            .push_str(text);
                    }
                    false
                }
                _ => true,
            })
        });
        if let Some(text) = exit_text {
            self.exit_date_mask_to_free_text(ctx, &text);
            return true;
        }
        if commit {
            self.commit_date_mask(ctx);
            return true;
        }
        for c in feed.iter().copied() {
            if c.is_ascii_digit() {
                if let Some(mask) = self.date_mask.as_mut() {
                    mask.feed_digit(c);
                }
            } else if let Some(mask) = self.date_mask.as_mut() {
                // 分隔符：当前段已输入数字才跳到下一段；段空时忽略。
                if !mask.typed_str(mask.date_idx, mask.part).is_empty() {
                    mask.advance();
                }
            }
        }
        let ops_fed = !ops.is_empty();
        for c in ops {
            let accepted = self
                .date_mask
                .as_mut()
                .is_some_and(|mask| mask.feed_op_char(c));
            if !accepted {
                // 闭区间没有比较符可改：比较符字符就是查询内容。
                self.exit_date_mask_to_free_text(ctx, &c.to_string());
                return true;
            }
        }
        let mut handled = !feed.is_empty() || ops_fed;
        // Backspace：删段内数字；段已空且在最后一段时不消费（留给 chip 删除）。
        let ctrl_bksp = ctx.input(|i| i.modifiers.ctrl)
            && ctx.input(|i| i.key_pressed(Key::Backspace));
        let seg_typed_empty = self
            .date_mask
            .as_ref()
            .is_some_and(|mask| mask.typed_str(mask.date_idx, mask.part).is_empty());
        if ctrl_bksp && !seg_typed_empty {
            if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Backspace)) {
                if let Some(mask) = self.date_mask.as_mut() {
                    mask.feed_backspace();
                }
                handled = true;
            }
        } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace))
            || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete))
        {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.feed_backspace();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.advance();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.retreat();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowRight)) {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.advance();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowLeft)) {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.retreat();
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.spin(1);
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.spin(-1);
            }
            handled = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            self.commit_date_mask(ctx);
            return true;
        }
        if handled {
            self.sync_date_mask(ctx);
        }
        handled
    }

    /// 在任何会改变条件/chip 的操作前记录一份快照。
    fn record_condition_undo(&mut self) {
        let snapshot = self.condition_snapshot();
        self.condition_history.record(snapshot);
    }

    /// 恢复一份条件快照。输入里若有完整条件文本，进入「回车确认」态：
    /// 不自动搜索，避免撤回后被立刻重新收成 chip。
    fn restore_condition_snapshot(&mut self, ctx: &egui::Context, snapshot: ConditionSnapshot) {
        self.filters = snapshot.filters;
        self.input = snapshot.input;
        self.scope = snapshot.scope;
        self.chip_caret = snapshot.chip_caret;
        self.condition_edit_backup = None;
        self.suggest_dismissed = None;
        self.suggest_index = 0;
        self.condition_error = None;
        self.menu_open = false;
        self.filter_menu = None;
        self.date_mask = None;
        self.size_mask = None;
        self.condition_hold = self.conditions_apply(self.scope)
            && !crate::search::conditions::extract(&self.input).is_empty();
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
    }

    /// Ctrl+Z：撤回最近一次条件操作。
    fn undo_condition(&mut self, ctx: &egui::Context) {
        let current = self.condition_snapshot();
        let Some(previous) = self.condition_history.undo(current) else {
            return;
        };
        self.restore_condition_snapshot(ctx, previous);
        self.toast(ctx, "已撤回条件".to_string());
        self.trigger(ctx);
    }

    /// Ctrl+Y / Ctrl+Shift+Z：取消撤回（重做）。
    fn redo_condition(&mut self, ctx: &egui::Context) {
        let current = self.condition_snapshot();
        let Some(next) = self.condition_history.redo(current) else {
            return;
        };
        self.restore_condition_snapshot(ctx, next);
        self.toast(ctx, "已重做条件".to_string());
        self.trigger(ctx);
    }

    /// 退出历史召回状态（下一次 Ctrl+↑ 重新从最新一条开始）。
    fn reset_history_recall(&mut self) {
        self.history_cursor = None;
        self.history_draft.clear();
    }

    /// 历史输入召回：`delta < 0` 更旧（Ctrl+↑），`delta > 0` 更新（Ctrl+↓）。
    /// 第一次召回先抓一份最近输入列表，并记住当前输入作为草稿；
    /// 一直按 ↓ 回到草稿。
    fn recall_input(&mut self, ctx: &egui::Context, delta: i32) {
        if self.history_cursor.is_none() {
            let list = self
                .history
                .lock()
                .map(|history| history.recent_inputs(100))
                .unwrap_or_default();
            if list.is_empty() {
                self.toast(ctx, "没有历史输入".to_string());
                return;
            }
            self.input_history = list;
            self.history_draft = self.input.clone();
            self.history_cursor = Some(0);
        } else if delta > 0 {
            let cursor = self.history_cursor.unwrap_or(0);
            if cursor == 0 {
                // 已到最新一条：再 ↓ 回到召回前的草稿。
                self.history_cursor = None;
                self.input = std::mem::take(&mut self.history_draft);
                self.apply_recalled_input(ctx);
                return;
            }
            self.history_cursor = Some(cursor - 1);
        } else {
            let cursor = self.history_cursor.unwrap_or(0);
            let last = self.input_history.len().saturating_sub(1);
            self.history_cursor = Some((cursor + 1).min(last));
        }
        let index = self.history_cursor.unwrap_or(0);
        self.input = self.input_history[index].clone();
        self.apply_recalled_input(ctx);
    }

    /// 把召回的历史输入放进输入框并重新搜索。
    fn apply_recalled_input(&mut self, ctx: &egui::Context) {
        self.chip_caret = None;
        self.condition_hold = false;
        self.condition_edit_backup = None;
        self.date_mask = None;
        self.size_mask = None;
        self.suggest_dismissed = None;
        self.suggest_index = 0;
        Self::reset_input_cursor(ctx);
        self.focus_requested = true;
        self.trigger(ctx);
    }
    /// 返回 true = 这次回车已被条件编辑消费（不再打开选中结果）。
    fn finish_condition_edit(&mut self, ctx: &egui::Context) -> bool {
        if self.condition_hold {
            self.condition_hold = false;
            // 路径 chip 还原的文本没有终结符（`path:D:\work`），回车时补冒号；
            // 盘符 `C:` 已带终结符、范围 `a ` 不需要，原样提交。
            self.input = condition_enter_text(&self.input);
            if self.commit_conditions(ctx) {
                self.trigger(ctx);
            } else {
                // 用户把盘符改没了：保持编辑态，别偷偷开始搜索。
                self.condition_hold = true;
                self.dirty_at = None;
                self.waiting = false;
                ctx.request_repaint();
            }
            return true;
        }
        let mut text = self.input.trim_end().to_string();
        let last = text.chars().last();
        // 只有比较符还没有值（`size>`）：不补冒号，也不搜索。
        if last.is_none_or(|c| matches!(c, '>' | '<' | '=' | '≥' | '≤' | '＞' | '＜' | '＝')) {
            self.dirty_at = None;
            self.waiting = false;
            ctx.request_repaint();
            return true;
        }
        if !text.ends_with(':') && !text.ends_with('：') {
            text.push(':');
        }
        self.input = text;
        self.commit_conditions(ctx);
        self.trigger(ctx);
        true
    }

    /// `C:` 已收成 chip 后用户继续打 `\`：还原成 `C:\`，回到路径浏览。
    fn restore_drive_for_path(&mut self) -> bool {
        if !self.input.starts_with(['\\', '/']) || self.filters.drives.is_empty() {
            return false;
        }
        // 撤回这次还原 = 回到「盘符 chip + 分隔符文本」的状态。
        self.record_condition_undo();
        let Some(drive) = self.filters.drives.pop() else {
            return false;
        };
        self.input = format!("{}{}", drive.source(), self.input);
        self.chip_caret = None;
        true
    }

    /// 扫描输入框里的条件并收成 chip：范围前缀（`a `/`f `/>`）→ scope，
    /// 日期 / 大小 / 路径 / 盘符 → filters。成功的片段从文本删除，非法条件
    /// 保留原文、只记录错误。返回是否改动了输入。
    ///
    /// 只在 TextEdit changed 后调用；IME 组合期不调用（避免把预编辑当条件）。
    fn commit_conditions(&mut self, ctx: &egui::Context) -> bool {
        if self.condition_hold {
            return false;
        }
        // 搜索模式（搜索 chip 已存在）：输入框里的一切都是搜索词——不再解析
        // 范围/日期/大小/路径/盘符，也不会再收一个关键字 chip 来换引擎
        //（换引擎用 × 退出或双击 chip 改文本）。正则模式不算搜索模式。
        if self.filters.keyword.is_some() && !self.filters.matches.regex {
            return false;
        }
        // 盘符 chip 后面继续打路径分隔符：先把盘符还原成文本，交给路径模式。
        if self.restore_drive_for_path() {
            self.move_caret_to_end(ctx);
            return true;
        }

        // 范围前缀：只在普通搜索里收 chip；正则/浏览模式下前缀就是普通文本。
        let scope_change = if self.filters.matches.regex || self.in_browse() {
            None
        } else {
            crate::search::query::split_scope_prefix(&self.input)
                .map(|(scope, rest)| (scope, rest.trim_start().to_string()))
        };

        // 条件提取在剥掉范围前缀之后的文本上做（`f date>…` 的 date 不能被
        // 前缀影响）；scope 用提交后的值判断条件是否生效。
        let mut text = match &scope_change {
            Some((_, rest)) => rest.clone(),
            None => self.input.clone(),
        };
        let effective_scope = scope_change
            .as_ref()
            .map_or(self.scope, |(scope, _)| *scope);

        // 网络搜索关键字：`b rust` → 搜索 chip（Bing），剩余文本才是查询词。
        // 与范围前缀一样要求 trigger 后有空白（只打 `b` 仍按普通查询）。
        // 只有「全部」页有搜索模式；正则/浏览模式下关键字就是普通文本。
        let keyword_change =
            if self.filters.matches.regex || self.in_browse() || effective_scope != Scope::All {
                None
            } else {
                crate::search::split_keyword(&self.cfg.keywords, &text)
            };
        if let Some((_, rest)) = &keyword_change {
            text = rest.clone();
        }
        // 同一个改动里刚收下关键字（`b date>…:` 一次粘贴）：剩余文本就是
        // 搜索词，不再当条件解析。
        let extracted = if keyword_change.is_none() && self.conditions_apply(effective_scope) {
            let out = crate::search::conditions::extract(&text);
            self.condition_error = out.errors.first().cloned();
            (!out.is_empty()).then_some(out)
        } else {
            // 范围切到条件不生效的页（应用/命令/书签）、或刚进搜索模式时，
            // 旧错误也要清掉。
            self.condition_error = None;
            None
        };
        if scope_change.is_none() && keyword_change.is_none() && extracted.is_none() {
            return false;
        }

        // 先全部合并校验（失败不动任何状态），通过后再一次性写回。
        let mut dates = self.filters.modified.clone().unwrap_or_default();
        let mut sizes = self.filters.size.clone().unwrap_or_default();
        let mut drives = self.filters.drives.clone();
        let mut path = self.filters.path.clone();
        let mut location = self.filters.location.clone();
        let mut cleaned = text;
        if let Some(out) = &extracted {
            for range in &out.dates {
                if let Err(err) = dates.merge(range.clone()) {
                    self.condition_error = Some(err);
                    return false;
                }
            }
            for range in &out.sizes {
                if let Err(err) = sizes.merge(range.clone()) {
                    self.condition_error = Some(err);
                    return false;
                }
            }
            for drive in &out.drives {
                match drives
                    .iter()
                    .position(|existing| existing.letter == drive.letter)
                {
                    Some(pos) => drives[pos] = drive.clone(),
                    None => drives.push(drive.clone()),
                }
            }
            // 路径只保留最后一个（同一查询里写两次 = 后者覆盖），并且与
            // 「位置」筛选是同一语义：路径 chip 覆盖掉位置选择。
            if let Some(last) = out.paths.last() {
                path = Some(last.clone());
                location = None;
            }
            cleaned = out.cleaned.clone();
        }

        // 所有合并都通过后再记录快照：撤回会恢复条件原文（chip 撤掉），
        // 重做再收成 chip。
        self.record_condition_undo();
        if let Some((scope, _)) = scope_change {
            self.scope = scope;
        }
        if let Some((trigger, _)) = keyword_change {
            self.filters.keyword = Some(trigger);
        }
        self.input = cleaned;
        self.filters.modified = (!dates.is_empty()).then_some(dates);
        self.filters.size = (!sizes.is_empty()).then_some(sizes);
        self.filters.drives = drives;
        self.filters.path = path;
        self.filters.location = location;
        self.condition_hold = false;
        self.condition_edit_backup = None;
        self.chip_caret = None;
        self.reset_history_recall();
        self.move_caret_to_end(ctx);
        // 条件提交会程序化改写输入，TextEdit 自带的文本撤销历史不再对应
        // 当前内容；清掉它，避免条件撤销栈空后 Ctrl+Z 又翻出旧文本。
        Self::clear_text_undo(ctx);
        true
    }

    /// 清掉输入框 TextEdit 自带的撤销历史（不改变文本/光标）。
    fn clear_text_undo(ctx: &egui::Context) {
        let id = egui::Id::new("lstr-search-input");
        if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ctx, id) {
            state.clear_undoer();
            state.store(ctx, id);
        }
    }

    /// 程序化清空输入（不清筛选）：同时清掉条件错误与编辑/联想/遮罩瞬态，
    /// 避免错误态或半截编辑一直挡住后续查询。
    fn clear_input_text(&mut self) {
        self.input.clear();
        self.condition_error = None;
        self.chip_caret = None;
        self.condition_hold = false;
        self.suggest_dismissed = None;
        self.suggest_index = 0;
        self.condition_edit_backup = None;
        self.date_mask = None;
        self.size_mask = None;
        self.reset_history_recall();
    }

    fn trigger(&mut self, ctx: &egui::Context) {
        // 条件编辑态 / 联想弹框打开 / 半截条件输入：都不发查询。
        // 半截条件当文件名搜只会闪出 0 结果，等冒号提交或输入被改掉再搜。
        if self.condition_hold
            || !self.condition_suggestions().is_empty()
            || (self.conditions_apply(self.scope)
                && crate::search::conditions::is_composing(&self.input))
        {
            self.dirty_at = None;
            self.waiting = false;
            ctx.request_repaint();
            return;
        }
        // 一旦输入/筛选变化就进入「搜索中」：防抖窗口里结果还没回来，
        // 空列表不应该先闪一下「没有匹配项」。
        self.waiting = true;
        // 新查询从首屏 100 条重新开始，加载更多的状态也必须清零。
        self.more_pending = false;
        self.more_exhausted = false;
        // 打字防抖 40ms：只合并同一帧内的连击/粘贴，不人为拖慢首字响应。
        // 请求在 worker 里 latest-wins 合并，且引擎热循环按 CANCEL_GEN 代次取消
        // （新击键到达后旧查询数百 µs 内中止），逐键查询不会堆积；稳态查询
        // 通常 <10ms，故无需旧 150ms 那么保守（那会让首字结果明显迟滞）。
        self.dirty_at = Some(Instant::now() + Duration::from_millis(40));
        ctx.request_repaint_after(Duration::from_millis(16));
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
            self.clear_input_text();
            self.selected = 0;
            self.scroll_y = 0.0;
            self.scroll_to_selection = false;
            self.filters = SearchFilters::default();
            self.condition_error = None;
            self.browse_root = None;
            self.browse_forward.clear();
            self.condition_history.clear();
            self.date_mask = None;
            self.size_mask = None;
        }
        Self::reset_input_cursor(ctx);
        self.filter_menu = None;
        // 唤出时条件编辑/联想/虚拟光标一律从头开始（输入与 chip 仍可保留）。
        self.chip_caret = None;
        self.condition_hold = false;
        self.suggest_dismissed = None;
        self.suggest_index = 0;
        self.condition_edit_backup = None;
        self.reset_history_recall();
        self.pending_delete = None;
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
        self.clear_input_text();
        self.entries.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.filters = SearchFilters::default();
        self.condition_error = None;
        self.filter_menu = None;
        self.condition_history.clear();
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

    /// 空查询 + 空结果：显示「搜索技巧」帮助面板（paint_rows 里绘制）。
    /// 窗口定高（apply_size）与底部提示条定位（paint）必须用同一判断，
    /// 否则提示条会叠在帮助内容中间。
    fn help_tips_active(&self, query_empty: bool) -> bool {
        // 「全部」页才显示技巧面板：范围 chip 指向应用/命令页时，空查询
        // 应该显示那些页自己的列表（apps.top / 全部命令 / 全部书签）。
        self.scope == Scope::All
            && query_empty
            && self.filters.is_empty()
            && self.entries.is_empty()
            && self.browse_root.is_none()
            && !self.settings_open
    }

    /// 结果区顶部提示条占用的高度。窗口定高与绘制循环必须共用这一处：
    /// worker 错误与输入错误可以同时存在（两条 24px 横幅 + 间距）。
    fn banner_height(&self) -> f32 {
        let mut count = 0u32;
        if self.error.is_some() {
            count += 1;
        }
        if self.regex_error.is_some() || self.condition_error.is_some() {
            count += 1;
        }
        // 索引状态提示只在没有错误横幅时显示（见绘制处的 gate）。
        if count == 0 && self.index_note.is_some() {
            count = 1;
        }
        count as f32 * 30.0
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
            // 窗口高度按「一屏结果数」定：worker 会多送候选（可滚动），
            // 窗口只长到设置的行数，不会因为候选多就撑满屏幕。
            let rows = self.entries.len().min(self.cfg.max_results);
            let mut list_h = if rows == 0 { 120.0 } else { rows as f32 * self.theme.row_h };
            // 空态技巧面板：仅空查询+空结果时撑高窗口。
            // 判断必须和 paint_rows 一致（按 parse 后的 query 为空，不是
            // input.trim() 为空）：`a ` 这类只剩范围前缀的输入也算空查询，
            // 面板会显示，窗口就不能只按 120px 定高。
            let tips_active =
                self.help_tips_active(search::query::parse(&self.input, self.scope).is_empty());
            if tips_active {
                let content_w = (width - PAD - if self.preview_open { 216.0 } else { 0.0 }).max(1.0);
                list_h = list_h.max(help_panel_height(content_w));
            }
            let banner_h = self.banner_height();
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
        // 虚拟 chip 光标在条件之间时直接打字/粘贴：自动回到输入框末尾。
        // 必须在 IME 早退之前处理，否则中文预编辑会插到文本开头。
        if self.chip_caret.is_some() {
            let typing = ctx.input(|i| {
                i.events.iter().any(|event| match event {
                    egui::Event::Text(_) | egui::Event::Paste(_) => true,
                    egui::Event::Ime(egui::ImeEvent::Preedit(text)) => !text.is_empty(),
                    egui::Event::Ime(egui::ImeEvent::Commit(_)) => true,
                    _ => false,
                })
            });
            if typing {
                self.chip_caret = None;
                self.move_caret_to_end(ctx);
            }
        }
        // 遮罩期间 TextEdit 不渲染：IME 上屏的中文（今天/本周）必须在这里
        // 接管，退出遮罩转自由文本，否则字符会丢在隐藏的 TextEdit 里。
        if self.date_mask.is_some() || self.size_mask.is_some() {
            let mut committed: Option<String> = None;
            ctx.input_mut(|i| {
                i.events.retain(|event| match event {
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                        committed.get_or_insert_with(|| text.clone());
                        false
                    }
                    _ => true,
                })
            });
            if let Some(text) = committed {
                if self.date_mask.is_some() {
                    self.exit_date_mask_to_free_text(ctx, &text);
                } else {
                    self.exit_size_mask_to_free_text(ctx, &text);
                }
                return;
            }
        }
        if ime_active {
            return;
        }

        // 删除确认是模态弹层：焦点在「删除/取消」之间切换，Enter 执行
        // 当前焦点，Esc 取消，其它按键不穿透到输入框。
        if self.pending_delete.is_some() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.cancel_pending_delete(ctx);
                return;
            }
            let toggle = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowLeft))
                || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowRight))
                || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab))
                || ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab));
            if toggle {
                if let Some(pending) = self.pending_delete.as_mut() {
                    pending.focus_delete = !pending.focus_delete;
                }
                ctx.request_repaint();
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                let focus_delete = self
                    .pending_delete
                    .as_ref()
                    .is_none_or(|pending| pending.focus_delete);
                if focus_delete {
                    self.confirm_pending_delete(ctx);
                } else {
                    self.cancel_pending_delete(ctx);
                }
                return;
            }
            // 模态：其余按键/文本/IME 事件全部丢弃，不能漏到后面的输入框/遮罩。
            ctx.input_mut(|i| {
                i.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Key { .. }
                            | egui::Event::Ime(_)
                    )
                })
            });
            return;
        }

        // 条件 chip 可能在别处被清掉（筛选条/时间预设）：虚拟光标及时失效。
        // 边界下标合法范围是 0..=count（count = 最后一个 chip 右侧），不能
        // 用 >= 判，否则「光标在最后 chip 右侧」每帧都被清掉、← 走不动。
        let chip_count = self.condition_chips().len();
        if self.chip_caret.is_some_and(|index| index > chip_count) {
            self.chip_caret = None;
        }

        // 日期分段遮罩：数字进段、Tab/←→ 跳段、← 到比较符段（↑↓ 或直接
        // 打比较符换档）、↑↓ 加减、回车提交；其余字符退出遮罩转自由文本
        //（别名仍可用）。
        if self.date_mask.is_some() && self.handle_date_mask_keys(ctx) {
            return;
        }
        if self.size_mask.is_some() && self.handle_size_mask_keys(ctx) {
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
            // 条件联想弹框 / 条件编辑态是输入框内最上层：先取消它们。
            if !self.condition_suggestions().is_empty() {
                self.dismiss_suggestion(ctx);
                return;
            }
            if self.cancel_condition_edit(ctx) {
                return;
            }
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
                    self.clear_input_text();
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

        // 条件撤回 / 取消撤回：Ctrl+Z；Ctrl+Y 或 Ctrl+Shift+Z 重做。
        // 只要条件历史里还有记录就接管这两个键（避免条件栈空后 TextEdit
        // 翻出与当前 chip 不对应的旧文本）；完全没有历史时仍交给 TextEdit。
        let history_active =
            self.condition_history.can_undo() || self.condition_history.can_redo();
        if history_active
            && ctx.input_mut(|i| {
                i.consume_key(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Z)
            })
        {
            if self.condition_history.can_redo() {
                self.redo_condition(ctx);
            }
            return;
        }
        if history_active && ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Y)) {
            if self.condition_history.can_redo() {
                self.redo_condition(ctx);
            }
            return;
        }
        if history_active && ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Z)) {
            if self.condition_history.can_undo() {
                self.undo_condition(ctx);
            }
            return;
        }

        // Shift+Ctrl+Backspace：把光标左侧的条件 chip 还原成文本，进入编辑态
        // （没有虚拟光标时 chip 全在文本左侧，编辑最右 chip）。
        if self.condition_edit_target().is_some()
            && ctx.input_mut(|i| {
                i.consume_key(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Backspace)
            })
        {
            if let Some(index) = self.condition_edit_target() {
                self.edit_chip(index, ctx);
            }
            return;
        }

        // Ctrl+Backspace：删光标左侧的条件 chip（虚拟光标优先；文本在开头时
        // 删最右 chip）。没有 chip 可删时不拦截，让 TextEdit 按词删除。
        // 普通 Backspace 不删 chip（约定：只有 Ctrl+Backspace 动条件）。
        if self.condition_delete_target(ctx).is_some()
            && ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Backspace))
        {
            if let Some(index) = self.condition_delete_target(ctx) {
                self.record_condition_undo();
                if self.remove_chip(index) {
                    self.condition_error = None;
                    self.menu_open = false;
                    self.filter_menu = None;
                    self.condition_edit_backup = None;
                    self.trigger(ctx);
                }
            }
            return;
        }

        // 选中结果按 Del：和菜单「删除到回收站」走同一个二次确认流程。
        // 输入框有文字时只在光标位于末尾且没有选区时抢——否则 Del 仍是
        // TextEdit 的前向删除。
        if self
            .entries
            .get(self.selected)
            .is_some_and(|entry| {
                entry.path.is_some()
                    && matches!(entry.kind, Kind::File | Kind::Folder | Kind::Program)
            })
            && (self.input.is_empty() || self.caret_at_end(ctx))
            && !Self::text_has_selection(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete))
        {
            if let Some(entry) = self.entries.get(self.selected).cloned() {
                self.start_delete_confirm(entry, ctx);
            }
            return;
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
        let has_selection = Self::text_has_selection(ctx);
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

        // 虚拟 chip 光标：文本光标在开头时 ← 进入 chip 区（先落在最后一个
        // chip 右侧，再逐个左移）；→ 逐个右移，走出最后一个 chip 回到文本开头。
        if self.has_input_chips() {
            let count = self.condition_chips().len();
            let left_should_handle = self.chip_caret.is_some() || self.text_at_start(ctx);
            if count > 0
                && left_should_handle
                && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowLeft))
            {
                self.chip_caret = chip_caret_left(self.chip_caret, count);
                return;
            }
            if self.chip_caret.is_some()
                && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowRight))
            {
                self.chip_caret = chip_caret_right(self.chip_caret, count);
                return;
            }
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

        // 联想弹框打开时 ↑↓ 改选候选（不移动结果列表）。
        let suggestions = self.condition_suggestions();
        if !suggestions.is_empty() {
            let len = suggestions.len();
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.suggest_index = (self.suggest_index + 1) % len;
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.suggest_index = (self.suggest_index + len - 1) % len;
                return;
            }
        }

        // 历史输入召回：Ctrl+↑/↓ 始终可用；Alt+↑/↓ 在非浏览/路径模式也接受
        //（浏览下 Alt+↑/↓ 是目录导航，不能抢）。
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::ArrowUp)) {
            self.recall_input(ctx, -1);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::ArrowDown)) {
            self.recall_input(ctx, 1);
            return;
        }
        if !self.in_browse() && !self.path_mode() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowUp)) {
                self.recall_input(ctx, -1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowDown)) {
                self.recall_input(ctx, 1);
                return;
            }
        }

        // 没有结果时 ↑↓ 没有列表可导航：直接用于召回历史输入
        //（有结果时 ↑↓ 仍是结果选择，避免抢走核心导航）。
        if self.entries.is_empty() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.recall_input(ctx, -1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.recall_input(ctx, 1);
                return;
            }
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
        // Home/End 跳列表首/尾；End 落到末尾后若还有未加载的候选，
        // paint_rows 的滚到底检测会自动续拉。
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Home)) {
            self.select_edge(false);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::End)) {
            self.select_edge(true);
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
            self.step_scope(ctx, -1);
        } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            self.step_scope(ctx, 1);
        }

        // 联想/条件编辑态：Enter 是「确认条件」，不打开选中结果。
        if !self.condition_suggestions().is_empty()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
        {
            self.accept_suggestion(ctx);
            return;
        }
        if (self.condition_hold
            || (self.conditions_apply(self.scope)
                && crate::search::conditions::is_composing(&self.input)))
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
        {
            self.finish_condition_edit(ctx);
            return;
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
                self.clear_input_text();
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

    /// Home/End：跳到列表首/尾（和 ↑↓/PageUp/PageDown 一样是列表导航，
    /// 不移动输入框光标）。
    fn select_edge(&mut self, last: bool) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = if last { self.entries.len() - 1 } else { 0 };
        self.hover_suppress_until = Instant::now() + Duration::from_millis(300);
        self.scroll_to_selection = true;
    }

    /// 滚到列表底部时加载更多：把交付上限翻倍（100→200→…→1000）并原样
    /// 重发当前查询。响应到达前不重复触发；后端给不出更多就停止。
    fn load_more(&mut self, ctx: &egui::Context) {
        if self.waiting || self.more_pending || self.more_exhausted {
            return;
        }
        // total 是精确命中数：已经全部显示（或本身没有结果）就无需再拉。
        if self.entries.is_empty() || self.entries.len() >= self.total {
            self.more_exhausted = true;
            return;
        }
        match self.service.more() {
            Some(generation) => {
                self.pending_gen = generation;
                self.more_pending = true;
                self.waiting = true;
                self.search_sent_at = Some(Instant::now());
                self.ui_total_ms = None;
                ctx.request_repaint();
            }
            None => self.more_exhausted = true,
        }
    }

    /// Tab / Shift+Tab / Alt+←→：在当前类别上步进。
    ///
    /// 范围现在只有一个来源（`self.scope`）：文本前缀在同一帧已被
    /// `commit_conditions` 收成 chip，这里不再从输入里推导，也不改写文本。
    fn step_scope(&mut self, ctx: &egui::Context, delta: i32) {
        let next = self.scope.step(delta);
        self.set_scope(ctx, next);
    }

    fn set_scope(&mut self, ctx: &egui::Context, scope: Scope) {
        // 正则是文件名语义，应用/命令/书签页没有可执行的正则匹配。
        if self.filters.matches.regex
            && matches!(scope, Scope::App | Scope::Bookmark | Scope::Command)
        {
            self.toast(ctx, "正则模式只搜文件 / 文件夹 / 程序".to_string());
            return;
        }
        // 浏览模式只列文件系统条目，应用/命令/书签页没有意义。
        if self.in_browse() && matches!(scope, Scope::App | Scope::Bookmark | Scope::Command) {
            self.toast(ctx, "浏览模式只支持文件夹 / 程序 / 文件".to_string());
            return;
        }
        if self.scope != scope {
            // 范围也是 chip（可删/可改），切换同样进撤销栈。
            self.record_condition_undo();
            self.scope = scope;
        }
        // 正在编辑范围 chip 时点 Tab：退出编辑态，以点击结果为准——
        // 还原出来的 `a `/`f `/>` 文本要收回，否则下一次击键又把点击结果顶掉。
        if self.condition_hold {
            self.condition_hold = false;
            if let Some((_, rest)) = search::query::split_scope_prefix(&self.input) {
                self.input = rest.trim_start().to_string();
                Self::reset_input_cursor(ctx);
            }
        }
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
            if matches!(entry.kind, Kind::Command | Kind::Bookmark) {
                self.toast(ctx, "对话框模式只能选择文件或文件夹".to_string());
                return;
            }
            // 收藏的应用（.lnk / shell:AppsFolder）没有能填进对话框的路径：
            // 直接交给 shell 启动，和普通模式一致。
            if entry.kind == Kind::App {
                match shell::open_entry(&entry) {
                    Ok(()) => {
                        self.service.record_open(&entry);
                        if let Ok(mut history) = self.history.lock() {
                            history.record_query(&self.input, &entry);
                        }
                        self.suppressed_dialog = Some(dialog.hwnd);
                        self.dialog = None;
                        self.hide_keep(ctx);
                    }
                    Err(err) => self.toast(ctx, format!("打开失败: {err}")),
                }
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
        if entry.kind == Kind::Bookmark {
            // 书签 = URL：交给系统默认浏览器打开，不经过 explorer。
            match shell::open_url(&entry.full_path()) {
                Ok(()) => {
                    self.service.record_open(&entry);
                    if let Ok(mut history) = self.history.lock() {
                        if self.browse_root.is_none() {
                            history.record_query(&self.input, &entry);
                        }
                    }
                    self.hide_keep(ctx);
                }
                Err(err) => self.toast(ctx, format!("打开失败: {err}")),
            }
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
        if matches!(entry.kind, Kind::Command | Kind::Bookmark) {
            self.toast(ctx, "该结果没有所在位置".to_string());
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
        if matches!(entry.kind, Kind::Command | Kind::Folder | Kind::Bookmark) {
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
        let what = if entry.kind == Kind::Bookmark { "链接" } else { "路径" };
        self.toast(ctx, format!("已复制{what}"));
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
        if entry.id == "calc:" {
            // 计算器结果行:path 存结果字符串,Enter 复制到剪贴板(不收起,便于继续)。
            if let Some(path) = &entry.path {
                ctx.copy_text(path.to_string_lossy().to_string());
                self.toast(ctx, "已复制计算结果".to_string());
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
                self.service.set_bookmarks_enabled(self.cfg.index.bookmarks);
                self.service.invalidate_apps();
                self.preview_open = self.cfg.preview;
                self.trigger(ctx);
                self.toast(ctx, "已重新加载（含应用/书签索引）".to_string());
            }
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
                note = Some(format!(
                    "正在构建索引 {} · 已收录 {} 万项（结果暂不完整）",
                    progress,
                    total / 10_000,
                ));
            } else if st.phase == lstr_index::Phase::Loading {
                // 主界面不显示任何提示时，用户会以为「卡住了」：等 UAC、等 worker
                // 连管道都可能停在这一阶段（旧 worker 占管道时甚至永久停住）。
                note = Some(
                    "索引准备中：等待管理员授权 / worker 连接（首次构建约 1~3 分钟）"
                        .to_string(),
                );
            } else if st.phase == lstr_index::Phase::Fallback {
                note = Some(
                    "内置索引不可用（未获管理员授权或 worker 未运行），暂时没有搜索结果"
                        .to_string(),
                );
            } else if st.phase == lstr_index::Phase::Ready {
                // USN 增量层:异常状态要浮出来(退避/停用 = 实时增量不可靠,
                // worker 正在自动对账或已停用该卷);正常 Watching 不打扰(安静原则)。
                if self.cfg.index.incremental == "usn" {
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
        let preview_w = if self.preview_open { 216.0 } else { 0.0 };
        let list_h = if entries.is_empty() {
            // 帮助面板显示时按面板真实高度算：底部提示条（文件对话框/
            // 错误条）用 list_h 定位，按空列表的 120 算会叠在帮助内容中间。
            if self.help_tips_active(query.is_empty()) {
                help_panel_height(
                    (self.last_width.max(self.theme.width) - PAD - preview_w).max(1.0),
                )
            } else {
                120.0
            }
        } else {
            entries.len() as f32 * row_h
        };
        let banner_h = self.banner_height();
        let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
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
                } else if let Some(trigger) = &self.filters.keyword {
                    // 搜索模式：输入框内容全部作为网络搜索词。
                    let name = self
                        .cfg
                        .keywords
                        .iter()
                        .find(|kw| kw.trigger.eq_ignore_ascii_case(trigger))
                        .map(|kw| kw.name.clone())
                        .unwrap_or_else(|| trigger.clone());
                    format!("输入 {name} 搜索内容…")
                } else {
                    hint_for(query.scope).to_string()
                };
                // 条件 chip：固定在输入框左侧，文本区整体右移。
                // 标签用比例字体、值用等宽字体；日期=琥珀色，大小=冷色，盘符=绿色。
                // 搜索模式下筛选条件不参与查询（本地索引整个跳过），对应 chip
                // 置灰且不可操作；范围/搜索 chip 自己是模式，不随此变灰（见下）。
                let conditions_ok = self.conditions_apply(query.scope);
                let chips = self.condition_chips();
                let mut text_rect = search_rect;
                if !chips.is_empty() {
                    // 多个条件时整体缩小一号（字号/内边距/高度），给输入框留空间。
                    let dense = chips.len() > 1;
                    // 窄窗口下再退一步：去掉标签（只留值），保证文本区至少留出 80px。
                    let with_labels: f32 = chips
                        .iter()
                        .map(|chip| {
                            condition_chip_width(&painter, chip.label, &chip.value, dense)
                        })
                        .sum::<f32>()
                        + 6.0 * chips.len().saturating_sub(1) as f32;
                    let compact = search_rect.width() - with_labels < 90.0;
                    // 光标边界：0 = 最左，count = 最后一个 chip 右侧。
                    if self.chip_caret.is_some_and(|index| index > chips.len()) {
                        self.chip_caret = None;
                    }
                    let chip_h = if dense { 22.0 } else { 24.0 };
                    // 输入文字视觉上贴底：chip 再往下压 2px（仍在分隔线上方）。
                    let chip_top = search_rect.bottom() - chip_h + 2.0;
                    let mut x = search_rect.left();
                    let mut chip_rights = Vec::with_capacity(chips.len());
                    let mut removed: Option<usize> = None;
                    let mut caret_target: Option<usize> = None;
                    let mut edit_target: Option<usize> = None;
                    for (index, chip) in chips.iter().enumerate() {
                        let label = if compact { "" } else { chip.label };
                        // 范围/搜索 chip 是「当前模式」本身，不随条件失效置灰
                        // （它们是别的 chip 失效的原因），也仍然可操作。
                        let dead = chip_inactive(chip.kind, conditions_ok);
                        let (w, clear_clicked, body_clicked, body_double) = paint_condition_chip(
                            ui,
                            &painter,
                            x,
                            chip_top,
                            label,
                            &chip.value,
                            if dead { theme.dim } else { chip.color },
                            if dead { theme.hover } else { chip.bg },
                            &chip.id,
                            &format!(
                                "{}{}",
                                chip.tooltip,
                                if dead { "；当前上下文不生效" } else { "" }
                            ),
                            dense,
                            !dead,
                        );
                        if clear_clicked {
                            removed = Some(index);
                        }
                        if body_double {
                            edit_target = Some(index);
                        } else if body_clicked {
                            // 点 chip = 选中它：光标放在它右侧，编辑/删除才对得上。
                            caret_target = Some(index + 1);
                        }
                        chip_rights.push(x + w);
                        x += w + 6.0;
                    }
                    // 虚拟 chip 光标：画在边界处的 6px 间隙里（0 = 最左，
                    // count = 最后一个 chip 右侧），高度与 chip 对齐。
                    if let Some(index) = self.chip_caret {
                        // 边界 0 理论上到不了（见 caret 更新处），兜底贴最左。
                        let caret_x = index
                            .checked_sub(1)
                            .and_then(|prev| chip_rights.get(prev))
                            .map_or(search_rect.left() + 1.0, |right| right + 3.0);
                        let caret = Rect::from_min_size(
                            egui::pos2(caret_x - 1.0, chip_top + 1.5),
                            egui::vec2(2.0, chip_h - 3.0),
                        );
                        painter.rect_filled(caret, CornerRadius::same(1), theme.amber);
                    }
                    if let Some(index) = removed {
                        self.record_condition_undo();
                        if self.remove_chip(index) {
                            self.condition_error = None;
                            self.menu_open = false;
                            self.filter_menu = None;
                            self.condition_edit_backup = None;
                            self.trigger(ctx);
                        }
                    } else if let Some(index) = edit_target {
                        self.edit_chip(index, ctx);
                    } else if let Some(index) = caret_target {
                        self.chip_caret = Some(index);
                    }
                    text_rect.min.x = (x + 2.0)
                        .min(search_rect.right() - 40.0)
                        .max(search_rect.left());
                }
                // 日期/大小分段遮罩：先失效检查（输入与遮罩不一致 = 被外部改写），
                // 再按「字段+操作符且值未输入」自动激活。
                if self.date_mask.is_some() {
                    let mirror = self
                        .date_mask
                        .as_ref()
                        .map(|mask| mask.mirror_text())
                        .unwrap_or_default();
                    if mirror != self.input {
                        self.date_mask = None;
                    }
                }
                if self.size_mask.is_some() {
                    let mirror = self
                        .size_mask
                        .as_ref()
                        .map(|mask| mask.mirror_text())
                        .unwrap_or_default();
                    if mirror != self.input {
                        self.size_mask = None;
                    }
                }
                if self.date_mask.is_none()
                    && self.size_mask.is_none()
                    && self.pending_delete.is_none()
                    && !self.ime_composing
                    && !self.condition_hold
                    && self.conditions_apply(query.scope)
                {
                    if let Some((field, op)) =
                        crate::search::conditions::date_mask_prefix(&self.input)
                    {
                        let mask = DateMask::new(field, op, self.input.clone());
                        let text = mask.mirror_text();
                        self.date_mask = Some(mask);
                        // 激活即镜像默认日期到输入框（查询仍由 is_composing 暂停）。
                        self.input = text;
                    } else if let Some((field, op)) =
                        crate::search::conditions::size_mask_prefix(&self.input)
                    {
                        let mask = SizeMask::new(field, op, self.input.clone());
                        let text = mask.mirror_text();
                        self.size_mask = Some(mask);
                        // 默认 10 MB 先镜像进输入框，回车即可提交。
                        self.input = text;
                    }
                }
                let mask_active = self.date_mask.is_some() || self.size_mask.is_some();
                let mut text_response = None;
                if self.date_mask.is_some() {
                    self.paint_date_mask(ui, &painter, text_rect);
                } else if self.size_mask.is_some() {
                    self.paint_size_mask(ui, &painter, text_rect);
                } else {
                    let response = ui.put(
                        text_rect,
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
                        // 用户真的改了文本：退出条件编辑 hold 与虚拟 chip 光标；
                        // 联想取消标记只在输入还以该串开头时保留。
                        self.condition_hold = false;
                        self.chip_caret = None;
                        self.suggest_index = 0;
                        self.reset_history_recall();
                        if let Some(dismissed) = &self.suggest_dismissed {
                            let trimmed = self.input.trim();
                            if trimmed.is_empty() || !trimmed.starts_with(dismissed.as_str()) {
                                self.suggest_dismissed = None;
                            }
                        }
                        // IME 组合期(预编辑未上屏)不触发搜索:避免拼音字母逐键闪动
                        // 结果;上屏后由 handle_keys 的组合结束检测补搜。
                        if !self.ime_composing {
                            // 正则模式下整串都是 pattern，不解析日期条件。
                            if !self.filters.matches.regex {
                                self.commit_conditions(ctx);
                            }
                            self.trigger(ctx);
                        }
                    }
                    if response.clicked() {
                        // 点进文本区 = 退出 chip 虚拟光标。
                        self.chip_caret = None;
                    }
                    let window_focused = ctx.input(|i| i.focused);
                    if self.focus_requested || (window_focused && !response.has_focus()) {
                        response.request_focus();
                        self.focus_requested = false;
                    }
                    text_response = Some(response);
                }
                // 条件联想弹层（Foreground，盖在筛选条/结果列表之上）。
                self.paint_condition_suggest(
                    ctx,
                    egui::pos2(search_rect.left(), search_rect.bottom() + 4.0),
                );
                let focused = mask_active
                    || text_response
                        .as_ref()
                        .is_some_and(|response| response.has_focus());
                let focus_color = if focused { theme.amber } else { theme.hair };
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
                            self.clear_input_text();
                            self.selected = 0;
                            self.menu_open = false;
                            self.focus_requested = true;
                            self.trigger(ctx);
                        }
                    }
                }

                self.paint_tabs(ui, &painter, rect);
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
                if self.error.is_none() && self.regex_error.is_none() && self.condition_error.is_none() {
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
                    painter.rect_filled(banner, CornerRadius::same(7), theme.amber_soft);
                    painter.rect_stroke(
                        banner,
                        CornerRadius::same(7),
                        Stroke::new(1.0_f32, theme.amber.gamma_multiply(0.35)),
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
                        error,
                        ui::text_font(11.5),
                        theme.text,
                    );
                    extra_top += 30.0;
                }
                let condition_error = self
                    .conditions_apply(query.scope)
                    .then(|| self.condition_error.clone())
                    .flatten();
                if let Some(error) = self.regex_error.clone().or(condition_error) {
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
                        // 预览高亮同样用产生这批结果的查询词（条件编辑期间
                        // 输入框里是半截条件）。
                        let preview_text = self.results_query_text.clone();
                        self.paint_preview(ui, &painter, pane, entry, &preview_text);
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

                self.paint_footer(ui, &painter, rect, &entries, &query);
                self.paint_menu(ctx, list_rect);
                self.paint_confirm_delete(ctx, rect);
                self.paint_drag_resize(ui, &painter, rect);
            });
    }

    fn paint_tabs(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let theme = self.theme;
        let y = rect.top() + SEARCH_H + 6.0;
        let mut x = rect.left() + PAD;
        let mut clicked = None;
        for scope in Scope::ALL {
            // 与范围 chip 共用同一状态：Tab 高亮不能再看文本解析结果，
            // 否则「chip 显示文件、Tab 高亮全部」会同时出现。
            let active = self.scope == scope;
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
                // 浏览模式下位置由面包屑决定：位置筛选/路径 chip/搜索 chip 都让位。
                self.filters.location = None;
                self.filters.path = None;
                self.filters.keyword = None;
                self.clear_input_text();
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
                    // 浏览模式下位置由面包屑决定：位置筛选/路径 chip/搜索 chip 都让位。
                    self.filters.location = None;
                    self.filters.path = None;
                    self.filters.keyword = None;
                    self.clear_input_text();
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
    /// 引擎查询规格——不需要记语法。
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
                        .unwrap_or_else(|| {
                            if self.filters.modified.is_some() {
                                "自定义"
                            } else {
                                "不限"
                            }
                        })
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
                self.record_condition_undo();
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

    /// 日期分段遮罩渲染：前缀（字段 + 可编辑比较符）+ 年-月-日三段
    /// （未输入部分暗色默认值），焦点段下划线 + 光标，点击段切换焦点。
    /// 输入框 TextEdit 在遮罩期间不渲染。
    fn paint_date_mask(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, text_rect: Rect) {
        let Some(mask) = self.date_mask.clone() else {
            return;
        };
        let theme = self.theme;
        let font = ui::text_font(22.0);
        let center_y = text_rect.center().y;
        let seg_h = MASK_SEG_H;
        let galley_w = |text: &str| {
            painter
                .layout_no_wrap(text.to_string(), font.clone(), theme.text)
                .size()
                .x
        };
        let (mut x, op_clicked) = paint_condition_prefix(
            ui,
            painter,
            center_y,
            text_rect.left(),
            &mask.head,
            &mask.op,
            &mask.tail,
            mask.op_focused,
            Id::new("date-mask-op"),
            &font,
            theme,
        );
        let date_count = if mask.two { 2 } else { 1 };
        let mut focus_request: Option<(usize, usize)> = None;
        for date_idx in 0..date_count {
            if date_idx == 1 {
                painter.text(
                    egui::pos2(x, center_y),
                    Align2::LEFT_CENTER,
                    "..",
                    font.clone(),
                    theme.dim,
                );
                x += galley_w("..");
            }
            for part in 0..mask.part_count(date_idx) {
                if part > 0 {
                    painter.text(
                        egui::pos2(x, center_y),
                        Align2::LEFT_CENTER,
                        "-",
                        font.clone(),
                        theme.dim,
                    );
                    x += galley_w("-");
                }
                let typed = mask.typed_str(date_idx, part).clone();
                let display = mask.display_part(date_idx, part);
                let tail = display[typed.len()..].to_string();
                let width = galley_w(if part == 0 { "0000" } else { "00" });
                let typed_w = galley_w(&typed);
                // 已输入部分（正常色）+ 默认补尾（暗色）。
                if !typed.is_empty() {
                    painter.text(
                        egui::pos2(x, center_y),
                        Align2::LEFT_CENTER,
                        &typed,
                        font.clone(),
                        theme.text,
                    );
                }
                if !tail.is_empty() {
                    painter.text(
                        egui::pos2(x + typed_w, center_y),
                        Align2::LEFT_CENTER,
                        &tail,
                        font.clone(),
                        theme.dim,
                    );
                }
                let focused = mask.date_idx == date_idx && mask.part == part;
                if focused {
                    // 焦点段下划线 + 光标竖线（停在下一段输入位）。
                    let underline_y = center_y + seg_h / 2.0 + 2.0;
                    painter.line_segment(
                        [
                            egui::pos2(x - 2.0, underline_y),
                            egui::pos2(x + width + 2.0, underline_y),
                        ],
                        Stroke::new(1.5_f32, theme.amber),
                    );
                    let caret_x = x + typed_w;
                    painter.line_segment(
                        [
                            egui::pos2(caret_x, center_y - seg_h / 2.0),
                            egui::pos2(caret_x, center_y + seg_h / 2.0),
                        ],
                        Stroke::new(1.5_f32, theme.amber),
                    );
                }
                let hit_rect = Rect::from_min_size(
                    egui::pos2(x, center_y - seg_h / 2.0),
                    egui::vec2(width, seg_h),
                );
                let response =
                    ui.interact(hit_rect, Id::new(("date-mask-seg", date_idx, part)), Sense::click());
                if response.clicked() {
                    focus_request = Some((date_idx, part));
                }
                x += width;
            }
        }
        if op_clicked {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.op_focused = true;
                mask.op_typing.clear();
            }
        }
        if let Some((date_idx, part)) = focus_request {
            if let Some(mask) = self.date_mask.as_mut() {
                mask.date_idx = date_idx;
                mask.part = part;
                mask.op_focused = false;
                mask.op_typing.clear();
                mask.replace_digit = true;
            }
        }
    }

    /// 大小分段遮罩渲染：前缀（字段 + 可编辑比较符）+ 数字段 + 单位段
    /// （默认 10 MB 暗色），焦点段下划线 + 光标，点击段切换焦点。
    fn paint_size_mask(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, text_rect: Rect) {
        let Some(mask) = self.size_mask.clone() else {
            return;
        };
        let theme = self.theme;
        let font = ui::text_font(22.0);
        let center_y = text_rect.center().y;
        let seg_h = MASK_SEG_H;
        let galley_w = |text: &str| {
            painter
                .layout_no_wrap(text.to_string(), font.clone(), theme.text)
                .size()
                .x
        };
        let (mut x, op_clicked) = paint_condition_prefix(
            ui,
            painter,
            center_y,
            text_rect.left(),
            &mask.head,
            &mask.op,
            &mask.tail,
            mask.op_focused,
            Id::new("size-mask-op"),
            &font,
            theme,
        );
        let count = if mask.two { 2 } else { 1 };
        let mut focus_request: Option<(usize, usize)> = None;
        for idx in 0..count {
            if idx == 1 {
                painter.text(
                    egui::pos2(x, center_y),
                    Align2::LEFT_CENTER,
                    "..",
                    font.clone(),
                    theme.dim,
                );
                x += galley_w("..");
            }
            let typed = mask.sizes[idx].number.clone();
            let display = mask.display_number(idx);
            let defaulting = typed.is_empty();
            let typed_w = galley_w(&typed);
            let number_w = galley_w(&display).max(galley_w("00"));
            if defaulting {
                painter.text(
                    egui::pos2(x, center_y),
                    Align2::LEFT_CENTER,
                    &display,
                    font.clone(),
                    theme.dim,
                );
            } else {
                painter.text(
                    egui::pos2(x, center_y),
                    Align2::LEFT_CENTER,
                    &typed,
                    font.clone(),
                    theme.text,
                );
            }
            if mask.idx == idx && mask.part == 0 {
                let underline_y = center_y + seg_h / 2.0 + 2.0;
                painter.line_segment(
                    [
                        egui::pos2(x - 2.0, underline_y),
                        egui::pos2(x + number_w + 2.0, underline_y),
                    ],
                    Stroke::new(1.5_f32, theme.amber),
                );
                let caret_x = x + typed_w;
                painter.line_segment(
                    [
                        egui::pos2(caret_x, center_y - seg_h / 2.0),
                        egui::pos2(caret_x, center_y + seg_h / 2.0),
                    ],
                    Stroke::new(1.5_f32, theme.amber),
                );
            }
            let hit = Rect::from_min_size(
                egui::pos2(x, center_y - seg_h / 2.0),
                egui::vec2(number_w, seg_h),
            );
            if ui
                .interact(hit, Id::new(("size-mask-num", idx)), Sense::click())
                .clicked()
            {
                focus_request = Some((idx, 0));
            }
            x += number_w;

            let unit = mask.unit_label(idx);
            let unit_w = galley_w(unit);
            painter.text(
                egui::pos2(x, center_y),
                Align2::LEFT_CENTER,
                unit,
                font.clone(),
                if defaulting { theme.dim } else { theme.text },
            );
            if mask.idx == idx && mask.part == 1 {
                let underline_y = center_y + seg_h / 2.0 + 2.0;
                painter.line_segment(
                    [
                        egui::pos2(x - 2.0, underline_y),
                        egui::pos2(x + unit_w + 2.0, underline_y),
                    ],
                    Stroke::new(1.5_f32, theme.amber),
                );
                painter.line_segment(
                    [
                        egui::pos2(x, center_y - seg_h / 2.0),
                        egui::pos2(x, center_y + seg_h / 2.0),
                    ],
                    Stroke::new(1.5_f32, theme.amber),
                );
            }
            let hit = Rect::from_min_size(
                egui::pos2(x, center_y - seg_h / 2.0),
                egui::vec2(unit_w, seg_h),
            );
            if ui
                .interact(hit, Id::new(("size-mask-unit", idx)), Sense::click())
                .clicked()
            {
                focus_request = Some((idx, 1));
            }
            x += unit_w;
        }
        if op_clicked {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.op_focused = true;
                mask.op_typing.clear();
            }
        }
        if let Some((idx, part)) = focus_request {
            if let Some(mask) = self.size_mask.as_mut() {
                mask.idx = idx;
                mask.part = part;
                mask.op_focused = false;
                mask.op_typing.clear();
                mask.replace_digit = true;
                mask.unit_typing.clear();
            }
        }
    }

    /// 条件联想弹层：贴在搜索行下方，Foreground 盖住筛选条与结果列表。
    fn paint_condition_suggest(&mut self, ctx: &egui::Context, anchor: egui::Pos2) {
        let suggestions = self.condition_suggestions();
        if suggestions.is_empty() {
            self.suggest_index = 0;
            return;
        }
        let theme = self.theme;
        let index = self.suggest_index.min(suggestions.len() - 1);
        let width = 340.0_f32
            .min(ctx.screen_rect().width() - 2.0 * PAD)
            .max(220.0);
        let row_h = 38.0;
        let footer_h = 22.0;
        let mut clicked: Option<usize> = None;
        let area = egui::Area::new(Id::new("lstr-cond-suggest"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
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
                    for (row_index, suggestion) in suggestions.iter().enumerate() {
                        let (row, response) = ui.allocate_exact_size(
                            egui::vec2(width - 8.0, row_h),
                            Sense::click(),
                        );
                        if row_index == index || response.hovered() {
                            ui.painter().rect_filled(
                                row.shrink(2.0),
                                CornerRadius::same(6),
                                theme.surface_hi,
                            );
                        }
                        ui.painter().text(
                            egui::pos2(row.left() + 12.0, row.center().y),
                            Align2::LEFT_CENTER,
                            suggestion.keyword,
                            ui::mono_font(13.0),
                            theme.amber,
                        );
                        ui.painter().text(
                            egui::pos2(row.left() + 88.0, row.center().y),
                            Align2::LEFT_CENTER,
                            suggestion.description,
                            ui::text_font(12.0),
                            theme.sub,
                        );
                        ui.painter().text(
                            egui::pos2(row.right() - 12.0, row.center().y),
                            Align2::RIGHT_CENTER,
                            "↵ 编辑",
                            ui::mono_font(10.0),
                            theme.dim,
                        );
                        if response.clicked() {
                            clicked = Some(row_index);
                        }
                    }
                    let (footer, _) = ui.allocate_exact_size(
                        egui::vec2(width - 8.0, footer_h),
                        Sense::click(),
                    );
                    ui.painter().text(
                        egui::pos2(footer.left() + 12.0, footer.center().y),
                        Align2::LEFT_CENTER,
                        "↑↓ 选择 · Enter 进入编辑 · Esc 取消",
                        ui::mono_font(10.0),
                        theme.dim,
                    );
                });
            });
        if let Some(row) = clicked {
            self.suggest_index = row;
            self.accept_suggestion(ctx);
            return;
        }
        // 点弹层外面 = 取消联想并立即恢复搜索。
        if ctx.input(|i| i.pointer.any_click()) {
            if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
                if !area.response.rect.contains(pos) {
                    self.dismiss_suggestion(ctx);
                }
            }
        }
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
                            && matches!(
                                self.scope,
                                Scope::App | Scope::Bookmark | Scope::Command
                            )
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
                    FilterChoice::Location(value) => {
                        // 位置与路径 chip 是同一语义：选位置就清掉路径 chip
                        //（后写覆盖），避免两个位置源同时生效。
                        if self.filters.path.is_some() {
                            self.record_condition_undo();
                            self.filters.path = None;
                            self.filters.keyword = None;
                        }
                        self.filters.location = value.clone();
                    }
                    FilterChoice::Time(value) => {
                        // 预设与输入框自定义区间是「替换」而非叠加：选了预设
                        // 就清掉自定义 chip，避免用户看到两个时间条件同时生效。
                        if self.filters.modified.is_some() {
                            self.record_condition_undo();
                        }
                        self.filters.time = *value;
                        self.filters.modified = None;
                    }
                    FilterChoice::Group(value) => self.filters.group = *value,
                }
                self.filter_menu = None;
                // 筛选条改动了 filters：条件编辑/虚拟光标不再对应原状态。
                self.chip_caret = None;
                self.condition_hold = false;
                self.condition_edit_backup = None;
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
        // 行内「拼音 / 模糊 / 关键词高亮」必须用真正产生这批结果的查询词：
        // 条件编辑/联想暂停期间输入框里是半截条件，用它标注会显示成
        // 「拼音 size >= 104857601B」这类错误提示。
        let display_text = self.results_query_text.clone();
        if entries.is_empty() {
            // 空查询（还没开始搜索）：分类两列的搜索技巧面板。
            if self.help_tips_active(query.is_empty()) {
                self.needle_target = 0.0;
                self.scrollable = false;
                // 左列「搜索语法」：位置 / 筛选 / 范围；右列「输入与快捷键」。
                // 分类标题（sub 色）+ 键名（mono sub）+ 说明（mono dim），
                // 窗口窄时退化为单列顺排。
                let syntax: [(&str, &[(&str, &str)]); 3] = [
                    (
                        "位置",
                        &[
                            ("c:  d:", "只搜这些盘，打 C: 即 chip"),
                            ("-c:", "排除某盘，如 ap -c:"),
                            ("path:D:\\目录", "空格/回车收成路径 chip"),
                            ("path:\"带 空格\"", "路径含空格时加引号"),
                            ("目录\\名字", "按路径分段精确查找"),
                        ],
                    ),
                    (
                        "筛选",
                        &[
                            ("ext:rs", "按扩展名"),
                            ("日期", "date>2026-11-10: 今年/2025"),
                            ("大小", "size>10M: 数字/单位分段"),
                            ("Ctrl+Bksp", "删条件；加 Shift 编辑"),
                            ("Ctrl+Z/Y", "撤回 / 取消撤回条件"),
                            ("匹配", "大小写 / 全词 / 正则"),
                        ],
                    ),
                    (
                        "范围 / 搜索",
                        &[
                            ("a 微信", "搜应用，a 收成范围 chip"),
                            ("f 文件", "只搜文件，f 收成范围 chip"),
                            ("> 命令", "命令页，> 收成范围 chip"),
                            ("g rust", "全部当搜索词，只出搜索行"),
                        ],
                    ),
                ];
                let misc: [(&str, &[(&str, &str)]); 2] = [
                    (
                        "输入",
                        &[
                            ("拼音", "全拼或首字母，wx → 微信"),
                            ("Tab", "切换范围"),
                            ("Alt+←", "返回上级范围"),
                            ("Ctrl+↑↓", "上/下一条历史输入"),
                        ],
                    ),
                    (
                        "快捷键",
                        &[
                            ("Esc", "收起（再唤出保留输入）"),
                            ("Ctrl+G", "对话框搜索并填入"),
                            ("Shift+F10", "动作菜单（文件夹也可）"),
                            ("Del", "删除到回收站（需确认）"),
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
                    *y += HELP_GROUP_HEAD_H;
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
                        *y += HELP_ROW_H;
                    }
                    *y += HELP_GROUP_GAP;
                };
                let mut y = list_top + HELP_TOP_PAD;
                painter.text(
                    egui::pos2(x0, y),
                    Align2::LEFT_TOP,
                    "搜索技巧",
                    ui::text_font(12.0),
                    theme.sub,
                );
                y += HELP_TITLE_H;
                for group in syntax {
                    draw_group(painter, x0, &mut y, group);
                }
                if two_col {
                    let mut ry = list_top + HELP_TOP_PAD + HELP_TITLE_H;
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
            // 正在输入日期条件：查询已暂停，给出明确反馈，别让用户以为
            // 「打了一半就搜不到」。
            if self.conditions_apply(query.scope)
                && crate::search::conditions::is_composing(&self.input)
            {
                painter.text(
                    egui::pos2(rect.center().x, list_top + 44.0),
                    Align2::CENTER_CENTER,
                    "正在输入日期/大小/路径/盘符条件",
                    ui::text_font(15.0),
                    theme.text,
                );
                painter.text(
                    egui::pos2(rect.center().x, list_top + 68.0),
                    Align2::CENTER_CENTER,
                    crate::search::conditions::composing_hint(&self.input),
                    ui::mono_font(11.5),
                    theme.dim,
                );
                self.needle_target = 0.0;
                self.scrollable = false;
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
                "试试 ext:rs、date>2026-11-10: 或 size>10M:"
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
        // 滚到底（或键盘 End 跳到末行）时续拉：100→200→…→1000 条。
        // 响应保留滚动位置，用户接着往下扫即可。
        if max_scroll > 0.0 && self.scroll_y >= max_scroll - 1.0 {
            self.load_more(ui.ctx());
        }
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
        let q = display_text.to_lowercase();
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
        // 收藏置顶视觉标记:收藏过的条目副标题加 ★(每帧构建一次集合)。
        let fav_set: std::collections::HashSet<String> =
            self.cfg.favorites.iter().map(|f| f.path.to_lowercase()).collect();
        // 学习徽标:查询词变化时刷新一次(每帧查 HashMap,不重复 SQL)。
        if self.learn_for != display_text {
            self.learn_badges = self
                .history
                .lock()
                .map(|h| h.learn_badges(&display_text))
                .unwrap_or_default();
            self.learn_for = display_text.clone();
        }
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

            let pinyin_hit = matches!(entry.kind, Kind::App | Kind::Bookmark)
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
                && matches!(
                    entry.kind,
                    Kind::App | Kind::Command | Kind::Program | Kind::Bookmark
                )
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
                &display_text
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
                is_path_row && !path_mode && is_pinyin_path_hit(&entry.name, &display_text);
            // 「已打开」(对话框 Quick Switch 的近期目录)与「拼音」是两种
            // 不同来源提示:同时满足时保留两者,读作「已打开 · 拼音 · …」,
            // 先交代条目已在打开列表里,再说明本次为何命中。
            // 拼音标注只显示本次查询词（与应用行「应用 · 拼音 weixin」一致）：
            // 目的只是标明「这条是拼音命中」，不列名字的完整拼音。
            let mut lead = match (entry.id.starts_with("qs:"), pinyin_path_hit) {
                (true, true) => format!("已打开 · 拼音 {} · ", display_text),
                (true, false) => "已打开 · ".to_string(),
                (false, true) => format!("拼音 {} · ", display_text),
                (false, false) => String::new(),
            };
            if let Some(b) = self.learn_badges.get(&entry.id) {
                if b.recent_pick {
                    lead = format!("上次选中 · {lead}");
                } else if b.frequent {
                    lead = format!("常用 · {lead}");
                }
            }
            if is_path_row && fav_set.contains(&full_path.to_lowercase()) {
                lead = format!("★ {lead}");
            }
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
                let needle = if path_mode { "" } else { &display_text };
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
                let mut subtitle = match entry.kind {
                    Kind::App => {
                        if pinyin_hit {
                            format!("应用 · 拼音 {}", display_text)
                        } else if fuzzy_hit.is_some() {
                            format!("应用 · 模糊 {}", display_text)
                        } else if entry.id.starts_with("sys:") {
                            format!("系统功能 · {}", ui::display_path(entry))
                        } else {
                            "应用".to_string()
                        }
                    }
                    Kind::Bookmark => {
                        // 副标题回答「在哪、是哪个网站的」：收藏夹层级（含各级
                        // 目录）+ host 足够区分同名书签；拼音/模糊命中沿用应用行
                        // 的提示方式。
                        let host = entry
                            .path
                            .as_ref()
                            .map(|p| ui::url_host(&p.to_string_lossy()).to_string())
                            .unwrap_or_default();
                        let folder = self
                            .bookmark_folders
                            .get(&entry.id)
                            .map(String::as_str)
                            .unwrap_or("");
                        let where_text = if !folder.is_empty() && !host.is_empty() {
                            format!("{folder} · {host}")
                        } else if !folder.is_empty() {
                            folder.to_string()
                        } else {
                            host
                        };
                        if pinyin_hit {
                            if where_text.is_empty() {
                                format!("书签 · 拼音 {}", display_text)
                            } else {
                                format!("书签 · {where_text} · 拼音 {}", display_text)
                            }
                        } else if fuzzy_hit.is_some() {
                            if where_text.is_empty() {
                                format!("书签 · 模糊 {}", display_text)
                            } else {
                                format!("书签 · {where_text} · 模糊 {}", display_text)
                            }
                        } else if where_text.is_empty() {
                            "书签 · Enter 打开浏览器".to_string()
                        } else {
                            format!("书签 · {where_text} · Enter 打开浏览器")
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
                        } else if entry.id == "calc:" {
                            "计算 · Enter 复制结果".to_string()
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
                // 应用/书签行也显示学习徽标(路径行已在 lead 里体现)。
                if matches!(entry.kind, Kind::App | Kind::Bookmark) {
                    if let Some(b) = self.learn_badges.get(&entry.id) {
                        if b.recent_pick {
                            subtitle = format!("上次选中 · {subtitle}");
                        } else if b.frequent {
                            subtitle = format!("常用 · {subtitle}");
                        }
                    }
                }
                let mut sub_job = ui::text_job_case(
                    &subtitle,
                    if path_mode { "" } else { &display_text },
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
        &mut self,
        ui: &mut egui::Ui,
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

        let mut default_text = if self.date_mask.is_some() || self.size_mask.is_some() {
            // 遮罩编辑态：比较符也能改（← 到比较符段，↑↓ 或直接打 > < =）。
            "编辑条件 · 比较符可改 · 回车确认".to_string()
        } else if !self.condition_suggestions().is_empty() {
            "条件联想 · Enter 进入编辑 · Esc 取消".to_string()
        } else if self.condition_hold {
            "编辑条件 · 回车确认 · Esc 取消".to_string()
        } else if self.conditions_apply(query.scope)
            && crate::search::conditions::is_composing(&self.input)
        {
            "正在输入日期/大小/路径/盘符条件 · 冒号或回车提交".to_string()
        } else if query.is_empty() && self.filters.is_empty() {
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
        // 上一次删除还在：页脚优先显示「已移入回收站：xxx + 撤回」按钮。
        let undo_name = self
            .last_deleted
            .as_ref()
            .filter(|_| active_toast.is_none())
            .map(|item| item.name.clone());
        let left_text = if let Some(name) = &undo_name {
            format!("已移入回收站：{}", ui::middle_ellipsis(name, 28))
        } else {
            active_toast.clone().unwrap_or(default_text)
        };
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
        let mut hints: Vec<(&str, &str)> = if self.date_mask.is_some() || self.size_mask.is_some() {
            vec![
                ("Tab", "跳段"),
                ("←", "比较符"),
                ("↑↓", "调整"),
                ("↵", "确认"),
            ]
        } else if self.in_browse() {
            vec![
                ("↑↓", "选择"),
                ("↵", "进入"),
                ("Alt↑↓", "上/下级"),
                ("Esc", "返回"),
            ]
        } else if path_mode {
            vec![
                ("↑↓", "选择"),
                ("→", "进入"),
                ("Alt↑", "上一级"),
                ("Esc", "清空"),
            ]
        } else if query.is_empty() {
            vec![
                ("↑↓", "选择"),
                ("↵", "打开"),
                ("Tab", "类别"),
                ("Esc", "关闭"),
            ]
        } else {
            vec![
                ("↑↓", "选择"),
                ("↵", "打开"),
                ("→", if selected_is_folder { "进入" } else { "动作" }),
                ("Esc", "关闭"),
            ]
        };
        // 输入框为空且已有条件 chip：提示键盘删除方式。
        if self.input.is_empty() && self.has_input_chips() {
            hints.push(("Ctrl+Bksp", "删条件"));
        }
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
        // 有撤回按钮时再预留 72px，避免文案把按钮挤出可视区。
        let undo_w = if undo_name.is_some() { 72.0 } else { 0.0 };
        let max_w = (right + 12.0 - (rect.left() + PAD) - undo_w).max(80.0);
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
        let text_w = galley.size().x;
        painter.galley(
            egui::pos2(rect.left() + PAD, footer_top + 18.0 - galley.size().y / 2.0),
            galley,
            left_color,
        );
        if let Some(name) = undo_name {
            let x = rect.left() + PAD + text_w + 8.0;
            let pill = Rect::from_min_size(
                egui::pos2(x, footer_top + 8.0),
                egui::vec2(68.0, 20.0),
            );
            let response = ui
                .interact(pill, Id::new("lstr-undo-delete"), Sense::click())
                .on_hover_text(format!("把 {name} 从回收站还原"));
            painter.rect_filled(
                pill,
                CornerRadius::same(6),
                if response.hovered() {
                    theme.amber_soft
                } else {
                    theme.surface_hi
                },
            );
            painter.rect_stroke(
                pill,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, theme.amber.gamma_multiply(0.35)),
                StrokeKind::Inside,
            );
            painter.text(
                pill.center(),
                Align2::CENTER_CENTER,
                "撤回删除",
                ui::text_font(11.0),
                theme.amber,
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if response.clicked() {
                self.undo_last_delete(ui.ctx());
            }
        }
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
        // 位置由面包屑决定（位置筛选/路径 chip/搜索 chip 都让位）；其它筛选
        //（时间/类型/匹配）保留并继续生效。
        self.filters.location = None;
        self.filters.path = None;
        self.filters.keyword = None;
        self.clear_input_text();
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
                self.clear_input_text();
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
        self.clear_input_text();
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
        self.clear_input_text();
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

    /// 输入框当前是否有选区。
    fn text_has_selection(ctx: &egui::Context) -> bool {
        let id = egui::Id::new("lstr-search-input");
        egui::widgets::text_edit::TextEditState::load(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index != range.secondary.index)
            .unwrap_or(false)
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
        // 每条结果右键都查一次历史（打开菜单时一次，不是每帧）。
        self.menu_in_history = self.entries.get(self.selected).is_some_and(|entry| {
            self.history
                .lock()
                .map(|history| history.contains(&entry.id))
                .unwrap_or(false)
        });
    }

    fn menu_step(&mut self, delta: i32) {
        let Some(entry) = self.entries.get(self.selected) else {
            self.menu_open = false;
            return;
        };
        let actions = self.cfg.actions.clone();
        let items = menu_items(entry, &actions, self.menu_in_history);
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
        let items = menu_items(entry, &actions, self.menu_in_history);
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
            MenuAction::DeleteToRecycleBin => {
                let Some(entry) = self.entries.get(self.selected).cloned() else {
                    return;
                };
                // 不立即删：先弹二次确认，确认后才进回收站。
                self.start_delete_confirm(entry, ctx);
            }
        }
    }

    /// 打开删除确认弹层，并把键盘焦点从输入框收走。
    fn start_delete_confirm(&mut self, entry: Entry, ctx: &egui::Context) {
        let Some(path) = entry.path.clone() else {
            return;
        };
        // 焦点交给弹层：否则输入框光标还在闪，方向键看起来「没反应」。
        let input_id = egui::Id::new("lstr-search-input");
        ctx.memory_mut(|memory| memory.surrender_focus(input_id));
        self.pending_delete = Some(PendingDelete {
            id: entry.id,
            name: entry.name,
            path,
            focus_delete: true,
        });
        ctx.request_repaint();
    }

    fn cancel_pending_delete(&mut self, ctx: &egui::Context) {
        self.pending_delete = None;
        self.focus_requested = true;
        ctx.request_repaint();
    }

    fn confirm_pending_delete(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_delete.take() else {
            return;
        };
        match shell::delete_to_recycle_bin(&pending.path) {
            Ok(ticket) => {
                let index = self
                    .entries
                    .iter()
                    .position(|entry| entry.id == pending.id)
                    .unwrap_or(0);
                let entry = self.entries.get(index).cloned();
                if let Some(position) = self
                    .entries
                    .iter()
                    .position(|entry| entry.id == pending.id)
                {
                    self.entries.remove(position);
                    if self.selected >= self.entries.len() {
                        self.selected = self.entries.len().saturating_sub(1);
                    }
                    self.total = self.total.saturating_sub(1);
                }
                if let Ok(mut history) = self.history.lock() {
                    history.remove(&pending.id);
                }
                // 保留回收站票据与原始行：页脚出现「撤回删除」，撤回后行直接回来。
                self.last_deleted = Some(DeletedItem {
                    name: pending.name,
                    entry,
                    index,
                    ticket,
                });
                self.focus_requested = true;
                ctx.request_repaint();
            }
            Err(err) => {
                self.focus_requested = true;
                self.toast(ctx, format!("删除失败：{err}"));
            }
        }
    }

    /// 一键撤回上一次删除：把回收站里的条目移回原位置。
    fn undo_last_delete(&mut self, ctx: &egui::Context) {
        let Some(item) = self.last_deleted.take() else {
            return;
        };
        let Some(ticket) = item.ticket.clone() else {
            self.toast(ctx, "这次删除没有拿到回收站票据，无法一键撤回".to_string());
            return;
        };
        match shell::restore_from_recycle_bin(&ticket) {
            Ok(()) => {
                // 行直接插回原位：索引同步前重新搜索反而会把刚还原的行刷掉。
                if let Some(entry) = item.entry.clone() {
                    let index = item.index.min(self.entries.len());
                    self.entries.insert(index, entry);
                    self.total = self.total.saturating_add(1);
                    self.selected = index;
                    self.scroll_to_selection = true;
                }
                self.toast(ctx, format!("已撤回删除：{}", item.name));
            }
            Err(err) => {
                // 失败保留按钮，用户可以重试（比如先处理同名文件）。
                self.last_deleted = Some(item);
                self.toast(ctx, err);
            }
        }
    }

    /// 删除二次确认：半透明遮罩 + 居中卡片；Enter 确认，Esc / 点遮罩取消。
    fn paint_confirm_delete(&mut self, ctx: &egui::Context, rect: Rect) {
        let Some(pending) = self.pending_delete.clone() else {
            return;
        };
        let theme = self.theme;
        let focus_delete = pending.focus_delete;
        let mut cancel = false;
        let mut confirm = false;
        egui::Area::new(Id::new("lstr-delete-dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                let (_, response) = ui.allocate_exact_size(rect.size(), Sense::click());
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(theme.radius),
                    Color32::from_black_alpha(80),
                );
                if response.clicked() {
                    cancel = true;
                }
            });
        let width = 320.0_f32.min(rect.width() - 48.0);
        let height = 138.0;
        let pos = egui::pos2(
            rect.center().x - width / 2.0,
            rect.center().y - height / 2.0,
        );
        egui::Area::new(Id::new("lstr-delete-confirm"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                let frame = egui::Frame {
                    fill: theme.menu,
                    stroke: Stroke::new(1.0_f32, theme.hair2),
                    corner_radius: CornerRadius::same(12),
                    inner_margin: egui::Margin::same(16),
                    shadow: egui::epaint::Shadow {
                        offset: [0, 12],
                        blur: 34,
                        spread: 0,
                        color: Color32::from_black_alpha(130),
                    },
                    ..Default::default()
                };
                frame.show(ui, |ui| {
                    let content_w = width - 32.0;
                    ui.set_width(content_w);
                    ui.label(
                        egui::RichText::new("删除到回收站？")
                            .font(ui::text_font(15.0))
                            .color(theme.text),
                    );
                    ui.add_space(6.0);
                    let name = ui::middle_ellipsis(&pending.name, 36);
                    ui.label(
                        egui::RichText::new(name)
                            .font(ui::mono_font(12.0))
                            .color(theme.sub),
                    );
                    ui.add_space(3.0);
                    ui.label(
                        egui::RichText::new("←/→ 切换 · Enter 确认 · Esc 取消")
                            .font(ui::mono_font(10.0))
                            .color(theme.dim),
                    );
                    ui.add_space(14.0);
                    let (row, _) =
                        ui.allocate_exact_size(egui::vec2(content_w, 32.0), Sense::hover());
                    let painter = ui.painter().clone();
                    // 两个按钮均分整行宽度：不留右侧空白，视觉更平衡。
                    let gap = 10.0;
                    let button_w = (content_w - gap) / 2.0;
                    let delete_rect =
                        Rect::from_min_size(row.min, egui::vec2(button_w, 30.0));
                    let cancel_rect = Rect::from_min_size(
                        egui::pos2(delete_rect.right() + gap, row.top()),
                        egui::vec2(button_w, 30.0),
                    );
                    let delete_resp =
                        ui.interact(delete_rect, Id::new("lstr-delete-yes"), Sense::click());
                    let cancel_resp =
                        ui.interact(cancel_rect, Id::new("lstr-delete-no"), Sense::click());
                    let warn = if theme.light {
                        Color32::from_rgb(190, 70, 60)
                    } else {
                        Color32::from_rgb(214, 96, 84)
                    };
                    painter.rect_filled(
                        delete_rect,
                        CornerRadius::same(7),
                        if delete_resp.hovered() {
                            warn
                        } else {
                            warn.gamma_multiply(0.88)
                        },
                    );
                    painter.text(
                        delete_rect.center(),
                        Align2::CENTER_CENTER,
                        "删除",
                        ui::text_font(13.0),
                        Color32::WHITE,
                    );
                    if focus_delete {
                        painter.rect_stroke(
                            delete_rect.expand(1.5),
                            CornerRadius::same(8),
                            Stroke::new(2.0_f32, theme.amber),
                            StrokeKind::Inside,
                        );
                    }
                    painter.rect_filled(
                        cancel_rect,
                        CornerRadius::same(7),
                        if cancel_resp.hovered() {
                            theme.hover
                        } else {
                            theme.surface_hi
                        },
                    );
                    painter.text(
                        cancel_rect.center(),
                        Align2::CENTER_CENTER,
                        "取消",
                        ui::text_font(13.0),
                        theme.text,
                    );
                    if !focus_delete {
                        painter.rect_stroke(
                            cancel_rect.expand(1.5),
                            CornerRadius::same(8),
                            Stroke::new(2.0_f32, theme.amber),
                            StrokeKind::Inside,
                        );
                    }
                    if delete_resp.hovered() || cancel_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if delete_resp.clicked() {
                        confirm = true;
                    }
                    if cancel_resp.clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            self.confirm_pending_delete(ctx);
        } else if cancel {
            self.pending_delete = None;
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
        let items = menu_items(&entry, &self.cfg.actions, self.menu_in_history);
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
        // 行的屏幕位置必须减掉滚动偏移：否则列表一滚动，菜单就按"第几行 ×
        // 行高"算到未滚动的原始位置，跑到选中行下方很远的地方。
        let row_top = list_top + self.selected as f32 * theme.row_h - self.scroll_y;
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

        // 预览内容(图片缩略图/文本头部):后台加载,就绪后展示。
        let pkey = preview_key(entry);
        self.previews.request(pkey.clone(), entry);
        let preview = self.previews.get(&pkey);

        // 图片缩略图就绪时顶部画缩略图(替代小图标);否则画 32x32 图标。
        let thumb = match preview {
            Some(PreviewContent::Image(tex)) => Some(tex),
            _ => None,
        };
        if let Some(tex) = thumb {
            let side = (pane.width() - 16.0).min(160.0);
            let thumb_rect =
                Rect::from_min_size(egui::pos2(pane.left() + 8.0, y), egui::vec2(side, side));
            painter.image(
                tex.id(),
                thumb_rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            y += side + 8.0;
        } else {
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
        }

        let kind_label = match entry.kind {
            Kind::File => "文件",
            Kind::Folder => "文件夹",
            Kind::Program => "程序",
            Kind::App => "应用",
            Kind::Command => "命令",
            Kind::Bookmark => "书签",
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
            y += 16.0;
        }
        // 文本速览:元数据下方画文件头部(等宽小字,裁剪到面板剩余高度)。
        if let Some(PreviewContent::Text(text)) = preview {
            y += 6.0;
            let line_h = 14.0;
            let max_y = pane.bottom() - 6.0;
            for line in text.lines() {
                if y + line_h > max_y {
                    break;
                }
                let mut job =
                    ui::text_job_case(line, "", &ui::mono_font(10.5), theme.sub, theme.amber, false);
                job.wrap.max_width = pane.width() - 16.0;
                job.wrap.max_rows = 1;
                job.wrap.overflow_character = Some('…');
                job.wrap.break_anywhere = true;
                let galley = painter.layout_job(job);
                painter.galley(egui::pos2(pane.left() + 8.0, y), galley, theme.sub);
                y += line_h;
            }
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
            (SettingsTab::Noise, "噪声"),
            (SettingsTab::Maintenance, "维护"),
            (SettingsTab::Assets, "资产"),
            (SettingsTab::Learning, "学习数据"),
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
            SettingsTab::Learning => self.paint_settings_learning(ui, ctx, &painter, area),
        }
    }

    fn paint_settings_learning(
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
            "学习数据",
            "从你的打开与选择中学习：常用度随时间衰减，按词学习记住「搜这个词时你要哪个」",
        );

        // 概览统计
        let (nq, np, ni) = self
            .history
            .lock()
            .map(|h| h.learning_stats())
            .unwrap_or((0, 0, 0));
        painter.text(
            egui::pos2(area.left(), area.top() + 48.0),
            Align2::LEFT_TOP,
            format!("已学习 {nq} 个查询词 · {np} 条选择记录 · {ni} 个常用条目"),
            ui::mono_font(11.5),
            theme.sub,
        );

        // 视图切换 + 清空按钮
        let toggle_y = area.top() + 72.0;
        let mk_btn = |ui: &mut egui::Ui, x: f32, label: &str, active: bool| -> bool {
            let r = Rect::from_min_size(egui::pos2(x, toggle_y), egui::vec2(96.0, 26.0));
            let resp = ui.interact(r, Id::new(("lstr-learn-view", label)), Sense::click());
            let bg = if active {
                theme.amber_soft
            } else if resp.hovered() {
                theme.hover
            } else {
                theme.surface_hi
            };
            painter.rect_filled(r, CornerRadius::same(6), bg);
            painter.rect_stroke(
                r,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, if active { theme.amber } else { theme.hair }),
                StrokeKind::Inside,
            );
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                label,
                ui::text_font(11.5),
                if active { theme.text } else { theme.sub },
            );
            resp.clicked()
        };
        if mk_btn(ui, area.left(), "按词学习", !self.learning_view_frecency) {
            self.learning_view_frecency = false;
        }
        if mk_btn(ui, area.left() + 104.0, "常用排行", self.learning_view_frecency) {
            self.learning_view_frecency = true;
        }
        // 清空按词学习(保留使用历史)
        let clear_rect =
            Rect::from_min_size(egui::pos2(area.right() - 120.0, toggle_y), egui::vec2(120.0, 26.0));
        let clear_resp = ui.interact(clear_rect, Id::new("lstr-learn-clear"), Sense::click());
        painter.rect_filled(
            clear_rect,
            CornerRadius::same(6),
            if clear_resp.hovered() { theme.hover } else { theme.surface_hi },
        );
        painter.rect_stroke(
            clear_rect,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, theme.hair),
            StrokeKind::Inside,
        );
        painter.text(
            clear_rect.center(),
            Align2::CENTER_CENTER,
            "清空按词学习",
            ui::text_font(11.5),
            theme.sub,
        );
        if clear_resp.clicked() {
            if let Ok(mut h) = self.history.lock() {
                h.clear_queries();
            }
            self.learn_for.clear(); // 强制徽标缓存刷新
            self.toast(ctx, "已清空按词学习(保留使用历史)".to_string());
        }

        // 表头 + 滚动明细
        let list_top = toggle_y + 38.0;
        let list_rect = Rect::from_min_max(
            egui::pos2(area.left(), list_top),
            egui::pos2(area.right(), area.bottom()),
        );
        let w = list_rect.width();
        let time_w = 72.0;
        let hits_w = 48.0;
        let del_w = 24.0;
        let gap = 8.0;
        let mono = ui::mono_font(11.0);
        let head_font = ui::mono_font(10.5);

        // 列宽:按词学习 = 查询词|选中条目|次数|最近|删;常用排行 = 条目|类型|次数|最近|删
        let (c0_w, c1_w, c0_title, c1_title) = if self.learning_view_frecency {
            let kind_w = 52.0;
            (w - kind_w - hits_w - time_w - del_w - gap * 4.0, kind_w, "条目", "类型")
        } else {
            let q_w = 96.0;
            (q_w, w - q_w - hits_w - time_w - del_w - gap * 4.0, "查询词", "选中条目")
        };
        let x0 = list_rect.left();
        let x1 = x0 + c0_w + gap;
        let x2 = x1 + c1_w + gap;
        let x3 = x2 + hits_w + gap;
        let x4 = x3 + time_w + gap;
        // 表头
        painter.text(egui::pos2(x0, list_top), Align2::LEFT_TOP, c0_title, head_font.clone(), theme.dim);
        painter.text(egui::pos2(x1, list_top), Align2::LEFT_TOP, c1_title, head_font.clone(), theme.dim);
        painter.text(egui::pos2(x2, list_top), Align2::LEFT_TOP, "次数", head_font.clone(), theme.dim);
        painter.text(egui::pos2(x3, list_top), Align2::LEFT_TOP, "最近", head_font, theme.dim);

        let rows_top = list_top + 20.0;
        let rows_rect = Rect::from_min_max(
            egui::pos2(list_rect.left(), rows_top),
            egui::pos2(list_rect.right(), list_rect.bottom()),
        );
        let row_h = 22.0;
        let frecency = self.learning_view_frecency;
        let qrows = if frecency {
            Vec::new()
        } else {
            self.history.lock().map(|h| h.query_hit_rows(300)).unwrap_or_default()
        };
        let frows = if frecency {
            self.history.lock().map(|h| h.frecency_rows(300)).unwrap_or_default()
        } else {
            Vec::new()
        };
        let empty = (frecency && frows.is_empty()) || (!frecency && qrows.is_empty());
        if empty {
            painter.text(
                egui::pos2(x0, rows_top + 8.0),
                Align2::LEFT_TOP,
                "暂无记录——打开/选中结果后自动学习",
                ui::mono_font(11.0),
                theme.dim,
            );
            return;
        }
        // 单条删除请求:循环里只记录,循环后统一写库(避免边借边改)。
        let mut del_qh: Option<(String, String)> = None;
        let mut del_freq: Option<String> = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(rows_rect), |ui| {
            egui::ScrollArea::vertical()
                .id_salt("lstr-learning-rows")
                .max_height(rows_rect.height())
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(rows_rect.width());
                    // 单元格:直接用 painter.galley 左对齐绘制。不用 ui.put——它内部
                    // 是 centered_and_justified 布局,会把短文本居中(Label.halign
                    // 只改 galley 内部对齐,改变不了整体居中)。
                    let cell = |ui: &mut egui::Ui, x: f32, width: f32, text: String, color: Color32| {
                        let r = Rect::from_min_size(egui::pos2(x, ui.cursor().top()), egui::vec2(width, row_h));
                        let mut job = ui::text_job_case(&text, "", &mono, color, color, false);
                        job.wrap.max_width = width;
                        job.wrap.max_rows = 1;
                        job.wrap.break_anywhere = true;
                        job.wrap.overflow_character = Some('…');
                        job.halign = egui::Align::LEFT;
                        let galley = ui.painter().layout_job(job);
                        let y = r.center().y - galley.size().y * 0.5;
                        ui.painter().galley(egui::pos2(x, y), galley, color);
                    };
                    let del_btn = |ui: &mut egui::Ui, x: f32, id: Id| -> bool {
                        let r = Rect::from_min_size(egui::pos2(x, ui.cursor().top()), egui::vec2(del_w, row_h));
                        let resp = ui.interact(r, id, Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        let color = if resp.hovered() { theme.text } else { theme.dim };
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, "×", ui::text_font(13.0), color);
                        resp.clicked()
                    };
                    if frecency {
                        for (idx, (id, name, kind, count, last)) in frows.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.set_height(row_h);
                                let kind_label = match kind {
                                    Kind::File => "文件",
                                    Kind::Folder => "文件夹",
                                    Kind::Program => "程序",
                                    Kind::App => "应用",
                                    Kind::Command => "命令",
                                    Kind::Bookmark => "书签",
                                };
                                cell(ui, x0, c0_w, name.clone(), theme.text);
                                cell(ui, x1, c1_w, kind_label.to_string(), theme.sub);
                                cell(ui, x2, hits_w, count.to_string(), theme.sub);
                                cell(ui, x3, time_w, ui::format_relative_time(*last), theme.dim);
                                if del_btn(ui, x4, Id::new(("lstr-learn-del-f", idx))) {
                                    del_freq = Some(id.clone());
                                }
                            });
                        }
                    } else {
                        for (idx, (q, entry, hits, last)) in qrows.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.set_height(row_h);
                                let name = std::path::Path::new(entry)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| entry.clone());
                                cell(ui, x0, c0_w, q.clone(), theme.text);
                                cell(ui, x1, c1_w, name, theme.sub);
                                cell(ui, x2, hits_w, hits.to_string(), theme.sub);
                                cell(ui, x3, time_w, ui::format_relative_time(*last), theme.dim);
                                if del_btn(ui, x4, Id::new(("lstr-learn-del-q", idx))) {
                                    del_qh = Some((q.clone(), entry.clone()));
                                }
                            });
                        }
                    }
                });
        });
        // 应用单条删除(并刷新徽标缓存)。
        if let Some((q, e)) = del_qh {
            if let Ok(mut h) = self.history.lock() {
                h.remove_query_hit(&q, &e);
            }
            self.learn_for.clear();
        }
        if let Some(id) = del_freq {
            if let Ok(mut h) = self.history.lock() {
                h.remove(&id);
            }
            self.learn_for.clear();
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
        Self::settings_heading(painter, &theme, area, "行为", "唤起热键、一屏行数/滚动加载、对话框联动与开机启动");
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

        Self::settings_label(painter, &theme, area.left(), y, "一屏结果数");
        let mut max_results = self.cfg.max_results as f32;
        let response = ui.put(
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0)),
            egui::Slider::new(&mut max_results, 4.0..=20.0)
                .integer()
                .suffix(" 行"),
        );
        if response.changed() {
            self.cfg.max_results = max_results.round() as usize;
            self.service.set_limit(self.cfg.max_results);
            let _ = self.cfg.save();
            self.trigger(ctx);
        }
        response.on_hover_text(
            "窗口自动高度按这个行数算：一屏显示这么多行。\n\
             应用页、最近使用、Quick Switch 也最多显示这么多条。\n\
             手动拖高窗口可以显示更多行。",
        );
        painter.text(
            egui::pos2(ctrl_x + ctrl_w + 10.0, y + 14.0),
            Align2::LEFT_CENTER,
            "窗口高度；应用/最近使用也受此限制",
            ui::mono_font(10.5),
            theme.dim,
        );
        y += ROW;

        Self::settings_label(painter, &theme, area.left(), y, "滚动加载上限");
        let mut max_load = self.cfg.max_load_results as f32;
        let response = ui.put(
            Rect::from_min_size(egui::pos2(ctrl_x, y), egui::vec2(ctrl_w, 22.0)),
            egui::Slider::new(
                &mut max_load,
                crate::config::MIN_LOAD_RESULTS as f32..=crate::config::MAX_LOAD_RESULTS as f32,
            )
            .integer()
            .suffix(" 条"),
        );
        if response.changed() {
            self.cfg.max_load_results = max_load.round() as usize;
            self.service.set_max_load(self.cfg.max_load_results);
            let _ = self.cfg.save();
            self.trigger(ctx);
        }
        response.on_hover_text(
            "普通搜索首屏固定 100 条（路径模式 200 条）。\n\
             滚到列表底部会继续加载，每档翻倍，最多加载到这里设置的条数。\n\
             调大后单次加载会变慢（候选更多）。",
        );
        painter.text(
            egui::pos2(ctrl_x + ctrl_w + 10.0, y + 14.0),
            Align2::LEFT_CENTER,
            "首屏 100 条，滚到底继续加载到此数",
            ui::mono_font(10.5),
            theme.dim,
        );
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

        painter.text(
            egui::pos2(area.left(), area.bottom() - 40.0),
            Align2::LEFT_TOP,
            "键入即搜：资源管理器内直接打字，无需先唤出 lstr",
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

    /// 「噪声」页:统一规则列表。每行一个行为——排除(整树不索引,重建后
    /// 生效)或降权(片段命中扣分,立即生效)。内置段名(node_modules 等)
    /// 也是普通排除行,可删。改了排除类规则后「立即重建」生效。
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
            "噪声",
            "排除 = 不索引、搜不到、省内存（需重建）；降权 = 仍可搜、命中时沉底（立即生效）",
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
                        let edit_w = (list_rect.width() - 236.0).max(140.0);
                        let response = ui.add_sized(
                            [edit_w, 22.0],
                            egui::TextEdit::singleline(&mut rule.pattern)
                                .hint_text("目录或片段，如 D:\\ProgramData / node_modules / xxx-target/"),
                        );
                        if response.changed() {
                            changed = true;
                        }
                        // 行为切换:点击在 排除/降权 间翻转
                        let label = if rule.exclude { "排除" } else { "降权" };
                        let chip_id = Id::new(("lstr-noise-behavior", index));
                        let chip_w = 52.0;
                        let cur_top = ui.cursor().top();
                        let chip = Rect::from_min_size(
                            egui::pos2(ui.cursor().left() + 6.0, cur_top),
                            egui::vec2(chip_w, 22.0),
                        );
                        let chip_resp = ui.interact(chip, chip_id, Sense::click());
                        if chip_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        ui.painter().rect_filled(
                            chip,
                            CornerRadius::same(5),
                            if rule.exclude { theme.amber_soft } else { theme.surface_hi },
                        );
                        ui.painter().rect_stroke(
                            chip,
                            CornerRadius::same(5),
                            Stroke::new(
                                1.0_f32,
                                if rule.exclude { theme.amber } else { theme.hair },
                            ),
                            StrokeKind::Inside,
                        );
                        ui.painter().text(
                            chip.center(),
                            Align2::CENTER_CENTER,
                            label,
                            ui::text_font(11.0),
                            if rule.exclude { theme.text } else { theme.sub },
                        );
                        if chip_resp.clicked() {
                            rule.exclude = !rule.exclude;
                            changed = true;
                        }
                        if rule.exclude {
                            // 排除行:罚分无意义,占位保持列对齐(是否待重建由
                            // 底部「立即重建」按钮统一表达,不逐行标注)
                            ui.allocate_rect(
                                Rect::from_min_size(
                                    egui::pos2(chip.right() + 6.0, cur_top),
                                    egui::vec2(96.0, 22.0),
                                ),
                                Sense::hover(),
                            );
                        } else {
                            let mut penalty = rule.penalty as f32;
                            let slider = ui.put(
                                Rect::from_min_size(
                                    egui::pos2(chip.right() + 6.0, cur_top),
                                    egui::vec2(96.0, 22.0),
                                ),
                                egui::Slider::new(&mut penalty, 0.0..=300.0).integer(),
                            );
                            if slider.changed() {
                                rule.penalty = penalty.round() as i32;
                                changed = true;
                            }
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

        // 排除集是否已生效:归一化后比较(顺序无关)
        let norm = |p: &String| p.trim().to_lowercase().replace('\\', "/");
        let current: std::collections::BTreeSet<String> = rules
            .iter()
            .filter(|r| r.exclude)
            .map(|r| norm(&r.pattern))
            .filter(|p| !p.is_empty())
            .collect();
        let applied: std::collections::BTreeSet<String> = self
            .noise_exclude_applied
            .iter()
            .map(norm)
            .filter(|p| !p.is_empty())
            .collect();
        let needs_rebuild = current != applied;

        // 底部：添加规则 / 立即重建 / 恢复默认
        let mk_btn = |ui: &mut egui::Ui, x: f32, w: f32, id: &str, label: &str, accent: bool| -> bool {
            let r = Rect::from_min_size(egui::pos2(x, area.bottom() - 28.0), egui::vec2(w, 24.0));
            let resp = ui.interact(r, Id::new(id), Sense::click());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            painter.rect_filled(r, CornerRadius::same(6), if accent { theme.amber_soft } else { theme.hover });
            painter.rect_stroke(
                r,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, if accent { theme.amber } else { theme.hair }),
                StrokeKind::Inside,
            );
            painter.text(r.center(), Align2::CENTER_CENTER, label, ui::text_font(11.5), theme.text);
            resp.clicked()
        };
        if mk_btn(ui, area.left(), 104.0, "lstr-noise-add", "＋ 添加规则", false) {
            rules.push(crate::config::NoiseRule {
                pattern: String::new(),
                penalty: 100,
                exclude: false,
            });
            changed = true;
        }
        if needs_rebuild
            && mk_btn(ui, area.left() + 112.0, 110.0, "lstr-noise-rebuild", "立即重建", true)
        {
            self.cfg.rank.noise_rules = rules.clone();
            let _ = self.cfg.save();
            let patterns = rules
                .iter()
                .filter(|r| r.exclude)
                .map(|r| r.pattern.clone())
                .collect();
            self.service.rebuild_noise_index(patterns);
            self.noise_exclude_applied = rules
                .iter()
                .filter(|r| r.exclude)
                .map(|r| r.pattern.clone())
                .collect();
            self.toast(ctx, "正在重建索引（后台进行，完成即生效）".to_string());
            return;
        }
        if needs_rebuild {
            // 待重建提示:紧挨按钮,直接回答「怎么才算生效」
            painter.text(
                egui::pos2(area.left() + 232.0, area.bottom() - 16.0),
                Align2::LEFT_CENTER,
                "改动尚未生效：点「立即重建」后台重扫（重启 lstr 也会自动重扫）",
                ui::mono_font(10.0),
                theme.amber,
            );
        }
        if mk_btn(
            ui,
            area.right() - 96.0,
            92.0,
            "lstr-noise-reset",
            "恢复默认",
            false,
        ) {
            // 默认 = 内置排除段名(排除行) + 降权规则默认表
            rules = lstr_index::noise::builtin_patterns()
                .into_iter()
                .map(|pattern| crate::config::NoiseRule { pattern, penalty: 0, exclude: true })
                .chain(crate::config::default_noise_rules())
                .collect();
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
                format!("{base} · 后端:内置索引")
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
                let mut line = format!(
                    "{} 卷 {}:{}  {} 万项 · {}MB",
                    mark, v.letter, pct, v.count / 10_000, v.heap_mb
                );
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

        // —— 浏览器书签 ——
        // 书签只进「全部」结果（筛选条生效时不混入）；关闭后 worker 立即
        // 清空索引，再打开会重新读取浏览器收藏夹文件。
        Self::settings_label(painter, &theme, area.left(), y, "浏览器书签");
        let bm_rect = Rect::from_min_size(
            egui::pos2(area.left() + 130.0, y),
            egui::vec2((area.right() - area.left() - 130.0).max(120.0), 30.0),
        );
        let mut bookmarks_on = self.cfg.index.bookmarks;
        if Self::settings_checkbox(
            ui,
            painter,
            &theme,
            Id::new("lstr-idx-bookmarks"),
            bm_rect,
            &mut bookmarks_on,
        ) {
            self.cfg.index.bookmarks = bookmarks_on;
            let _ = self.cfg.save();
            self.service.set_bookmarks_enabled(bookmarks_on);
            // 立即重载/清空，不用等 120s TTL。
            self.service.invalidate_apps();
            self.trigger(ctx);
        }
        painter.text(
            egui::pos2(bm_rect.left() + 30.0, bm_rect.center().y),
            Align2::LEFT_CENTER,
            "Chrome / Edge / Brave / Vivaldi / Firefox · 即时生效",
            ui::text_font(11.0),
            theme.sub,
        );
        y += ROW;

        // —— 内置索引实时层 ——
        // 程序只使用自研内置索引；实时层决定索引怎么追文件变化。
        let current_incremental = self.cfg_index_incremental();
        const INCREMENTALS: [(&str, &str); 2] = [
            ("usn", "USN journal(默认)"),
            ("watch", "目录监视"),
        ];
        const BACKEND_CHIP_H: f32 = 26.0;
        const BACKEND_CHIP_GAP: f32 = 8.0;
        const BACKEND_COLS: usize = 2;
        let font = ui::text_font(10.5);
        let start_x = area.left() + 130.0;
        let avail = (area.right() - start_x).max(160.0);
        let chip_w = ((avail - BACKEND_CHIP_GAP * (BACKEND_COLS - 1) as f32) / BACKEND_COLS as f32).floor();
        let groups: [(&str, bool, &[(&str, &str)], &str); 1] = [
            ("内置索引实时层", true, &INCREMENTALS, current_incremental),
        ];
        for (label, incremental_group, items, current) in groups {
            Self::settings_label(painter, &theme, area.left(), y, label);
            for (i, (key, text)) in items.iter().enumerate() {
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
                painter.text(rect.center(), Align2::CENTER_CENTER, *text, font.clone(), if accent { theme.amber } else { theme.text });
                if response.clicked() {
                    if incremental_group {
                        self.set_cfg_index_incremental(key);
                    }
                }
            }
            let rows = (items.len() + BACKEND_COLS - 1) / BACKEND_COLS;
            y += rows as f32 * BACKEND_CHIP_H + (rows.saturating_sub(1)) as f32 * BACKEND_CHIP_GAP + 10.0;
        }

        // 噪声排除开关已移入「噪声」页(统一规则列表,支持自定义排除目录)。
        y += ROW;

        // 帮助:按实际生效时机分开写——切卷立即生效,实时层要重启。
        // 这里用 layout+wrap 按 area 宽度自动折行:文案再长也不会跑出右边界
        // (painter.text 是 no-wrap,旧版单行长段落其实早就被窗口右侧裁掉了)。
        let help = "内置索引实时层:USN journal(默认)=目录首扫后按 journal 增量,快照完整且游标可信时免扫直读,卷不可用自动退回目录监视;目录监视=ReadDirectoryChangesW,零 USN FSCTL。\n\
索引卷勾选立即生效(worker 自动起停该盘扫描);实时层需重启 lstr 生效;噪声排除见「噪声」页。\n\
日期/时间筛选依赖索引里的文件修改时间(目录扫描写入)。\n\
内置索引只覆盖本机固定 NTFS 卷。";
        let galley = painter.layout(
            help.to_string(),
            ui::text_font(10.5),
            theme.sub,
            area.width(),
        );
        painter.galley(egui::pos2(area.left(), y + 8.0), galley, theme.sub);
    }

    fn cfg_index_incremental(&self) -> &'static str {
        if self.cfg.index.incremental == "watch" {
            "watch"
        } else {
            "usn"
        }
    }

    fn set_cfg_index_incremental(&mut self, key: &str) {
        self.cfg.index.incremental = key.to_string();
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
    DeleteToRecycleBin,
    CustomAction(usize),
}

/// 删除确认弹层的数据（菜单「删除到回收站」/ Del 键二次确认）。
#[derive(Clone, Debug)]
struct PendingDelete {
    id: String,
    name: String,
    path: std::path::PathBuf,
    /// true = 焦点在「删除」，false = 焦点在「取消」；←/→/Tab 切换。
    focus_delete: bool,
}

/// 上一次删除到回收站的条目：保留回收站票据与原始行，支持一键撤回。
#[derive(Clone, Debug)]
struct DeletedItem {
    name: String,
    entry: Option<Entry>,
    index: usize,
    ticket: Option<crate::win::shell::RecycleTicket>,
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
    Learning = 7,
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

fn menu_items(entry: &Entry, actions: &[crate::assets::ActionDef], in_history: bool) -> Vec<MenuItem> {
    let is_command = entry.kind == Kind::Command;
    let is_program = entry.kind == Kind::Program;
    let is_file = entry.kind == Kind::File;
    let is_bookmark = entry.kind == Kind::Bookmark;
    // 应用（开始菜单 .lnk / App Paths / UWP）也允许尝试提权：普通 .lnk 与
    // 系统工具（appwiz.cpl、taskmgr）都能提权；UWP 由执行层给出明确提示。
    let can_admin = is_program || is_file || (entry.kind == Kind::App && entry.path.is_some());
    // 「在终端打开」只对文件系统条目有意义（文件夹自身 / 文件所在目录）。
    let can_terminal = entry.path.is_some()
        && matches!(entry.kind, Kind::Folder | Kind::File | Kind::Program);
    // 删除只对真实文件系统条目开放；应用（快捷方式/UWP）、命令不删。
    let can_delete = entry.path.is_some()
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
            enabled: !is_command && !is_bookmark,
            action: MenuAction::Reveal,
        },
        MenuItem {
            label: if is_bookmark { "复制链接" } else { "复制路径" }.to_string(),
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
    ];
    // 只有真正在「最近使用」历史里的条目才显示移除项；普通搜索结果
    // 没有可移除的东西，显示出来只会让人误以为能清掉搜索结果。
    if in_history {
        items.push(MenuItem {
            label: "从最近使用中移除".to_string(),
            shortcut: "",
            enabled: true,
            action: MenuAction::RemoveHistory,
        });
    }
    items.push(MenuItem {
        label: "加入收藏".to_string(),
        shortcut: "",
        // 收藏是 Quick Switch 的文件/文件夹/应用入口，URL 加进去没有语义。
        enabled: !is_command && !is_bookmark,
        action: MenuAction::AddFavorite,
    });
    if can_delete {
        items.push(MenuItem {
            label: "删除到回收站…".to_string(),
            shortcut: "",
            enabled: true,
            action: MenuAction::DeleteToRecycleBin,
        });
    }
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

        // 常驻启动器不该占任务栏：窗口「隐藏」只是移到屏幕外，留着任务栏
        // 按钮就是一个点不动的图标。winit 在可见性等标志重同步时会把
        // WS_EX_APPWINDOW 算回来（按钮幽灵式复活），所以每帧廉价校验一次。
        if let Some(hwnd) = self.hwnd {
            window::detach_from_taskbar(hwnd);
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
        self.previews.drain(ctx);

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
        let ime_was_composing = self.ime_composing;
        self.handle_keys(ctx);
        // IME 组合结束(上屏/取消):补一次搜索,确保最终文本被搜到。
        // 组合期不触发搜索(防拼音字母逐键闪动),上屏后拼音匹配仍生效。
        if ime_was_composing && !self.ime_composing {
            self.trigger(ctx);
        }
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
        Scope::App | Scope::Bookmark | Scope::Command => "对话框模式…",
    }
}

fn hint_for(scope: Scope) -> &'static str {
    match scope {
        Scope::All => "搜索文件、应用、书签或命令…",
        Scope::Folder => "搜索文件夹…",
        Scope::Program => "搜索可执行程序…",
        Scope::File => "搜索文件…",
        Scope::App => "搜索应用…",
        Scope::Bookmark => "搜索书签…",
        Scope::Command => "搜索命令…",
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

    /// 回归：书签图标回退只属于书签。应用条目必须走自己的路径取 shell 图标，
    /// 否则「远程桌面连接 / UU远程」会全部显示成 Edge 浏览器图标。
    #[test]
    fn icon_source_keeps_apps_on_shell_icons() {
        match icon_source(&menu_entry(Kind::App, Some(r"C:\Start Menu\远程桌面连接.lnk"))) {
            IconSource::Shell(path) => {
                assert!(path.ends_with("远程桌面连接.lnk"), "应用应走 shell 图标: {path}")
            }
            _ => panic!("应用条目不能走 favicon/类型图标"),
        }
        match icon_source(&menu_entry(Kind::Bookmark, Some("https://example.com/"))) {
            IconSource::Favicon(url) => assert_eq!(url, "https://example.com/"),
            _ => panic!("书签必须优先 favicon"),
        }
        assert!(matches!(
            icon_source(&menu_entry(Kind::Folder, Some(r"D:\work"))),
            IconSource::Generic(_, _)
        ));
        assert!(matches!(
            icon_source(&menu_entry(Kind::File, Some(r"D:\a.txt"))),
            IconSource::Generic(_, _)
        ));
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
            false,
        );
        assert!(item(&app, MenuAction::Admin).enabled);
        let uwp = menu_items(
            &menu_entry(
                Kind::App,
                Some(r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"),
            ),
            &[],
            false,
        );
        assert!(
            item(&uwp, MenuAction::Admin).enabled,
            "打包应用也应可尝试，执行层会给出明确提示"
        );
        let no_path = menu_items(&menu_entry(Kind::App, None), &[], false);
        assert!(!item(&no_path, MenuAction::Admin).enabled);
        let folder = menu_items(&menu_entry(Kind::Folder, Some(r"C:\work")), &[], false);
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
            let items = menu_items(&menu_entry(kind, Some(path)), &[], false);
            assert!(item(&items, MenuAction::OpenTerminal).enabled, "{kind:?} 应可终端打开");
        }
        for (kind, path) in [
            (Kind::App, Some(r"C:\Start Menu\Terminal.lnk")),
            (Kind::Command, None),
        ] {
            let items = menu_items(&menu_entry(kind, path), &[], false);
            assert!(!item(&items, MenuAction::OpenTerminal).enabled, "{kind:?} 不应终端打开");
        }
    }

    #[test]
    fn menu_only_offers_history_removal_when_recorded() {
        let file = menu_entry(Kind::File, Some(r"C:\work\notes.txt"));
        let not_recorded = menu_items(&file, &[], false);
        assert!(
            !not_recorded
                .iter()
                .any(|item| item.action == MenuAction::RemoveHistory),
            "普通搜索结果不应出现「从最近使用中移除」"
        );
        let recorded = menu_items(&file, &[], true);
        assert!(item(&recorded, MenuAction::RemoveHistory).enabled);
    }

    #[test]
    fn delete_is_only_offered_for_file_system_entries() {
        for (kind, path, expected) in [
            (Kind::File, Some(r"C:\work\notes.txt"), true),
            (Kind::Folder, Some(r"C:\work"), true),
            (Kind::Program, Some(r"C:\Tools\app.exe"), true),
            (Kind::App, Some(r"C:\Start Menu\Terminal.lnk"), false),
            (Kind::Command, None, false),
        ] {
            let items = menu_items(&menu_entry(kind, path), &[], false);
            let has = items
                .iter()
                .any(|item| item.action == MenuAction::DeleteToRecycleBin);
            assert_eq!(has, expected, "{kind:?}");
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

    #[test]
    fn left_chip_target_prefers_virtual_caret() {
        assert_eq!(left_chip_index(None, 0, true), None);
        // 无虚拟光标：文本在开头时 chip 全在左侧，最右 chip 是左邻。
        assert_eq!(left_chip_index(None, 3, true), Some(2));
        assert_eq!(left_chip_index(None, 3, false), None);
        // 有虚拟光标：光标左侧的 chip；最左就没有左邻。
        assert_eq!(left_chip_index(Some(2), 3, true), Some(1));
        assert_eq!(left_chip_index(Some(1), 3, true), Some(0));
        assert_eq!(left_chip_index(Some(0), 3, true), None);
        // ← 从文本开头进 chip 区落在最后一个 chip 右侧（边界 = count）：
        // 编辑/删除命中的是最后一个 chip，和没光标时一致。
        assert_eq!(left_chip_index(Some(3), 3, true), Some(2));
    }

    #[test]
    fn chip_caret_moves_between_boundaries() {
        // ← 从文本区进入：落在最后一个 chip 右侧，然后一路左移，最左停在 1。
        assert_eq!(chip_caret_left(None, 3), Some(3));
        assert_eq!(chip_caret_left(Some(3), 3), Some(2));
        assert_eq!(chip_caret_left(Some(2), 3), Some(1));
        assert_eq!(chip_caret_left(Some(1), 3), Some(1), "边界 0 没有左邻");
        // → 右移，最后一个边界之后回到文本区。
        assert_eq!(chip_caret_right(Some(1), 3), Some(2));
        assert_eq!(chip_caret_right(Some(2), 3), Some(3));
        assert_eq!(chip_caret_right(Some(3), 3), None);
        assert_eq!(chip_caret_right(None, 3), None);
    }

    #[test]
    fn inactive_chip_predicate_only_dims_conditions() {
        use ConditionChipKind::*;
        // 条件生效时全部可操作（点击/编辑/删除）。
        for kind in [Scope, Keyword, Date, Size, Path, Drive(0)] {
            assert!(!chip_inactive(kind, true), "{kind:?}");
        }
        // 条件不生效（应用/书签/命令页、正则、路径模式、网络搜索）：
        // 只有范围/搜索 chip 还能操作，其余只置灰展示。
        assert!(!chip_inactive(Scope, false));
        assert!(!chip_inactive(Keyword, false));
        assert!(chip_inactive(Date, false));
        assert!(chip_inactive(Size, false));
        assert!(chip_inactive(Path, false));
        assert!(chip_inactive(Drive(0), false));
    }

    #[test]
    fn clicking_chip_selects_that_chip() {
        // 点第 i 个 chip → 光标放在它右侧（i + 1），编辑/删除都命中它自己。
        for i in 0..4 {
            assert_eq!(left_chip_index(Some(i + 1), 4, true), Some(i));
        }
    }

    #[test]
    fn caret_after_remove_keeps_logical_position() {
        // 删光标右侧的 chip：光标左移一位。
        assert_eq!(caret_after_remove(Some(2), 0, 2), Some(1));
        // 删光标左侧的 chip：光标位置不变（仍指向同一个后续 chip）。
        assert_eq!(caret_after_remove(Some(1), 1, 2), Some(1));
        // 光标在最后一个 chip 右侧时删第一个：仍停在新的最后右侧。
        assert_eq!(caret_after_remove(Some(3), 0, 2), Some(2));
        // 第一个 chip 左侧没有左邻：光标夹到 1。
        assert_eq!(caret_after_remove(Some(1), 0, 1), Some(1));
        // 删到没有 chip：回到文本区。
        assert_eq!(caret_after_remove(Some(0), 0, 0), None);
        assert_eq!(caret_after_remove(None, 2, 2), None);
    }

    #[test]
    fn condition_terminator_is_stripped_for_editing() {        assert_eq!(
            strip_condition_terminator("date>2026-11-10:"),
            "date>2026-11-10"
        );
        assert_eq!(strip_condition_terminator("size>10M： "), "size>10M");
        assert_eq!(
            strip_condition_terminator("size 10M..100M"),
            "size 10M..100M"
        );
    }

    #[test]
    fn condition_enter_text_terminates_only_paths() {
        // 路径 chip 还原文本没有终结符 → 回车补冒号。
        assert_eq!(condition_enter_text(r"path:D:\work"), r"path:D:\work:");
        // 引号路径右引号本身就是终结符，不需要再补。
        assert_eq!(
            condition_enter_text(r#"path:"C:\Program Files""#),
            r#"path:"C:\Program Files""#
        );
        // 已有终结符 / 不需要终结符的 chip 原样提交。
        assert_eq!(condition_enter_text("C:"), "C:");
        assert_eq!(condition_enter_text("a "), "a ");
        assert_eq!(condition_enter_text(">"), ">");
        // 半截路径（只打了 `path:`）保持原样，交给 composing 继续等。
        assert_eq!(condition_enter_text("path:"), "path:");
        // 查询词 + 路径 chip：路径在末尾，同样补终结符。
        assert_eq!(
            condition_enter_text(r"report path:D:\work"),
            r"report path:D:\work:"
        );
    }

    #[test]
    fn path_chip_display_and_edit_text_roundtrip() {
        // 短路径原样显示；长路径只留末尾，前面加省略号。
        assert_eq!(shorten_path_for_chip(r"D:\work", 26), r"D:\work");
        let long = r"D:\a\very\deep\nested\folder\project";
        let short = shorten_path_for_chip(long, 26);
        assert!(short.starts_with('…'), "{short}");
        assert!(long.ends_with(short.trim_start_matches("…\\")), "{short}");
        assert!(short.chars().count() <= 26 + 2, "{short}");

        // 编辑文本：无空格直接还原；有空格自动加引号（重新解析一致）。
        assert_eq!(
            path_edit_text(std::path::Path::new(r"D:\work")),
            r"path:D:\work"
        );
        assert_eq!(
            path_edit_text(std::path::Path::new(r"C:\Program Files")),
            r#"path:"C:\Program Files""#
        );

        // 范围前缀文本（范围 chip 还原用）。
        assert_eq!(scope_prefix_text(Scope::App), "a ");
        assert_eq!(scope_prefix_text(Scope::File), "f ");
        assert_eq!(scope_prefix_text(Scope::Command), ">");
        assert_eq!(scope_prefix_text(Scope::All), "");
    }

    fn condition_snap(input: &str) -> ConditionSnapshot {
        ConditionSnapshot {
            filters: SearchFilters::default(),
            input: input.to_string(),
            chip_caret: None,
            scope: Scope::All,
        }
    }

    #[test]
    fn condition_history_undo_redo_roundtrip() {
        let mut history = ConditionHistory::default();
        let before = condition_snap("size>10M:");
        history.record(before.clone());
        assert!(history.can_undo());
        assert!(!history.can_redo());

        let after = condition_snap("");
        let previous = history.undo(after.clone()).expect("undo");
        assert_eq!(previous, before);
        assert!(!history.can_undo());
        assert!(history.can_redo());

        let next = history.redo(before.clone()).expect("redo");
        assert_eq!(next, after);
        assert!(history.can_undo());
        assert!(!history.can_redo());
    }

    #[test]
    fn condition_history_new_record_clears_redo_and_dedupes() {
        let mut history = ConditionHistory::default();
        history.record(condition_snap("a"));
        let previous = history.undo(condition_snap("b")).expect("undo");
        assert_eq!(previous, condition_snap("a"));
        assert!(history.can_redo());

        history.record(condition_snap("c"));
        assert!(!history.can_redo(), "新操作必须清空 redo");
        history.record(condition_snap("c"));
        assert_eq!(history.undo.len(), 1, "连续相同状态不重复入栈");
    }

    #[test]
    fn condition_history_forgets_last_on_cancel() {
        let mut history = ConditionHistory::default();
        let before = condition_snap("a");
        history.record(before.clone());
        history.forget_last(&before.filters, "a", Scope::All);
        assert!(!history.can_undo(), "取消编辑应撤掉进入编辑的快照");
    }

    fn date_mask_op(op: &str) -> DateMask {
        DateMask::new("date".into(), op.to_string(), format!("date{op}"))
    }

    #[test]
    fn date_mask_year_feeds_and_advances() {
        let mut mask = date_mask_op(">");
        for c in "2025".chars() {
            mask.feed_digit(c);
        }
        assert_eq!(mask.dates[0].y, "2025");
        assert_eq!((mask.date_idx, mask.part), (0, 1), "年满 4 位跳到月");
        let today = crate::search::conditions::today_ymd();
        assert_eq!(
            mask.mirror_text(),
            format!("date>2025-{:02}-{:02}", today.1, today.2),
            "月/日未输入用今天默认补足"
        );
    }

    #[test]
    fn date_mask_month_smart_digits() {
        // 首位 3：补 0 立即跳段。
        let mut mask = date_mask_op("");
        mask.part = 1;
        mask.feed_digit('3');
        assert_eq!(mask.dates[0].m, "03");
        assert_eq!(mask.part, 2);
        // 首位 1：等待；第二位 9 非法拒绝，2 合法跳段。
        let mut mask = date_mask_op("");
        mask.part = 1;
        mask.feed_digit('1');
        assert_eq!(mask.dates[0].m, "1");
        assert_eq!(mask.part, 1, "等待第二位");
        mask.feed_digit('9');
        assert_eq!(mask.dates[0].m, "1", "13 非法被拒");
        mask.feed_digit('2');
        assert_eq!(mask.dates[0].m, "12");
        assert_eq!(mask.part, 2);
        // 首位 0：等待，默认补尾显示 01..09。
        let mut mask = date_mask_op("");
        mask.part = 1;
        mask.feed_digit('0');
        let today = crate::search::conditions::today_ymd();
        let expected = if today.1 >= 10 { "01" } else { "00" };
        assert_eq!(mask.display_part(0, 1), expected, "首位 0 + 默认补尾");
    }

    #[test]
    fn date_mask_spin_wraps_month_and_clamps_day() {
        let mut mask = date_mask_op("");
        mask.part = 1;
        mask.dates[0].m = "12".into();
        mask.spin(1);
        assert_eq!(mask.dates[0].m, "01", "月回绕");
        mask.spin(-1);
        assert_eq!(mask.dates[0].m, "12");
        let mut mask = date_mask_op("");
        mask.part = 2;
        mask.dates[0].m = "12".into();
        mask.dates[0].d = "31".into();
        mask.spin(1);
        assert_eq!(mask.dates[0].d, "31", "日夹紧到当月天数（12 月有 31 天）");
        mask.spin(-1);
        assert_eq!(mask.dates[0].d, "30");
        let mut mask = date_mask_op("");
        mask.part = 0;
        mask.dates[0].y = "1970".into();
        mask.spin(-1);
        assert_eq!(mask.dates[0].y, "1970", "年下界夹紧");
    }

    #[test]
    fn date_mask_commit_builds_canonical_text() {
        // 等值区间（裸值）：date A..B:
        let mut mask = date_mask_op("");
        mask.dates[0] = DateMask::from_ymd((2025, 1, 1));
        mask.dates[1] = DateMask::from_ymd((2025, 12, 31));
        mask.two = true;
        assert_eq!(mask.commit_text(), "date 2025-01-01..2025-12-31:");
        // 带操作符：date>=A:
        let mut mask = date_mask_op(">=");
        mask.dates[0] = DateMask::from_ymd((2026, 11, 13));
        assert_eq!(mask.commit_text(), "date>=2026-11-13:");
        // 未输完的段在提交时按显示文本固化（typed "1" + 默认补尾 "1" → 11 月）。
        let mut mask = date_mask_op(">");
        mask.dates[0].y = "2025".into();
        mask.dates[0].m = "1".into();
        mask.dates[0].d.clear();
        let today = crate::search::conditions::today_ymd();
        let text = mask.commit_text();
        assert_eq!(text, format!("date>2025-11-{:02}:", today.2), "{text:?}");
        // 中文段名保留。
        let mut mask = DateMask::new("日期".into(), ">".into(), String::new());
        mask.dates[0] = DateMask::from_ymd((2026, 1, 1));
        assert_eq!(mask.commit_text(), "日期>2026-01-01:");
    }

    #[test]
    fn date_mask_partial_text_keeps_typed_only() {
        let mut mask = date_mask_op(">");
        for c in "2026".chars() {
            mask.feed_digit(c);
        }
        assert_eq!(mask.partial_text(), "date>2026", "默认段不带入自由文本");
        mask.feed_digit('1');
        mask.feed_digit('1');
        assert_eq!(mask.partial_text(), "date>2026-11");
        // 完整日期镜像 = 全量显示。
        let mut mask = date_mask_op(">");
        mask.dates[0] = DateMask::from_ymd((2026, 11, 13));
        assert_eq!(mask.mirror_text(), "date>2026-11-13");
    }

    #[test]
    fn size_mask_defaults_and_units() {
        let mut mask = SizeMask::new("size".into(), ">".into(), "size>".into());
        // 默认 10 MB，回车即可提交。
        assert_eq!(mask.mirror_text(), "size>10MB");
        for c in "25".chars() {
            mask.feed_digit(c);
        }
        assert_eq!(mask.number().as_str(), "25");
        // Tab 到单位段，↑ 换档到 GB。
        mask.advance();
        assert_eq!((mask.idx, mask.part), (0, 1));
        mask.spin(1);
        assert_eq!(mask.unit_label(0), "GB");
        assert_eq!(mask.mirror_text(), "size>25GB");
        assert_eq!(mask.commit_text(), "size>25GB:");
    }

    #[test]
    fn size_mask_spin_and_range_text() {
        let mut mask = SizeMask::new("size".into(), String::new(), "size ".into());
        mask.sizes[0] = SizeMask::from_parts(("10".into(), 2));
        mask.sizes[1] = SizeMask::from_parts(("100".into(), 2));
        mask.two = true;
        assert_eq!(mask.mirror_text(), "size 10MB..100MB");
        mask.spin(1);
        assert_eq!(mask.sizes[0].number, "11");
        mask.idx = 1;
        mask.part = 0;
        mask.spin(-1);
        assert_eq!(mask.sizes[1].number, "99");
        assert_eq!(mask.commit_text(), "size 11MB..99MB:");
    }

    #[test]
    fn size_mask_partial_text_keeps_typed_only() {
        let mut mask = SizeMask::new("size".into(), ">".into(), "size>".into());
        assert_eq!(
            mask.partial_text_for_exit(" "),
            "size>",
            "默认值不带入自由文本"
        );
        mask.feed_digit('5');
        assert_eq!(mask.partial_text_for_exit(" "), "size>5MB");
        // 继续输单位字母时不能带默认单位，否则会拼成 10MBM。
        mask.feed_digit('0');
        assert_eq!(mask.partial_text_for_exit("M"), "size>50");
        assert_eq!(mask.partial_text_for_exit(" "), "size>50MB");
    }

    #[test]
    fn date_mask_year_only_edits_year_segment() {
        use crate::search::conditions::DateEditValue;

        // chip 编辑带来的年份条件：只渲染年段，提交仍是年份文本。
        let mut mask = DateMask::new("date".into(), ">".into(), "date > ".into());
        mask.from_chip_edit = true;
        apply_date_edit_value(&mut mask, 0, DateEditValue::Year(2026));
        assert_eq!(mask.part_count(0), 1, "年份条件只有年段");
        assert_eq!(mask.mirror_text(), "date > 2026");

        // 第一次输入数字整体覆盖旧年份；满 4 位不跳段（没有月/日）。
        mask.feed_digit('2');
        assert_eq!(mask.dates[0].y.as_str(), "2", "chip 旧值被整体覆盖");
        for c in "025".chars() {
            mask.feed_digit(c);
        }
        assert_eq!(mask.dates[0].y.as_str(), "2025");
        assert_eq!((mask.date_idx, mask.part), (0, 0), "年份不自动跳段");
        mask.advance();
        assert_eq!((mask.date_idx, mask.part), (0, 0), "唯一一段无处可跳");
        mask.spin(1);
        assert_eq!(mask.dates[0].y.as_str(), "2026");
        assert_eq!(mask.commit_text(), "date>2026:");
    }

    #[test]
    fn date_mask_year_range_edits_both_years() {
        use crate::search::conditions::DateEditValue;

        let mut mask = DateMask::new("date".into(), String::new(), "date ".into());
        mask.from_chip_edit = true;
        apply_date_edit_value(&mut mask, 0, DateEditValue::Year(2023));
        apply_date_edit_value(&mut mask, 1, DateEditValue::Year(2025));
        mask.two = true;
        assert_eq!(mask.mirror_text(), "date 2023..2025");
        mask.advance();
        assert_eq!((mask.date_idx, mask.part), (1, 0), "年段直接跳到下一侧");
        mask.retreat();
        assert_eq!((mask.date_idx, mask.part), (0, 0));
        mask.dates[1].y = "2024".into();
        assert_eq!(mask.commit_text(), "date 2023..2024:");
    }

    #[test]
    fn size_mask_chip_edit_replaces_old_value_and_takes_units() {
        let mut mask = SizeMask::new("size".into(), ">".into(), "size>".into());
        mask.from_chip_edit = true;
        mask.sizes[0] = SizeMask::from_parts(("10".into(), 2));

        // chip 带来的旧值在第一次输入数字时整体覆盖。
        mask.feed_digit('5');
        assert_eq!(mask.number().as_str(), "5");
        mask.feed_digit('0');
        assert_eq!(mask.number().as_str(), "50");

        // 字母按单位输入：G → GB、GB → GB，焦点自动落到单位段。
        assert!(mask.feed_unit_letter('g'));
        assert_eq!((mask.idx, mask.part), (0, 1));
        assert_eq!(mask.unit_label(0), "GB");
        assert!(mask.feed_unit_letter('B'));
        assert_eq!(mask.unit_label(0), "GB");
        assert_eq!(mask.mirror_text(), "size>50GB");
        assert_eq!(mask.commit_text(), "size>50GB:");

        // 不认识的大写/其它字母不接受，交给调用方转普通查询。
        assert!(!mask.feed_unit_letter('X'));

        // 自动激活的遮罩保持原有追加行为（不覆盖用户已输入的数字）。
        let mut auto = SizeMask::new("size".into(), ">".into(), "size>".into());
        auto.feed_digit('1');
        auto.feed_digit('0');
        auto.advance();
        auto.retreat();
        auto.feed_digit('5');
        assert_eq!(auto.number().as_str(), "105");
    }

    #[test]
    fn mask_prefix_splits_operator_for_painting() {
        assert_eq!(
            split_op_prefix("size>", ">"),
            ("size".to_string(), String::new())
        );
        assert_eq!(
            split_op_prefix("size > ", ">"),
            ("size ".to_string(), " ".to_string())
        );
        assert_eq!(
            split_op_prefix("date:", ""),
            ("date:".to_string(), String::new())
        );
        assert_eq!(
            split_op_prefix("date>=", ">="),
            ("date".to_string(), String::new())
        );
    }

    #[test]
    fn date_mask_operator_can_be_edited() {
        let today = date_mask_fmt(crate::search::conditions::today_ymd());

        // 自动激活的 `date>`：不退出遮罩，直接打 `<` 就地改比较符。
        let mut mask = date_mask_op(">");
        assert!(mask.feed_op_char('<'));
        assert_eq!(mask.op, "<");
        assert_eq!(mask.mirror_text(), format!("date<{today}"));
        assert_eq!(mask.commit_text(), format!("date<{today}:"));

        // 连续输入比较符合并成更长的写法：`>` 再 `=` → `>=`。
        let mut mask = date_mask_op(">");
        assert!(mask.feed_op_char('>'));
        assert!(mask.feed_op_char('='));
        assert_eq!(mask.op, ">=");

        // ← 从第一个值段到比较符段，↑↓ 换档，→ 回到值段。
        let mut mask = date_mask_op(">");
        mask.retreat();
        assert!(mask.op_focused, "第一个值段往左 = 比较符段");
        mask.spin(1);
        assert_eq!(mask.op, ">=");
        mask.spin(-1);
        assert_eq!(mask.op, ">");
        mask.advance();
        assert!(!mask.op_focused);
        assert_eq!((mask.date_idx, mask.part), (0, 0));

        // 比较符段上直接输数字：焦点回到值段，比较符不动。
        mask.retreat();
        assert!(mask.op_focused);
        mask.feed_digit('2');
        assert!(!mask.op_focused);
        assert_eq!(mask.dates[0].y, "2");
        assert_eq!(mask.op, ">");

        // 闭区间（`A..B` 没有比较符）不接受比较符输入。
        let mut range = date_mask_op("");
        range.two = true;
        assert!(!range.feed_op_char('<'));
    }

    #[test]
    fn size_mask_operator_can_be_edited() {
        let mut mask = SizeMask::new("size".into(), ">".into(), "size>".into());
        assert!(mask.feed_op_char('<'));
        assert_eq!(mask.mirror_text(), "size<10MB");
        assert_eq!(mask.commit_text(), "size<10MB:");

        // 带空格的原始前缀：改比较符后格式原样保留。
        let mut spaced = SizeMask::new("size".into(), ">".into(), "size > ".into());
        assert!(spaced.feed_op_char('='));
        assert_eq!(spaced.mirror_text(), "size = 10MB");

        // 紧凑写法的冒号留在前缀里（以前镜像会丢成 `size10MB`）。
        let mut compact = SizeMask::new("size".into(), String::new(), "size:".into());
        assert!(compact.feed_op_char('>'));
        assert_eq!(compact.mirror_text(), "size:>10MB");

        // ← 进入比较符段，↑↓ 换档，→ 回到数字段。
        let mut mask = SizeMask::new("size".into(), ">".into(), "size>".into());
        mask.retreat();
        assert!(mask.op_focused);
        mask.spin(-1);
        assert_eq!(mask.op, "<");
        mask.advance();
        assert!(!mask.op_focused);
        assert_eq!((mask.idx, mask.part), (0, 0));

        // 闭区间不接受比较符输入。
        let mut range = SizeMask::new("size".into(), String::new(), "size ".into());
        range.two = true;
        assert!(!range.feed_op_char('<'));
    }

    #[test]
    fn mask_commit_text_parses_after_operator_change() {
        use crate::search::conditions::extract;

        // 日期：改成 `<` 后提交文本仍是一条合法条件（只设 before）。
        let mut date = date_mask_op(">");
        assert!(date.feed_op_char('<'));
        let text = date.commit_text();
        let out = extract(&text);
        assert_eq!(out.dates.len(), 1, "{text:?}");
        assert!(out.dates[0].after.is_none(), "{text:?}");
        assert!(out.dates[0].before.is_some(), "{text:?}");

        // 大小：改成 `=` 后提交文本解析成等值区间。
        let mut size = SizeMask::new("size".into(), ">".into(), "size>".into());
        assert!(size.feed_op_char('='));
        let text = size.commit_text();
        let out = extract(&text);
        assert_eq!(out.sizes.len(), 1, "{text:?}");
        assert_eq!(out.sizes[0].after, out.sizes[0].before, "{text:?}");

        // chip 编辑文本（sources 原文）也要能再进遮罩，比较符不丢。
        let parts = crate::search::conditions::parse_size_edit_parts(
            crate::search::conditions::strip_terminator(&text),
        )
        .expect("edit parts");
        assert_eq!(parts.0, "=");
    }

    #[test]
    fn size_mask_bare_prefix_is_equality() {
        use crate::search::conditions::extract;

        // 只打 `size:` 进遮罩：和 `date:` 一样不显示比较符，裸值默认「等于」。
        let mut mask = SizeMask::new("size".into(), String::new(), "size:".into());
        assert_eq!(mask.mirror_text(), "size:10MB");
        let text = mask.commit_text();
        assert_eq!(text, "size:10MB:");
        let out = extract(&text);
        assert_eq!(out.sizes.len(), 1, "{text:?}");
        assert_eq!(out.sizes[0].after, out.sizes[0].before, "裸值 = 精确大小");

        // 默认档改比较符照样生效。
        assert!(mask.feed_op_char('<'));
        assert_eq!(mask.commit_text(), "size:<10MB:");
        let out = extract(&mask.commit_text());
        assert!(out.sizes[0].after.is_none(), "改 < 后只设 before");
        assert!(out.sizes[0].before.is_some());
    }

    #[test]
    fn condition_op_text_and_empty_mask_state() {
        for op in [">", "<", "=", "≥", "≤", "＞", "＜", "＝"] {
            assert!(is_condition_op_text(op), "{op:?}");
        }
        for text in ["", "y", "10", ">y", "今天"] {
            assert!(!is_condition_op_text(text), "{text:?}");
        }

        // 自动激活的遮罩：值没输入 → 允许继续打比较符/别名（保留前缀）。
        let date = DateMask::new("date".into(), ">".into(), "date > ".into());
        assert!(date.all_typed_empty() && !date.from_chip_edit);
        let size = SizeMask::new("size".into(), ">".into(), "size>".into());
        assert!(size.typed_empty() && !size.from_chip_edit);

        // 紧凑写法的冒号在退出文本里要保留（`size:` + `>` → `size:>`）。
        let mut compact = SizeMask::new("size".into(), String::new(), "size:".into());
        assert_eq!(compact.partial_text_for_exit(">"), "size:");
        compact.feed_digit('1');
        assert_eq!(compact.partial_text_for_exit(">"), "size:1MB");
        assert!(!compact.typed_empty());
    }
}
