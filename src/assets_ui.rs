//! 设置页「资产」：收藏 / 自定义动作 / 自定义命令。
//!
//! 布局：顶部分类切换 → 中间单一列表（占满可用高度、可滚动）→ 底部固定编辑条。
//! 一次只处理一类，避免三块 CRUD 纵向堆叠造成的留白与嵌套滚动。
//! 视觉沿用项目设置页的 painter 手绘语言（圆角、主题色、自绘按钮），
//! 不用 egui 原生 Grid / 斑马纹，风格与「噪声规则」等页一致。

use crate::assets::{ActionDef, CommandDef, Favorite};
use crate::config::Config;
use crate::ui::{self, Theme};
use eframe::egui::{
    self, Align2, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind,
};

const ROW_H: f32 = 30.0;
const TAB_H: f32 = 28.0;
const EDITOR_H: f32 = 152.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AssetTab {
    #[default]
    Favorite,
    Action,
    Command,
}

impl AssetTab {
    const ALL: [AssetTab; 3] = [AssetTab::Favorite, AssetTab::Action, AssetTab::Command];

    fn label(self) -> &'static str {
        match self {
            AssetTab::Favorite => "收藏",
            AssetTab::Action => "动作",
            AssetTab::Command => "命令",
        }
    }

    fn count(self, cfg: &Config) -> usize {
        match self {
            AssetTab::Favorite => cfg.favorites.len(),
            AssetTab::Action => cfg.actions.len(),
            AssetTab::Command => cfg.commands.len(),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            AssetTab::Favorite => "在搜索结果与快速切换中置顶常用目录",
            AssetTab::Action => "对选中的搜索结果执行程序；{action_path} 会被替换为结果路径",
            AssetTab::Command => "在搜索框用触发词调用程序；{query} 是触发词之后的内容",
        }
    }
}

pub struct AssetUiState {
    pub tab: AssetTab,
    selected: Option<usize>,
    filter: String,
    f_name: String,
    f_path: String,
    a_title: String,
    a_program: String,
    a_args: String,
    a_admin: bool,
    c_trigger: String,
    c_title: String,
    c_program: String,
    c_args: String,
    notice: Option<String>,
}

impl Default for AssetUiState {
    fn default() -> Self {
        Self {
            tab: AssetTab::Favorite,
            selected: None,
            filter: String::new(),
            f_name: String::new(),
            f_path: String::new(),
            a_title: String::new(),
            a_program: String::new(),
            a_args: String::new(),
            a_admin: false,
            c_trigger: String::new(),
            c_title: String::new(),
            c_program: String::new(),
            c_args: String::new(),
            notice: None,
        }
    }
}

fn shorten(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head: String = chars[..max.saturating_sub(1)].iter().collect();
    format!("{head}…")
}

