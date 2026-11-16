//! 有道云笔记索引（只读）。
//!
//! 数据来源（Windows 客户端 8.x）：
//! `%APPDATA%\ynote-desktop\<账号>\ynote-data\<账号>.db`
//! 主库是未加密的 SQLite；客户端运行时会同时持有 `-wal`，只读连接可以并发读，
//! 读不到（同步/检查点占用）时退回「复制 db + wal + shm 到临时目录再读」。
//!
//! 两棵表：
//! - `note`：笔记/剪藏（`fileId`、`title`、`parentId`、`modifyTime`、`size`…）
//! - `note_book`：笔记本（文件夹）树（`fileId`、`title`、`parentId`…）
//!
//! 重要实测语义（2026-11-14，客户端 8.1.60）：
//! - 有效笔记 = `deleted IS NULL AND del=0 AND remoteRemoved=0 AND parentDeleted=0`；
//!   回收站里的 370 条 `del=1`，必须排掉。
//! - `namePath` 全为 NULL，文件夹路径要按 `parentId` 自己拼。
//! - `parentId='-2'` 是「桌面便签」，不在 `note_book`；`parentId='1'` 与
//!   `root` 表 id 都是虚拟根。
//! - 标题带内部后缀：`.note` / `.clip` 要剥掉显示，`.md` 保留。
//! - 没有 `youdaonote://` 深链，Enter 只能打开网页版：
//!   `https://note.youdao.com/web/#/file/<parentId>/note/<fileId>/`
//!   父目录不是真实笔记本时用 `/file/recent/note/<fileId>` 兜底。
//!
//! 命名（笔记/文件夹）检索在本模块完成；文件夹命中排在名称命中之后，避免
//! 「搜 rust 时整夹 8 条笔记压过标题就叫 Rust 的笔记」。

use crate::config::RankConfig;
use crate::history::History;
use crate::model::{Entry, Kind};
use crate::search::drive_weight::EffectiveWeights;
use crate::search::{apps, fuzzy, pinyin, rank};
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

/// 笔记总量上限：异常账号/多账号聚合也不至于把内存拖垮。
const MAX_NOTES: usize = 20_000;
/// 本地数据根（Windows 客户端）。账号目录名不固定（`yd.xxx` 或邮箱）。
const DATA_DIR: &str = "ynote-desktop";
/// 网页版深链前缀。
const WEB_BASE: &str = "https://note.youdao.com/web/#/file";
/// 桌面便签的虚拟父目录。
const STICKY_PARENT: &str = "-2";
/// 拿不到更具体层级时的顶层名称。
const ROOT_FOLDER: &str = "我的笔记";
/// 文件夹层级分隔符（与浏览器书签一致）。
const FOLDER_SEP: &str = " › ";

/// 一次加载结果：笔记与一一对应的文件夹显示路径。
#[derive(Debug)]
struct NoteRow {
    entry: Entry,
    /// 「学习 › Rust」；空串 = 不显示层级。
    folder: String,
    /// 小写文件夹路径 + 拼音 keys，供文件夹名命中用。
    folder_key: String,
}

#[derive(Debug)]
pub struct NoteIndex {
    rows: Vec<NoteRow>,
    /// 笔记 id → 文件夹层级（UI 行副标题用）。
    folders: HashMap<String, String>,
}

