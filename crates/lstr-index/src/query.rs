//! 候选生成:精确 / 前缀 / 子串(打包 buffer memmem)+ 拼音缓存。
//!
//! 语义对齐现有链路(方案 §4.3):
//! - 候选 = 名字匹配(多 token AND);
//! - 收集封顶 candidate_cap,total 照常精确统计(全扫计数);
//! - 精确/前缀不单独走二分:memmem 一次扫出全部命中,按「命中起点/长度」
//!   判定精确与前缀(ready 模式的二分加速留作 P4 杠杆);
//! - scan 模式(未 compact,无 offs)同样适用,渐进搜索白拿;
//! - 名字区 `\0` 结尾保证命中不跨条目,仍按起点校验兜底。

use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::{fold_ascii, VolumeStore, ATTR_INTERNAL};

pub const FILE_ATTRIBUTE_DIRECTORY_U16: u16 = 0x0010;

/// 查询取消代次:UI 每次发出新搜索前写入最新 generation;查询线程在
/// 热循环里每 4096 个命中读一次,发现代次变化即提前中止,避免旧查询
/// (如单字符 "a" 命中 200 万条要 600ms+)把新击键堵在队列里。
pub static CANCEL_GEN: AtomicU64 = AtomicU64::new(0);

/// total 精确计数的上限:单字符/高频 token(如 "a")能命中 200 万条,
/// 逐条计数是 600ms 级的成本,而 UI 也只会显示「10万+」。
/// 到达上限即停止计数(收集仍照常封顶 candidate_cap)。
pub const TOTAL_CAP: usize = 100_000;

