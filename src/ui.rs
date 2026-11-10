use crate::config::Config;
use crate::model::{Entry, Kind};
use eframe::egui::text::LayoutJob;
use eframe::egui::{
    self, Align2, Color32, FontFamily, FontId, Stroke, TextFormat,
};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color32,
    pub surface_hi: Color32,
    pub hover: Color32,
    pub text: Color32,
    pub sub: Color32,
    pub dim: Color32,
    pub amber: Color32,
    pub amber_soft: Color32,
    pub hair: Color32,
    pub hair2: Color32,
    pub menu: Color32,
    /// 选中行背景（强调色淡染，比悬浮底色更明显）。
    pub sel: Color32,
    pub radius: u8,
    pub row_h: f32,
    pub width: f32,
    pub light: bool,
}

impl Theme {
    pub fn from_config(cfg: &Config) -> Self {
        let light = cfg.theme.is_light();
        let accent = cfg.theme.accent_rgb();
        // 浅色底上把琥珀压深，保证下划线/指针的对比度。
        let amber = if light {
            let factor = 0.62f32;
            Color32::from_rgb(
                (accent[0] as f32 * factor).round() as u8,
                (accent[1] as f32 * factor).round() as u8,
                (accent[2] as f32 * factor).round() as u8,
            )
        } else {
            Color32::from_rgb(accent[0], accent[1], accent[2])
        };
        let bg_alpha = (cfg.theme.opacity.clamp(0.6, 1.0) * 255.0 * 0.72) as u8;
        if light {
            Self {
                bg: Color32::from_rgba_unmultiplied(238, 240, 243, bg_alpha),
                surface_hi: Color32::from_rgb(226, 230, 235),
                hover: Color32::from_rgb(233, 236, 240),
                text: Color32::from_rgb(22, 25, 29),
                sub: Color32::from_rgb(74, 82, 92),
                dim: Color32::from_rgb(104, 112, 122),
                amber,
                amber_soft: Color32::from_rgba_unmultiplied(accent[0], accent[1], accent[2], 24),
                hair: Color32::from_black_alpha(18),
                hair2: Color32::from_black_alpha(30),
                menu: Color32::from_rgb(252, 252, 251),
                sel: Color32::from_rgba_unmultiplied(accent[0], accent[1], accent[2], 52),
                radius: cfg.theme.radius.clamp(8.0, 24.0) as u8,
                row_h: cfg.theme.row_height.clamp(36.0, 56.0),
                width: cfg.width.clamp(480.0, 900.0),
                light,
            }
        } else {
            Self {
                bg: Color32::from_rgba_unmultiplied(24, 28, 34, (cfg.theme.opacity.clamp(0.6, 1.0) * 255.0 * 0.55) as u8),
                surface_hi: Color32::from_rgb(36, 41, 50),
                hover: Color32::from_rgb(32, 37, 45),
                text: Color32::from_rgb(232, 230, 225),
                sub: Color32::from_rgb(138, 144, 153),
                dim: Color32::from_rgb(95, 103, 114),
                amber,
                amber_soft: Color32::from_rgba_unmultiplied(accent[0], accent[1], accent[2], 26),
                hair: Color32::from_white_alpha(20),
                hair2: Color32::from_white_alpha(32),
                menu: Color32::from_rgb(22, 26, 32),
                sel: Color32::from_rgba_unmultiplied(accent[0], accent[1], accent[2], 64),
                radius: cfg.theme.radius.clamp(8.0, 24.0) as u8,
                row_h: cfg.theme.row_height.clamp(36.0, 56.0),
                width: cfg.width.clamp(480.0, 900.0),
                light,
            }
        }
    }
}

/// 全局字号缩放。默认字号在高分屏/高缩放下偏小，统一放大一档。
pub const TEXT_SCALE: f32 = 1.18;

/// 关键词高亮用的粗体字体族名。
pub const BOLD_FAMILY: &str = "lstr-bold";

/// 取同字号的粗体 FontId（高亮片段用）。
pub fn bold_of(font: &FontId) -> FontId {
    FontId::new(font.size, FontFamily::Name(BOLD_FAMILY.into()))
}

pub fn text_font(size: f32) -> FontId {
    FontId::new(size * TEXT_SCALE, FontFamily::Proportional)
}

pub fn mono_font(size: f32) -> FontId {
    FontId::new(size * TEXT_SCALE, FontFamily::Monospace)
}