impl NoteIndex {
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            folders: HashMap::new(),
        }
    }

    /// 扫描全部账号并合并。任何单库失败都只跳过该库，不影响其他账号。
    pub fn load() -> Self {
        Self::load_from(&database_files())
    }

    fn load_from(dbs: &[PathBuf]) -> Self {
        let multi_account = dbs.len() > 1;
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        for db in dbs {
            let account = account_label(db);
            load_one(db, &account, multi_account, &mut seen, &mut rows);
            if rows.len() >= MAX_NOTES {
                break;
            }
        }
        rows.truncate(MAX_NOTES);
        // 空输入「笔记」页按最近修改排序（笔记是活跃内容，不按名字）。
        rows.sort_by(|a, b| {
            b.entry
                .modified
                .cmp(&a.entry.modified)
                .then_with(|| a.entry.name.cmp(&b.entry.name))
        });
        let folders = rows
            .iter()
            .filter(|row| !row.folder.is_empty())
            .map(|row| (row.entry.id.clone(), row.folder.clone()))
            .collect();
        Self { rows, folders }
    }

    pub fn count(&self) -> usize {
        self.rows.len()
    }

    /// 笔记 id → 文件夹层级。列表副标题据此显示「学习 › Rust」。
    pub fn folder_map(&self) -> HashMap<String, String> {
        self.folders.clone()
    }

    /// 「笔记」页空输入：按修改时间列出前 `limit` 条。
    pub fn top(&self, limit: usize) -> Vec<Entry> {
        self.rows
            .iter()
            .take(limit)
            .map(|row| row.entry.clone())
            .collect()
    }

    /// 名称/拼音/模糊 + （可选）文件夹路径匹配。
    ///
    /// 返回 `(条目, 命中总数)`；条目已截断到 `limit`，总数用于「加载更多」。
    /// 名称命中先于文件夹命中；有历史时名称命中交给全局 rank（学习/最近），
    /// 文件夹命中按修改时间倒序追加。
    pub fn search(
        &self,
        q: &str,
        limit: usize,
        include_folder: bool,
        history: Option<&History>,
        rank_cfg: &RankConfig,
    ) -> (Vec<Entry>, usize) {
        let needle = q.trim().to_lowercase();
        if needle.is_empty() || limit == 0 || self.rows.is_empty() {
            return (Vec::new(), 0);
        }
        let mut title_hits: Vec<(u8, Entry)> = Vec::new();
        let mut folder_hits: Vec<Entry> = Vec::new();
        for row in &self.rows {
            let name_lc = row.entry.name.to_lowercase();
            let py = row.entry.pinyin.as_deref().unwrap_or("");
            let tier = title_tier(&name_lc, &row.entry.name, py, &needle);
            match tier {
                Some(tier) => title_hits.push((tier, row.entry.clone())),
                None if include_folder && row.folder_key.contains(&needle) => {
                    folder_hits.push(row.entry.clone());
                }
                None => {}
            }
        }
        let total = title_hits.len() + folder_hits.len();
        let mut ordered: Vec<Entry> = match history {
            Some(history) => {
                let mut hits: Vec<Entry> = title_hits.into_iter().map(|(_, e)| e).collect();
                rank::rank_with(
                    &mut hits,
                    &needle,
                    history,
                    0,
                    rank_cfg,
                    &EffectiveWeights::neutral(),
                );
                hits
            }
            None => {
                title_hits.sort_by(|(ta, a), (tb, b)| {
                    ta.cmp(tb)
                        .then_with(|| b.modified.cmp(&a.modified))
                        .then_with(|| a.name.cmp(&b.name))
                });
                title_hits.into_iter().map(|(_, e)| e).collect()
            }
        };
        folder_hits.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.name.cmp(&b.name))
        });
        ordered.extend(folder_hits);
        ordered.truncate(limit);
        (ordered, total)
    }
}

/// 名称命中档位：0 前缀 / 1 包含 / 2 拼音前缀 / 3 拼音包含 / 4 模糊。
fn title_tier(name_lc: &str, name: &str, py: &str, needle: &str) -> Option<u8> {
    if name_lc.starts_with(needle) {
        return Some(0);
    }
    if name_lc.contains(needle) {
        return Some(1);
    }
    if py.split(' ').any(|token| token.starts_with(needle)) {
        return Some(2);
    }
    if py.split(' ').any(|token| token.contains(needle)) {
        return Some(3);
    }
    fuzzy::score_gated(name, needle).map(|_| 4)
}

/// 全部候选主库（含账号目录发现）。只读，不创建/修改任何文件。
fn database_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in data_roots() {
        let Ok(accounts) = fs::read_dir(&root) else {
            continue;
        };
        for account in accounts.flatten() {
            let dir = account.path().join("ynote-data");
            if !dir.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !name.ends_with(".db") {
                    continue;
                }
                // 只有主库带 note/note_book；content/search/file-link 是别的用途。
                if name.ends_with("-content.db")
                    || name.ends_with("-search.db")
                    || name.ends_with("-file-link.db")
                {
                    continue;
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn data_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join(DATA_DIR));
    }
    roots
}

