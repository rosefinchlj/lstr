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

use crate::history::History;
use crate::model::{Entry, Kind};
use crate::search::{apps, fuzzy, pinyin, rank};
use crate::win::browserdata;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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

/// 一次加载结果：条目、一一对应的小写 URL（搜索时零分配）与收藏夹路径。
pub struct BookmarkIndex {
    marks: Vec<Entry>,
    urls: Vec<String>,
    /// 与 `marks` 一一对应：「收藏夹栏 › 工作 › 需求」；无目录时为空串。
    folders: Vec<String>,
}

impl BookmarkIndex {
    pub fn empty() -> Self {
        Self {
            marks: Vec::new(),
            urls: Vec::new(),
            folders: Vec::new(),
        }
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
        let (marks, folders) = build_entries(raw);
        let urls = marks
            .iter()
            .map(|entry| entry.full_path().to_lowercase())
            .collect();
        Self {
            marks,
            urls,
            folders,
        }
    }

    pub fn count(&self) -> usize {
        self.marks.len()
    }

    /// 书签 id → 所属收藏夹路径。列表行据此显示「收藏夹栏 › 工作 › 需求」。
    pub fn folder_map(&self) -> HashMap<String, String> {
        self.marks
            .iter()
            .zip(&self.folders)
            .filter(|(_, folder)| !folder.is_empty())
            .map(|(entry, folder)| (entry.id.clone(), folder.clone()))
            .collect()
    }

    /// 按名称列出前 `limit` 条（加载时已按名称排序，不读历史）。
    /// 只用作空输入的**兜底**顺序：正常路径见 [`Self::used_page`]（读过历史，
    /// 用过的按 frecency 排前），本方法留给历史读不到（锁中毒）时的降级场景。
    pub fn top(&self, limit: usize) -> Vec<Entry> {
        self.marks.iter().take(limit).cloned().collect()
    }

    /// 书签页空输入的使用学习排序：用过的按 frecency 排前，其余保持名称序。
    /// 传全量索引（不要先 `top`），名称窗口外的常用书签才升得上来。
    pub fn used_page(&self, history: &History, take: usize) -> Vec<Entry> {
        rank::usage_page(self.marks.iter(), history, take)
    }

    /// 测试辅助：用给定条目直接建索引，跳过浏览器收藏夹读取。
    /// 保持传入顺序（生产路径按名称升序，测试自己控制「自然序」）；
    /// `urls` 由条目路径推出，`folders` 留空（`used_page` 不看这两个）。
    #[cfg(test)]
    pub(crate) fn for_test(entries: Vec<Entry>) -> Self {
        let urls = entries
            .iter()
            .map(|entry| entry.full_path().to_lowercase())
            .collect();
        Self {
            marks: entries,
            urls,
            folders: Vec::new(),
        }
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
    /// 各级收藏夹名称，用「 › 」连接（含浏览器根目录，如「收藏夹栏」）。
    folder: String,
}

fn load_chromium_file(path: &Path, raw: &mut Vec<RawBookmark>) {
    let Ok(text) = fs::read_to_string(path) else {
        lstr_index::snapshot::debug_log(&format!(
            "bookmarks: read failed: {}",
            path.display()
        ));
        return;
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        lstr_index::snapshot::debug_log(&format!(
            "bookmarks: invalid JSON: {}",
            path.display()
        ));
        return;
    };
    let Some(roots) = root.get("roots").and_then(Value::as_object) else {
        return;
    };
    for node in roots.values() {
        walk_chromium_node(node, &mut Vec::new(), raw);
    }
}

fn walk_chromium_node(node: &Value, folders: &mut Vec<String>, raw: &mut Vec<RawBookmark>) {
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
            raw.push(RawBookmark {
                name,
                url: url.to_string(),
                folder: folders.join(" › "),
            });
        }
        // folder（或缺少 type 的容器）：把目录名压栈后继续下潜，书签节点
        // 记录当前完整层级（收藏夹栏 › 工作 › 需求）。根目录也在 roots 下，
        // 所以「收藏夹栏 / 其他收藏夹 / 移动收藏夹」自然成为第一级。
        _ => {
            let name = node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let pushed = !name.is_empty();
            if pushed {
                folders.push(name);
            }
            if let Some(children) = node.get("children").and_then(Value::as_array) {
                for child in children {
                    walk_chromium_node(child, folders, raw);
                }
            }
            if pushed {
                folders.pop();
            }
        }
    }
}

