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

    // E:单字符 ASCII 查询(打字过程中的临时串)直接用名字字母位图过滤,
    // 否则要对整卷 names_lc 做 memmem + 每个命中二分,20 万~200 万命中就是数百 ms。
    if tokens.len() == 1 && tokens[0].len() == 1 && tokens[0][0].is_ascii_alphabetic() {
        let bit = 1u32 << (tokens[0][0] - b'a');
        let mut slots: Vec<u32> = Vec::with_capacity(256);
        let mut total = 0usize;
        let mut capped = false;
        for slot in 0..store.name_off.len() as u32 {
            if slot & 4095 == 0 && cancelled(cancel) {
                return (slots, total, capped, true);
            }
            if store.char_mask[slot as usize] & bit == 0 {
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

    // 生成 token 取最长(一般最稀有)
    let gen_idx = tokens
        .iter()
        .enumerate()
        .max_by_key(|(_, t)| t.len())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let gen = &tokens[gen_idx];

    // N:n-gram 倒排快路径。2/3 字符走单条精确链,≥4 字符 trigram 链求交后
    // memmem 验证;候选序(slot 升序 ≡ name_off 序)与 memmem 偏移序一致,
    // 结果与全量扫描路径完全相同(同序同分),只是省掉全量扫描与逐命中二分。
    if let Some(base) = store.ngram_candidates(gen) {
        return query_ngram_path(store, spec, &tokens, gen_idx, gen, base, cap, cancel);
    }
    query_volume_memmem_scan(store, spec, &tokens, gen_idx, gen, cap, cancel)
}

/// memmem 全量扫描路径(倒排不可用时兜底;等价性测试用它对拍快路径)。
/// 语义:按 names_lc 偏移序枚举 gen 的全部命中 → slot 解析(丢弃陈项)→
/// 同条目去重 → 其余 token AND → 过滤 → total 精确计数(TOTAL_CAP 封顶)、
/// 收集封顶 cap。
pub(crate) fn query_volume_memmem_scan(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, usize, bool, bool) {
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

/// n-gram 快路径主体。候选序 = base 链(slot 升序)++ overlay(name_off 序):
/// compact 后 slot 序 ≡ name_off 序;overlay 槽(新增/改名)的名字区偏移恒大于
/// 构建期偏移,故拼接后仍是全局偏移序 —— 与 memmem 路径的输出同序同分。
fn query_ngram_path(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    mut cands: Vec<u32>,
    cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, usize, bool, bool) {
    // overlay:倒排构建后的新增/改名 slot 逐个验证(不在倒排里,或旧名
    // posting 已失效)。字母位图先粗筛(子串出现 ⇒ 名字含 gen 的全部字母),
    // 再 memmem 验证;renames>0 时 name_off 跳到名字区尾,按偏移重排。
    let re = store.reindex_slots();
    if !re.is_empty() {
        let gen_mask = crate::store::mask_of_bytes(gen);
        let mut ov: Vec<u32> = Vec::new();
        for &slot in re {
            if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
                continue;
            }
            if store.char_mask[slot as usize] & gen_mask != gen_mask {
                continue;
            }
            if memchr::memmem::find(store.name_lc(slot), gen).is_some() {
                ov.push(slot);
            }
        }
        if store.has_pending_rename() {
            ov.sort_unstable_by_key(|&s| store.name_off[s as usize]);
        }
        // base 里被改名的槽是旧名假阳性,剔除(新名由 overlay 覆盖)
        cands.retain(|&s| !store.base_posting_stale(s));
        cands.extend(ov);
    }
    // ≥4 字符候选是 trigram 交集超集,须逐个验证 gen 真实出现;
    // 2/3 字符链是精确命中集,免验证。
    let verify_gen = gen.len() >= 4;
    let mut slots: Vec<u32> = Vec::with_capacity(256);
    let mut total = 0usize;
    let mut capped = false;
    for (i, &slot) in cands.iter().enumerate() {
        if i & 4095 == 4095 && cancelled(cancel) {
            return (slots, total, capped, true);
        }
        let name = store.name_lc(slot);
        if verify_gen && memchr::memmem::find(name, gen).is_none() {
            continue;
        }
        // 其余 token AND(与 memmem 路径同语义)
        if !tokens
            .iter()
            .enumerate()
            .all(|(j, t)| j == gen_idx || memchr::memmem::find(name, t).is_some())
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

    /// E:单字符走字母位图快路径,结果应与名字里含该字母的条目一致。
    #[test]
    fn single_char_uses_char_mask_and_matches_names() {
        let s = setup();
        let spec = QuerySpec { tokens: vec!["a".into()], ..Default::default() };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 3, "alpha/Program Files/xabcxabc 含 a");
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        assert!(names.contains(&&b"alpha.txt"[..]));
        assert!(names.contains(&&b"Program Files"[..]));
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

    // ---------- n-gram 快路径等价性:与 memmem 路径同序同分 ----------

    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next(&mut self) -> u64 {
            // xorshift64*
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// 与 query_volume_cancel 相同的 gen 选择(最长 token,平局取后者)。
    fn gen_of(tokens: &[Vec<u8>]) -> (usize, Vec<u8>) {
        let i = tokens
            .iter()
            .enumerate()
            .max_by_key(|(_, t)| t.len())
            .map(|(x, _)| x)
            .unwrap_or(0);
        (i, tokens[i].clone())
    }

    /// 快路径(生产入口 query_volume_cancel)与强制 memmem 路径逐字段对拍:
    /// (slots, total, capped) 完全一致 —— 同序同分。覆盖多 cap 档位。
    fn assert_equivalent(s: &VolumeStore, spec: &QuerySpec) {
        let tokens: Vec<Vec<u8>> = spec
            .tokens
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| fold_ascii(t.as_bytes()))
            .collect();
        let (gi, g) = gen_of(&tokens);
        for cap in [3usize, 100, 100_000] {
            let fast = query_volume_cancel(s, spec, cap, None);
            let slow = query_volume_memmem_scan(s, spec, &tokens, gi, &g, cap, None);
            assert_eq!(
                fast, slow,
                "快路径与 memmem 不一致: tokens={:?} cap={cap}",
                spec.tokens
            );
        }
    }

    fn random_spec(rng: &mut Lcg) -> QuerySpec {
        // 混合字母表:大小写(折叠)、数字、点/下划线、空格、非 ASCII。
        // 主 token ≥2 字符(ngram 快路径适用域);空格与非 ASCII token 走
        // memmem 兜底路径,一并纳入对拍。单字符查询走位图路径(既有行为,
        // 与 memmem 的排序语义本就不同,不在本对拍范围)。
        const ALPHA: &[u8] = b"abcdefghij0123456789_. ";
        const CJK: &[&str] = &["数", "据", "报", "告"];
        let len = 2 + rng.below(6);
        let mut t = String::new();
        for _ in 0..len {
            if rng.below(8) == 0 {
                t.push_str(CJK[rng.below(CJK.len())]);
            } else {
                let c = ALPHA[rng.below(ALPHA.len())] as char;
                if rng.below(4) == 0 {
                    t.push(c.to_ascii_uppercase());
                } else {
                    t.push(c);
                }
            }
        }
        let mut spec = QuerySpec { tokens: vec![t], ..Default::default() };
        if rng.below(5) == 0 {
            // 多 token AND(第二 token 可能比主 token 短,gen 仍是主 token)
            spec.tokens.push("e".into());
        }
        spec
    }

    fn random_name(rng: &mut Lcg) -> String {
        const ALPHA: &[u8] = b"aAbBcCdDeEfFgG0123456789_.- xz";
        let len = 1 + rng.below(16);
        let mut n = String::new();
        for _ in 0..len {
            if rng.below(10) == 0 {
                n.push_str(["数据", "报告", "文件"][rng.below(3)]);
            } else {
                n.push(ALPHA[rng.below(ALPHA.len())] as char);
            }
        }
        n
    }

    /// 随机名集 + 随机查询词 + 随机维护流(新建/改名/删除/复活 + 周期 compact),
    /// 全程快路径 ≡ memmem。覆盖:scan 模式、delta overlay、改名重排、
    /// 大小写折叠、空格/非 ASCII 兜底、多 token AND。
    #[test]
    fn ngram_fast_path_matches_memmem_random_stream() {
        let mut rng = Lcg::new(0x5EED_2026_1117_0001);
        let mut s = VolumeStore::new(false);
        let mut next_frn = 100u64;
        let mut alive: Vec<u64> = Vec::new();
        let mut dead: Vec<(u64, u32, String)> = Vec::new(); // (frn, attrs, name)
        // 种子集
        for _ in 0..300 {
            let frn = next_frn;
            next_frn += 1;
            let name = random_name(&mut rng);
            let attrs = if rng.below(5) == 0 { 0x10 } else { 0 };
            s.insert(frn, 0, attrs, name.as_bytes());
            alive.push(frn);
        }
        let query_batch = |s: &VolumeStore, rng: &mut Lcg| {
            for _ in 0..6 {
                let spec = random_spec(rng);
                assert_equivalent(s, &spec);
            }
        };
        // scan 模式(无索引,快路径应自动兜底)就对拍
        query_batch(&s, &mut rng);
        s.compact();
        for step in 0..1200 {
            match rng.below(100) {
                0..=44 => {
                    let frn = next_frn;
                    next_frn += 1;
                    let name = random_name(&mut rng);
                    let attrs = if rng.below(5) == 0 { 0x10 } else { 0 };
                    s.insert(frn, 0, attrs, name.as_bytes());
                    alive.push(frn);
                }
                45..=64 if !alive.is_empty() => {
                    // 就地改名(旧名 posting 失效/新名进 overlay)
                    let pos = rng.below(alive.len());
                    let frn = alive[pos];
                    let name = random_name(&mut rng);
                    s.insert(frn, 0, 0, name.as_bytes());
                }
                65..=79 if !alive.is_empty() => {
                    let pos = rng.below(alive.len());
                    let frn = alive.swap_remove(pos);
                    s.remove(frn);
                    dead.push((frn, 0, String::new()));
                }
                _ if !dead.is_empty() => {
                    // 复活(同 FRN 任意新名;名字可能变化 → 改名分支)
                    let pos = rng.below(dead.len());
                    let (frn, _, _) = dead.swap_remove(pos);
                    let name = random_name(&mut rng);
                    s.insert(frn, 0, 0, name.as_bytes());
                    alive.push(frn);
                }
                _ => {}
            }
            if step % 173 == 0 {
                s.compact();
            }
            if step % 37 == 0 {
                query_batch(&s, &mut rng);
            }
        }
        s.compact();
        query_batch(&s, &mut rng);
    }

    /// 语义级维护用例:改名槽跳过 base 链、overlay 精确补位,顺序与 memmem
    /// 偏移序一致;删除/复活/compact 前后查询不变。
    #[test]
    fn ngram_overlay_rename_remove_revive_semantics() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alphabet.txt");
        s.insert(11, 5, 0, b"beta");
        s.insert(12, 5, 0, b"gamma");
        s.compact();
        let spec = |t: &str| QuerySpec { tokens: vec![t.into()], ..Default::default() };

        // 改名 alphabet.txt → zzz.txt:slot 10 的旧名 posting 全部失效
        s.insert(10, 5, 0, b"zzz.txt");
        // "be":base 链(旧 alphabet.txt/beta)− 改名槽 = [beta]
        let (slots, total, _) = query_volume(&s, &spec("be"), 100);
        assert_eq!((slots, total), (vec![s.find_slot(11).unwrap()], 1));
        // "zzz":不在 base 链(构建时无此名),只能由 overlay 命中
        let (slots, total, _) = query_volume(&s, &spec("zzz"), 100);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"zzz.txt");
        // 改回含 "aa" 的名字并追加一个 delta 槽,验证 overlay 按偏移排序
        s.insert(10, 5, 0, b"aaz.txt");
        s.insert(13, 5, 0, b"aabc");
        let (slots, total, _) = query_volume(&s, &spec("aa"), 100);
        assert_eq!(total, 2);
        assert_eq!(s.name(slots[0]), b"aaz.txt");
        assert_eq!(s.name(slots[1]), b"aabc");
        assert_equivalent(&s, &spec("aa"));
        assert_equivalent(&s, &spec("zzz"));
        assert_equivalent(&s, &spec("be"));

        // 删除:可见性过滤,倒排与 overlay 都不再命中
        s.remove(13);
        let (_, total, _) = query_volume(&s, &spec("aa"), 100);
        assert_eq!(total, 1);
        // 复活(名字未变):base posting 仍有效
        s.insert(13, 5, 0, b"aabc");
        let (_, total, _) = query_volume(&s, &spec("aa"), 100);
        assert_eq!(total, 2);
        // 改名 + 删除 + 复活(新名走 overlay,旧名不再命中)
        s.insert(13, 5, 0, b"aabc-renamed");
        s.remove(13);
        s.insert(13, 5, 0, b"aabc-renamed");
        assert_equivalent(&s, &spec("aabc"));
        assert_equivalent(&s, &spec("renamed"));
        s.compact();
        assert_equivalent(&s, &spec("aa"));
        assert_equivalent(&s, &spec("aabc"));
        assert_equivalent(&s, &spec("zzz"));
        assert_equivalent(&s, &spec("renamed"));
        assert_equivalent(&s, &spec("be"));
    }

    /// 死槽随快照残留(删除数 < compact 阈值时 save 不触发 compact,attrs 带
    /// DEAD 位落盘)→ 加载后倒排仍覆盖这些槽,原地复活必须可查。
    #[test]
    fn ngram_covers_dead_slots_surviving_snapshot_load() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"ghost.txt");
        s.insert(11, 5, 0, b"keep.txt");
        s.compact();
        s.remove(10); // dead_count=1 < 4096:save 不 compact
        let dir = std::env::temp_dir().join(format!("lstr-ngram-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = crate::snapshot::SnapshotHeader {
            guid: 0xDEAD_BEEF_1234,
            letter: 'C',
            complete: true,
            exclude_noise: false,
            journal_id: 1,
            next_usn: 1,
            resume_frn: 0,
            est_records: 0,
        };
        crate::snapshot::save(&dir, &h, &mut s).unwrap();
        let mut s = crate::snapshot::load(&dir, h.guid, false).unwrap().store;
        let spec = |t: &str| QuerySpec { tokens: vec![t.into()], ..Default::default() };
        let (_, total, _) = query_volume(&s, &spec("ghost"), 100);
        assert_eq!(total, 0, "死槽不可见");
        s.insert(10, 5, 0, b"ghost.txt"); // 原地复活
        let (slots, total, _) = query_volume(&s, &spec("ghost"), 100);
        assert_eq!(total, 1, "复活后必须可查(倒排覆盖加载时的死槽)");
        assert_eq!(s.name(slots[0]), b"ghost.txt");
        assert_equivalent(&s, &spec("ghost"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
