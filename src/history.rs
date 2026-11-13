use crate::model::{Entry, Kind};
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

-- 输入框历史：记录实际发出过查询的输入串，供 Ctrl+↑/↓ 调出。
-- 连续打字/退格只保留最终形态（record_input 合并前缀关系），
-- 避免 w / wx / wxi 把历史刷满。
CREATE TABLE IF NOT EXISTS input_history (
    text TEXT PRIMARY KEY,
    last INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_input_history_last ON input_history(last DESC);
";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 单次查询用的历史只读快照：一次 SQL 取出全部 frecency 行与该查询词的选择行，
/// rank 逐候选查 HashMap，不再每条候选一次 SQL（300+ 候选 × 2 次查询是 finalize
/// 的主要成本）。公式与 [`History::score`] / [`History::query_boost`] 完全一致。
pub(crate) struct RankSnapshot {
    scores: HashMap<String, (u32, i64)>,
    boosts: HashMap<String, (i64, i64)>,
    now: u64,
}

fn score_value(count: u32, last: i64, now: u64) -> f32 {
    let last = last.max(0) as u64;
    let age = now.saturating_sub(last) as f64;
    count as f32 + 2.0 * (-(age / DECAY_SECS)).exp() as f32
}

fn boost_value(hits: i64, last: i64, now: u64) -> i32 {
    let age = now.saturating_sub(last.max(0) as u64) as f64;
    let recency = (-(age / QUERY_DECAY_SECS)).exp();
    (hits as f64 * 60.0 * recency)
        .round()
        .clamp(0.0, QUERY_BOOST_CAP) as i32
}

impl RankSnapshot {
    /// 快照建立时刻（unix 秒），供 rank 的 recency 复用，避免逐候选取系统时间。
    pub(crate) fn now(&self) -> i64 {
        self.now as i64
    }

    /// 与 `History::score` 同公式；未知 id 返回 0。
    pub(crate) fn score(&self, id: &str) -> f32 {
        self.scores
            .get(id)
            .map_or(0.0, |&(count, last)| score_value(count, last, self.now))
    }

    /// 与 `History::query_boost` 同公式；无记录/空查询返回 0。
    pub(crate) fn boost(&self, entry_id: &str) -> i32 {
        self.boosts
            .get(entry_id)
            .map_or(0, |&(hits, last)| boost_value(hits, last, self.now))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistItem {
    pub count: u32,
    pub last: u64,
    pub entry: Entry,
}

/// 结果行学习徽标(自学习可视化)。
#[derive(Clone, Copy, Default, Debug)]
pub struct LearnBadge {
    /// 当前查询词下用户选过它(query_hits)。
    pub recent_pick: bool,
    /// 全局常用(frecency count≥3)。
    pub frequent: bool,
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

    /// 基准用:打开真实的文件型 SQLite(路径由测试指定,不碰 APPDATA)。
    #[cfg(test)]
    pub(crate) fn open_test_db(path: &Path) -> Self {
        Self::open(path).expect("open test history db")
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
    ///
    /// 生产 rank 走 [`Self::rank_snapshot`] 的批量快照；逐条版本保留为快照
    /// 语义的对照基准（单测直接断言两者同值）。
    #[cfg(test)]
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
        match result {
            Ok((hits, last)) => boost_value(hits, last, now_secs()),
            Err(_) => 0,
        }
    }

    /// frecency 分数：`count + 2 * e^(-age_days / 7)`；未知条目返回 0。
    /// 同 [`Self::query_boost`]：逐条基准，生产 rank 用批量快照。
    #[cfg(test)]
    pub fn score(&self, id: &str) -> f32 {
        let result = self.db.query_row(
            "SELECT count, last FROM history WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?)),
        );
        match result {
            Ok((count, last)) => score_value(count, last, now_secs()),
            Err(_) => 0.0,
        }
    }

    /// rank 批量快照：一次取全部 frecency 行（上限 1000）与该查询词的选择行。
    /// 等价于逐候选调用 [`Self::score`] / [`Self::query_boost`]，只是把 N 次
    /// SQL 折成 2 次；`now` 在快照建立时取一次（衰减是秒级，排序结果不变）。
    pub(crate) fn rank_snapshot(&self, query: &str) -> RankSnapshot {
        let mut scores = HashMap::new();
        if let Ok(mut stmt) = self.db.prepare("SELECT id, count, last FROM history") {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    scores.insert(row.0, (row.1, row.2));
                }
            }
        }
        let key = query.trim().to_lowercase();
        let mut boosts = HashMap::new();
        if !key.is_empty() {
            if let Ok(mut stmt) = self
                .db
                .prepare("SELECT entry_id, hits, last FROM query_hits WHERE query = ?1")
            {
                if let Ok(rows) = stmt.query_map(params![key], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                }) {
                    for row in rows.flatten() {
                        boosts.insert(row.0, (row.1, row.2));
                    }
                }
            }
        }
        RankSnapshot {
            scores,
            boosts,
            now: now_secs(),
        }
    }

    /// 学习徽标:返回每个条目的「上次选中」(当前查询词下选过)与
    /// 「常用」(全局 frecency count≥3)标记,供结果行可视化自学习。
    /// 与排序用的 rank_snapshot 同源(history + query_hits),只是阅后即弃。
    pub fn learn_badges(&self, query: &str) -> HashMap<String, LearnBadge> {
        let mut out: HashMap<String, LearnBadge> = HashMap::new();
        if let Ok(mut stmt) = self.db.prepare("SELECT id FROM history WHERE count >= 3") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for id in rows.flatten() {
                    out.entry(id).or_default().frequent = true;
                }
            }
        }
        let key = query.trim().to_lowercase();
        if !key.is_empty() {
            if let Ok(mut stmt) = self
                .db
                .prepare("SELECT entry_id FROM query_hits WHERE query = ?1")
            {
                if let Ok(rows) = stmt.query_map(params![key], |row| row.get::<_, String>(0)) {
                    for id in rows.flatten() {
                        out.entry(id).or_default().recent_pick = true;
                    }
                }
            }
        }
        out
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

    /// 删除单条按词学习记录(某查询词下某条目的选择)。
    pub fn remove_query_hit(&mut self, query: &str, entry_id: &str) {
        let _ = self.db.execute(
            "DELETE FROM query_hits WHERE query = ?1 AND entry_id = ?2",
            params![query, entry_id],
        );
    }

    /// 学习概览统计:( distinct 查询词数, 选择记录总数, 常用条目数 )。
    pub fn learning_stats(&self) -> (usize, usize, usize) {
        let q = self
            .db
            .query_row("SELECT COUNT(DISTINCT query) FROM query_hits", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as usize;
        let p = self
            .db
            .query_row("SELECT COUNT(*) FROM query_hits", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize;
        let i = self
            .db
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize;
        (q, p, i)
    }

    /// 按词学习明细行(query, entry_id, hits, last),按次数/最近降序。
    pub fn query_hit_rows(&self, limit: usize) -> Vec<(String, String, i64, i64)> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self.db.prepare(
            "SELECT query, entry_id, hits, last FROM query_hits ORDER BY hits DESC, last DESC LIMIT ?1",
        ) {
            if let Ok(rows) = stmt.query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// 常用排行明细行(id, name, kind, count, last),按次数/最近降序。
    pub fn frecency_rows(&self, limit: usize) -> Vec<(String, String, Kind, u32, i64)> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self.db.prepare(
            "SELECT id, entry, count, last FROM history ORDER BY count DESC, last DESC LIMIT ?1",
        ) {
            if let Ok(rows) = stmt.query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            }) {
                for (id, json, count, last) in rows.flatten() {
                    if let Ok(e) = serde_json::from_str::<Entry>(&json) {
                        out.push((id, e.name, e.kind, count, last));
                    }
                }
            }
        }
        out
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

    /// 条目是否在「最近使用」历史里（决定菜单要不要显示「从最近使用中移除」）。
    pub fn contains(&self, id: &str) -> bool {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM history WHERE id = ?1)",
                params![id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// 记录一条输入框查询串（每次真正发出搜索时调用）。
    /// 与最近一条是前缀关系时直接替换：连续打字/退格只留最终形态。
    pub fn record_input(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let now = now_secs() as i64;
        let recent: Option<String> = self
            .db
            .query_row(
                "SELECT text FROM input_history ORDER BY last DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(recent) = recent {
            if recent == text {
                let _ = self.db.execute(
                    "UPDATE input_history SET last = ?2 WHERE text = ?1",
                    params![text, now],
                );
                return;
            }
            if text.starts_with(&recent) || recent.starts_with(text) {
                let _ = self
                    .db
                    .execute("DELETE FROM input_history WHERE text = ?1", params![recent]);
            }
        }
        let _ = self.db.execute(
            "INSERT OR REPLACE INTO input_history (text, last) VALUES (?1, ?2)",
            params![text, now],
        );
        // 只留最近 100 条，避免无限增长。
        let _ = self.db.execute(
            "DELETE FROM input_history WHERE text NOT IN
                (SELECT text FROM input_history ORDER BY last DESC, rowid DESC LIMIT 100)",
            [],
        );
    }

    /// 最近输入串（最新在前）。
    pub fn recent_inputs(&self, limit: usize) -> Vec<String> {
        let Ok(mut stmt) = self
            .db
            .prepare("SELECT text FROM input_history ORDER BY last DESC, rowid DESC LIMIT ?1")
        else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![limit as i64], |row| row.get::<_, String>(0));
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        }
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
    fn record_increments_count_and_updates_snapshot() {        let mut history = History::in_memory();
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

    /// rank 的批量快照必须与逐条 `score` / `query_boost` 同值(包括未知 id、
    /// 不匹配的 query、过期行)。这是 finalize 去掉逐候选 SQL 后的正确性锁。
    #[test]
    fn rank_snapshot_matches_per_entry_queries() {
        let mut history = History::in_memory();
        let now = now_secs();
        for (id, count, age) in [
            ("alpha", 3u32, 0u64),
            ("beta", 7, 6 * 86400),
            ("gamma", 1, 40 * 86400),
            ("delta", 12, 900),
        ] {
            history.seed(&entry(id), count, now.saturating_sub(age));
        }
        let target = entry("alpha");
        history.record_query("re", &target);
        history.record_query("re", &target);
        history.record_query("other", &target);

        let snap = history.rank_snapshot("  RE ");
        for id in ["alpha", "beta", "gamma", "delta", "missing"] {
            let direct = history.score(id);
            let batched = snap.score(id);
            assert!(
                (direct - batched).abs() < 1e-6,
                "score({id}) 不一致: {direct} vs {batched}"
            );
            let direct = history.query_boost("re", id);
            let batched = snap.boost(id);
            assert_eq!(direct, batched, "query_boost({id}) 不一致");
        }
        let empty = history.rank_snapshot("");
        assert_eq!(empty.boost("alpha"), history.query_boost("", "alpha"));
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

    #[test]
    fn learn_badges_reflect_query_and_frecency() {
        let mut history = History::in_memory();
        let a = entry("alpha");
        let b = entry("beta");
        history.seed(&a, 5, now_secs()); // 常用(count≥3)
        history.seed(&b, 1, now_secs()); // 不常用
        history.record_query("al", &a); // 搜 al 时选过 alpha
        let badges = history.learn_badges("al");
        assert!(badges.get("alpha").unwrap().frequent);
        assert!(badges.get("alpha").unwrap().recent_pick);
        assert!(!badges.get("beta").map(|x| x.frequent).unwrap_or(false));
        // 换查询词:recent_pick 不再生效,frequent 仍在
        let other = history.learn_badges("zz");
        assert!(other.get("alpha").unwrap().frequent);
        assert!(!other.get("alpha").unwrap().recent_pick);
    }

    #[test]
    fn learning_summary_orders_by_hits_and_count() {
        let mut history = History::in_memory();
        let a = entry("alpha");
        let b = entry("beta");
        history.seed(&a, 10, now_secs());
        history.seed(&b, 2, now_secs());
        history.record_query("re", &a);
        history.record_query("re", &a);
        history.record_query("re", &b);
        let qh = history.query_hit_rows(10);
        assert_eq!(qh[0].0, "re");
        assert_eq!(qh[0].1, "alpha"); // hits 2 > beta 1
        assert_eq!(qh[0].2, 2);
        let freq = history.frecency_rows(10);
        assert_eq!(freq[0].3, 10); // alpha count 10 排首
    }

    #[test]
    fn input_history_coalesces_typing_prefixes() {
        let mut history = History::in_memory();
        for text in ["w", "wx", "wxi", "wxin"] {
            history.record_input(text);
        }
        // 连续打字只保留最终形态。
        assert_eq!(history.recent_inputs(10), vec!["wxin"]);

        // 退格回退同样合并。
        history.record_input("wxi");
        assert_eq!(history.recent_inputs(10), vec!["wxi"]);

        // 完全不同的词另起一条（最新在前）。
        history.record_input("hello");
        assert_eq!(history.recent_inputs(10), vec!["hello", "wxi"]);
        history.record_input("world");
        assert_eq!(history.recent_inputs(10), vec!["world", "hello", "wxi"]);
    }

    #[test]
    fn input_history_ignores_blank_and_caps_entries() {
        let mut history = History::in_memory();
        history.record_input("   ");
        assert!(history.recent_inputs(10).is_empty());

        for index in 0..120 {
            history.record_input(&format!("query{index:03}"));
        }
        assert_eq!(history.recent_inputs(200).len(), 100, "只保留最近 100 条");
    }

    #[test]
    fn contains_only_reports_recorded_entries() {
        let mut history = History::in_memory();
        history.record(&entry("alpha"));
        assert!(history.contains("alpha"));
        assert!(!history.contains("beta"));
        history.remove("alpha");
        assert!(!history.contains("alpha"));
    }
}