/// egui 自带控件（滑块/复选框等）走 TextStyle，字号也要跟着放大。
pub fn apply_text_styles(ctx: &egui::Context) {
    use egui::TextStyle;
    ctx.style_mut(|style| {
        style
            .text_styles
            .insert(TextStyle::Body, text_font(13.0));
        style
            .text_styles
            .insert(TextStyle::Button, text_font(13.0));
        style
            .text_styles
            .insert(TextStyle::Small, text_font(11.0));
        style
            .text_styles
            .insert(TextStyle::Heading, text_font(18.0));
        style
            .text_styles
            .insert(TextStyle::Monospace, mono_font(12.5));
    });
}

/// 系统功能入口的显示路径：把 appwiz.cpl / ms-settings: 之类换成中文位置。
pub fn display_path(entry: &Entry) -> String {
    let Some(path) = entry.path.as_ref() else {
        return entry.name.clone();
    };
    let raw = path.display().to_string();
    let lower = raw.to_ascii_lowercase();
    let group = match lower.as_str() {
        "appwiz.cpl" | "control" | "ncpa.cpl" | "sysdm.cpl" | "powercfg.cpl" | "mmsys.cpl" => {
            Some("控制面板")
        }
        "devmgmt.msc" | "diskmgmt.msc" | "services.msc" | "wf.msc" | "eventvwr.msc"
        | "taskmgr" | "regedit" | "msinfo32" => Some("系统工具"),
        _ if lower.starts_with("ms-settings:") => Some("设置"),
        _ if lower.starts_with("shell:appsfolder") => Some("应用"),
        _ => None,
    };
    match group {
        Some(group) => format!("{} › {}", group, entry.name),
        None => entry.full_path(),
    }
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = PathBuf::from(r"C:\Windows\Fonts");

    let mut add = |fonts: &mut egui::FontDefinitions, name: &str, file: &str, index: u32| -> bool {
        let path = dir.join(file);
        let Ok(bytes) = std::fs::read(&path) else {
            return false;
        };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert(name.to_string(), Arc::new(data));
        true
    };

    let ui_ok = add(&mut fonts, "lstr-ui", "segoeui.ttf", 0);
    let ui_bold_ok = add(&mut fonts, "lstr-ui-bold", "segoeuib.ttf", 0);
    let mono_ok = add(&mut fonts, "lstr-mono", "CascadiaMono.ttf", 0)
        || add(&mut fonts, "lstr-mono", "consola.ttf", 0);
    let cjk_ok = add(&mut fonts, "lstr-cjk", "msyh.ttc", 0)
        || add(&mut fonts, "lstr-cjk", "Deng.ttf", 0)
        || add(&mut fonts, "lstr-cjk", "simhei.ttf", 0);
    let cjk_bold_ok = add(&mut fonts, "lstr-cjk-bold", "msyhbd.ttc", 0);

    if ui_ok {
        if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
            list.insert(0, "lstr-ui".to_string());
        }
    }
    if mono_ok {
        if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
            list.insert(0, "lstr-mono".to_string());
        }
    }
    if cjk_ok {
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("lstr-cjk".to_string());
        }
    }

    // 粗体族：关键词高亮用加粗代替背景色。缺字体时逐级回退到常规字体。
    let mut bold_list: Vec<String> = Vec::new();
    if ui_bold_ok {
        bold_list.push("lstr-ui-bold".to_string());
    }
    if ui_ok {
        bold_list.push("lstr-ui".to_string());
    }
    if cjk_bold_ok {
        bold_list.push("lstr-cjk-bold".to_string());
    }
    if cjk_ok {
        bold_list.push("lstr-cjk".to_string());
    }
    if !bold_list.is_empty() {
        fonts
            .families
            .insert(FontFamily::Name(BOLD_FAMILY.into()), bold_list);
    }
    ctx.set_fonts(fonts);
}

fn format(font: &FontId, color: Color32) -> TextFormat {
    TextFormat {
        font_id: font.clone(),
        color,
        ..Default::default()
    }
}

/// 关键词高亮：加粗 + 强调色文字 + 2px 下划线（不用背景色，避免和选中行底色打架）。
fn format_underlined(font: &FontId, _color: Color32, accent: Color32) -> TextFormat {
    TextFormat {
        font_id: bold_of(font),
        color: accent,
        underline: Stroke::new(2.0, accent),
        ..Default::default()
    }
}

