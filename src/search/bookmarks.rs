//! 浏览器收藏夹索引。
//!
//! 支持的来源（只读，不写回任何浏览器数据）：
//! - Chromium 系：Chrome / Edge / Brave / Vivaldi / Chromium 的
//!   `<User Data>\<Profile>\Bookmarks`（JSON），以及 Opera 的
//!   `Opera Stable\Bookmarks`；
//! - Firefox：`Profiles\<profile>\places.sqlite`（复制到临时目录后用
//!   rusqlite 只读查询，避免浏览器持有 WAL 锁时打不开）。
//!
//! 书签只参与「全部」范围的搜索：空输入不展示，筛选条（位置/时间/类型）
//! 生效时也不混入——与应用的语义一致。Enter 由调用方交给默认浏览器打开。

use crate::model::{Entry, Kind};
use crate::search::{apps, fuzzy, pinyin};
use crate::win::browserdata;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

/// 书签总量上限：异常 profile（同步/导入出错）也不至于把内存和结果数拖垮。
const MAX_BOOKMARKS: usize = 20_000;
/// 只有这些协议能安全交给系统默认程序打开；`javascript:`/`chrome:` 之类
/// 离开浏览器上下文没有意义，直接跳过。
const WEB_SCHEMES: [&str; 4] = ["http://", "https://", "ftp://", "file://"];

/// 一次加载结果：条目与一一对应的小写 URL（搜索时零分配）。
pub struct BookmarkIndex {
    marks: Vec<Entry>,
    urls: Vec<String>,
}

impl BookmarkIndex {
    pub fn empty() -> Self {
        Self { marks: Vec::new(), urls: Vec::new() }
    }

    /// 扫描全部已知浏览器并合并（同一 URL 只保留首次出现的书签）。
    pub fn load() -> Self {
        let mut raw: Vec<RawBookmark> = Vec::new();
        for root in browserdata::chromium_user_data_roots() {
            for profile in browserdata::chromium_profile_dirs(&root) {
                let file = profile.join("Bookmarks");
                if file.is_file() {
                    load_chromium_file(&file, &mut raw);
                }
            }
        }
        for profile in browserdata::firefox_profile_dirs() {
            let db = profile.join("places.sqlite");
            if db.is_file() {
                load_firefox_db(&db, &mut raw);
            }
        }
        let marks = build_entries(raw);
        let urls = marks
            .iter()
            .map(|entry| entry.full_path().to_lowercase())
            .collect();
        Self { marks, urls }
    }

    pub fn count(&self) -> usize {
        self.marks.len()
    }

    /// 书签页空输入：按名称列出前 `limit` 条（加载时已按名称排序）。
    /// 与「应用」页的空输入语义一致，不掺历史/常用排序。
    pub fn top(&self, limit: usize) -> Vec<Entry> {
        self.marks.iter().take(limit).cloned().collect()
    }

    /// 名称/拼音/模糊/URL 匹配。空查询返回空（空输入展示「最近使用」）。
    pub fn search(&self, q: &str, limit: usize) -> Vec<Entry> {
        let needle = q.trim().to_lowercase();
        if needle.is_empty() || limit == 0 || self.marks.is_empty() {
            return Vec::new();
        }
        // 单字符不搜 URL：否则任意一个字母都会把整份书签扫进「全部」结果。
        let url_needle = needle.chars().count() >= 2;
        let mut hits: Vec<(u8, String, &Entry)> = Vec::new();
        for (entry, url) in self.marks.iter().zip(&self.urls) {
            let name = entry.name.to_lowercase();
            let py_storage;
            let py = match &entry.pinyin {
                Some(pinyin) => pinyin.as_str(),
                None => {
                    py_storage = pinyin::keys(&entry.name);
                    py_storage.as_str()
                }
            };
            let py_prefix = py.split(' ').any(|token| token.starts_with(&needle));
            let py_contains = py.split(' ').any(|token| token.contains(&needle));
            let rank = if name.starts_with(&needle) {
                0
            } else if name.contains(&needle) {
                1
            } else if py_prefix {
                2
            } else if py_contains {
                3
            } else if fuzzy::score_gated(&entry.name, &needle).is_some() {
                // 名字本身的逐字符闸门模糊（gh → GitHub）。书签标题常是
                // 整句中文，拼音串很长：不再做「散字母密度」模糊，否则
                // rust 会命中「阮一峰…示例教程」这类伪结果。
                4
            } else if url_needle && url.contains(&needle) {
                5
            } else {
                continue;
            };
            hits.push((rank, name, entry));
        }
        hits.sort_by(|(rank_a, lower_a, a), (rank_b, lower_b, b)| {
            rank_a
                .cmp(rank_b)
                .then_with(|| lower_a.cmp(lower_b))
                .then_with(|| a.name.cmp(&b.name))
        });
        hits.into_iter()
            .take(limit)
            .map(|(_, _, entry)| entry.clone())
            .collect()
    }
}