fn chip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    theme: &Theme,
    rect: Rect,
    label: &str,
    active: bool,
    index: usize,
) -> bool {
    // Id 用固定下标，避免标签里的计数变化导致交互状态错位。
    let response = ui.interact(rect, Id::new(("lstr-asset-tab", index)), Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if active || response.hovered() {
        painter.rect_filled(
            rect,
            CornerRadius::same(6),
            if active { theme.amber_soft } else { theme.hover },
        );
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        ui::text_font(12.5),
        if active { theme.amber } else { theme.sub },
    );
    if active {
        let bar = Rect::from_min_size(
            egui::pos2(rect.left() + 14.0, rect.bottom() - 3.0),
            egui::vec2(rect.width() - 28.0, 2.0),
        );
        painter.rect_filled(bar, CornerRadius::same(1), theme.amber);
    }
    response.clicked()
}

fn action_button(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    theme: &Theme,
    rect: Rect,
    label: &str,
    enabled: bool,
) -> bool {
    let response = ui.interact(rect, Id::new(("lstr-asset-btn", label)), Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let fill = if !enabled {
        theme.hover
    } else if response.hovered() {
        theme.surface_hi
    } else {
        theme.hover
    };
    painter.rect_filled(rect, CornerRadius::same(6), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0_f32, theme.hair),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        ui::text_font(11.5),
        if enabled { theme.text } else { theme.dim },
    );
    enabled && response.clicked()
}

fn field_label(painter: &egui::Painter, theme: &Theme, x: f32, y: f32, text: &str) {
    painter.text(
        egui::pos2(x, y + 11.0),
        Align2::LEFT_CENTER,
        text,
        ui::text_font(12.0),
        theme.sub,
    );
}

fn field(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    theme: &Theme,
    x: f32,
    y: f32,
    width: f32,
    label: &str,
    value: &mut String,
    hint: &str,
) {
    field_label(painter, theme, x, y, label);
    let rect = Rect::from_min_size(egui::pos2(x + 46.0, y), egui::vec2(width - 46.0, 22.0));
    ui.put(rect, egui::TextEdit::singleline(value).hint_text(hint));
}

/// 返回 true 表示配置发生变更（调用方负责保存配置并同步搜索服务）。
pub fn paint(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    theme: &Theme,
    area: Rect,
    cfg: &mut Config,
    state: &mut AssetUiState,
) -> bool {
    let mut changed = false;
    let w = area.width();

    // —— 顶部分类切换 ——
    let tab_w = (w - 16.0) / 3.0;
    for (index, tab) in AssetTab::ALL.iter().enumerate() {
        let rect = Rect::from_min_size(
            egui::pos2(area.left() + index as f32 * (tab_w + 8.0), area.top()),
            egui::vec2(tab_w, TAB_H),
        );
        let label = format!("{}  {}", tab.label(), tab.count(cfg));
        if chip(ui, painter, theme, rect, &label, state.tab == *tab, index) {
            state.tab = *tab;
            state.selected = None;
            state.filter.clear();
            clear_form(state);
        }
    }

    // —— 筛选行 ——
    let filter_y = area.top() + TAB_H + 8.0;
    field_label(painter, theme, area.left(), filter_y, "筛选");
    ui.put(
        Rect::from_min_size(
            egui::pos2(area.left() + 46.0, filter_y),
            egui::vec2((w - 130.0).max(120.0), 22.0),
        ),
        egui::TextEdit::singleline(&mut state.filter).hint_text("输入名称或路径片段"),
    );
    if action_button(
        ui,
        painter,
        theme,
        Rect::from_min_size(
            egui::pos2(area.right() - 74.0, filter_y),
            egui::vec2(74.0, 22.0),
        ),
        "清除",
        !state.filter.is_empty(),
    ) {
        state.filter.clear();
    }

    // —— 列表（占满剩余高度，单一滚动区） ——
    // 窗口被拖小时也要给列表留出可读的高度。
    let editor_top = (area.bottom() - EDITOR_H).max(area.top() + 190.0);
    let list_rect = Rect::from_min_max(
        egui::pos2(area.left(), filter_y + 32.0),
        egui::pos2(area.right(), editor_top - 8.0),
    );
    let needle = state.filter.trim().to_lowercase();
    let mut clicked: Option<usize> = None;
    let mut shown = 0usize;

    let rows = collect_rows(state.tab, cfg);
    ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("lstr-assets-list")
            .max_height(list_rect.height())
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(list_rect.width());
                let p = ui.painter().clone();
                for (index, (primary, secondary)) in rows.iter().enumerate() {
                    if !needle.is_empty()
                        && !primary.to_lowercase().contains(&needle)
                        && !secondary.to_lowercase().contains(&needle)
                    {
                        continue;
                    }
                    shown += 1;
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(list_rect.width() - 6.0, ROW_H),
                        Sense::click(),
                    );
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }

                    let selected = state.selected == Some(index);
                    if selected || response.hovered() {
                        p.rect_filled(
                            rect.shrink(2.0),
                            CornerRadius::same(6),
                            if selected { theme.sel } else { theme.hover },
                        );
                    }
                    p.line_segment(
                        [
                            egui::pos2(rect.left() + 4.0, rect.bottom()),
                            egui::pos2(rect.right() - 4.0, rect.bottom()),
                        ],
                        Stroke::new(1.0_f32, theme.hair),
                    );
                    p.text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        primary,
                        ui::text_font(12.5),
                        theme.text,
                    );
                    p.text(
                        egui::pos2(rect.right() - 10.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        shorten(secondary, 34),
                        ui::mono_font(10.5),
                        theme.dim,
                    );
                    if response.clicked() {
                        clicked = Some(index);
                    }
                }
                if shown == 0 {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(list_rect.width() - 6.0, ROW_H),
                        Sense::hover(),
                    );
                    p.text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        if needle.is_empty() {
                            "暂无条目，在下方添加"
                        } else {
                            "没有匹配的条目"
                        },
                        ui::text_font(12.0),
                        theme.dim,
                    );
                }
            });
    });

    if let Some(index) = clicked {
        state.selected = Some(index);
        fill_form(state.tab, cfg, index, state);
        state.notice = None;
    }

    // —— 底部编辑条 ——
    painter.line_segment(
        [
            egui::pos2(area.left(), editor_top),
            egui::pos2(area.right(), editor_top),
        ],
        Stroke::new(1.0_f32, theme.hair),
    );
    let editing = state.selected.is_some();
    painter.text(
        egui::pos2(area.left(), editor_top + 6.0),
        Align2::LEFT_TOP,
        if editing { "编辑选中项" } else { "新增条目" },
        ui::text_font(12.5),
        theme.text,
    );

    let field_w = (w - 16.0) / 2.0;
    let y1 = editor_top + 28.0;
    let y2 = y1 + 28.0;
    let right_x = area.left() + field_w + 16.0;
    match state.tab {
        AssetTab::Favorite => {
            field(ui, painter, theme, area.left(), y1, field_w, "名称", &mut state.f_name, "可留空");
            field(ui, painter, theme, right_x, y1, field_w, "路径", &mut state.f_path, r"D:\work");
        }
        AssetTab::Action => {
            field(ui, painter, theme, area.left(), y1, field_w, "标题", &mut state.a_title, "菜单名称");
            field(ui, painter, theme, right_x, y1, field_w, "程序", &mut state.a_program, "code");
            field(ui, painter, theme, area.left(), y2, field_w, "参数", &mut state.a_args, r#""{action_path}""#);
            ui.put(
                Rect::from_min_size(egui::pos2(right_x + 46.0, y2), egui::vec2(field_w - 46.0, 22.0)),
                egui::Checkbox::new(&mut state.a_admin, "管理员运行"),
            );
        }
        AssetTab::Command => {
            field(ui, painter, theme, area.left(), y1, field_w, "触发词", &mut state.c_trigger, "note");
            field(ui, painter, theme, right_x, y1, field_w, "标题", &mut state.c_title, "新建笔记");
            field(ui, painter, theme, area.left(), y2, field_w, "程序", &mut state.c_program, "notepad.exe");
            field(ui, painter, theme, right_x, y2, field_w, "参数", &mut state.c_args, "{query}");
        }
    }

    let hint_y = y2 + 26.0;
    painter.text(
        egui::pos2(area.left(), hint_y),
        Align2::LEFT_TOP,
        state.tab.hint(),
        ui::mono_font(10.5),
        theme.dim,
    );

    let btn_y = editor_top + EDITOR_H - 28.0;
    let bw = 88.0;
    if action_button(
        ui, painter, theme,
        Rect::from_min_size(egui::pos2(area.left(), btn_y), egui::vec2(bw, 24.0)),
        if editing { "保存" } else { "添加" },
        true,
    ) {
        match save_form(state.tab, cfg, state) {
            Ok(()) => {
                changed = true;
                state.notice = Some(if editing { "已保存" } else { "已添加" }.to_string());
                state.selected = None;
                clear_form(state);
            }
            Err(err) => state.notice = Some(err),
        }
    }
    if action_button(
        ui, painter, theme,
        Rect::from_min_size(egui::pos2(area.left() + bw + 8.0, btn_y), egui::vec2(bw, 24.0)),
        "删除",
        editing,
    ) {
        if let Some(index) = state.selected {
            remove_at(state.tab, cfg, index);
            changed = true;
            state.selected = None;
            clear_form(state);
            state.notice = Some("已删除".to_string());
        }
    }
    if action_button(
        ui, painter, theme,
        Rect::from_min_size(egui::pos2(area.left() + (bw + 8.0) * 2.0, btn_y), egui::vec2(bw, 24.0)),
        "新建",
        editing,
    ) {
        state.selected = None;
        clear_form(state);
        state.notice = None;
    }
    if let Some(notice) = &state.notice {
        painter.text(
            egui::pos2(area.right(), btn_y + 12.0),
            Align2::RIGHT_CENTER,
            notice,
            ui::text_font(11.0),
            theme.sub,
        );
    }
    changed
}

