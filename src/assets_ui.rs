//! 设置页「资产」：可视化管理收藏 / 自定义动作 / 自定义命令。
//!
//! 独立模块，只依赖 `config / assets / egui`，不碰 Win32，
//! 这样 `app.rs` 里只需要一个分页入口，合并时冲突面最小。
//!
//! 可读性与可扩展性约定：
//! - 不再叠加 `.small()` + `.weak()`（字号与对比度同时削弱会看不清）；
//! - 三个可增长列表各自有独立限高滚动视口、计数与筛选，不会顶掉别的分区。

use crate::assets::{ActionDef, CommandDef, Favorite};
use crate::config::Config;
use eframe::egui::{self, RichText};

const BODY: f32 = 13.0;
const SUB: f32 = 12.0;
const LIST_H: f32 = 118.0;

#[derive(Default)]
pub struct AssetUiState {
    favorite_name: String,
    favorite_path: String,
    favorite_filter: String,
    action_title: String,
    action_program: String,
    action_args: String,
    action_admin: bool,
    command_trigger: String,
    command_title: String,
    command_program: String,
    command_args: String,
    notice: Option<String>,
}

fn shorten(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head: String = chars[..max.saturating_sub(1)].iter().collect();
    format!("{head}…")
}

/// 返回 true 表示配置发生变更（调用方负责保存配置并同步搜索服务）。
pub fn paint(ui: &mut egui::Ui, cfg: &mut Config, state: &mut AssetUiState) -> bool {
    let mut changed = false;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            changed |= favorite_section(ui, cfg, state);

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            changed |= action_section(ui, cfg, state);

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            changed |= command_section(ui, cfg, state);

            ui.add_space(10.0);
            if let Some(notice) = &state.notice {
                ui.label(RichText::new(notice).size(SUB));
            }
        });
    changed
}

fn favorite_section(ui: &mut egui::Ui, cfg: &mut Config, s: &mut AssetUiState) -> bool {
    let mut changed = false;
    ui.label(
        RichText::new(format!("收藏文件夹   ·   {} 项", cfg.favorites.len()))
            .size(14.0)
            .strong(),
    );
    ui.add_space(5.0);

    egui::Grid::new("lstr-assets-fav-form")
        .num_columns(2)
        .spacing([10.0, 7.0])
        .show(ui, |ui| {
            ui.label(RichText::new("名称").size(BODY));
            ui.add(
                egui::TextEdit::singleline(&mut s.favorite_name)
                    .hint_text("可留空，自动取目录名")
                    .desired_width(230.0),
            );
            ui.end_row();

            ui.label(RichText::new("路径").size(BODY));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.favorite_path)
                        .hint_text(r"例如 D:\work")
                        .desired_width(160.0),
                );
                if ui.button(RichText::new("添加").size(BODY)).clicked() {
                    let path = s.favorite_path.trim().to_string();
                    if path.is_empty() {
                        s.notice = Some("请填写收藏路径".to_string());
                    } else if cfg
                        .favorites
                        .iter()
                        .any(|f| f.path.eq_ignore_ascii_case(&path))
                    {
                        s.notice = Some("该路径已在收藏中".to_string());
                    } else {
                        let name = if s.favorite_name.trim().is_empty() {
                            std::path::Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.clone())
                        } else {
                            s.favorite_name.trim().to_string()
                        };
                        cfg.favorites.push(Favorite { name, path });
                        s.favorite_name.clear();
                        s.favorite_path.clear();
                        s.notice = Some("已添加收藏".to_string());
                        changed = true;
                    }
                }
            });
            ui.end_row();
        });

    ui.add_space(7.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("筛选").size(BODY));
        ui.add(
            egui::TextEdit::singleline(&mut s.favorite_filter)
                .hint_text("按名称或路径片段过滤")
                .desired_width(180.0),
        );
        if ui.button(RichText::new("清除").size(SUB)).clicked() {
            s.favorite_filter.clear();
        }
    });

    ui.add_space(5.0);
    let needle = s.favorite_filter.trim().to_lowercase();
    let mut remove = None;
    egui::ScrollArea::vertical()
        .id_salt("lstr-assets-fav-list")
        .max_height(LIST_H)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("lstr-assets-fav-grid")
                .num_columns(3)
                .striped(true)
                .spacing([10.0, 5.0])
                .show(ui, |ui| {
                    let mut shown = 0usize;
                    for (index, fav) in cfg.favorites.iter().enumerate() {
                        if !needle.is_empty()
                            && !fav.name.to_lowercase().contains(&needle)
                            && !fav.path.to_lowercase().contains(&needle)
                        {
                            continue;
                        }
                        shown += 1;
                        ui.label(RichText::new(&fav.name).size(BODY));
                        ui.label(RichText::new(shorten(&fav.path, 34)).size(SUB));
                        if ui.button(RichText::new("删除").size(SUB)).clicked() {
                            remove = Some(index);
                        }
                        ui.end_row();
                    }
                    if shown == 0 {
                        ui.label(RichText::new(if needle.is_empty() {
                            "暂无收藏"
                        } else {
                            "没有匹配的收藏"
                        })
                        .size(SUB));
                        ui.end_row();
                    }
                });
        });
    if let Some(index) = remove {
        cfg.favorites.remove(index);
        s.notice = Some("已移除收藏".to_string());
        changed = true;
    }
    changed
}

