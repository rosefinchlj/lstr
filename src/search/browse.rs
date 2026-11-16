//! 路径模式：输入形如 `D:\foo\bar` 或 `\\server\share` 时直接列目录，不走索引。
//! 这是 Listary 的「就地翻目录」：路径模式下 Enter/→ 进入子文件夹，
//! Backspace 沿文本自然回退上级，全程不需要资源管理器。

use crate::model::{Entry, Kind};
use crate::search::pinyin;
use std::time::SystemTime;
use windows::core::PCWSTR;
use windows::Win32::NetworkManagement::NetManagement::{NetApiBufferFree, MAX_PREFERRED_LENGTH};
use windows::Win32::Storage::FileSystem::{
    NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_MASK, STYPE_SPECIAL,
};

pub struct PathQuery {
    /// 要列出的目录（含末尾分隔符），如 `D:\proj\`。
    pub dir: String,
    /// 最后一段：按它过滤子项名字（子串或拼音）。
    pub filter: String,
}

/// 判定输入是否处于路径模式。驱动器路径（`D:\` / `D:/` / `D:prog`）或 UNC（`\\server`）触发；
/// 查询语法（`path:E:\foo`）、`>`/`a `/`f ` 前缀都不会误判。
/// 只打 `D:` 现在交给输入框盘符条件 chip（只搜 D 盘），不再浏览根目录；
/// 浏览根目录请打 `D:\`。`D:prog`（冒号后无分隔符但有文字）仍按
/// 「D:\ 根目录 + 过滤词」浏览。
/// **例外**：`D: <词>`（冒号后跟空白）同样不按路径浏览处理——这是搜索框的
/// 盘符过滤语法（`d: ap` = 只在 D 盘搜 ap）。
pub fn parse(raw: &str) -> Option<PathQuery> {
    let trimmed = raw.trim_start();
    let bytes = trimmed.as_bytes();
    let is_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let is_unc = trimmed.starts_with("\\\\");
    if !is_drive && !is_unc {
        return None;
    }
    // `d: <词>`：冒号后是空白 → 盘符过滤，不是目录浏览。
    if is_drive && matches!(bytes.get(2), Some(b' ' | b'\t')) {
        return None;
    }
    // 裸盘符 `D:` → 盘符条件 chip（只搜 D 盘），不是目录浏览。
    if is_drive && bytes.len() == 2 {
        return None;
    }
    // "D:prog"：冒号后没有分隔符，统一按 D:\ 根目录处理，
    // 残留文字当过滤词（D:prog → D:\ 下过滤 prog）。
    if is_drive && !(bytes.len() >= 3 && matches!(bytes[2], b'\\' | b'/')) {
        return Some(PathQuery {
            dir: format!("{}:\\", &trimmed[..1]),
            filter: trimmed.get(2..).unwrap_or("").to_string(),
        });
    }
    // UNC 服务器根（`\\server` / `\\server\`）：列共享；不能按「父目录 + 过滤词」
    // 拆，否则 dir 会变成 `\\`、filter 变成服务器名，列表永远为空。
    if is_unc {
        let rest = trimmed.trim_start_matches(['\\', '/']);
        let rest = rest.trim_end_matches(['\\', '/']);
        if !rest.is_empty() && !rest.contains(['\\', '/']) {
            return Some(PathQuery {
                dir: format!("\\\\{rest}\\"),
                filter: String::new(),
            });
        }
    }
    let split = trimmed.rfind(['\\', '/'])?;
    let (dir, filter) = trimmed.split_at(split + 1);
    Some(PathQuery {
        dir: dir.replace('/', "\\"),
        filter: filter.to_string(),
    })
}

/// `\\server` / `\\server\` → `Some("server")`；带共享名的路径返回 None。
fn unc_server_root(dir: &str) -> Option<&str> {
    let rest = dir.strip_prefix("\\\\")?;
    let rest = rest.trim_end_matches(['\\', '/']);
    if rest.is_empty() || rest.contains(['\\', '/']) {
        return None;
    }
    Some(rest)
}

fn share_entry(server: &str, name: &str) -> Entry {
    let path = std::path::PathBuf::from(format!(r"\\{server}\{name}"));
    Entry {
        id: format!("path:{}", path.display()),
        kind: Kind::Folder,
        pinyin: Some(pinyin::keys(name)),
        name: name.to_string(),
        path: Some(path),
        size: None,
        modified: None,
        initial: String::new(),
        tone: 0,
    }
}