pub fn text_job(
    text: &str,
    q: &str,
    font: &FontId,
    color: Color32,
    accent: Color32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let needle = q.to_lowercase();
    if needle.is_empty() {
        job.append(text, 0.0, format(font, color));
        return job;
    }
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find(&needle) {
        let end = pos + needle.len();
        if text.is_char_boundary(pos) && text.is_char_boundary(end) {
            job.append(&text[..pos], 0.0, format(font, color));
            job.append(
                &text[pos..end],
                0.0,
                format_underlined(font, color, accent),
            );
            job.append(&text[end..], 0.0, format(font, color));
            return job;
        }
    }
    job.append(text, 0.0, format(font, color));
    job
}

/// 按字符下标逐个高亮（子序列模糊命中用）：positions 为升序 char 下标。
pub fn text_job_positions(
    text: &str,
    positions: &[usize],
    font: &FontId,
    color: Color32,
    accent: Color32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    if positions.is_empty() {
        job.append(text, 0.0, format(font, color));
        return job;
    }
    // 把连续命中合并成区间，减少 LayoutJob 分片。
    let mut runs: Vec<(usize, usize)> = Vec::new(); // [start, end) char 下标
    for &pos in positions {
        if let Some(last) = runs.last_mut() {
            if pos == last.1 {
                last.1 += 1;
                continue;
            }
        }
        runs.push((pos, pos + 1));
    }
    let chars: Vec<char> = text.chars().collect();
    let mut cursor = 0usize;
    for (start, end) in runs {
        if start > chars.len() || end > chars.len() || start >= end {
            break;
        }
        if cursor < start {
            let segment: String = chars[cursor..start].iter().collect();
            job.append(&segment, 0.0, format(font, color));
        }
        let hit: String = chars[start..end].iter().collect();
        job.append(&hit, 0.0, format_underlined(font, color, accent));
        cursor = end;
    }
    if cursor < chars.len() {
        let tail: String = chars[cursor..].iter().collect();
        job.append(&tail, 0.0, format(font, color));
    }
    job
}

/// 从 URL 里取出 host 用于副标题展示；取不到就原样返回。
pub fn url_host(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme.split('/').next().unwrap_or(after_scheme)
}

/// 路径中段省略：保留盘符/根 + **尽可能多**的尾部目录段。
///
/// 不能只保留固定两段——同名结果（都叫 `Z`）靠路径区分，`D:\a\x\3\z` 与
/// `D:\b\y\3\z` 会被压成同一串 `D:\…\3\z`，用户根本没法选。
pub fn middle_ellipsis(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        // "D:\name" / 单段：没有可压缩的中间层，直接头截断。
        return head_truncate(path, max_chars);
    }
    // UNC（\\server\share\...）前两段属于根，一起保留。
    let (root_len, root) = if path.starts_with("\\\\") && parts.len() >= 3 {
        (2, format!("\\\\{}\\{}", parts[0], parts[1]))
    } else {
        (1, parts[0].to_string())
    };
    // 尾部段数从 2 开始逐段加多，取最后一个放得下的组合。
    let mut best: Option<String> = None;
    for start in (root_len..=parts.len() - 2).rev() {
        let candidate = format!("{}\\…\\{}", root, parts[start..].join("\\"));
        if candidate.chars().count() <= max_chars {
            best = Some(candidate);
        } else {
            break; // 再往前加只会更长
        }
    }
    best.unwrap_or_else(|| {
        // 连「根 + 最后两段」都放不下：退到只留最后一段，再不行头截断。
        let tail = format!("{}\\…\\{}", root, parts[parts.len() - 1]);
        if tail.chars().count() <= max_chars {
            tail
        } else {
            head_truncate(path, max_chars)
        }
    })
}

fn head_truncate(path: &str, max_chars: usize) -> String {
    let mut truncated: String = path.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

pub fn format_relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = (now - ts).max(0);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{} 分钟前", diff / 60)
    } else if diff < 86400 {
        format!("{} 小时前", diff / 3600)
    } else if diff < 30 * 86400 {
        format!("{} 天前", diff / 86400)
    } else {
        format_local_date(ts)
    }
}

