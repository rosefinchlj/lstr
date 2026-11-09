use crate::model::Entry;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 7 天衰减常数：每过 7 天，最近使用奖励衰减到约 37%（e^-1）。
const DECAY_SECS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// query_hits 的时间衰减：3 天减半，让「上次选了什么」保持新鲜。
const QUERY_DECAY_SECS: f64 = 3.0 * 24.0 * 60.0 * 60.0;
/// 单个查询词命中能带来的最高加权，避免盖过「名称完全一致」档位。
const QUERY_BOOST_CAP: f64 = 240.0;
/// history 条目数上限；超过后按 frecency 砍掉一半。
const MAX_ITEMS: i64 = 1000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS history (
    id    TEXT PRIMARY KEY,
    count INTEGER NOT NULL,
    last  INTEGER NOT NULL,
    entry TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_history_last ON history(last DESC);

-- P1-1：记录「某个查询词下用户最终选了哪个结果」。
-- 与 history 的全局 frecency 分开：frecency 回答「这个条目常用吗」，
-- query_hits 回答「搜这个词时，用户上次要的是哪一个」。
CREATE TABLE IF NOT EXISTS query_hits (
    query    TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    hits     INTEGER NOT NULL,
    last     INTEGER NOT NULL,
    PRIMARY KEY (query, entry_id)
);
CREATE INDEX IF NOT EXISTS idx_query_hits_lookup ON query_hits(query, hits DESC);
";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistItem {
    pub count: u32,
    pub last: u64,
    pub entry: Entry,
}

/// 老版本（JSON 存储）的文件格式，仅用于一次性迁移。
#[derive(Deserialize)]
struct LegacyHistory {
    items: HashMap<String, HistItem>,
}

/// 使用历史，存 SQLite（`%APPDATA%\lstr\history.db`，WAL 模式）。
/// 相比整表 JSON 读写：启动只建连接，查询按 id/时间走索引，
/// 写入是 UPSERT 单条，不再每次打开都全量序列化。
pub struct History {
    db: Connection,
}

impl History {
    pub fn path() -> PathBuf {
        crate::config::attach_dir().join("history.db")
    }

    fn legacy_json_path() -> PathBuf {
        crate::config::attach_dir().join("history.json")
    }

    pub fn load() -> Self {
        if let Err(err) = fs::create_dir_all(crate::config::attach_dir()) {
            eprintln!("lstr: 创建数据目录失败（使用内存历史）: {err}");
            return Self::open_memory();
        }
        let path = Self::path();
        match Self::open(&path) {
            Ok(history) => history,
            Err(err) => {
                eprintln!("lstr: 打开 history.db 失败（使用内存历史）: {err}");
                Self::open_memory()
            }
        }
    }

    fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let db = Connection::open(path)?;
        // WAL：写入不阻塞读，崩溃也安全。
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(SCHEMA)?;
        let history = Self { db };
        history.migrate_json();
        Ok(history)
    }

    fn open_memory() -> Self {
        let db = Connection::open_in_memory().expect("open in-memory sqlite");
        db.execute_batch(SCHEMA).expect("create schema");
        Self { db }
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::open_memory()
    }

    /// 首次运行把旧 history.json 导入 SQLite（表为空才迁移），
    /// 成功后把 json 改名为 history.json.bak，避免重复导入。
    fn migrate_json(&self) {
        let json_path = Self::legacy_json_path();
        let Ok(text) = fs::read_to_string(&json_path) else {
            return;
        };
        let existing: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
            .unwrap_or(1);
        if existing > 0 {
            return;
        }
        let Ok(legacy) = serde_json::from_str::<LegacyHistory>(&text) else {
            return;
        };
        let mut migrated = 0usize;
        if let Ok(mut stmt) = self.db.prepare(
            "INSERT OR REPLACE INTO history (id, count, last, entry) VALUES (?1, ?2, ?3, ?4)",
        ) {
            for (id, item) in legacy.items {
                if let Ok(entry_json) = serde_json::to_string(&item.entry) {
                    if stmt
                        .execute(params![id, item.count, item.last as i64, entry_json])
                        .is_ok()
                    {
                        migrated += 1;
                    }
                }
            }
        }
        if migrated > 0 {
            let _ = fs::rename(&json_path, json_path.with_extension("json.bak"));
        }
    }

    /// 记录一次打开：UPSERT（次数 +1、时间更新、快照覆盖），随后按需裁剪。
    pub fn record(&mut self, entry: &Entry) {
        let Ok(entry_json) = serde_json::to_string(entry) else {
            return;
        };
        let now = now_secs();
        let _ = self.db.execute(
            "INSERT INTO history (id, count, last, entry) VALUES (?1, 1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET count = count + 1, last = ?2, entry = ?3",
            params![entry.id, now as i64, entry_json],
        );
        self.evict(now);
    }

    /// P1-1：记录「搜索 query 时用户选了这个条目」。
    /// 空查询不记录——零查询推荐本来就是历史列表，记了没有区分度。
    pub fn record_query(&mut self, query: &str, entry: &Entry) {
        let key = query.trim().to_lowercase();
        if key.is_empty() {
            return;
        }
        let now = now_secs();
        let _ = self.db.execute(
            "INSERT INTO query_hits (query, entry_id, hits, last) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(query, entry_id) DO UPDATE SET hits = hits + 1, last = ?3",
            params![key, entry.id, now as i64],
        );
    }

    /// P1-1：同一查询词下的历史选择加成。次数越多、越近，分越高。
    /// 上限 240，保证不会盖过「名称完全一致 = 1000」这类匹配档位。
    pub fn query_boost(&self, query: &str, entry_id: &str) -> i32 {
        let key = query.trim().to_lowercase();
        if key.is_empty() {
            return 0;
        }
        let result = self.db.query_row(
            "SELECT hits, last FROM query_hits WHERE query = ?1 AND entry_id = ?2",
            params![key, entry_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        );
        let Ok((hits, last)) = result else {
            return 0;
        };
        let age = now_secs().saturating_sub(last.max(0) as u64) as f64;
        let recency = (-(age / QUERY_DECAY_SECS)).exp();
        (hits as f64 * 60.0 * recency)
            .round()
            .clamp(0.0, QUERY_BOOST_CAP) as i32
    }

    /// frecency 分数：`count + 2 * e^(-age_days / 7)`；未知条目返回 0。
    pub fn score(&self, id: &str) -> f32 {
        let result = self.db.query_row(
            "SELECT count, last FROM history WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?)),
        );
        let Ok((count, last)) = result else {
            return 0.0;
        };
        let last = last.max(0) as u64;
        let age = now_secs().saturating_sub(last) as f64;
        count as f32 + 2.0 * (-(age / DECAY_SECS)).exp() as f32
    }

    /// 按最后使用时间倒序返回历史条目快照。
    pub fn recent_entries(&self, limit: usize) -> Vec<Entry> {
        let Ok(mut stmt) = self.db.prepare(
            "SELECT entry FROM history ORDER BY last DESC, count DESC, id ASC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![limit as i64], |row| row.get::<_, String>(0));
        match rows {
            Ok(rows) => rows
                .filter_map(Result::ok)
                .filter_map(|json| serde_json::from_str(&json).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 清空全部历史（设置里的「清空历史」用），连带清空查询词学习数据。
    pub fn clear(&mut self) {
        let _ = self.db.execute("DELETE FROM history", []);
        let _ = self.db.execute("DELETE FROM query_hits", []);
    }

    /// 清空查询词学习数据，保留打开历史。
    pub fn clear_queries(&mut self) {
        let _ = self.db.execute("DELETE FROM query_hits", []);
    }

    /// 从历史里移除单条（结果动作菜单「从最近使用中移除」用）。
    pub fn remove(&mut self, id: &str) {
        let _ = self
            .db
            .execute("DELETE FROM history WHERE id = ?1", params![id]);
        let _ = self
            .db
            .execute("DELETE FROM query_hits WHERE entry_id = ?1", params![id]);
    }

    fn evict(&mut self, now: u64) {
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
            .unwrap_or(0);
        if count <= MAX_ITEMS {
            return;
        }
        // 最多 1001 条，拉到 Rust 里按 frecency 排序，删掉分数最低的一半。
        let Ok(mut stmt) = self.db.prepare("SELECT id, count, last FROM history") else {
            return;
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, i64>(2)?,
            ))
        });
        let Ok(rows) = rows else {
            return;
        };
        let mut scored: Vec<(String, f32)> = rows
            .filter_map(Result::ok)
            .map(|(id, count, last)| {
                let age = now.saturating_sub(last.max(0) as u64) as f64;
                let score = count as f32 + 2.0 * (-(age / DECAY_SECS)).exp() as f32;
                (id, score)
            })
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let delete: Vec<String> = scored
            .into_iter()
            .take((count - MAX_ITEMS / 2) as usize)
            .map(|(id, _)| id)
            .collect();
        for id in delete {
            let _ = self.db.execute("DELETE FROM history WHERE id = ?1", params![id]);
            let _ = self
                .db
                .execute("DELETE FROM query_hits WHERE entry_id = ?1", params![id]);
        }
    }

    /// 测试辅助：直接种一条带指定次数/时间的历史。
    #[cfg(test)]
    pub fn seed(&mut self, entry: &Entry, count: u32, last: u64) {
        let entry_json = serde_json::to_string(entry).expect("serialize entry");
        self.db
            .execute(
                "INSERT OR REPLACE INTO history (id, count, last, entry) VALUES (?1, ?2, ?3, ?4)",
                params![entry.id, count, last as i64, entry_json],
            )
            .expect("seed history");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn entry(id: &str) -> Entry {
        Entry {
            id: id.to_string(),
            kind: Kind::File,
            name: format!("{id}.txt"),
            path: Some(PathBuf::from(format!("C:/tmp/{id}.txt"))),
            size: Some(1),
            modified: Some(0),
            pinyin: None,
            initial: String::new(),
            tone: 0,
        }
    }

    #[test]
    fn record_increments_count_and_updates_snapshot() {
        let mut history = History::in_memory();
        let mut current = entry("alpha");
        history.record(&current);
        current.name = "renamed.txt".to_string();
        history.record(&current);

        let recent = history.recent_entries(1);
        assert_eq!(recent[0].id, "alpha");
        assert_eq!(recent[0].name, "renamed.txt");
        assert_eq!(
            history
                .db
                .query_row("SELECT count FROM history WHERE id = 'alpha'", [], |row| {
                    row.get::<_, u32>(0)
                })
                .unwrap(),
            2
        );
    }

    #[test]
    fn score_unknown_id_is_zero() {
        let history = History::in_memory();
        assert_eq!(history.score("missing"), 0.0);
    }

    #[test]
    fn score_weights_count_and_recency() {
        let mut history = History::in_memory();
        let now = now_secs();
        history.seed(&entry("recent"), 2, now);
        history.seed(&entry("old"), 2, now.saturating_sub(30 * 24 * 60 * 60));
        history.seed(&entry("often"), 10, now);
        assert!(history.score("recent") > history.score("old"));
        assert!(history.score("often") > history.score("recent"));
    }

    #[test]
    fn query_boost_only_applies_to_matching_query() {
        let mut history = History::in_memory();
        let target = entry("report");
        history.record_query("rep", &target);
        history.record_query("rep", &target);
        assert!(history.query_boost("rep", "report") > 0);
        assert_eq!(history.query_boost("rep", "other"), 0);
        assert_eq!(history.query_boost("zzz", "report"), 0);
        assert_eq!(history.query_boost("", "report"), 0);
    }

    #[test]
    fn query_boost_is_capped_and_clearable() {
        let mut history = History::in_memory();
        let target = entry("report");
        for _ in 0..50 {
            history.record_query("rep", &target);
        }
        assert!(history.query_boost("rep", "report") <= QUERY_BOOST_CAP as i32);
        history.clear_queries();
        assert_eq!(history.query_boost("rep", "report"), 0);
    }

    #[test]
    fn recent_entries_sorted_desc_and_limited() {
        let mut history = History::in_memory();
        let now = now_secs();
        history.seed(&entry("old"), 1, now - 100);
        history.seed(&entry("newest"), 1, now);
        history.seed(&entry("middle"), 1, now - 10);

        let recent: Vec<String> = history
            .recent_entries(2)
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(recent, vec!["newest".to_string(), "middle".to_string()]);
    }

    #[test]
    fn evict_keeps_top_scored_half() {
        let mut history = History::in_memory();
        let now = now_secs();
        for index in 0..1200u32 {
            let id = format!("item-{index}");
            // 后半段次数多，裁剪后应保留。
            let count = if index < 600 { 1 } else { 50 };
            history.seed(&entry(&id), count, now);
        }
        history.evict(now);
        let remaining: i64 = history
            .db
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, MAX_ITEMS / 2);
        assert_eq!(history.score("item-0"), 0.0);
        assert!(history.score("item-1199") > 0.0);
    }

    #[test]
    fn clear_empties_table() {
        let mut history = History::in_memory();
        history.seed(&entry("a"), 1, now_secs());
        history.clear();
        assert!(history.recent_entries(10).is_empty());
    }
}