fn action_section(ui: &mut egui::Ui, cfg: &mut Config, s: &mut AssetUiState) -> bool {
    let mut changed = false;
    ui.label(
        RichText::new(format!("自定义动作   ·   {} 项", cfg.actions.len()))
            .size(14.0)
            .strong(),
    );
    ui.add_space(5.0);

    egui::Grid::new("lstr-assets-action-form")
        .num_columns(2)
        .spacing([10.0, 7.0])
        .show(ui, |ui| {
            ui.label(RichText::new("标题").size(BODY));
            ui.add(
                egui::TextEdit::singleline(&mut s.action_title)
                    .hint_text("菜单里显示的名称")
                    .desired_width(230.0),
            );
            ui.end_row();

            ui.label(RichText::new("程序").size(BODY));
            ui.add(
                egui::TextEdit::singleline(&mut s.action_program)
                    .hint_text("例如 code / wt.exe")
                    .desired_width(230.0),
            );
            ui.end_row();

            ui.label(RichText::new("参数").size(BODY));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.action_args)
                        .hint_text(r#""{action_path}""#)
                        .desired_width(160.0),
                );
                ui.checkbox(&mut s.action_admin, "提权");
                if ui.button(RichText::new("添加").size(BODY)).clicked() {
                    let program = s.action_program.trim().to_string();
                    let title = s.action_title.trim().to_string();
                    if program.is_empty() || title.is_empty() {
                        s.notice = Some("动作需要填写标题和程序".to_string());
                    } else {
                        cfg.actions.push(ActionDef {
                            title,
                            program,
                            args: s.action_args.trim().to_string(),
                            workdir: String::new(),
                            admin: s.action_admin,
                            silent: false,
                        });
                        s.action_title.clear();
                        s.action_program.clear();
                        s.action_args.clear();
                        s.notice = Some("已添加动作".to_string());
                        changed = true;
                    }
                }
            });
            ui.end_row();
        });

    ui.add_space(6.0);
    ui.label(
        RichText::new("变量：{action_path} / {action_path:parent} / {query} / {current_folder}")
            .size(SUB),
    );
    ui.label(RichText::new("含空格的路径请用双引号包住").size(SUB));

    ui.add_space(5.0);
    let mut remove = None;
    egui::ScrollArea::vertical()
        .id_salt("lstr-assets-action-list")
        .max_height(LIST_H)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("lstr-assets-action-grid")
                .num_columns(3)
                .striped(true)
                .spacing([10.0, 5.0])
                .show(ui, |ui| {
                    for (index, action) in cfg.actions.iter().enumerate() {
                        ui.label(RichText::new(&action.title).size(BODY));
                        let detail = format!(
                            "{}{}  →  {}",
                            action.program,
                            if action.admin { " (提权)" } else { "" },
                            shorten(&action.args, 24)
                        );
                        ui.label(RichText::new(detail).size(SUB));
                        if ui.button(RichText::new("删除").size(SUB)).clicked() {
                            remove = Some(index);
                        }
                        ui.end_row();
                    }
                    if cfg.actions.is_empty() {
                        ui.label(RichText::new("暂无自定义动作").size(SUB));
                        ui.end_row();
                    }
                });
        });
    if let Some(index) = remove {
        cfg.actions.remove(index);
        s.notice = Some("已移除动作".to_string());
        changed = true;
    }
    changed
}