fn collect_rows(tab: AssetTab, cfg: &Config) -> Vec<(String, String)> {
    match tab {
        AssetTab::Favorite => cfg
            .favorites
            .iter()
            .map(|f| (f.name.clone(), f.path.clone()))
            .collect(),
        AssetTab::Action => cfg
            .actions
            .iter()
            .map(|a| {
                (
                    a.title.clone(),
                    format!("{} {} {}", a.program, a.args, if a.admin { "· 提权" } else { "" }),
                )
            })
            .collect(),
        AssetTab::Command => cfg
            .commands
            .iter()
            .map(|c| (c.trigger.clone(), format!("{} · {} {}", c.title, c.program, c.args)))
            .collect(),
    }
}

fn fill_form(tab: AssetTab, cfg: &Config, index: usize, s: &mut AssetUiState) {
    match tab {
        AssetTab::Favorite => {
            if let Some(f) = cfg.favorites.get(index) {
                s.f_name = f.name.clone();
                s.f_path = f.path.clone();
            }
        }
        AssetTab::Action => {
            if let Some(a) = cfg.actions.get(index) {
                s.a_title = a.title.clone();
                s.a_program = a.program.clone();
                s.a_args = a.args.clone();
                s.a_admin = a.admin;
            }
        }
        AssetTab::Command => {
            if let Some(c) = cfg.commands.get(index) {
                s.c_trigger = c.trigger.clone();
                s.c_title = c.title.clone();
                s.c_program = c.program.clone();
                s.c_args = c.args.clone();
            }
        }
    }
}