#[inline]
fn cancelled(cancel: Option<u64>) -> bool {
    matches!(cancel, Some(my) if CANCEL_GEN.load(Ordering::Relaxed) != my)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KindFilter {
    #[default]
    All,
    Files,
    Folders,
    Programs,
}

#[derive(Clone, Debug, Default)]
pub struct QuerySpec {
    /// 原始查询文本 token 序列(provider 按 lstr 语义拆好)。
    pub tokens: Vec<String>,
    pub kind: KindFilter,
    /// 小写扩展名(无点)。
    pub ext: Option<String>,
    /// 小写路径前缀(如 `c:\users\x\desktop`);带盘符时引擎只在对应卷查询,
    /// 深路径对收集结果做后置过滤(total 为近似值)。
    pub path_prefix: Option<String>,
    /// 类型组的扩展名集合(小写、无点)。
    pub group_exts: Vec<String>,
    /// 精确匹配(名字完全一致,小写比较;wfn: 语义)。设置后只返回精确名。
    pub exact: Option<String>,
    /// 修改时间下界(unix 秒,含);None=不限。mtime=0(未知)条目在时间过滤下一律不通过。
    pub mtime_after: Option<i64>,
    /// 修改时间上界(unix 秒,含)。
    pub mtime_before: Option<i64>,
}

impl QuerySpec {
    pub fn is_empty_text(&self) -> bool {
        self.tokens.iter().all(|t| t.is_empty())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub vol: u32,
    pub slot: u32,
    /// 卷索引代次:compact/重扫/重置/卷移除后校验,拒绝过期命中(H8)。
    pub generation: u32,
}

#[derive(Clone, Debug, Default)]
pub struct QueryOutcome {
    pub hits: Vec<Hit>,
    pub total: usize,
    /// 因 candidate_cap 提前截断(total 仍精确,仅收集截断)。
    pub capped: bool,
    /// 新查询已到达,本次查询被提前中止(hits/total 均为部分结果,调用方应丢弃)。
    pub aborted: bool,
}

/// 判断 slot 是否通过 kind/ext/组 过滤(不含路径)。
pub fn passes_filters(store: &VolumeStore, slot: u32, spec: &QuerySpec) -> bool {
    let attrs = store.attrs[slot as usize];
    if attrs & ATTR_INTERNAL != 0 {
        return false;
    }
    if spec.mtime_after.is_some() || spec.mtime_before.is_some() {
        let m = store.mtime[slot as usize] as i64;
        if m == 0 {
            return false;
        }
        if let Some(after) = spec.mtime_after {
            if m < after {
                return false;
            }
        }
        if let Some(before) = spec.mtime_before {
            if m > before {
                return false;
            }
        }
    }
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY_U16 != 0;
    let name = store.name_lc(slot);
    let is_exe = name.ends_with(b".exe");
    let kind_ok = match spec.kind {
        KindFilter::All => true,
        KindFilter::Folders => is_dir,
        KindFilter::Files => !is_dir && !is_exe,
        KindFilter::Programs => is_exe,
    };
    if !kind_ok {
        return false;
    }
    let ext_ok = |ext: &str| {
        name.len() > ext.len() + 1
            && name.ends_with(ext.as_bytes())
            && name[name.len() - ext.len() - 1] == b'.'
    };
    if let Some(ext) = &spec.ext {
        if !ext_ok(ext) {
            return false;
        }
    }
    if !spec.group_exts.is_empty() && !spec.group_exts.iter().any(|e| ext_ok(e)) {
        return false;
    }
    true
}

/// 单卷候选收集。返回 (slots, total, capped)。
pub fn query_volume(store: &VolumeStore, spec: &QuerySpec, cap: usize) -> (Vec<u32>, usize, bool) {
    let (slots, total, capped, _) = query_volume_cancel(store, spec, cap, None);
    (slots, total, capped)
}

/// 可取消版本:返回 `(slots, total, capped, aborted)`。查询线程传入发查询时
/// 的 `CANCEL_GEN` 快照,UI 每次新搜索都会刷新该代次,热循环据此提前中止。
pub(crate) fn query_volume_cancel(
    store: &VolumeStore,
    spec: &QuerySpec,
    cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, usize, bool, bool) {
    let tokens: Vec<Vec<u8>> = spec
        .tokens
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| fold_ascii(t.as_bytes()))
        .collect();
    if tokens.is_empty() {
        // H5:空 token(单独 ext:rs / path:C:\dir 等过滤器查询)按过滤器全量列举,
        // 逐 slot 走 passes_filters(名字/类型/扩展名/组),收集封顶 candidate_cap,
        // total 精确统计。旧实现在此直接返回空 → 内置 Ready 后这两类筛选静默 0 结果。
        let mut slots = Vec::with_capacity(256);
        let mut total = 0usize;
        let mut capped = false;
        for slot in 0..store.name_off.len() as u32 {
            if slot & 4095 == 0 && cancelled(cancel) {
                return (slots, total, capped, true);
            }
            if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
                continue;
            }
            if !passes_filters(store, slot, spec) {
                continue;
            }
            total += 1;
            if total >= TOTAL_CAP {
                break;
            }
            if slots.len() < cap {
                slots.push(slot);
            } else {
                capped = true;
            }
        }
        return (slots, total, capped, false);
    }
    // 精确模式:wfn 语义,直接二分 offs(ready 模式);scan 模式退化为逐项比对
    if cancelled(cancel) {
        return (Vec::new(), 0, false, true);
    }
    if let Some(ex) = &spec.exact {
        let exb = ex.as_bytes();
        let mut slots = Vec::new();
        if !store.offs.is_empty() {
            let lo = store.offs.partition_point(|&s| store.name_lc(s) < exb);
            for &slot in &store.offs[lo..] {
                if store.name_lc(slot) == exb {
                    if passes_filters(store, slot, spec) {
                        slots.push(slot);
                    }
                } else {
                    break;
                }
            }
            let total = slots.len();
            return (slots, total, false, false);
        }
        for slot in 0..store.name_off.len() as u32 {
            if store.name_lc(slot) == exb && passes_filters(store, slot, spec) {
                slots.push(slot);
            }
        }
        let total = slots.len();
        return (slots, total, false, false);
    }

    // 生成 token 取最长(一般最稀有)
    let gen_idx = tokens
        .iter()
        .enumerate()
        .max_by_key(|(_, t)| t.len())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let gen = &tokens[gen_idx];

    let mut slots: Vec<u32> = Vec::with_capacity(256);
    let mut total = 0usize;
    let mut capped = false;
    let mut last_slot: Option<u32> = None; // 同一条目内多次命中只算一次

    let finder = memchr::memmem::Finder::new(gen);
    let mut seen = 0usize;
    for off in finder.find_iter(&store.names_lc) {
        seen += 1;
        if seen & 4095 == 0 && cancelled(cancel) {
            return (slots, total, capped, true);
        }
        // 命中偏移 → slot(off_index 二分 + 陈项过滤;就地改名的幽灵命中在此被丢弃)
        let Some(slot) = store.slot_at_offset(off) else {
            continue;
        };
        let start = store.name_off[slot as usize] as usize;
        let end = start + store.name(slot).len();
        if off + gen.len() > end {
            continue; // 越界兜底(\0 已保证不会发生)
        }
        if last_slot == Some(slot) {
            continue;
        }
        last_slot = Some(slot);
        // 其余 token AND
        let name = store.name_lc(slot);
        if !tokens
            .iter()
            .enumerate()
            .all(|(i, t)| i == gen_idx || memchr::memmem::find(name, t).is_some())
        {
            continue;
        }
        if !passes_filters(store, slot, spec) {
            continue;
        }
        total += 1;
        if total >= TOTAL_CAP {
            break;
        }
        if slots.len() < cap {
            slots.push(slot);
        } else {
            capped = true;
        }
    }
    (slots, total, capped, false)
}