fn command_section(ui: &mut egui::Ui, cfg: &mut Config, s: &mut AssetUiState) -> bool {
    let mut changed = false;
    ui.label(
        RichText::new(format!("自定义命令   ·   {} 项", cfg.commands.len()))
            .size(14.0)
            .strong(),
    );
    ui.add_space(5.0);

    egui::Grid::new("lstr-assets-command-form")
        .num_columns(2)
        .spacing([10.0, 7.0])
        .show(ui, |ui| {
            ui.label(RichText::new("触发词").size(BODY));
            ui.add(
                egui::TextEdit::singleline(&mut s.command_trigger)
                    .hint_text("例如 note")
                    .desired_width(230.0),
            );
            ui.end_row();

            ui.label(RichText::new("标题").size(BODY));
            ui.add(
                egui::TextEdit::singleline(&mut s.command_title)
                    .hint_text("搜索结果里显示的名称")
                    .desired_width(230.0),
            );
            ui.end_row();

            ui.label(RichText::new("程序").size(BODY));
            ui.add(
                egui::TextEdit::singleline(&mut s.command_program)
                    .hint_text("例如 notepad.exe")
                    .desired_width(230.0),
            );
            ui.end_row();

            ui.label(RichText::new("参数").size(BODY));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.command_args)
                        .hint_text("例如 {query}")
                        .desired_width(160.0),
                );
                if ui.button(RichText::new("添加").size(BODY)).clicked() {
                    let trigger = s.command_trigger.trim().to_string();
                    let title = s.command_title.trim().to_string();
                    let program = s.command_program.trim().to_string();
                    if trigger.is_empty() || title.is_empty() || program.is_empty() {
                        s.notice = Some("命令需要触发词、标题和程序".to_string());
                    } else if cfg
                        .commands
                        .iter()
                        .any(|c| c.trigger.eq_ignore_ascii_case(&trigger))
                    {
                        s.notice = Some("该触发词已被占用".to_string());
                    } else {
                        cfg.commands.push(CommandDef {
                            trigger,
                            title,
                            program,
                            args: s.command_args.trim().to_string(),
                            workdir: String::new(),
                        });
                        s.command_trigger.clear();
                        s.command_title.clear();
                        s.command_program.clear();
                        s.command_args.clear();
                        s.notice = Some("已添加命令".to_string());
                        changed = true;
                    }
                }
            });
            ui.end_row();
        });

    ui.add_space(6.0);
    ui.label(
        RichText::new("变量：{query}（触发词之后的内容）、{current_folder}").size(SUB),
    );

    ui.add_space(5.0);
    let mut remove = None;
    egui::ScrollArea::vertical()
        .id_salt("lstr-assets-command-list")
        .max_height(LIST_H)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("lstr-assets-command-grid")
                .num_columns(3)
                .striped(true)
                .spacing([10.0, 5.0])
                .show(ui, |ui| {
                    for (index, command) in cfg.commands.iter().enumerate() {
                        ui.label(RichText::new(&command.trigger).size(BODY));
                        let detail = format!(
                            "{}  →  {} {}",
                            command.title,
                            command.program,
                            shorten(&command.args, 18)
                        );
                        ui.label(RichText::new(detail).size(SUB));
                        if ui.button(RichText::new("删除").size(SUB)).clicked() {
                            remove = Some(index);
                        }
                        ui.end_row();
                    }
                    if cfg.commands.is_empty() {
                        ui.label(RichText::new("暂无自定义命令").size(SUB));
                        ui.end_row();
                    }
                });
        });
    if let Some(index) = remove {
        cfg.commands.remove(index);
        s.notice = Some("已移除命令".to_string());
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_has_empty_drafts() {
        let state = AssetUiState::default();
        assert!(state.favorite_path.is_empty());
        assert!(state.favorite_filter.is_empty());
        assert!(state.command_trigger.is_empty());
        assert!(!state.action_admin);
    }

    #[test]
    fn config_starts_without_assets() {
        let cfg = Config::default();
        assert!(cfg.favorites.is_empty());
        assert!(cfg.actions.is_empty());
        assert!(cfg.commands.is_empty());
    }

    #[test]
    fn shorten_keeps_short_text_and_truncates_long_text() {
        assert_eq!(shorten("D:\\work", 34), "D:\\work");
        let long = "D:\\a\\very\\long\\path\\that\\keeps\\going\\and\\going\\forever";
        let short = shorten(long, 20);
        assert!(short.chars().count() <= 20);
        assert!(short.ends_with('…'));
    }
}