fn clear_form(s: &mut AssetUiState) {
    s.f_name.clear();
    s.f_path.clear();
    s.a_title.clear();
    s.a_program.clear();
    s.a_args.clear();
    s.a_admin = false;
    s.c_trigger.clear();
    s.c_title.clear();
    s.c_program.clear();
    s.c_args.clear();
}

fn save_form(tab: AssetTab, cfg: &mut Config, s: &mut AssetUiState) -> Result<(), String> {
    match tab {
        AssetTab::Favorite => {
            let path = s.f_path.trim().to_string();
            if path.is_empty() {
                return Err("请填写路径".to_string());
            }
            if cfg
                .favorites
                .iter()
                .enumerate()
                .any(|(i, f)| f.path.eq_ignore_ascii_case(&path) && s.selected != Some(i))
            {
                return Err("该路径已在收藏中".to_string());
            }
            let name = if s.f_name.trim().is_empty() {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone())
            } else {
                s.f_name.trim().to_string()
            };
            let item = Favorite { name, path };
            if let Some(index) = s.selected {
                if let Some(slot) = cfg.favorites.get_mut(index) {
                    *slot = item;
                }
            } else {
                cfg.favorites.push(item);
            }
        }
        AssetTab::Action => {
            let title = s.a_title.trim().to_string();
            let program = s.a_program.trim().to_string();
            if title.is_empty() || program.is_empty() {
                return Err("需要标题和程序".to_string());
            }
            // 表单里没有 workdir/silent 字段：编辑既有条目时必须沿用原值，
            // 否则只改标题也会把 config.toml 里手工配的 workdir/silent 抹掉。
            let (workdir, silent) = s
                .selected
                .and_then(|index| cfg.actions.get(index))
                .map(|prev| (prev.workdir.clone(), prev.silent))
                .unwrap_or_default();
            let item = ActionDef {
                title,
                program,
                args: s.a_args.trim().to_string(),
                workdir,
                admin: s.a_admin,
                silent,
            };
            if let Some(index) = s.selected {
                if let Some(slot) = cfg.actions.get_mut(index) {
                    *slot = item;
                }
            } else {
                cfg.actions.push(item);
            }
        }
        AssetTab::Command => {
            let trigger = s.c_trigger.trim().to_string();
            let title = s.c_title.trim().to_string();
            let program = s.c_program.trim().to_string();
            if trigger.is_empty() || title.is_empty() || program.is_empty() {
                return Err("需要触发词、标题和程序".to_string());
            }
            if cfg
                .commands
                .iter()
                .enumerate()
                .any(|(i, c)| c.trigger.eq_ignore_ascii_case(&trigger) && s.selected != Some(i))
            {
                return Err("触发词已被占用".to_string());
            }
            // 同 Action：表单不暴露 workdir，编辑时必须保留原值。
            let workdir = s
                .selected
                .and_then(|index| cfg.commands.get(index))
                .map(|prev| prev.workdir.clone())
                .unwrap_or_default();
            let item = CommandDef {
                trigger,
                title,
                program,
                args: s.c_args.trim().to_string(),
                workdir,
            };
            if let Some(index) = s.selected {
                if let Some(slot) = cfg.commands.get_mut(index) {
                    *slot = item;
                }
            } else {
                cfg.commands.push(item);
            }
        }
    }
    Ok(())
}