fn account_label(db: &Path) -> String {
    db.parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 主库 + WAL 的时间戳/大小：文件没变就不重复解析。
pub fn source_stamp() -> Vec<(String, u64, u64)> {
    let mut stamp = Vec::new();
    for db in database_files() {
        for path in [db.clone(), with_suffix(&db, "-wal")] {
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let nanos = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            stamp.push((path.display().to_string(), nanos, meta.len()));
        }
    }
    stamp.sort();
    stamp
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn load_one(
    db: &Path,
    account: &str,
    multi_account: bool,
    seen: &mut HashSet<String>,
    out: &mut Vec<NoteRow>,
) {
    // 正常路径：客户端在跑也能并发读（WAL 多读一写）。
    if let Some(conn) = open_direct(db) {
        if let Some(rows) = read_db(&conn, account, multi_account, seen) {
            if !rows.is_empty() {
                lstr_index::snapshot::debug_log(&format!(
                    "notes: loaded {} entries from {}",
                    rows.len(),
                    db.display()
                ));
            }
            out.extend(rows);
            return;
        }
    }
    // 兜底：复制 db + -wal + -shm 到临时目录再读（同步/检查点占用时）。
    if let Some((conn, tmp)) = open_copy(db) {
        if let Some(rows) = read_db(&conn, account, multi_account, seen) {
            lstr_index::snapshot::debug_log(&format!(
                "notes: loaded {} entries from {} (temp copy)",
                rows.len(),
                db.display()
            ));
            out.extend(rows);
        }
        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
    } else {
        // B16：整条笔记源静默为空时，至少留一条可诊断日志。
        lstr_index::snapshot::debug_log(&format!(
            "notes: skipped {} (open direct/temp copy failed; client may be writing or DB locked)",
            db.display()
        ));
    }
}

fn open_direct(db: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(Duration::from_millis(300)).ok()?;
    Some(conn)
}

/// 复制到临时目录后只读打开。返回的目录由调用方在读完后删除。
fn open_copy(db: &Path) -> Option<(Connection, PathBuf)> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "lstr-notes-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).ok()?;
    let copy = dir.join("note.db");
    if fs::copy(db, &copy).is_err() {
        let _ = fs::remove_dir_all(&dir);
        return None;
    }
    for suffix in ["-wal", "-shm"] {
        let from = with_suffix(db, suffix);
        if from.is_file() {
            let _ = fs::copy(&from, with_suffix(&copy, suffix));
        }
    }
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
    match conn {
        Some(conn) => {
            let _ = conn.busy_timeout(Duration::from_millis(300));
            Some((conn, dir))
        }
        None => {
            let _ = fs::remove_dir_all(&dir);
            None
        }
    }
}

fn read_db(
    conn: &Connection,
    account: &str,
    multi_account: bool,
    seen: &mut HashSet<String>,
) -> Option<Vec<NoteRow>> {
    let note_cols = table_columns(conn, "note")?;
    if !note_cols.contains("fileId") {
        return None;
    }
    let books = read_books(conn);
    let folder_paths = build_folder_paths(&books);
    let sql = notes_sql(&note_cols);
    let mut stmt = conn.prepare(&sql).ok()?;
    let mut raw: Vec<RawNote> = Vec::new();
    {
        let rows = stmt
            .query_map([], |row| {
                Ok(RawNote {
                    file_id: row.get(0)?,
                    title: row.get(1)?,
                    parent: row.get(2)?,
                    modify: row.get(3)?,
                    size: row.get(4)?,
                })
            })
            .ok()?;
        for note in rows.flatten() {
            raw.push(note);
        }
    }
    let mut out = Vec::new();
    for note in raw {
        if note.file_id.is_empty() {
            continue;
        }
        let key = format!("{account}:{}", note.file_id);
        if !seen.insert(key) {
            continue;
        }
        let name = display_title(&note.title);
        let name = if name.is_empty() {
            "无标题笔记".to_string()
        } else {
            name
        };
        let parent_is_folder = books.contains_key(&note.parent);
        let folder = folder_for(&note.parent, &folder_paths);
        let folder = if multi_account && !account.is_empty() {
            format!("{account}{FOLDER_SEP}{folder}")
        } else {
            folder
        };
        let folder_key = format!("{} {}", folder.to_lowercase(), pinyin::keys(&folder));
        out.push(NoteRow {
            entry: Entry {
                id: format!("yn:{account}:{}", note.file_id),
                kind: Kind::Note,
                name: name.clone(),
                path: Some(PathBuf::from(web_url(
                    &note.parent,
                    &note.file_id,
                    parent_is_folder,
                ))),
                size: (note.size > 0).then_some(note.size as u64),
                modified: (note.modify > 0).then_some(note.modify),
                pinyin: Some(pinyin::keys(&name)),
                initial: apps::initial_for(&name),
                tone: apps::tone_for(&name),
            },
            folder,
            folder_key,
        });
        if out.len() >= MAX_NOTES {
            break;
        }
    }
    Some(out)
}

struct RawNote {
    file_id: String,
    title: String,
    parent: String,
    modify: i64,
    size: i64,
}

fn table_columns(conn: &Connection, table: &str) -> Option<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).ok()?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?
        .filter_map(Result::ok)
        .collect();
    Some(names)
}