/// 全部书签源文件（含存在性过滤）——用于「文件没变就不重载」的戳。
pub fn source_stamp() -> Vec<(String, u64, u64)> {
    let mut stamp = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for root in browserdata::chromium_user_data_roots() {
        for profile in browserdata::chromium_profile_dirs(&root) {
            let file = profile.join("Bookmarks");
            if file.is_file() {
                files.push(file);
            }
        }
    }
    for profile in browserdata::firefox_profile_dirs() {
        let db = profile.join("places.sqlite");
        if db.is_file() {
            files.push(db);
        }
    }
    for file in files {
        let Ok(meta) = fs::metadata(&file) else { continue };
        let Ok(modified) = meta.modified() else { continue };
        let secs = modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        stamp.push((file.display().to_string(), secs, meta.len()));
    }
    stamp.sort();
    stamp
}

#[derive(Debug)]
struct RawBookmark {
    name: String,
    url: String,
}

fn load_chromium_file(path: &Path, raw: &mut Vec<RawBookmark>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(roots) = root.get("roots").and_then(Value::as_object) else {
        return;
    };
    for node in roots.values() {
        walk_chromium_node(node, raw);
    }
}

fn walk_chromium_node(node: &Value, raw: &mut Vec<RawBookmark>) {
    match node.get("type").and_then(Value::as_str) {
        Some("url") => {
            let url = node.get("url").and_then(Value::as_str).unwrap_or("").trim();
            if !is_web_url(url) {
                return;
            }
            let name = node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            raw.push(RawBookmark { name, url: url.to_string() });
        }
        // folder（或缺少 type 的容器）：继续下潜。书签栏/其他书签/移动设备
        // 书签都在 roots 下，递归即可，不需要关心具体分区名。
        _ => {
            if let Some(children) = node.get("children").and_then(Value::as_array) {
                for child in children {
                    walk_chromium_node(child, raw);
                }
            }
        }
    }
}

/// Firefox：复制主库 + WAL/SHM 到临时目录再只读查询（直接打开运行中的
/// places.sqlite 常撞 WAL 锁）。查询失败/文件被占用时静默跳过该 profile。
fn load_firefox_db(db: &Path, raw: &mut Vec<RawBookmark>) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "lstr-bookmarks-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if fs::create_dir_all(&tmp).is_err() {
        return;
    }
    let copy = tmp.join("places.sqlite");
    let copied = fs::copy(db, &copy).is_ok();
    if copied {
        for suffix in ["-wal", "-shm"] {
            let mut from: OsString = db.as_os_str().to_os_string();
            from.push(suffix);
            let from = PathBuf::from(from);
            if !from.is_file() {
                continue;
            }
            let mut to: OsString = copy.as_os_str().to_os_string();
            to.push(suffix);
            let _ = fs::copy(&from, PathBuf::from(to));
        }
    }
    if copied {
        if let Ok(conn) = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            let sql = "SELECT COALESCE(b.title, ''), p.url \
                       FROM moz_bookmarks b JOIN moz_places p ON p.id = b.fk \
                       WHERE b.type = 1 AND p.url IS NOT NULL";
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                }) {
                    for (name, url) in rows.flatten() {
                        if is_web_url(&url) {
                            raw.push(RawBookmark { name: name.trim().to_string(), url });
                        }
                    }
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&tmp);
}

fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    WEB_SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
}

/// URL 主机名（去掉 `www.`）：标题为空的书签用 host 当名字。
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    host.trim_start_matches("www.").to_string()
}