/// unix 秒 → 本地 "YYYY-MM-DD"。
/// 走 Win32 `FileTimeToLocalFileTime` + `FileTimeToSystemTime`（自动处理时区/夏令时），
/// 文件管理器习惯的本地日期；API 异常时回退到粗略 UTC 日期（不应发生）。
fn format_local_date(ts: i64) -> String {
    match local_systemtime(ts) {
        Some(st) => format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay),
        None => {
            // 回退：粗略 UTC 日期（旧实现，仅 API 异常时走到）。
            let days = ts.div_euclid(86400);
            let z = days + 719468;
            let era = z.div_euclid(146097);
            let doe = z - era * 146097;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let year = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let day = doy - (153 * mp + 2) / 5 + 1;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = if month <= 2 { year + 1 } else { year };
            format!("{year:04}-{month:02}-{day:02}")
        }
    }
}

/// unix 秒 → 本地 SYSTEMTIME（时区/夏令时由 Win32 处理，与 format_local_date 同源）。
fn local_systemtime(ts: i64) -> Option<windows::Win32::Foundation::SYSTEMTIME> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    // windows crate 0.62 的模块归属：FileTimeToLocalFileTime 在 Storage::FileSystem，
    // FileTimeToSystemTime 在 System::Time（别按直觉猜 System::Time 全在这）。
    use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows::Win32::System::Time::FileTimeToSystemTime;
    // 1601-01-01 → 1970-01-01 的 100ns tick 数（18 位，勿再写错位数）。
    const UNIX_EPOCH_FILETIME: i128 = 116_444_736_000_000_000;
    let ticks = ts as i128 * 10_000_000 + UNIX_EPOCH_FILETIME;
    if !(0..=u64::MAX as i128).contains(&ticks) {
        return None;
    }
    let t = ticks as u64;
    let ft = FILETIME {
        dwLowDateTime: (t & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (t >> 32) as u32,
    };
    let mut local = FILETIME::default();
    let mut st = SYSTEMTIME::default();
    unsafe {
        if FileTimeToLocalFileTime(&ft, &mut local).is_ok()
            && FileTimeToSystemTime(&local, &mut st).is_ok()
        {
            return Some(st);
        }
    }
    None
}

/// 列表页的紧凑时间（本地时区）：今天 `HH:mm` / 今年 `MM-DD` / 更早 `YYYY-MM-DD`。
/// 定宽短格式，配合右侧右对齐列实现跨行对齐（相对时间「N 天前」宽度不定，
/// 会让大小列左缘参差）；预览面板仍用 format_relative_time（空间充足、可读性优先）。
pub fn format_list_time(ts: i64) -> String {
    let Some(st) = local_systemtime(ts) else {
        return format_local_date(ts);
    };
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    if st.wYear == now.wYear && st.wMonth == now.wMonth && st.wDay == now.wDay {
        format!("{:02}:{:02}", st.wHour, st.wMinute)
    } else if st.wYear == now.wYear {
        format!("{:02}-{:02}", st.wMonth, st.wDay)
    } else {
        format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
    }
}

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn paint_app_tile(painter: &egui::Painter, rect: egui::Rect, entry: &Entry) {
    let color = Color32::from_rgb(
        ((entry.tone >> 16) & 0xff) as u8,
        ((entry.tone >> 8) & 0xff) as u8,
        (entry.tone & 0xff) as u8,
    );
    painter.rect_filled(rect, egui::CornerRadius::same(5), color);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        &entry.initial,
        text_font(8.5),
        Color32::WHITE,
    );
}

pub fn paint_file_icon(painter: &egui::Painter, rect: egui::Rect, kind: Kind, color: Color32) {
    let stroke = Stroke::new(1.2, color);
    match kind {
        Kind::Folder => {
            let tab = egui::Rect::from_min_size(rect.min + egui::vec2(1.0, 3.0), egui::vec2(6.0, 3.0));
            painter.rect_filled(tab, egui::CornerRadius::same(1), color.gamma_multiply(0.7));
            let body = egui::Rect::from_min_size(rect.min + egui::vec2(0.0, 5.0), egui::vec2(16.0, 10.0));
            painter.rect_stroke(
                body,
                egui::CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Kind::Program => {
            let body = rect.shrink2(egui::vec2(1.0, 2.0));
            painter.rect_stroke(
                body,
                egui::CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(body.left() + 3.0, body.top() + 4.0),
                    egui::pos2(body.right() - 3.0, body.top() + 4.0),
                ],
                stroke,
            );
        }
        Kind::Command => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                ">_",
                mono_font(9.0),
                color,
            );
        }
        _ => {
            let body = egui::Rect::from_min_size(rect.min + egui::vec2(2.0, 1.0), egui::vec2(12.0, 16.0));
            painter.rect_stroke(
                body,
                egui::CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
            for y in [7.0, 10.0, 13.0] {
                painter.line_segment(
                    [
                        egui::pos2(body.left() + 3.0, body.top() + y),
                        egui::pos2(body.right() - 3.0, body.top() + y),
                    ],
                    Stroke::new(1.0, color.gamma_multiply(0.65)),
                );
            }
        }
    }
}

