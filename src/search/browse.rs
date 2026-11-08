//! 路径模式：输入形如 `D:\foo\bar` 或 `\\server\share` 时直接列目录，不走 Everything。
//! 这是 Listary 的「就地翻目录」：路径模式下 Enter/→ 进入子文件夹，
//! Backspace 沿文本自然回退上级，全程不需要资源管理器。

use crate::model::{Entry, Kind};
use crate::search::pinyin;
use std::time::SystemTime;

pub struct PathQuery {
    /// 要列出的目录（含末尾分隔符），如 `D:\proj\`。
    pub dir: String,
    /// 最后一段：按它过滤子项名字（子串或拼音）。
    pub filter: String,
}

/// 判定输入是否处于路径模式。驱动器路径（`D:` / `D:\` / `D:/`）或 UNC（`\\server`）触发；
/// Everything 语法（`path:E:\foo`）、`>`/`a `/`f ` 前缀都不会误判。
/// 只打 `D:`（冒号后没分隔符）也算——按 Listary 习惯直接列 D:\ 根目录，
/// 否则会落进 Everything 被当成「D 盘全盘」查询（三百万条候选、排序无意义）。
pub fn parse(raw: &str) -> Option<PathQuery> {
    let trimmed = raw.trim_start();
    let bytes = trimmed.as_bytes();
    let is_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let is_unc = trimmed.starts_with("\\\\");
    if !is_drive && !is_unc {
        return None;
    }
    // "D:" / "D:prog"：冒号后没有分隔符，统一按 D:\ 根目录处理，
    // 残留文字当过滤词（D:prog → D:\ 下过滤 prog）。
    if is_drive && !(bytes.len() >= 3 && matches!(bytes[2], b'\\' | b'/')) {
        return Some(PathQuery {
            dir: format!("{}:\\", &trimmed[..1]),
            filter: trimmed.get(2..).unwrap_or("").to_string(),
        });
    }
    let split = trimmed.rfind(['\\', '/'])?;
    let (dir, filter) = trimmed.split_at(split + 1);
    Some(PathQuery {
        dir: dir.replace('/', "\\"),
        filter: filter.to_string(),
    })
}

/// 读取目录子项；目录不存在或无权限时返回空（UI 显示「没有匹配项」）。
pub fn list_children(dir: &str) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .filter_map(|item| {
            let item = item.ok()?;
            let name = item.file_name().to_string_lossy().to_string();
            let path = item.path();
            let file_type = item.file_type().ok()?;
            // Windows 上 DirEntry::metadata 复用枚举时的 WIN32_FIND_DATA，
            // 不产生额外磁盘 IO，大目录也能逐条取。
            let meta = item.metadata().ok();
            let kind = if file_type.is_dir() {
                Kind::Folder
            } else if name
                .rsplit_once('.')
                .map(|(_, ext)| ext.eq_ignore_ascii_case("exe"))
                .unwrap_or(false)
            {
                Kind::Program
            } else {
                Kind::File
            };
            Some(Entry {
                id: format!("path:{}", path.display()),
                kind,
                pinyin: Some(pinyin::keys(&name)),
                name,
                path: Some(path),
                size: meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
                modified: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
                initial: String::new(),
                tone: 0,
            })
        })
        .collect();
    // 文件夹在前，各自按名称（忽略大小写）排序；rank 阶段再叠加过滤词得分。
    entries.sort_by(|a, b| {
        let folder_a = a.kind == Kind::Folder;
        let folder_b = b.kind == Kind::Folder;
        folder_b
            .cmp(&folder_a)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// 目录内容缓存：以目录自身 mtime 为失效依据。
/// 大目录（如 System32 约四千项）每个击键都重读会掉帧，mtime 未变就复用。
pub struct DirCache {
    dir: String,
    stamp: Option<SystemTime>,
    children: Vec<Entry>,
}

impl DirCache {
    pub fn new() -> Self {
        Self {
            dir: String::new(),
            stamp: None,
            children: Vec::new(),
        }
    }

    pub fn children(&mut self, dir: &str) -> &[Entry] {
        let stamp = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
        if self.dir != dir || self.stamp != stamp {
            self.children = list_children(dir);
            self.dir = dir.to_string();
            self.stamp = stamp;
        }
        &self.children
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_drive_root() {
        let pq = parse(r"D:\").expect("驱动器根目录");
        assert_eq!(pq.dir, r"D:\");
        assert_eq!(pq.filter, "");
    }

    #[test]
    fn bare_drive_colon_browses_root() {
        // Listary 习惯：只打 "D:" 直接列 D:\ 根目录。
        let pq = parse("D:").expect("裸盘符");
        assert_eq!(pq.dir, r"D:\");
        assert_eq!(pq.filter, "");
        let pq = parse("d:prog").expect("盘符 + 过滤词");
        assert_eq!(pq.dir, r"d:\");
        assert_eq!(pq.filter, "prog");
    }

    #[test]
    fn splits_dir_and_filter() {
        let pq = parse(r"D:\proj\lis").expect("子目录");
        assert_eq!(pq.dir, r"D:\proj\");
        assert_eq!(pq.filter, "lis");
    }

    #[test]
    fn normalizes_forward_slashes() {
        let pq = parse("D:/proj/src").expect("正斜杠");
        assert_eq!(pq.dir, "D:\\proj\\");
        assert_eq!(pq.filter, "src");
    }

    #[test]
    fn parses_unc() {
        let pq = parse(r"\\server\share\do").expect("UNC");
        assert_eq!(pq.dir, r"\\server\share\");
        assert_eq!(pq.filter, "do");
    }

    #[test]
    fn rejects_non_paths() {
        assert!(parse("hello").is_none());
        assert!(parse(r"path:E:\foo").is_none());
        assert!(parse(r"> D:\foo").is_none());
        assert!(parse(r"a D:\foo").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn lists_current_project_dir() {
        // cargo 测试的工作目录是 crate 根，里面必有 Cargo.toml 和 src。
        let children = list_children(".");
        assert!(children.iter().any(|e| e.name == "Cargo.toml"));
        assert!(children
            .iter()
            .any(|e| e.kind == Kind::Folder && e.name == "src"));
        // 文件夹排在文件前。
        let first_file = children.iter().position(|e| e.kind != Kind::Folder);
        if let Some(pos) = first_file {
            assert!(children[..pos].iter().all(|e| e.kind == Kind::Folder));
        }
    }

    #[test]
    fn missing_dir_yields_empty() {
        assert!(list_children(r"Z:\no\such\dir\lstr-test").is_empty());
    }
}