fn build_entries(raw: Vec<RawBookmark>) -> Vec<Entry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut marks = Vec::new();
    for item in raw {
        let url = item.url.trim().to_string();
        if !is_web_url(&url) || !seen.insert(url.to_lowercase()) {
            continue;
        }
        let name = if item.name.trim().is_empty() {
            url_host(&url)
        } else {
            item.name.trim().to_string()
        };
        if name.is_empty() {
            continue;
        }
        marks.push(Entry {
            id: format!("bm:{url}"),
            kind: Kind::Bookmark,
            pinyin: Some(pinyin::keys(&name)),
            initial: apps::initial_for(&name),
            tone: apps::tone_for(&name),
            name,
            path: Some(PathBuf::from(url)),
            size: None,
            modified: None,
        });
        if marks.len() >= MAX_BOOKMARKS {
            break;
        }
    }
    marks.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chromium_bookmark(name: &str, url: &str) -> Value {
        serde_json::json!({"type": "url", "name": name, "url": url})
    }

    #[test]
    fn walks_nested_folders_and_skips_non_web_urls() {
        let json = serde_json::json!({
            "roots": {
                "bookmark_bar": {
                    "type": "folder", "name": "书签栏",
                    "children": [
                        chromium_bookmark("Rust", "https://www.rust-lang.org/"),
                        {"type": "folder", "name": "开发", "children": [
                            chromium_bookmark("Docs", "https://doc.rust-lang.org/book/"),
                            chromium_bookmark("脚本", "javascript:alert(1)")
                        ]}
                    ]
                },
                "other": {"type": "folder", "name": "其他", "children": [
                    chromium_bookmark("", "https://example.com/a/b")
                ]}
            }
        });
        let mut raw = Vec::new();
        for node in json["roots"].as_object().unwrap().values() {
            walk_chromium_node(node, &mut raw);
        }
        let marks = build_entries(raw);
        assert_eq!(marks.len(), 3, "javascript: 应被过滤: {marks:?}");
        let rust = marks.iter().find(|entry| entry.name == "Rust").unwrap();
        assert_eq!(rust.kind, Kind::Bookmark);
        assert_eq!(rust.full_path(), "https://www.rust-lang.org/");
        // 标题为空 → host 兜底。
        assert!(marks.iter().any(|entry| entry.name == "example.com"));
    }

    #[test]
    fn duplicate_urls_are_merged() {
        let raw = vec![
            RawBookmark { name: "Rust".into(), url: "https://rust-lang.org/".into() },
            RawBookmark { name: "Rust 2".into(), url: "HTTPS://rust-lang.org/".into() },
        ];
        let marks = build_entries(raw);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].name, "Rust");
    }

    fn index_from(raw: Vec<RawBookmark>) -> BookmarkIndex {
        let marks = build_entries(raw);
        let urls = marks
            .iter()
            .map(|entry| entry.full_path().to_lowercase())
            .collect();
        BookmarkIndex { marks, urls }
    }

    #[test]
    fn top_is_name_sorted_and_limited() {
        // 「书签」页空输入 = 按名称列出全部书签；limit 只取一屏。
        let index = index_from(vec![
            RawBookmark { name: "beta".into(), url: "https://b.example/".into() },
            RawBookmark { name: "Alpha".into(), url: "https://a.example/".into() },
            RawBookmark { name: "gamma".into(), url: "https://c.example/".into() },
        ]);
        let top = index.top(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "Alpha", "按名称升序（大小写不敏感）");
        assert_eq!(top[1].name, "beta");
    }

    #[test]
    fn search_hits_name_pinyin_and_url() {
        let index = index_from(vec![
            RawBookmark { name: "GitHub".into(), url: "https://github.com/".into() },
            RawBookmark { name: "掘金".into(), url: "https://juejin.cn/".into() },
            RawBookmark { name: "文档".into(), url: "https://doc.rust-lang.org/book/".into() },
        ]);
        assert_eq!(index.search("git", 10)[0].name, "GitHub", "名称前缀优先");
        assert_eq!(index.search("juejin", 10)[0].name, "掘金", "拼音/URL 命中");
        assert_eq!(index.search("github.com", 10)[0].name, "GitHub", "URL 命中");
        assert!(index.search("", 10).is_empty(), "空查询不返回书签");
        assert!(
            index.search("z", 10).is_empty(),
            "单字符不扫 URL（否则任意字母都会扫出整份书签）"
        );
    }

    #[test]
    fn firefox_places_query_reads_bookmarks() {
        let dir = std::env::temp_dir().join(format!(
            "lstr-bm-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("places.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
                 CREATE TABLE moz_bookmarks (id INTEGER PRIMARY KEY, type INTEGER, fk INTEGER, title TEXT);
                 INSERT INTO moz_places (id, url) VALUES (1, 'https://mozilla.org/'), (2, 'place:sort=8');
                 INSERT INTO moz_bookmarks (id, type, fk, title) VALUES (1, 1, 1, 'Mozilla'), (2, 1, 2, '最近');",
            )
            .unwrap();
        }
        let mut raw = Vec::new();
        load_firefox_db(&db, &mut raw);
        let marks = build_entries(raw);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].name, "Mozilla");
        assert_eq!(marks[0].full_path(), "https://mozilla.org/");
        let _ = fs::remove_dir_all(&dir);
    }
}