/// 枚举 `\\server` 的磁盘共享（NetShareEnum level 1）。
/// 失败（主机离线/无权限/非 Windows 网络）返回空，UI 显示「没有匹配项」。
fn list_shares(server: &str) -> Vec<Entry> {
    let server_w: Vec<u16> = format!("\\\\{server}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut read: u32 = 0;
    let mut total: u32 = 0;
    // SAFETY: 服务器名以 NUL 结尾且在本函数存活；`buf`/`read`/`total` 是有效的
    // 输出指针；返回码为 0 时系统分配了 `buf`，下面用 NetApiBufferFree 释放。
    let status = unsafe {
        NetShareEnum(
            PCWSTR(server_w.as_ptr()),
            1,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
            None,
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let mut entries = Vec::new();
    if !buf.is_null() {
        for i in 0..read as usize {
            // SAFETY: `buf` 指向 `read` 个连续的 SHARE_INFO_1。
            let info = unsafe { &*(buf as *const SHARE_INFO_1).add(i) };
            if info.shi1_type.0 & STYPE_MASK.0 != STYPE_DISKTREE.0 {
                continue; // 跳过打印队列/IPC 等
            }
            if info.shi1_type.0 & STYPE_SPECIAL.0 != 0 {
                continue; // 跳过 C$、ADMIN$ 等管理共享
            }
            // SAFETY: NetShareEnum 返回的 netname 是缓冲区内的 NUL 结尾宽字符串。
            let Ok(name) = (unsafe { info.shi1_netname.to_string() }) else {
                continue;
            };
            if name.is_empty() || name.ends_with('$') {
                continue;
            }
            entries.push(share_entry(server, &name));
        }
        // SAFETY: `buf` 由 NetShareEnum 分配，必须用 NetApiBufferFree 释放。
        unsafe {
            let _ = NetApiBufferFree(Some(buf as *const core::ffi::c_void));
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// 读取目录子项；目录不存在或无权限时返回空（UI 显示「没有匹配项」）。
pub fn list_children(dir: &str) -> Vec<Entry> {
    // `\\server\` 不是文件系统目录：列出共享（B17）。
    if let Some(server) = unc_server_root(dir) {
        return list_shares(server);
    }
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

/// 完整路径消歧：`E:\foo\bar`（末尾无分隔符）若**本身就是已存在的目录**，
/// 按「列出 bar 的内容」处理；否则沿用 `parse` 的「父目录 + 过滤词」行为
/// （打字过程中的 `D:\proj\li` 仍按前缀过滤）。
///
/// 只对本地盘符路径做存在性检查：UNC 的 `is_dir` 是网络请求，不能每次击键都发。
pub fn resolve_exact(pq: PathQuery) -> PathQuery {
    if pq.filter.is_empty() {
        return pq;
    }
    let candidate = format!("{}{}", pq.dir, pq.filter);
    let is_local = candidate.len() >= 2 && candidate.as_bytes()[1] == b':';
    if is_local && std::path::Path::new(&candidate).is_dir() {
        PathQuery {
            dir: format!("{}\\", candidate.trim_end_matches(['\\', '/'])),
            filter: String::new(),
        }
    } else {
        pq
    }
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
    fn bare_drive_colon_is_not_browse() {
        // 裸盘符现在是输入框盘符条件 chip（只搜该盘），浏览根目录打 "D:\"。
        assert!(parse("D:").is_none(), "裸盘符应交给条件 chip");
        let pq = parse(r"D:\").expect("带分隔符仍是路径浏览");
        assert_eq!(pq.dir, r"D:\");
        let pq = parse("d:prog").expect("盘符 + 过滤词");
        assert_eq!(pq.dir, r"d:\");
        assert_eq!(pq.filter, "prog");
    }

    #[test]
    fn spaced_drive_is_filter_not_browse() {
        // `d: ap`（冒号后跟空白）是盘符过滤语法，不能吞成目录浏览。
        assert!(parse("d: ap").is_none(), "d: 空格后跟词 = 盘符过滤");
        assert!(parse("c:\tdir").is_none(), "制表符同理");
        // 无空白的老行为不受影响
        assert!(parse("d:prog").is_some());
        assert!(parse("d:\\prog").is_some());
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
    fn parses_bare_unc_server_root() {
        // B17：`\\server` / `\\server\` 是服务器根，列表应走共享枚举。
        for raw in [r"\\server", "\\\\server\\", r"\\server\\"] {
            let pq = parse(raw).expect("UNC server root");
            assert_eq!(pq.dir, r"\\server\");
            assert_eq!(pq.filter, "");
        }
        assert_eq!(unc_server_root(r"\\server\"), Some("server"));
        assert_eq!(unc_server_root(r"\\server"), Some("server"));
        assert_eq!(unc_server_root(r"\\server\share\"), None);
        assert_eq!(unc_server_root(r"D:\foo\"), None);
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
    fn exact_existing_dir_lists_its_children() {
        // 验收表 F23：输入完整目录路径（末尾无分隔符）应列出该目录内容，
        // 而不是把最后一段当成父目录的过滤词（那样只会显示目录自身）。
        let tag = format!("lstr-browse-acc-{}", std::process::id());
        let base = std::env::temp_dir().join(&tag);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("child")).unwrap();
        std::fs::write(base.join("child").join("a.txt"), b"x").unwrap();

        let raw = base.display().to_string();
        let pq = resolve_exact(parse(&raw).expect("完整目录路径"));
        assert_eq!(pq.filter, "", "已是完整目录时应清空过滤词");
        assert!(pq.dir.ends_with('\\'), "目录应以分隔符结尾: {}", pq.dir);
        let children = list_children(&pq.dir);
        assert!(
            children.iter().any(|e| e.name == "child"),
            "应列出子目录: {:?}",
            children.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        // 尾段不存在时保持旧行为：父目录 + 过滤词。
        let missing = format!("{}\\no-such-dir", base.display());
        let pq = resolve_exact(parse(&missing).expect("路径模式"));
        assert_eq!(pq.filter, "no-such-dir");

        let _ = std::fs::remove_dir_all(&base);
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
