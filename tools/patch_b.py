# -*- coding: utf-8 -*-
"""M2.4 patch: preview pane + settings overlay in app.rs"""

from pathlib import Path

p = Path(r"E:\use\ai\deepseek\Rust\lstr\src\app.rs")
s = p.read_text(encoding="utf-8")

# --- apply_size 重构（设置页固定高度 / 预览加宽） ---
old = '''    fn apply_size(&mut self, ctx: &egui::Context) {
        if self.resizing {
            return;
        }
        let rows = self.entries.len();
        let list_h = if rows == 0 {
            120.0
        } else {
            rows as f32 * self.theme.row_h
        };
        let banner_h = if self.error.is_some() { 30.0 } else { 0.0 };
        let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
        let height = (SEARCH_H
            + TABS_H
            + LIST_GAP
            + list_h
            + LIST_GAP
            + banner_h
            + dialog_h
            + FOOTER_H
            + 8.0)
            .clamp(160.0, 800.0);
        let width = self
            .manual_size
            .map(|size| size.x)
            .unwrap_or(self.theme.width)
            .clamp(420.0, 1000.0);
        let height = match self.manual_size {
            Some(manual) => computed_h.max(manual.y).clamp(160.0, 800.0),
            None => computed_h,
        };
        if (width - self.last_width).abs() > 0.5 || (height - self.last_height).abs() > 0.5 {
            self.last_width = width;
            self.last_height = height;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, height)));
        }
    }'''
new = '''    fn apply_size(&mut self, ctx: &egui::Context) {
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
            width = width.max(640.0);
        }
        let height = if self.settings_open {
            430.0
        } else {
            let rows = self.entries.len();
            let list_h = if rows == 0 { 120.0 } else { rows as f32 * self.theme.row_h };
            let banner_h = if self.error.is_some() { 30.0 } else { 0.0 };
            let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
            let computed_h = (SEARCH_H
                + TABS_H
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
    }'''
if old not in s:
    raise SystemExit("apply_size anchor not found")
s = s.replace(old, new)

# --- paint 高度：设置页固定 430 ---
old2 = '''        let banner_h = if self.error.is_some() { 30.0 } else { 0.0 };
        let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
        let height = (SEARCH_H
            + TABS_H
            + LIST_GAP
            + list_h
            + LIST_GAP
            + banner_h
            + dialog_h
            + FOOTER_H
            + 8.0)
            .clamp(150.0, 540.0);

        egui::CentralPanel::default()'''
new2 = '''        let banner_h = if self.error.is_some() { 30.0 } else { 0.0 };
        let dialog_h = if self.dialog.is_some() { 30.0 } else { 0.0 };
        let preview_w = if self.preview_open { 216.0 } else { 0.0 };
        let height = if self.settings_open {
            430.0
        } else {
            (SEARCH_H
                + TABS_H
                + LIST_GAP
                + list_h
                + LIST_GAP
                + banner_h
                + dialog_h
                + FOOTER_H
                + 8.0)
                .clamp(150.0, 540.0)
        };

        egui::CentralPanel::default()'''
if old2 not in s:
    raise SystemExit("paint height anchor not found")
s = s.replace(old2, new2)

# --- 设置页早退（背景绘制后） ---
old3 = '''                if !theme.light {
                    let y = rect.top() + 0.5;
                    painter.line_segment(
                        [
                            egui::pos2(rect.left() + theme.radius as f32 + 2.0, y),
                            egui::pos2(rect.right() - theme.radius as f32 - 2.0, y),
                        ],
                        Stroke::new(1.0, Color32::from_white_alpha(16)),
                    );
                }
'''
new3 = old3 + '''
                if self.settings_open {
                    self.paint_settings(ui, ctx, rect);
                    return;
                }
'''
if old3 not in s:
    raise SystemExit("top highlight anchor not found")
s = s.replace(old3, new3)

# --- list_rect：预览时列表变窄 ---
old4 = '''                self.paint_tabs(ui, &painter, rect, &query);
                let list_top = rect.top() + SEARCH_H + TABS_H + LIST_GAP;
                self.paint_rows(ui, &painter, rect, list_top, &entries, &query);'''