fn notes_sql(cols: &HashSet<String>) -> String {
    let title = if cols.contains("title") {
        "COALESCE(title,'')"
    } else {
        "''"
    };
    let parent = if cols.contains("parentId") {
        "COALESCE(parentId,'')"
    } else {
        "''"
    };
    let modify = if cols.contains("modifyTime") {
        "COALESCE(modifyTime,0)"
    } else {
        "0"
    };
    let size = if cols.contains("size") {
        "COALESCE(size,0)"
    } else {
        "0"
    };
    let mut where_ = Vec::new();
    if cols.contains("deleted") {
        where_.push("deleted IS NULL");
    }
    if cols.contains("del") {
        where_.push("(del IS NULL OR del = 0)");
    }
    if cols.contains("remoteRemoved") {
        where_.push("(remoteRemoved IS NULL OR remoteRemoved = 0)");
    }
    if cols.contains("parentDeleted") {
        where_.push("(parentDeleted IS NULL OR parentDeleted = 0)");
    }
    if cols.contains("dir") {
        // dir=1 是文件夹行（正常主库里 folders 只出现在 note_book）。
        where_.push("(dir IS NULL OR dir = 0)");
    }
    if where_.is_empty() {
        where_.push("1=1");
    }
    let mut sql = format!(
        "SELECT fileId, {title}, {parent}, {modify}, {size} FROM note WHERE {}",
        where_.join(" AND ")
    );
    if cols.contains("modifyTime") {
        sql.push_str(" ORDER BY modifyTime DESC");
    }
    sql
}

fn read_books(conn: &Connection) -> HashMap<String, (String, String)> {
    let Some(cols) = table_columns(conn, "note_book") else {
        return HashMap::new();
    };
    if !cols.contains("fileId") {
        return HashMap::new();
    }
    let title = if cols.contains("title") {
        "COALESCE(title,'')"
    } else {
        "''"
    };
    let parent = if cols.contains("parentId") {
        "COALESCE(parentId,'')"
    } else {
        "''"
    };
    let mut sql = format!("SELECT fileId, {title}, {parent} FROM note_book");
    let mut where_ = Vec::new();
    if cols.contains("deleted") {
        where_.push("deleted IS NULL");
    }
    if cols.contains("del") {
        where_.push("(del IS NULL OR del = 0)");
    }
    if !where_.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_.join(" AND "));
    }
    let mut out = HashMap::new();
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) else {
        return out;
    };
    for (id, title, parent) in rows.flatten() {
        if !id.is_empty() {
            out.insert(id, (title.trim().to_string(), parent));
        }
    }
    out
}

/// 笔记本 fileId → 完整层级（「学习 › Rust」）。
/// 父节点不在表内 / 是虚拟根 `1` / 自引用时按顶层处理；带环保护。
fn build_folder_paths(books: &HashMap<String, (String, String)>) -> HashMap<String, String> {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    for id in books.keys() {
        resolve_folder(id, books, &mut memo, &mut visiting, 0);
    }
    memo
}

fn resolve_folder(
    id: &str,
    books: &HashMap<String, (String, String)>,
    memo: &mut HashMap<String, String>,
    visiting: &mut HashSet<String>,
    depth: usize,
) -> String {
    if let Some(path) = memo.get(id) {
        return path.clone();
    }
    if depth > 32 || !visiting.insert(id.to_string()) {
        return String::new();
    }
    let Some((title, parent)) = books.get(id) else {
        visiting.remove(id);
        return String::new();
    };
    let parent_path =
        if parent.is_empty() || parent == id || parent == "1" || !books.contains_key(parent) {
            String::new()
        } else {
            resolve_folder(parent, books, memo, visiting, depth + 1)
        };
    visiting.remove(id);
    let path = if parent_path.is_empty() {
        title.clone()
    } else {
        format!("{parent_path}{FOLDER_SEP}{title}")
    };
    memo.insert(id.to_string(), path.clone());
    path
}

