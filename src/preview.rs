//! 预览面板内容：图片缩略图（shell 缩略图，免自解码）与文本头部速览。
//! 由后台线程加载（见 app.rs 的 PreviewStore），按扩展名判定预览类型。

use crate::model::{Entry, Kind};
use eframe::egui::ColorImage;

/// 加载完成的预览内容。
pub enum PreviewData {
    Image(ColorImage),
    Text(String),
}

/// 预览类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreviewKind {
    Image,
    Text,
    None,
}

pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff"
    )
}

pub fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt" | "md" | "rs" | "toml" | "json" | "xml" | "html" | "css" | "js" | "ts"
            | "py" | "c" | "cpp" | "h" | "hpp" | "go" | "java" | "log" | "csv"
            | "yaml" | "yml" | "ini" | "cfg" | "conf" | "sh" | "bat" | "ps1" | "sql"
    )
}

/// 按条目类型与扩展名判定预览类型（只对普通文件预览）。
pub fn kind_of(entry: &Entry) -> PreviewKind {
    if entry.kind != Kind::File {
        return PreviewKind::None;
    }
    let Some(path) = &entry.path else {
        return PreviewKind::None;
    };
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return PreviewKind::None;
    };
    let ext = ext.to_ascii_lowercase();
    if is_image_ext(&ext) {
        PreviewKind::Image
    } else if is_text_ext(&ext) {
        PreviewKind::Text
    } else {
        PreviewKind::None
    }
}

/// 阻塞加载（后台线程调用）。图片取 shell 缩略图；文本读头部。
pub fn load(entry: &Entry) -> Option<PreviewData> {
    let path = entry.path.as_ref()?;
    let path_str = path.to_string_lossy();
    match kind_of(entry) {
        PreviewKind::Image => {
            let img = crate::win::icons::load_thumbnail(&path_str, 256)?;
            Some(PreviewData::Image(img))
        }
        PreviewKind::Text => Some(PreviewData::Text(read_text_head(path)?)),
        PreviewKind::None => None,
    }
}

/// 读文件头部用于速览：最多 4KB、前 40 行、每行截断 200 字符；去 BOM，
/// 非 UTF-8 用 lossy。不预览二进制（按扩展名已筛）。
fn read_text_head(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    let text = text.trim_start_matches('\u{feff}');
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i >= 40 {
            break;
        }
        let line: String = line.chars().take(200).collect();
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file_entry(name: &str) -> Entry {
        Entry {
            id: format!("file:{name}"),
            kind: Kind::File,
            name: name.to_string(),
            path: Some(PathBuf::from(format!("D:\\x\\{name}"))),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        }
    }

    #[test]
    fn detects_preview_kind() {
        assert_eq!(kind_of(&file_entry("a.png")), PreviewKind::Image);
        assert_eq!(kind_of(&file_entry("a.JPG")), PreviewKind::Image);
        assert_eq!(kind_of(&file_entry("a.rs")), PreviewKind::Text);
        assert_eq!(kind_of(&file_entry("a.md")), PreviewKind::Text);
        assert_eq!(kind_of(&file_entry("a.exe")), PreviewKind::None);
        assert_eq!(kind_of(&file_entry("noext")), PreviewKind::None);
        let mut folder = file_entry("dir");
        folder.kind = Kind::Folder;
        assert_eq!(kind_of(&folder), PreviewKind::None);
    }

    #[test]
    fn reads_text_head_with_limits() {
        let dir = std::env::temp_dir().join(format!("lstr-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.txt");
        let content = "line1\nline2\nline3\n";
        std::fs::write(&path, content).unwrap();
        let head = read_text_head(&path).unwrap();
        assert!(head.contains("line1"));
        assert!(head.contains("line3"));
        // 行数上限
        let many: String = (0..100).map(|i| format!("l{i}\n")).collect();
        std::fs::write(&path, many).unwrap();
        let head = read_text_head(&path).unwrap();
        assert_eq!(head.lines().count(), 40);
        let _ = std::fs::remove_dir_all(&dir);
    }
}