/// Firefox 的根目录在库里存的是内部英文键（menu/toolbar/…），展示时换成
/// 中文；用户自建目录原样保留。
fn firefox_folder_name(title: &str) -> String {
    match title {
        "menu" => "书签菜单",
        "toolbar" => "书签工具栏",
        "other" | "unfiled" => "其他书签",
        "mobile" => "移动书签",
        "tags" => "标签",
        other => other,
    }
    .to_string()
}

/// 从 parent 往上收集目录链（跳过空标题的根节点），根在前的展示顺序。
fn firefox_folder_path(
    parent: i64,
    folders: &HashMap<i64, (i64, String)>,
) -> String {
    let mut names = Vec::new();
    let mut current = parent;
    for _ in 0..16 {
        let Some((next, title)) = folders.get(&current) else {
            break;
        };
        if title.is_empty() {
            break;
        }
        names.push(firefox_folder_name(title));
        if *next <= 0 {
            break;
        }
        current = *next;
    }
    names.reverse();
    names.join(" › ")
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
            // 先把所有目录（type=2）的 parent/title 读进内存，书签逐条向上
            // 拼层级；单表几千行，比逐条递归 SQL 快得多。
            let mut folders: HashMap<i64, (i64, String)> = HashMap::new();
            if let Ok(mut stmt) = conn.prepare(
                "SELECT id, parent, COALESCE(title, '') FROM moz_bookmarks WHERE type = 2",
            ) {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                }) {
                    for (id, parent, title) in rows.flatten() {
                        folders.insert(id, (parent, title));
                    }
                }
            }
            let sql = "SELECT COALESCE(b.title, ''), p.url, b.parent \
                       FROM moz_bookmarks b JOIN moz_places p ON p.id = b.fk \
                       WHERE b.type = 1 AND p.url IS NOT NULL";
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                }) {
                    for (name, url, parent) in rows.flatten() {
                        if is_web_url(&url) {
                            raw.push(RawBookmark {
                                name: name.trim().to_string(),
                                url,
                                folder: firefox_folder_path(parent, &folders),
                            });
                        }
                    }
                }
            }
        } else {
            lstr_index::snapshot::debug_log(&format!(
                "bookmarks: firefox copy open failed: {}",
                db.display()
            ));
        }
    } else {
        lstr_index::snapshot::debug_log(&format!(
            "bookmarks: firefox places copy failed: {}",
            db.display()
        ));
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

/// 去重、构建 Entry，并保留每条书签的收藏夹路径（与 Entry 一一对应）。
fn build_entries(raw: Vec<RawBookmark>) -> (Vec<Entry>, Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut marks: Vec<(Entry, String)> = Vec::new();
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
        marks.push((
            Entry {
                id: format!("bm:{url}"),
                kind: Kind::Bookmark,
                pinyin: Some(pinyin::keys(&name)),
                initial: apps::initial_for(&name),
                tone: apps::tone_for(&name),
                name,
                path: Some(PathBuf::from(url)),
                size: None,
                modified: None,
            },
            item.folder,
        ));
        if marks.len() >= MAX_BOOKMARKS {
            break;
        }
    }
    marks.sort_by(|(a, _), (b, _)| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    marks.into_iter().unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chromium_bookmark(name: &str, url: &str) -> Value {
        serde_json::json!({"type": "url", "name": name, "url": url})
    }

    fn raw(name: &str, url: &str) -> RawBookmark {
        RawBookmark {
            name: name.into(),
            url: url.into(),
            folder: String::new(),
        }
    }

    fn raw_in(name: &str, url: &str, folder: &str) -> RawBookmark {
        RawBookmark {
            name: name.into(),
            url: url.into(),
            folder: folder.into(),
        }
    }

    #[test]
    fn walks_nested_folders_and_records_full_folder_path() {
        let json = serde_json::json!({
            "roots": {
                "bookmark_bar": {
                    "type": "folder", "name": "收藏夹栏",
                    "children": [
                        chromium_bookmark("Rust", "https://www.rust-lang.org/"),
                        {"type": "folder", "name": "开发", "children": [
                            chromium_bookmark("Docs", "https://doc.rust-lang.org/book/"),
                            chromium_bookmark("脚本", "javascript:alert(1)")
                        ]}
                    ]
                },
                "other": {"type": "folder", "name": "其他收藏夹", "children": [
                    chromium_bookmark("", "https://example.com/a/b")
                ]}
            }
        });
        let mut raw = Vec::new();
        for node in json["roots"].as_object().unwrap().values() {
            walk_chromium_node(node, &mut Vec::new(), &mut raw);
        }
        let (marks, folders) = build_entries(raw);
        assert_eq!(marks.len(), 3, "javascript: 应被过滤: {marks:?}");
        let rust = marks.iter().position(|entry| entry.name == "Rust").unwrap();
        assert_eq!(marks[rust].kind, Kind::Bookmark);
        assert_eq!(marks[rust].full_path(), "https://www.rust-lang.org/");
        assert_eq!(folders[rust], "收藏夹栏", "根目录也算一级");
        let docs = marks.iter().position(|entry| entry.name == "Docs").unwrap();
        assert_eq!(folders[docs], "收藏夹栏 › 开发", "嵌套目录逐级保留");
        // 标题为空 → host 兜底。
        assert!(marks.iter().any(|entry| entry.name == "example.com"));
    }

    #[test]
    fn duplicate_urls_are_merged_and_keep_first_folder() {
        let raw = vec![
            raw_in("Rust", "https://rust-lang.org/", "收藏夹栏 › 开发"),
            raw_in("Rust 2", "HTTPS://rust-lang.org/", "其他收藏夹"),
        ];
        let (marks, folders) = build_entries(raw);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].name, "Rust");
        assert_eq!(folders[0], "收藏夹栏 › 开发");
    }

    fn index_from(raw: Vec<RawBookmark>) -> BookmarkIndex {
        let (marks, folders) = build_entries(raw);
        let urls = marks
            .iter()
            .map(|entry| entry.full_path().to_lowercase())
            .collect();
        BookmarkIndex {
            marks,
            urls,
            folders,
        }
    }

    #[test]
    fn top_is_name_sorted_and_limited() {
        // 「书签」页空输入 = 按名称列出全部书签；limit 只取一屏。
        let index = index_from(vec![
            raw("beta", "https://b.example/"),
            raw("Alpha", "https://a.example/"),
            raw("gamma", "https://c.example/"),
        ]);
        let top = index.top(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "Alpha", "按名称升序（大小写不敏感）");
        assert_eq!(top[1].name, "beta");
    }

    #[test]
    fn search_hits_name_pinyin_and_url() {
        let index = index_from(vec![
            raw("GitHub", "https://github.com/"),
            raw("掘金", "https://juejin.cn/"),
            raw("文档", "https://doc.rust-lang.org/book/"),
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
    fn folder_map_exposes_hierarchy_for_ui() {
        let index = index_from(vec![
            raw_in("GitHub", "https://github.com/", "收藏夹栏 › 开发"),
            raw("无目录", "https://example.com/"),
        ]);
        let map = index.folder_map();
        let github = index
            .search("github", 5)
            .into_iter()
            .find(|entry| entry.name == "GitHub")
            .unwrap();
        assert_eq!(map.get(&github.id).map(String::as_str), Some("收藏夹栏 › 开发"));
        assert_eq!(map.len(), 1, "没有目录的书签不写空值");
    }

    #[test]
    fn firefox_places_query_reads_bookmarks_and_folder_path() {
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
                 CREATE TABLE moz_bookmarks (id INTEGER PRIMARY KEY, type INTEGER, fk INTEGER,
                     title TEXT, parent INTEGER);
                 INSERT INTO moz_places (id, url) VALUES (1, 'https://mozilla.org/'), (2, 'place:sort=8');
                 INSERT INTO moz_bookmarks (id, type, fk, title, parent) VALUES
                     (1, 2, NULL, '', 0),
                     (2, 2, NULL, 'menu', 1),
                     (3, 2, NULL, '工作', 2),
                     (4, 1, 1, 'Mozilla', 3),
                     (5, 1, 2, '最近', 2);",
            )
            .unwrap();
        }
        let mut raw = Vec::new();
        load_firefox_db(&db, &mut raw);
        let (marks, folders) = build_entries(raw);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].name, "Mozilla");
        assert_eq!(marks[0].full_path(), "https://mozilla.org/");
        // Firefox 的内部根目录键 menu → 中文「书签菜单」，用户目录原样保留。
        assert_eq!(folders[0], "书签菜单 › 工作");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn used_page_lifts_bookmark_used_outside_the_name_window() {
        let index = index_from(vec![
            raw("Alpha", "https://a.example/"),
            raw("beta", "https://b.example/"),
            raw("gamma", "https://c.example/"),
        ]);
        let mut history = History::in_memory();
        let gamma = index
            .marks
            .iter()
            .find(|mark| mark.name == "gamma")
            .expect("gamma 书签应存在");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        history.seed(gamma, 3, now);

        let page = index.used_page(&history, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "gamma", "用过的书签必须排到名称窗口之前");
        assert_eq!(page[1].name, "Alpha", "没用过的仍按名称序");
    }
}
