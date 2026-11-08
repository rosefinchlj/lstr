//! 设置页「资产」：可视化管理收藏 / 自定义动作 / 自定义命令。
//!
//! 独立模块，只依赖 `config / assets / egui`，不碰 Win32，
//! 这样 `app.rs` 里只需要一个分页入口，合并时冲突面最小。

use crate::assets::{ActionDef, CommandDef, Favorite};
use crate::config::Config;
use eframe::egui;

#[derive(Default)]
pub struct AssetUiState {
    favorite_name: String,
    favorite_path: String,
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

/// 返回 true 表示配置发生变更（调用方负责保存配置并同步搜索服务）。
pub fn paint(ui: &mut egui::Ui, cfg: &mut Config, state: &mut AssetUiState) -> bool {
    let mut changed = false;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());

            ui.label(egui::RichText::new("收藏文件夹").strong());
            changed |= favorite_section(ui, cfg, state);

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            ui.label(egui::RichText::new("自定义动作（作用于选中的结果）").strong());
            changed |= action_section(ui, cfg, state);
            ui.label(
                egui::RichText::new(
                    "变量：{action_path}、{action_path:parent}、{query}、{current_folder}；路径请加引号",
                )
                .weak()
                .small(),
            );

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            ui.label(egui::RichText::new("自定义命令（作用于搜索词）").strong());
            changed |= command_section(ui, cfg, state);
            ui.label(
                egui::RichText::new("变量：{query}（触发词之后的内容）、{current_folder}")
                    .weak()
                    .small(),
            );

            ui.add_space(8.0);
            if let Some(notice) = &state.notice {
                ui.label(egui::RichText::new(notice).weak().small());
            }
        });
    changed
}

fn favorite_section(ui: &mut egui::Ui, cfg: &mut Config, s: &mut AssetUiState) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut s.favorite_name)
                .hint_text("名称（可空）")
                .desired_width(100.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut s.favorite_path)
                .hint_text("路径，例如 D:\\work")
                .desired_width(190.0),
        );
        if ui.button("添加").clicked() {
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

    let mut remove = None;
    for (index, fav) in cfg.favorites.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&fav.name).small());
            ui.label(egui::RichText::new(&fav.path).weak().small());
            if ui.small_button("×").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        cfg.favorites.remove(index);
        s.notice = Some("已移除收藏".to_string());
        changed = true;
    }
    changed
}

fn action_section(ui: &mut egui::Ui, cfg: &mut Config, s: &mut AssetUiState) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut s.action_title)
                .hint_text("标题")
                .desired_width(96.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut s.action_program)
                .hint_text("程序，如 code")
                .desired_width(96.0),
        );
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut s.action_args)
                .hint_text("参数，如 \"{action_path}\"")
                .desired_width(196.0),
        );
        ui.checkbox(&mut s.action_admin, "提权");
        if ui.button("添加").clicked() {
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

    let mut remove = None;
    for (index, action) in cfg.actions.iter().enumerate() {
        ui.horizontal(|ui| {
            let flag = if action.admin { " [提权]" } else { "" };
            ui.label(
                egui::RichText::new(format!("{}{}  →  {}", action.title, flag, action.program))
                    .small(),
            );
            if ui.small_button("×").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        cfg.actions.remove(index);
        s.notice = Some("已移除动作".to_string());
        changed = true;
    }
    changed
}

fn command_section(ui: &mut egui::Ui, cfg: &mut Config, s: &mut AssetUiState) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut s.command_trigger)
                .hint_text("触发词")
                .desired_width(80.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut s.command_title)
                .hint_text("标题")
                .desired_width(96.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut s.command_program)
                .hint_text("程序")
                .desired_width(88.0),
        );
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut s.command_args)
                .hint_text("参数，例如 {query}")
                .desired_width(196.0),
        );
        if ui.button("添加").clicked() {
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

    let mut remove = None;
    for (index, command) in cfg.commands.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{}  {}  →  {}",
                    command.trigger, command.title, command.program
                ))
                .small(),
            );
            if ui.small_button("×").clicked() {
                remove = Some(index);
            }
        });
    }
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
}