fn folder_for(parent: &str, paths: &HashMap<String, String>) -> String {
    if parent == STICKY_PARENT {
        return "桌面便签".to_string();
    }
    if let Some(path) = paths.get(parent) {
        if !path.is_empty() {
            return path.clone();
        }
    }
    // 直接挂在虚拟根（1 / root id）下的笔记；见模块注释。
    ROOT_FOLDER.to_string()
}

/// 显示标题：剥掉客户端内部后缀 `.note` / `.clip`；`.md` 保留。
fn display_title(raw: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_lowercase();
    for suffix in [".note", ".clip"] {
        if lower.ends_with(suffix) {
            let cut = trimmed.len() - suffix.len();
            return trimmed[..cut].trim_end().to_string();
        }
    }
    trimmed.to_string()
}

/// 网页版深链。父目录不是真实笔记本（桌面便签/虚拟根）时走 recent 兜底。
fn web_url(parent: &str, file_id: &str, parent_is_folder: bool) -> String {
    if parent_is_folder && !parent.is_empty() {
        format!("{WEB_BASE}/{parent}/note/{file_id}/")
    } else {
        format!("{WEB_BASE}/recent/note/{file_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_row(name: &str, folder: &str, parent: &str, modified: i64) -> NoteRow {
        let folder_key = format!("{} {}", folder.to_lowercase(), pinyin::keys(folder));
        NoteRow {
            entry: Entry {
                id: format!("yn:test:{name}"),
                kind: Kind::Note,
                name: name.to_string(),
                path: Some(PathBuf::from(web_url(parent, name, !parent.is_empty()))),
                size: None,
                modified: Some(modified),
                pinyin: Some(pinyin::keys(name)),
                initial: apps::initial_for(name),
                tone: apps::tone_for(name),
            },
            folder: folder.to_string(),
            folder_key,
        }
    }

    fn index(rows: Vec<NoteRow>) -> NoteIndex {
        let folders = rows
            .iter()
            .filter(|row| !row.folder.is_empty())
            .map(|row| (row.entry.id.clone(), row.folder.clone()))
            .collect();
        NoteIndex { rows, folders }
    }

    #[test]
    fn display_title_strips_internal_suffixes_only() {
        assert_eq!(display_title("语法.note"), "语法");
        assert_eq!(display_title("网页.clip"), "网页");
        assert_eq!(display_title("运维手册.md"), "运维手册.md");
        assert_eq!(display_title("  .note  "), "");
        assert_eq!(display_title("普通标题"), "普通标题");
    }

    #[test]
    fn web_url_uses_folder_when_known() {
        assert_eq!(
            web_url("FOLDER", "WEB1", true),
            "https://note.youdao.com/web/#/file/FOLDER/note/WEB1/"
        );
        assert_eq!(
            web_url("-2", "WEB2", false),
            "https://note.youdao.com/web/#/file/recent/note/WEB2"
        );
    }

    #[test]
    fn folder_tree_survives_virtual_roots_and_cycles() {
        let mut books = HashMap::new();
        books.insert("root".to_string(), ("学习".to_string(), "1".to_string()));
        books.insert("rust".to_string(), ("Rust".to_string(), "root".to_string()));
        books.insert("deep".to_string(), ("进阶".to_string(), "rust".to_string()));
        books.insert(
            "cycle-a".to_string(),
            ("A".to_string(), "cycle-b".to_string()),
        );
        books.insert(
            "cycle-b".to_string(),
            ("B".to_string(), "cycle-a".to_string()),
        );
        let paths = build_folder_paths(&books);
        assert_eq!(paths.get("root").map(String::as_str), Some("学习"));
        assert_eq!(paths.get("rust").map(String::as_str), Some("学习 › Rust"));
        assert_eq!(
            paths.get("deep").map(String::as_str),
            Some("学习 › Rust › 进阶")
        );
        // 环不会无限递归；两者至少能拿到自己的名字。
        assert!(paths.contains_key("cycle-a"));
        assert!(paths.contains_key("cycle-b"));
        assert_eq!(folder_for("-2", &paths), "桌面便签");
        assert_eq!(folder_for("1", &paths), "我的笔记");
    }

    #[test]
    fn search_prefers_title_over_folder_and_respects_limit() {
        let idx = index(vec![
            note_row("无关标题", "学习 › Rust", "rust", 100),
            note_row("Rust 语法", "学习", "root", 50),
            note_row("所有权", "学习 › Rust", "rust", 200),
        ]);
        let cfg = RankConfig::default();
        let (hits, total) = idx.search("rust", 10, true, None, &cfg);
        assert_eq!(total, 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].name, "Rust 语法", "标题命中必须排在文件夹命中前");
        // 文件夹命中按修改时间倒序：所有权(200) 在 无关标题(100) 前面。
        assert_eq!(hits[1].name, "所有权");
        assert_eq!(hits[2].name, "无关标题");

        let (folder_off, total) = idx.search("rust", 10, false, None, &cfg);
        assert_eq!(total, 1);
        assert_eq!(folder_off[0].name, "Rust 语法");

        let (limited, total) = idx.search("rust", 1, true, None, &cfg);
        assert_eq!(limited.len(), 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn search_supports_pinyin_and_folder_pinyin() {
        let idx = index(vec![
            note_row("学习笔记", "工作", "root", 10),
            note_row("前端", "学习 › Rust", "rust", 20),
        ]);
        let cfg = RankConfig::default();
        let (hits, _) = idx.search("xuexi", 10, true, None, &cfg);
        assert!(hits.iter().any(|e| e.name == "学习笔记"));
        assert!(hits.iter().any(|e| e.name == "前端"));
    }

    #[test]
    fn synthetic_database_round_trip_filters_deleted_and_builds_urls() {
        let dir = std::env::temp_dir().join(format!(
            "lstr-notes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("acc").join("ynote-data").join("acc.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE note (
                    fileId TEXT PRIMARY KEY NOT NULL,
                    title TEXT,
                    parentId TEXT,
                    del INTEGER NOT NULL DEFAULT 0,
                    deleted INTEGER,
                    remoteRemoved INTEGER DEFAULT 0,
                    parentDeleted INTEGER DEFAULT 0,
                    dir INTEGER,
                    modifyTime INTEGER NOT NULL DEFAULT 0,
                    size INTEGER DEFAULT 0
                );
                CREATE TABLE note_book (
                    fileId TEXT PRIMARY KEY NOT NULL,
                    title TEXT,
                    parentId TEXT,
                    del INTEGER NOT NULL DEFAULT 0,
                    deleted INTEGER
                );
                INSERT INTO note_book VALUES ('study','学习','1',0,NULL);
                INSERT INTO note_book VALUES ('rust','Rust','study',0,NULL);
                INSERT INTO note VALUES ('N1','语法.note','rust',0,NULL,0,0,0,100,12);
                INSERT INTO note VALUES ('N2','前端.clip','study',0,NULL,0,0,0,50,0);
                INSERT INTO note VALUES ('N3','已删除.note','rust',1,NULL,0,0,0,10,0);
                INSERT INTO note VALUES ('N4','远端删除.note','rust',0,NULL,1,0,0,10,0);
                INSERT INTO note VALUES ('N5','便签.note','-2',0,NULL,0,0,0,5,0);",
            )
            .unwrap();
        }
        let idx = NoteIndex::load_from(&[db]);
        assert_eq!(idx.count(), 3, "回收站/远端删除必须排掉");
        let names: Vec<&str> = idx.rows.iter().map(|r| r.entry.name.as_str()).collect();
        assert!(names.contains(&"语法"));
        assert!(names.contains(&"前端"));
        assert!(names.contains(&"便签"));
        let rust = idx.rows.iter().find(|r| r.entry.name == "语法").unwrap();
        assert_eq!(rust.folder, "学习 › Rust");
        assert!(rust.entry.full_path().contains("/file/rust/note/N1/"));
        let sticky = idx.rows.iter().find(|r| r.entry.name == "便签").unwrap();
        assert_eq!(sticky.folder, "桌面便签");
        assert!(sticky.entry.full_path().contains("/file/recent/note/N5"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// 真实库只读验收（本机有道云笔记 8.x）。没有安装时跳过。
    #[test]
    fn live_youdao_db_is_readable_when_present() {
        let dbs = database_files();
        if dbs.is_empty() {
            return;
        }
        let idx = NoteIndex::load();
        assert!(
            idx.count() > 0,
            "发现 {} 个主库但解析出 0 条笔记：schema 可能已变",
            dbs.len()
        );
        assert!(
            idx.rows.iter().any(|row| !row.folder.is_empty()),
            "真实库应至少有一条带文件夹路径的笔记"
        );
        assert!(
            idx.rows
                .iter()
                .all(|row| row.entry.full_path().starts_with(WEB_BASE)),
            "所有条目都必须是可打开的网页深链"
        );
    }
}