new4 = '''                self.paint_tabs(ui, &painter, rect, &query);
                let list_top = rect.top() + SEARCH_H + TABS_H + LIST_GAP;
                let list_rect = Rect::from_min_max(
                    rect.left_top(),
                    egui::pos2(rect.right() - PAD - preview_w, rect.bottom()),
                );
                self.paint_rows(ui, &painter, list_rect, list_top, &entries, &query);'''
if old4 not in s:
    raise SystemExit("rows anchor not found")
s = s.replace(old4, new4)

# --- 预览面板绘制（横幅之后） ---
old5 = '''                    extra_top += 30.0;
                }
                if let Some(dialog) = &self.dialog {'''
new5 = '''                    extra_top += 30.0;
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
                if let Some(dialog) = &self.dialog {'''
if old5 not in s:
    raise SystemExit("banner anchor not found")
s = s.replace(old5, new5)

s = s.replace("                self.paint_menu(ctx, rect);", "                self.paint_menu(ctx, list_rect);")

# --- open_settings 方法 ---
s = s.replace(
    "    fn hide(&mut self, _ctx: &egui::Context) {",
    """    fn open_settings(&mut self, ctx: &egui::Context) {
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

    fn hide(&mut self, _ctx: &egui::Context) {""",
)

# --- paint_preview + paint_settings 方法（插在 paint_drag_resize 前） ---
anchor = "    fn paint_drag_resize(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {"
methods = '''    fn paint_preview(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        pane: Rect,
        entry: &Entry,
        q: &str,
    ) {
        let theme = self.theme;
        let mut y = pane.top() + 6.0;

        let icon_key = format!("preview:{}", entry.id);
        let icon = self.icons.entry(icon_key).or_insert_with(|| {
            crate::win::icons::load_icon(&entry.full_path(), 32).map(|image| {
                ui.ctx().load_texture(
                    format!("preview-icon:{}", entry.id),
                    image,
                    egui::TextureOptions::LINEAR,
                )
            })
        });
        let icon_rect =
            Rect::from_min_size(egui::pos2(pane.left() + 8.0, y), egui::vec2(32.0, 32.0));
        if let Some(texture) = icon.as_ref() {
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
            &FontId::new(13.5, FontFamily::Proportional),
            theme.text,
            theme.amber,
        );
        name_job.wrap.max_width = pane.width() - 16.0;
        name_job.wrap.max_rows = 2;
        name_job.wrap.overflow_character = Some('…');
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
            FontId::new(10.5, FontFamily::Monospace),
            theme.sub,
        );
        y += 18.0;

        let full = entry.full_path();
        let mut path_job = ui::text_job(
            &full,
            q,
            &FontId::new(10.5, FontFamily::Monospace),
            theme.sub,
            theme.amber,
        );
        path_job.wrap.max_width = pane.width() - 16.0;
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
                FontId::new(10.5, FontFamily::Monospace),
                theme.sub,
            );
            y += 16.0;
        }
        if let Some(modified) = entry.modified {
            painter.text(
                egui::pos2(pane.left() + 8.0, y),
                Align2::LEFT_TOP,
                format!("修改   {}", ui::format_relative_time(modified)),
                FontId::new(10.5, FontFamily::Monospace),
                theme.sub,
            );
            y += 16.0;
        }
    }

    fn paint_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rect: Rect) {
        let theme = self.theme;
        let painter = ui.painter().clone();

        let close_rect = Rect::from_min_size(
            egui::pos2(rect.right() - PAD - 18.0, rect.top() + 12.0),
            egui::vec2(18.0, 18.0),
        );
        let close = ui.interact(close_rect, Id::new("lstr-settings-close"), Sense::click());
        painter.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::new(13.0, FontFamily::Proportional),
            if close.hovered() { theme.text } else { theme.sub },
        );
        if close.clicked() {
            self.settings_open = false;
        }

        painter.text(
            egui::pos2(rect.left() + PAD, rect.top() + 18.0),
            Align2::LEFT_TOP,
            "设置",
            FontId::new(18.0, FontFamily::Proportional),
            theme.text,
        );

        let mut y = rect.top() + 62.0;
        let row_label = |y: f32, label: &str| {
            painter.text(
                egui::pos2(rect.left() + PAD, y + 11.0),
                Align2::LEFT_CENTER,
                label,
                FontId::new(11.5, FontFamily::Proportional),
                theme.sub,
            );
        };

        row_label(y, "主题");
        let themes = [("obsidian", "Obsidian"), ("frost", "Frost")];
        for (index, (value, label)) in themes.iter().enumerate() {
            let button_rect = Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0 + index as f32 * 104.0, y),
                egui::vec2(96.0, 26.0),
            );
            let selected = self.cfg.theme.mode == *value;
            let response =
                ui.interact(button_rect, Id::new(("lstr-theme", index)), Sense::click());
            painter.rect_filled(
                button_rect,
                CornerRadius::same(6),
                if selected { theme.surface_hi } else { theme.hover },
            );
            painter.rect_stroke(
                button_rect,
                CornerRadius::same(6),
                Stroke::new(1.0, if selected { theme.amber } else { theme.hair }),
                StrokeKind::Inside,
            );
            painter.text(
                button_rect.center(),
                Align2::CENTER_CENTER,
                *label,
                FontId::new(11.5, FontFamily::Proportional),
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
        y += 42.0;

        row_label(y, "不透明度");
        let mut opacity = self.cfg.theme.opacity;
        let response = ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0, y),
                egui::vec2(200.0, 22.0),
            ),
            egui::Slider::new(&mut opacity, 0.72..=0.98),
        );
        if response.changed() {
            self.cfg.theme.opacity = opacity;
            let _ = self.cfg.save();
            if let Some(hwnd) = self.hwnd {
                window::apply_effects(hwnd, self.theme.light, self.cfg.theme.opacity);
            }
        }
        y += 38.0;

        row_label(y, "圆角");
        let mut radius = self.cfg.theme.radius;
        let response = ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0, y),
                egui::vec2(200.0, 22.0),
            ),
            egui::Slider::new(&mut radius, 8.0..=24.0),
        );
        if response.changed() {
            self.cfg.theme.radius = radius;
            self.theme = Theme::from_config(&self.cfg);
            let _ = self.cfg.save();
        }
        y += 38.0;

        row_label(y, "行高");
        let mut row_height = self.cfg.theme.row_height;
        let response = ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0, y),
                egui::vec2(200.0, 22.0),
            ),
            egui::Slider::new(&mut row_height, 36.0..=52.0),
        );
        if response.changed() {
            self.cfg.theme.row_height = row_height;
            self.theme = Theme::from_config(&self.cfg);
            let _ = self.cfg.save();
        }
        y += 38.0;

        row_label(y, "最大结果数");
        let mut max_results = self.cfg.max_results as f32;
        let response = ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0, y),
                egui::vec2(200.0, 22.0),
            ),
            egui::Slider::new(&mut max_results, 4.0..=12.0),
        );
        if response.changed() {
            self.cfg.max_results = max_results.round() as usize;
            let _ = self.cfg.save();
        }
        y += 38.0;

        row_label(y, "预览面板");
        let mut preview = self.cfg.preview;
        let response = ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0, y),
                egui::vec2(160.0, 22.0),
            ),
            egui::Checkbox::new(&mut preview, ""),
        );
        if response.changed() {
            self.cfg.preview = preview;
            self.preview_open = preview;
            let _ = self.cfg.save();
        }
        y += 38.0;

        row_label(y, "开机自启");
        let mut autostart = self.autostart;
        let response = ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + PAD + 120.0, y),
                egui::vec2(160.0, 22.0),
            ),
            egui::Checkbox::new(&mut autostart, ""),
        );
        if response.changed() {
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
        y += 42.0;

        let open_rect = Rect::from_min_size(
            egui::pos2(rect.left() + PAD, y),
            egui::vec2(120.0, 26.0),
        );
        let response = ui.interact(open_rect, Id::new("lstr-open-config"), Sense::click());
        painter.rect_filled(open_rect, CornerRadius::same(6), theme.hover);
        painter.text(
            open_rect.center(),
            Align2::CENTER_CENTER,
            "打开配置文件",
            FontId::new(11.0, FontFamily::Proportional),
            theme.text,
        );
        if response.clicked() {
            let _ = shell::open_config();
        }
    }

''' + anchor
if anchor not in s:
    raise SystemExit("paint_drag_resize anchor not found")
s = s.replace(anchor, methods)

p.write_text(s, encoding="utf-8")
print("chunk B applied")