fn remove_at(tab: AssetTab, cfg: &mut Config, index: usize) {
    match tab {
        AssetTab::Favorite => {
            if index < cfg.favorites.len() {
                cfg.favorites.remove(index);
            }
        }
        AssetTab::Action => {
            if index < cfg.actions.len() {
                cfg.actions.remove(index);
            }
        }
        AssetTab::Command => {
            if index < cfg.commands.len() {
                cfg.commands.remove(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_starts_on_favorites_without_selection() {
        let state = AssetUiState::default();
        assert_eq!(state.tab, AssetTab::Favorite);
        assert!(state.selected.is_none());
        assert!(state.filter.is_empty());
    }

    #[test]
    fn shorten_keeps_short_text_and_truncates_long_text() {
        assert_eq!(shorten("D:\\work", 34), "D:\\work");
        let long = "D:\\a\\very\\long\\path\\that\\keeps\\going\\and\\going\\forever";
        let short = shorten(long, 20);
        assert!(short.chars().count() <= 20);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn save_form_adds_then_edits_a_favorite() {
        let mut cfg = Config::default();
        let mut s = AssetUiState::default();
        s.f_path = "D:\\work".to_string();
        assert!(save_form(AssetTab::Favorite, &mut cfg, &mut s).is_ok());
        assert_eq!(cfg.favorites.len(), 1);
        assert_eq!(cfg.favorites[0].name, "work");

        s.selected = Some(0);
        s.f_name = "工作".to_string();
        s.f_path = "D:\\work".to_string();
        assert!(save_form(AssetTab::Favorite, &mut cfg, &mut s).is_ok());
        assert_eq!(cfg.favorites.len(), 1);
        assert_eq!(cfg.favorites[0].name, "工作");
    }

    #[test]
    fn duplicate_path_is_rejected_but_editing_self_is_allowed() {
        let mut cfg = Config::default();
        let mut s = AssetUiState::default();
        s.f_path = "D:\\work".to_string();
        assert!(save_form(AssetTab::Favorite, &mut cfg, &mut s).is_ok());
        s.selected = None;
        assert!(save_form(AssetTab::Favorite, &mut cfg, &mut s).is_err());
        s.selected = Some(0);
        assert!(save_form(AssetTab::Favorite, &mut cfg, &mut s).is_ok());
    }
}