/// 拼音候选匹配:全部 token 命中 initials 或 full(前缀或子串)。
pub fn pinyin_match(initials: &[u8], full: &[u8], tokens: &[Vec<u8>]) -> bool {
    tokens
        .iter()
        .all(|t| memchr::memmem::find(initials, t).is_some() || memchr::memmem::find(full, t).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::VolumeStore;

    #[test]
    fn query_is_cancelled_when_newer_generation_arrives() {
        let mut s = VolumeStore::new(false);
        for i in 0..50_000u32 {
            s.insert(1000 + i as u64, 0, 0, format!("file{i}.txt").as_bytes());
        }
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["file".into()],
            ..Default::default()
        };
        let my = CANCEL_GEN.load(Ordering::Relaxed);
        let (_, total, _, aborted) = query_volume_cancel(&s, &spec, 2000, Some(my));
        assert_eq!(total, 50_000);
        assert!(!aborted, "代次未变时不应中止");
        CANCEL_GEN.store(my + 1, Ordering::Relaxed);
        let (_, _, _, aborted) = query_volume_cancel(&s, &spec, 2000, Some(my));
        assert!(aborted, "代次变化后旧查询应提前中止");
        CANCEL_GEN.store(my, Ordering::Relaxed);
    }

    /// B 步:mtime 过滤(内置时间筛选)必须排除 mtime=0(未知)与早于下界的条目。
    #[test]
    fn mtime_filter_excludes_unknown_and_old_entries() {
        let mut s = VolumeStore::new(false);
        s.insert_meta(5, 0, 0x10, b"root", 1_600_000_000);
        s.insert_meta(10, 5, 0, b"new.txt", 1_800_000_000);
        s.insert_meta(11, 5, 0, b"old.txt", 1_500_000_000);
        s.insert(12, 5, 0, b"unknown.txt");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["txt".into()],
            mtime_after: Some(1_700_000_000),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"new.txt");
    }

    fn setup() -> VolumeStore {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.insert(11, 5, 0x10, b"Program Files");
        s.insert(12, 5, 0, b"myprog.exe");
        s.insert(13, 5, 0, b"xabcxabc.txt");
        s.compact();
        s
    }

    #[test]
    fn substring_and_dedupe_within_name() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 2); // Program Files + myprog.exe
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        assert!(names.contains(&&b"Program Files"[..]));
        assert!(names.contains(&&b"myprog.exe"[..]));
        // "abc" 在 xabcxabc.txt 命中两次,只算一条
        let spec = QuerySpec {
            tokens: vec!["abc".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 1);
    }

    #[test]
    fn files_scope_excludes_exe_and_dirs() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::Files,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 0); // Program Files=目录,myprog.exe=exe
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::Programs,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"myprog.exe");
    }

    #[test]
    fn ext_and_group_filters() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["a".into()],
            kind: KindFilter::All,
            ext: Some("txt".into()),
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 2); // alpha.txt + xabcxabc.txt
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::All,
            group_exts: vec!["exe".into()],
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 1); // myprog.exe
    }

    #[test]
    fn cap_bounds_collection_but_total_exact() {
        let mut s = VolumeStore::new(false);
        for i in 0..100u64 {
            s.insert(1000 + i, 5, 0, format!("dup{i:03}.txt").as_bytes());
        }
        s.insert(5, 0, 0x10, b"root");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["dup".into()],
            kind: KindFilter::Files,
            ..Default::default()
        };
        let (slots, total, capped) = query_volume(&s, &spec, 10);
        assert_eq!(total, 100);
        assert_eq!(slots.len(), 10);
        assert!(capped);
    }

    #[test]
    fn scan_mode_query_works_before_compact() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.insert(11, 5, 0, b"alphabet.dat");
        assert!(s.is_scanning());
        let spec = QuerySpec {
            tokens: vec!["alpha".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 2);
        assert_eq!(slots.len(), 2);
    }

    #[test]
    fn empty_tokens_list_by_filters() {
        // H5:只有过滤器(无文字 token)时按过滤条件全量列举,而不是返回空。
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"a.rs");
        s.insert(11, 5, 0, b"b.txt");
        s.insert(12, 5, 0x10, b"dir.rs");
        let spec = QuerySpec {
            tokens: Vec::new(),
            kind: KindFilter::Files,
            ext: Some("rs".into()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1, "目录与 .txt 都不应计入");
        assert_eq!(slots.len(), 1);
        assert_eq!(s.name(slots[0]), b"a.rs");
    }

    #[test]
    fn case_folding_matches_uppercase_query() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["PROGRAM".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        // "program files" 前缀命中;"myprog.exe" 只含 "prog" 不含 "program"
        assert_eq!(total, 1);
    }
}