pub fn keycap(painter: &egui::Painter, pos: egui::Pos2, label: &str, theme: &Theme) -> egui::Rect {
    let font = mono_font(9.5);
    let galley = painter.layout_no_wrap(label.to_string(), font, theme.dim);
    let rect = egui::Rect::from_min_size(pos, galley.size() + egui::vec2(8.0, 3.0));
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(4),
        Stroke::new(1.0, theme.hair),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        pos + egui::vec2(4.0, 1.5),
        galley,
        theme.dim,
    );
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn app(name: &str, target: &str) -> Entry {
        Entry {
            id: format!("sys:{name}"),
            kind: Kind::App,
            name: name.to_string(),
            path: Some(PathBuf::from(target)),
            size: None,
            modified: None,
            pinyin: None,
            initial: name.chars().next().unwrap_or('?').to_string(),
            tone: 0,
        }
    }

    #[test]
    fn display_path_maps_control_panel_targets() {
        assert_eq!(
            display_path(&app("卸载程序", "appwiz.cpl")),
            "控制面板 › 卸载程序"
        );
        assert_eq!(
            display_path(&app("任务管理器", "taskmgr")),
            "系统工具 › 任务管理器"
        );
        assert_eq!(
            display_path(&app("应用和功能", "ms-settings:appsfeatures")),
            "设置 › 应用和功能"
        );
    }

    #[test]
    fn display_path_keeps_real_paths() {
        let path = r"E:\use\ai\deepseek\Rust\lstr\src\app.rs";
        let mut entry = app("app", path);
        entry.kind = Kind::File;
        assert_eq!(display_path(&entry), path);
    }

    #[test]
    fn middle_ellipsis_keeps_more_tail_segments_when_room_allows() {
        // 同名结果的区分点在中后段：预算允许时要多保留几级目录。
        let path = r"D:\project\python\Lib\site-packages\uc_micro\categories\Z";
        let wide = middle_ellipsis(path, 60);
        assert!(
            wide.contains("categories\\Z"),
            "宽预算应保留到 categories\\Z: {wide}"
        );
        let narrow = middle_ellipsis(path, 16);
        assert!(narrow.starts_with("D:\\…\\"), "窄预算保留根: {narrow}");
        assert!(
            narrow.ends_with("categories\\Z") || narrow.ends_with("Z"),
            "尾部至少保留最后一段: {narrow}"
        );
    }

    #[test]
    fn middle_ellipsis_distinguishes_different_deep_paths() {
        // 回归：两条不同的同名目录以前都会被压成 "D:\…\3\z"。
        let a = middle_ellipsis(r"D:\project\alpha\temp\3\z", 40);
        let b = middle_ellipsis(r"D:\project\beta\other\3\z", 40);
        assert_ne!(a, b, "不同路径的中段省略结果不能相同: {a} vs {b}");
    }

    #[test]
    fn middle_ellipsis_keeps_short_paths_and_handles_unc() {
        assert_eq!(middle_ellipsis(r"D:\use", 40), r"D:\use");
        let unc = middle_ellipsis(r"\\server\share\a\b\c\Z", 20);
        assert!(unc.starts_with("\\\\server\\share"), "UNC 根保留: {unc}");
        assert!(unc.ends_with('Z'));
    }

    #[test]
    fn local_date_has_canonical_shape() {
        // 形态断言（避免时区相关断言）：4-2-2 位数字，年份合理。
        let cases = [
            0i64,
            1_600_000_000,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
                - 40 * 86400,
        ];
        for ts in cases {
            let s = format_local_date(ts);
            let parts: Vec<&str> = s.split('-').collect();
            assert_eq!(parts.len(), 3, "非 YYYY-MM-DD: {s}");
            assert_eq!(parts[0].len(), 4, "年份位数: {s}");
            assert_eq!(parts[1].len(), 2, "月份位数: {s}");
            assert_eq!(parts[2].len(), 2, "日期位数: {s}");
            let y: i32 = parts[0].parse().expect("年份可解析");
            assert!((1970..=3000).contains(&y), "年份合理: {s}");
        }
    }
}
