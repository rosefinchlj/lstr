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

use crate::store::{fold_ascii, VolumeStore, ATTR_INTERNAL, NO_PARENT};

pub const FILE_ATTRIBUTE_DIRECTORY_U16: u16 = 0x0010;

/// 文件名严格匹配谓词（区分大小写 / 全词）。
///
/// 现有 ngram / char_mask / memmem 仍作为大小写不敏感的子串超集预筛；
/// 这里在 emit 前再做一次严格校验，保证 cap/total 只统计真正命中的条目。
pub(crate) struct MatchMatcher {
    tokens: Vec<Vec<u8>>,
    whole_word: bool,
    case_sensitive: bool,
}

impl MatchMatcher {
    pub(crate) fn new(spec: &QuerySpec) -> Option<Self> {
        if !spec.case_sensitive && !spec.whole_word {
            return None;
        }
        let tokens: Vec<Vec<u8>> = spec
            .tokens
            .iter()
            .filter(|token| !token.is_empty())
            .map(|token| {
                if spec.case_sensitive {
                    token.as_bytes().to_vec()
                } else {
                    fold_ascii(token.as_bytes())
                }
            })
            .collect();
        Some(Self {
            tokens,
            whole_word: spec.whole_word,
            case_sensitive: spec.case_sensitive,
        })
    }

    pub(crate) fn matches(&self, store: &VolumeStore, slot: u32) -> bool {
        let name = if self.case_sensitive {
            store.name(slot)
        } else {
            store.name_lc(slot)
        };
        self.tokens
            .iter()
            .all(|token| token_matches(name, token, self.whole_word))
    }
}

/// ASCII 字母数字算“词字符”；其余（标点/空白/非 ASCII 字节）都算边界。
#[inline]
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

fn token_matches(name: &[u8], token: &[u8], whole_word: bool) -> bool {
    if token.is_empty() {
        return true;
    }
    if !whole_word {
        return memchr::memmem::find(name, token).is_some();
    }
    let mut from = 0usize;
    while from + token.len() <= name.len() {
        let Some(rel) = memchr::memmem::find(&name[from..], token) else {
            return false;
        };
        let start = from + rel;
        let end = start + token.len();
        let start_ok = start == 0 || !is_word_byte(name[start - 1]);
        let end_ok = end == name.len() || !is_word_byte(name[end]);
        if start_ok && end_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// 路径前缀的祖先段匹配器（无分配，避免为每个候选拼完整路径）。
///
/// 关键点：路径过滤必须在 **候选封顶之前** 生效。旧实现先收集前
/// `candidate_cap` 条全局命中、再用 `path_in_prefix` 后滤；当查询词很常见
/// （`api-` / `dll`）时，前 2000 条可能全在目标目录之外，结果 total 很大、
/// hits 为 0。这里把前缀判断压进 emit，cap/总数都只统计目录内的命中。
///
/// 语义与 [`path_in_prefix`] 一致：候选必须是前缀目录的**后代**（目录自身
/// 不算），路径从卷根开始逐段完全一致。
pub(crate) struct PathMatcher {
    /// 前缀去掉盘符后的段（小写 ASCII；中文按原字节比较）。
    segments: Vec<Vec<u8>>,
}

impl PathMatcher {
    /// 只接受本地盘符前缀（`d:\...`）；UNC/相对前缀返回 None，退回后置过滤。
    pub(crate) fn new(prefix: &str) -> Option<Self> {
        let bytes = prefix.trim_end_matches(['\\', '/']).as_bytes();
        if bytes.len() < 2
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
        {
            return None;
        }
        let rest = &prefix.trim_end_matches(['\\', '/'])[2..];
        let segments: Vec<Vec<u8>> = rest
            .split(['\\', '/'])
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_ascii_lowercase())
            .collect();
        Some(Self { segments })
    }

    /// slot 是否位于前缀目录之下。从父链向上按「最后一段→第一段」连续匹配，
    /// 全部匹配后再确认前缀目录是卷根的直接子项（否则只是路径后缀碰巧相同）。
    pub(crate) fn matches(&self, store: &VolumeStore, slot: u32) -> bool {
        if self.segments.is_empty() {
            return true; // 盘符根：卷内所有条目都在前缀下
        }
        let mut cur = store.parent[slot as usize];
        let mut matched = 0usize;
        let mut hops = 0u32;
        while cur != NO_PARENT && hops < 64 {
            let name = store.name(cur);
            let last = self.segments[self.segments.len() - 1].as_slice();
            if matched < self.segments.len()
                && name.eq_ignore_ascii_case(&self.segments[self.segments.len() - 1 - matched])
            {
                matched += 1;
                if matched == self.segments.len() {
                    return store.parent[cur as usize] == NO_PARENT;
                }
            } else if matched > 0 {
                // 断链：当前节点可能重新成为「最后一段」（避免 a\x\b 误配 a\b）。
                matched = usize::from(name.eq_ignore_ascii_case(last));
                if matched == self.segments.len() {
                    return store.parent[cur as usize] == NO_PARENT;
                }
            }
            cur = store.parent[cur as usize];
            hops += 1;
        }
        false
    }
}

/// 查询取消代次:UI 每次发出新搜索前写入最新 generation;查询线程在
/// 热循环里每 [`CANCEL_CHECK_EVERY`] 个候选/命中读一次,发现代次变化即
/// 提前中止,避免旧查询(如单字符 "a" 命中 200 万条要 600ms+)把新击键
/// 堵在队列里。一条候选检查一次性原子读,1024 候选 ≈ 10-40µs,
/// 被取代的查询最迟在数百 µs 内中止(远低于 2ms 目标)。
pub static CANCEL_GEN: AtomicU64 = AtomicU64::new(0);

/// 取消检查步长(候选/命中条数)。所有候选遍历路径统一用它:
/// 空 token 枚举、单字符位图、n-gram 候选链、memmem 命中流。
const CANCEL_CHECK_EVERY: usize = 1024;
const CANCEL_CHECK_MASK: usize = CANCEL_CHECK_EVERY - 1;

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
    /// 小写路径前缀(如 `c:\users\x\desktop`);带盘符时引擎只在对应卷查询。
    /// 深路径对收集结果做后置过滤:目录自身不计入(只匹配其后代);
    /// 小结果集(本卷 total 未超 cap)时 total 按过滤后实际条数修正,
    /// 超出 cap 时为近似值。
    pub path_prefix: Option<String>,
    /// 类型组的扩展名集合(小写、无点)。
    pub group_exts: Vec<String>,
    /// 精确匹配(名字完全一致,小写比较;wfn: 语义)。设置后只返回精确名。
    pub exact: Option<String>,
    /// 修改时间下界(unix 秒,含);None=不限。mtime=0(未知)条目在时间过滤下一律不通过。
    pub mtime_after: Option<i64>,
    /// 修改时间上界(unix 秒,含)。
    pub mtime_before: Option<i64>,
    /// 卷掩码(bit i='A'+i):None=不限制,Some(mask)=只在这些卷查询
    /// (mask 为 0 表示排除掉了所有卷,查询结果为空)。
    pub vol_mask: Option<u32>,
    /// 区分 ASCII 大小写（默认 false = 现有大小写不敏感语义）。
    pub case_sensitive: bool,
    /// 全词匹配：每个 token 前后必须是词边界（ASCII 字母数字为词字符）。
    pub whole_word: bool,
}

/// 盘符字母 → 卷掩码位(bit i = 'A'+i)。非字母返回 0。
pub fn volume_bit(letter: char) -> u32 {
    let upper = letter.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        1u32 << (upper as u32 - 'A' as u32)
    } else {
        0
    }
}

/// 深路径前缀匹配。`path`/`prefix` 按调用约定均已小写。
///
/// - 目录自身(path == prefix)**不算**:`path:E:\dir` 只返回 dir 的后代,
///   不把 dir 自己列出来;
/// - prefix 末尾是分隔符时直接匹配其后所有内容(`e:\dir\`);
/// - 否则要求 prefix 后紧跟 `\` 或 `/` —— 同前缀兄弟目录不能被误命中
///   (`e:\dir2` 不匹配 `e:\dir`)。
///
/// 空 prefix 视为不限制(旧行为:所有路径 starts_with("") 都为真)。
pub fn path_in_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    path.len() > prefix.len()
        && path.starts_with(prefix)
        && (prefix.ends_with('\\')
            || prefix.ends_with('/')
            || matches!(path.as_bytes()[prefix.len()], b'\\' | b'/'))
}

impl QuerySpec {
    pub fn is_empty_text(&self) -> bool {
        self.tokens.iter().all(|t| t.is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub vol: u32,
    pub slot: u32,
    /// 卷索引代次:compact/重扫/重置/卷移除后校验,拒绝过期命中(H8)。
    pub generation: u32,
    /// 索引里的 mtime(unix 秒,0 = 未知)。候选排序(最近修改加权)直接
    /// 复用该值,零 FS 调用;展示条目的 mtime 仍由 finalize 从 FS 重新取。
    pub mtime: i64,
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
    // 快路径:All 且无 ext/组过滤时无需看名字;Folders 同理(目录判定只看
    // attrs)。单字符 "a" 要扫 290 万 slot,旧实现每条都做 name_lc + 后缀
    // 比较,是 folders 路 39ms 的主因;这里让绝大多数候选只读 attrs。
    let need_name = spec.ext.is_some()
        || !spec.group_exts.is_empty()
        || matches!(spec.kind, KindFilter::Files | KindFilter::Programs);
    if !need_name {
        return match spec.kind {
            KindFilter::All => true,
            KindFilter::Folders => is_dir,
            KindFilter::Files | KindFilter::Programs => unreachable!(),
        };
    }
    let name = store.name_lc(slot);
    // 只有 Files/Programs 需要 .exe 判定;All/Folders 不再无谓计算。
    let is_exe = matches!(spec.kind, KindFilter::Files | KindFilter::Programs)
        && name.ends_with(b".exe");
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

/// 归一化 token(去空、小写折叠),各查询路径共用。
fn query_tokens(spec: &QuerySpec) -> Vec<Vec<u8>> {
    spec.tokens
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| fold_ascii(t.as_bytes()))
        .collect()
}

/// exact 模式候选集:ready 走 offs 二分、scan 全扫,顺序/过滤与
/// `walk_candidates` 的 exact 分支逐条一致。旧 exact 查询不受 cap/TOTAL_CAP
/// 限制,这里同样收集全部同名槽位,供 `query_split_with_exact` 并趟复用。
pub(crate) fn exact_slots(store: &VolumeStore, spec: &QuerySpec, ex: &[u8]) -> Vec<u32> {
    let mut slots = Vec::new();
    // offs 按小写名排序；大小写敏感时先用折叠名定位区间，再按原始名判等。
    let folded = if spec.case_sensitive {
        fold_ascii(ex)
    } else {
        ex.to_vec()
    };
    if !store.offs.is_empty() {
        let lo = store.offs.partition_point(|&s| store.name_lc(s) < &folded[..]);
        for &slot in &store.offs[lo..] {
            if store.name_lc(slot) != folded {
                break;
            }
            let exact_ok = !spec.case_sensitive || store.name(slot) == ex;
            if exact_ok && passes_filters(store, slot, spec) {
                slots.push(slot);
            }
        }
        return slots;
    }
    for slot in 0..store.name_off.len() as u32 {
        let exact_ok = if spec.case_sensitive {
            store.name(slot) == ex
        } else {
            store.name_lc(slot) == ex
        };
        if exact_ok && passes_filters(store, slot, spec) {
            slots.push(slot);
        }
    }
    slots
}

/// 可取消版本:返回 `(slots, total, capped, aborted)`。查询线程传入发查询时
/// 的 `CANCEL_GEN` 快照,UI 每次新搜索都会刷新该代次,热循环据此提前中止。
pub(crate) fn query_volume_cancel(
    store: &VolumeStore,
    spec: &QuerySpec,
    cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, usize, bool, bool) {
    let tokens = query_tokens(spec);
    let matcher = spec.path_prefix.as_deref().and_then(PathMatcher::new);
    let match_matcher = MatchMatcher::new(spec);
    // 精确模式沿用旧语义:收集与 total 都不受 cap/TOTAL_CAP 限制(名字完全
    // 一致的条目本来就少,旧实现直接返回全部;wfn 补齐依赖这个行为)。
    let unbounded = !tokens.is_empty() && spec.exact.is_some();
    let cap_eff = if unbounded { usize::MAX } else { cap };
    let mut slots: Vec<u32> = Vec::with_capacity(256);
    let mut total = 0usize;
    let mut capped = false;
    let aborted = walk_candidates(
        store,
        spec,
        &tokens,
        matcher.as_ref(),
        match_matcher.as_ref(),
        cancel,
        &mut |slot| {
        total += 1;
        if !unbounded && total >= TOTAL_CAP {
            return false; // total 已到显示上限,停止扫描(收集也随之中止)
        }
        if slots.len() < cap_eff {
            slots.push(slot);
        } else {
            capped = true;
        }
        true
    });
    (slots, total, capped, aborted)
}

/// 单卷「文件夹 + 全部」一次遍历收集。一次候选遍历同时产出:
/// - `all_slots`:All-kind 命中的前 `all_cap` 条(含文件夹),对应旧 files 路
///   的引擎候选前缀(应用层再滤掉文件夹并截断);
/// - `folder_slots`:Folders 命中的前 `folder_cap` 条,对应旧 folders 路;
/// - `total_all`:All-kind 精确计数(TOTAL_CAP 封顶);
/// - `total_folders`:Folders 精确计数(TOTAL_CAP 封顶)。
/// 两个 total 各自独立封顶,与两次独立查询逐字节一致;旧实现要跑两遍引擎,
/// 现在候选生成/过滤只跑一遍。
pub(crate) fn query_volume_split_cancel(
    store: &VolumeStore,
    spec: &QuerySpec,
    folder_cap: usize,
    all_cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, Vec<u32>, usize, usize, bool) {
    debug_assert!(spec.exact.is_none(), "split 查询不使用 exact 模式");
    let tokens = query_tokens(spec);
    let matcher = spec.path_prefix.as_deref().and_then(PathMatcher::new);
    let match_matcher = MatchMatcher::new(spec);
    let mut all_slots: Vec<u32> = Vec::with_capacity(256);
    let mut folder_slots: Vec<u32> = Vec::with_capacity(256);
    let mut total_all = 0usize;
    let mut total_folders = 0usize;
    let mut all_done = false;
    let mut folder_done = false;
    let aborted = walk_candidates(
        store,
        spec,
        &tokens,
        matcher.as_ref(),
        match_matcher.as_ref(),
        cancel,
        &mut |slot| {
        if all_done && folder_done {
            return false;
        }
        let is_dir = store.is_dir(slot);
        if !all_done {
            total_all += 1;
            if total_all >= TOTAL_CAP {
                all_done = true; // 与旧 All 查询一致:第 TOTAL_CAP 条只计数不收集
            } else if all_slots.len() < all_cap {
                all_slots.push(slot);
            }
        }
        if is_dir && !folder_done {
            total_folders += 1;
            if total_folders >= TOTAL_CAP {
                folder_done = true;
            } else if folder_slots.len() < folder_cap {
                folder_slots.push(slot);
            }
        }
        !(all_done && folder_done)
    });
    (all_slots, folder_slots, total_all, total_folders, aborted)
}

/// 候选遍历内核:按 [`QuerySpec`] 的同一套语义(空 token 枚举 / 精确 / 单字符
/// 位图 / n-gram / memmem)产出通过过滤的 slot,顺序与旧实现完全一致。
/// `emit` 返回 false 表示调用方已集齐、停止扫描;返回值 = 是否被新查询中止。
fn walk_candidates(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    matcher: Option<&PathMatcher>,
    match_matcher: Option<&MatchMatcher>,
    cancel: Option<u64>,
    emit_inner: &mut impl FnMut(u32) -> bool,
) -> bool {
    // 路径前缀 + 严格匹配在 emit 前过滤：候选封顶与 total 都只统计
    // 目录内、且满足大小写/全词语义的命中。
    let mut emit_path = |slot: u32| {
        if matcher.is_some_and(|m| !m.matches(store, slot)) {
            return true;
        }
        if match_matcher.is_some_and(|m| !m.matches(store, slot)) {
            return true;
        }
        emit_inner(slot)
    };
    let emit = &mut emit_path;
    if tokens.is_empty() {
        // H5:空 token(单独 ext:rs / path:C:\dir 等过滤器查询)按过滤器全量列举,
        // 逐 slot 走 passes_filters(名字/类型/扩展名/组),收集封顶 candidate_cap,
        // total 精确统计。旧实现在此直接返回空 → 内置 Ready 后这两类筛选静默 0 结果。
        for slot in 0..store.name_off.len() as u32 {
            if slot & CANCEL_CHECK_MASK as u32 == 0 && cancelled(cancel) {
                return true;
            }
            if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
                continue;
            }
            if !passes_filters(store, slot, spec) {
                continue;
            }
            if !emit(slot) {
                break;
            }
        }
        return false;
    }
    // 精确模式:wfn 语义,直接二分 offs(ready 模式);scan 模式退化为逐项比对
    if cancelled(cancel) {
        return true;
    }
    if let Some(ex) = &spec.exact {
        let exb = ex.as_bytes();
        // offs 按小写名排序；大小写敏感时先用折叠名定位区间，再用原始名判等。
        let folded = if spec.case_sensitive {
            fold_ascii(exb)
        } else {
            exb.to_vec()
        };
        if !store.offs.is_empty() {
            let lo = store.offs.partition_point(|&s| store.name_lc(s) < &folded[..]);
            for &slot in &store.offs[lo..] {
                if store.name_lc(slot) != folded {
                    break;
                }
                let exact_ok = !spec.case_sensitive || store.name(slot) == exb;
                if exact_ok && passes_filters(store, slot, spec) && !emit(slot) {
                    return false;
                }
            }
            return false;
        }
        for slot in 0..store.name_off.len() as u32 {
            let exact_ok = if spec.case_sensitive {
                store.name(slot) == exb
            } else {
                store.name_lc(slot) == exb
            };
            if exact_ok && passes_filters(store, slot, spec) && !emit(slot) {
                return false;
            }
        }
        return false;
    }

    // E:单字符 ASCII 查询(打字过程中的临时串)直接用名字字母位图过滤,
    // 否则要对整卷 names_lc 做 memmem + 每个命中二分,20 万~200 万命中就是数百 ms。
    if tokens.len() == 1 && tokens[0].len() == 1 && tokens[0][0].is_ascii_alphabetic() {
        let bit = 1u32 << (tokens[0][0] - b'a');
        for slot in 0..store.name_off.len() as u32 {
            if slot & CANCEL_CHECK_MASK as u32 == 0 && cancelled(cancel) {
                return true;
            }
            if store.char_mask[slot as usize] & bit == 0 {
                continue;
            }
            if !passes_filters(store, slot, spec) {
                continue;
            }
            if !emit(slot) {
                break;
            }
        }
        return false;
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
        return walk_ngram(store, spec, tokens, gen_idx, gen, base, cancel, emit);
    }
    walk_memmem(store, spec, tokens, gen_idx, gen, cancel, emit)
}

/// n-gram 快路径遍历。候选序 = base 链(slot 升序)++ overlay(name_off 序):
/// compact 后 slot 序 ≡ name_off 序;overlay 槽(新增/改名)的名字区偏移恒大于
/// 构建期偏移,故拼接后仍是全局偏移序 —— 与 memmem 路径的输出同序同分。
fn walk_ngram(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    mut cands: Vec<u32>,
    cancel: Option<u64>,
    emit: &mut impl FnMut(u32) -> bool,
) -> bool {
    // overlay:倒排构建后的新增/改名 slot 逐个验证(不在倒排里,或旧名
    // posting 已失效)。字母位图先粗筛(子串出现 ⇒ 名字含 gen 的全部字母),
    // 再 memmem 验证;renames>0 时 name_off 跳到名字区尾,按偏移重排。
    let re = store.reindex_slots();
    if !re.is_empty() {
        let gen_mask = crate::store::mask_of_bytes(gen);
        let mut ov: Vec<u32> = Vec::new();
        for (j, &slot) in re.iter().enumerate() {
            if j & CANCEL_CHECK_MASK == CANCEL_CHECK_MASK && cancelled(cancel) {
                return true;
            }
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
        // 排序本身不可中断:入场前先看一次代次;改名风暴 overlay 最大 ~8 万,
        // sort 是这条路径上唯一的非循环段(实测 ~1-3ms)。
        if cancelled(cancel) {
            return true;
        }
        if store.has_pending_rename() {
            ov.sort_unstable_by_key(|&s| store.name_off[s as usize]);
        }
        if cancelled(cancel) {
            return true;
        }
        // base 里被改名的槽是旧名假阳性,剔除(新名由 overlay 覆盖)
        cands.retain(|&s| !store.base_posting_stale(s));
        cands.extend(ov);
    }
    // ≥4 字符候选是 trigram 交集超集,须逐个验证 gen 真实出现;
    // 2/3 字符链是精确命中集,免验证。
    let verify_gen = gen.len() >= 4;
    for (i, &slot) in cands.iter().enumerate() {
        if i & CANCEL_CHECK_MASK == CANCEL_CHECK_MASK && cancelled(cancel) {
            return true;
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
        if !emit(slot) {
            return false;
        }
    }
    false
}

/// memmem 全量扫描遍历(倒排不可用时兜底;等价性测试用独立实现做 oracle)。
fn walk_memmem(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    cancel: Option<u64>,
    emit: &mut impl FnMut(u32) -> bool,
) -> bool {
    let finder = memchr::memmem::Finder::new(gen);
    let mut seen = 0usize;
    let mut last_slot: Option<u32> = None; // 同一条目内多次命中只算一次
    for off in finder.find_iter(&store.names_lc) {
        seen += 1;
        if seen & CANCEL_CHECK_MASK == 0 && cancelled(cancel) {
            return true;
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
        if !emit(slot) {
            return false;
        }
    }
    false
}

/// memmem 全量扫描路径(倒排不可用时兜底;等价性测试用它对拍快路径)。
/// 语义:按 names_lc 偏移序枚举 gen 的全部命中 → slot 解析(丢弃陈项)→
/// 同条目去重 → 其余 token AND → 过滤 → total 精确计数(TOTAL_CAP 封顶)、
/// 收集封顶 cap。
#[cfg(test)]
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
    let matcher = spec.path_prefix.as_deref().and_then(PathMatcher::new);

    let finder = memchr::memmem::Finder::new(gen);
    let mut seen = 0usize;
    for off in finder.find_iter(&store.names_lc) {
        seen += 1;
        if seen & CANCEL_CHECK_MASK == 0 && cancelled(cancel) {
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
        if matcher.as_ref().is_some_and(|m| !m.matches(store, slot)) {
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

    /// 深路径前缀:目录自身不算命中;同前缀兄弟目录不误命中;末尾分隔符
    /// 与正/反斜杠后代都按预期。
    #[test]
    fn path_in_prefix_excludes_directory_itself_and_siblings() {
        let prefix = r"e:\data\lstracc-文档";
        // 目录自身:path == prefix → 不算
        assert!(!path_in_prefix(prefix, prefix));
        // 后代(反斜杠/正斜杠)命中
        assert!(path_in_prefix(r"e:\data\lstracc-文档\a.txt", prefix));
        assert!(path_in_prefix(r"e:\data\lstracc-文档/sub", prefix));
        // 同前缀兄弟目录及其子项不得命中(前缀后必须紧跟分隔符)
        assert!(!path_in_prefix(r"e:\data\lstracc-文档2", prefix));
        assert!(!path_in_prefix(r"e:\data\lstracc-文档2\a.txt", prefix));
        // prefix 自带末尾分隔符:直接匹配其后所有内容
        let with_sep = r"e:\data\lstracc-文档\";
        assert!(path_in_prefix(r"e:\data\lstracc-文档\a.txt", with_sep));
        assert!(!path_in_prefix(r"e:\data\lstracc-文档2\a.txt", with_sep));
        // 空 prefix 保持旧语义(不限制)
        assert!(path_in_prefix(r"e:\anything", ""));
        // 完全不同的路径
        assert!(!path_in_prefix(r"e:\other\a.txt", prefix));
    }

    /// 回归（2026-11-11 `api-` 目录内搜索 0 结果）：路径前缀必须在候选封顶
    /// **之前**过滤。旧实现先收前 2000 条全局命中再后滤路径，目标目录外的
    /// 常见词命中把 cap 占满，目录内的条目一条都进不了 hits。
    #[test]
    fn path_prefix_filters_before_candidate_cap() {
        let mut s = VolumeStore::new(false);
        // 卷根的直接子目录（生产快照里顶层条目 parent 就是 NO_PARENT）
        s.insert(10, 0, 0x10, b"target");
        s.insert(11, 0, 0x10, b"other");
        for i in 0..3000u64 {
            s.insert(1000 + i, 11, 0, format!("api-{i}.dll").as_bytes());
        }
        s.insert(9000, 10, 0, b"api-target.dll");
        s.compact();

        let spec = QuerySpec {
            tokens: vec!["api-".into()],
            path_prefix: Some(r"d:\target".into()),
            ..Default::default()
        };
        let (slots, total, _, _) = query_volume_cancel(&s, &spec, 2000, None);
        assert_eq!(total, 1, "total 只应统计目标目录内的命中");
        assert_eq!(slots.len(), 1, "目标目录内的条目不能被全局 cap 挤掉");
        assert_eq!(s.name(slots[0]), b"api-target.dll");
    }

    /// 前缀必须从卷根开始逐段连续匹配：`b\a\b\file` 不能命中 `a\b`。
    #[test]
    fn path_matcher_anchors_at_volume_root() {
        let mut s = VolumeStore::new(false);
        s.insert(10, 0, 0x10, b"b");
        s.insert(11, 10, 0x10, b"a");
        s.insert(12, 11, 0x10, b"b");
        s.insert(13, 12, 0, b"file.txt");
        s.compact();

        let matcher = PathMatcher::new(r"d:\a\b").expect("盘符前缀");
        assert!(!matcher.matches(&s, s.find_slot(13).unwrap()), "后缀碰巧相同不算");

        // 真正的 a\b\file 命中，目录自身不计。
        let mut ok = VolumeStore::new(false);
        ok.insert(20, 0, 0x10, b"a");
        ok.insert(21, 20, 0x10, b"b");
        ok.insert(22, 21, 0, b"file.txt");
        ok.compact();
        let matcher = PathMatcher::new(r"d:\a\b").expect("盘符前缀");
        assert!(matcher.matches(&ok, ok.find_slot(22).unwrap()));
        assert!(!matcher.matches(&ok, ok.find_slot(21).unwrap()), "前缀目录自身不算");
    }

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

    /// 任务 4 基准(手动运行,操作全局 CANCEL_GEN,不进默认套件):
    /// 取消检查的候选粒度 —— 在遍历中途改写代次,统计「换代次后还多走了
    /// 多少候选才中止」,再乘单候选实测成本折算中止延迟。
    /// `cargo test -p lstr-index --release --lib bench_cancel_granularity -- --ignored --nocapture`
    #[test]
    #[ignore = "manual bench: mutates global CANCEL_GEN"]
    fn bench_cancel_granularity() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        for i in 0..1_000_000u64 {
            s.insert(1000 + i, 5, 0, format!("alpha_{i:06}.txt").as_bytes());
        }
        s.compact();
        // 慢路径(memmem + 逐命中二分)便于在中途改写代次。
        let spec = QuerySpec {
            tokens: vec!["_".into()],
            kind: KindFilter::All,
            ext: Some("txt".into()),
            ..Default::default()
        };
        let tokens = query_tokens(&spec);
        let my = CANCEL_GEN.load(Ordering::Relaxed);
        let mut emitted = 0usize;
        let mut cancel_at = 0usize;
        let t0 = std::time::Instant::now();
        let aborted = walk_candidates(&s, &spec, &tokens, None, None, Some(my), &mut |_slot| {
            emitted += 1;
            if emitted == 100_000 {
                CANCEL_GEN.store(my + 1, Ordering::Relaxed);
                cancel_at = emitted;
            }
            true
        });
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        CANCEL_GEN.store(my, Ordering::Relaxed);
        assert!(aborted, "换代次后遍历必须中止");
        let overshoot = emitted - cancel_at;
        println!(
            "[cancel] memmem: emitted={emitted} 换代次后多走 {overshoot} 候选才中止 (stride={CANCEL_CHECK_EVERY}), 耗时 {elapsed:.3}ms"
        );
        assert!(
            overshoot <= 2 * CANCEL_CHECK_EVERY,
            "中止粒度应 ≤{CANCEL_CHECK_EVERY} 候选(加一次检查相位), got {overshoot}"
        );

        // 单字符位图路径(生产 "a" 的主路径):满库扫描总耗时 → 每 1024 候选
        // 的上界延迟。无目录 + Folders 过滤 → 扫完 1M 条不做任何收集。
        let spec_bitmap = QuerySpec {
            tokens: vec!["a".into()],
            kind: KindFilter::Folders,
            ext: Some("txt".into()),
            ..Default::default()
        };
        let t1 = std::time::Instant::now();
        let (_, total, _, _) = query_volume_cancel(&s, &spec_bitmap, 2000, None);
        let bitmap_ms = t1.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(total, 0);
        let per_candidate_ns = bitmap_ms * 1_000_000.0 / 1_000_000.0;
        let bound_us = per_candidate_ns * CANCEL_CHECK_EVERY as f64 / 1000.0;
        println!(
            "[cancel] bitmap+ext: 1M 全扫 {bitmap_ms:.3}ms → 每候选 {per_candidate_ns:.2}ns, 取消上界 {bound_us:.3}µs"
        );
        // 生产形态(无 ext/组过滤):passes_filters 快路径只读 attrs。
        let spec_plain = QuerySpec {
            tokens: vec!["a".into()],
            kind: KindFilter::Folders,
            ..Default::default()
        };
        let t2 = std::time::Instant::now();
        let (_, total2, _, _) = query_volume_cancel(&s, &spec_plain, 2000, None);
        let plain_ms = t2.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(total2, 0);
        let plain_ns = plain_ms * 1_000_000.0 / 1_000_000.0;
        let plain_bound_us = plain_ns * CANCEL_CHECK_EVERY as f64 / 1000.0;
        println!(
            "[cancel] bitmap(attrs-only): 1M 全扫 {plain_ms:.3}ms → 每候选 {plain_ns:.2}ns, 取消上界 {plain_bound_us:.3}µs"
        );
        assert!(plain_ms * 3.0 < 10.0, "2.9M 条生产规模全扫应在 10ms 内(旧实现 folders 路 39ms)");
    }

    /// 任务 1 对拍:一次 split 遍历 ≡ 两次独立查询(folders 路 + All 路)。
    /// 逐项比较 slots 顺序与两个 total;覆盖单字符位图、2/3 字符链、≥4 字符
    /// trigram 验证、ext/组过滤、空 token 过滤器、scan 模式与 overlay。
    fn assert_split_matches_two_pass(s: &VolumeStore, spec: &QuerySpec, cap: usize) {
        let mut spec_all = spec.clone();
        spec_all.kind = KindFilter::All;
        let mut spec_folders = spec.clone();
        spec_folders.kind = KindFilter::Folders;
        let all = query_volume_cancel(s, &spec_all, cap, None);
        let folders = query_volume_cancel(s, &spec_folders, cap, None);
        let (all_slots, folder_slots, total_all, total_folders, aborted) =
            query_volume_split_cancel(s, &spec_all, cap, cap, None);
        assert!(!aborted);
        assert_eq!(all_slots, all.0, "All 前缀顺序不一致: {:?}", spec.tokens);
        assert_eq!(total_all, all.1, "total_all 不一致: {:?}", spec.tokens);
        assert_eq!(
            folder_slots, folders.0,
            "Folders 前缀顺序不一致: {:?}",
            spec.tokens
        );
        assert_eq!(
            total_folders, folders.1,
            "total_folders 不一致: {:?}",
            spec.tokens
        );
    }

    #[test]
    fn split_query_matches_two_pass_on_mixed_store() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        for i in 0..20_000u64 {
            let slot_kind = i % 10;
            let name = match slot_kind {
                0 => format!("ai_dir_{i:05}"),
                1 => format!("ai_file_{i:05}.txt"),
                2 => format!("ai_tool_{i:05}.exe"),
                3 => format!("tail_{i:05}.log"),
                4 => format!("main_{i:05}.rs"),
                5 => format!("报告_{i:05}.docx"),
                6 => format!("alpha_{i:05}.tmp"),
                _ => format!("zip_{i:05}.dat"),
            };
            let attrs = if slot_kind == 0 { 0x10 } else { 0 };
            s.insert_meta(
                1000 + i,
                5,
                attrs,
                name.as_bytes(),
                1_600_000_000 + (i % 1000) as i64,
            );
        }
        s.compact();
        for spec in [
            QuerySpec { tokens: vec!["ai".into()], ..Default::default() },
            QuerySpec { tokens: vec!["a".into()], ..Default::default() },
            QuerySpec { tokens: vec!["alph".into()], ..Default::default() },
            QuerySpec {
                tokens: vec!["ai".into()],
                ext: Some("txt".into()),
                ..Default::default()
            },
            QuerySpec {
                tokens: vec!["tool".into()],
                group_exts: vec!["exe".into()],
                ..Default::default()
            },
            QuerySpec {
                tokens: Vec::new(),
                ext: Some("rs".into()),
                ..Default::default()
            },
            QuerySpec {
                tokens: vec!["ai".into()],
                mtime_after: Some(1_600_000_500),
                ..Default::default()
            },
        ] {
            assert_split_matches_two_pass(&s, &spec, 1000);
        }

        // overlay:改名 + 追加后,快路径与双路仍逐项一致。
        s.insert(1001, 5, 0, b"ai_renamed.txt");
        s.insert(21_000, 5, 0, b"ai_appended.txt");
        for spec in [
            QuerySpec { tokens: vec!["ai".into()], ..Default::default() },
            QuerySpec { tokens: vec!["appended".into()], ..Default::default() },
        ] {
            assert_split_matches_two_pass(&s, &spec, 1000);
        }
        // scan 模式(memmem 兜底路径)也一致。
        let mut scan = VolumeStore::new(false);
        scan.insert(5, 0, 0x10, b"root");
        scan.insert(10, 5, 0x10, b"ai_dir");
        scan.insert(11, 5, 0, b"ai_file.txt");
        scan.insert(12, 5, 0, b"ai_tool.exe");
        assert!(scan.is_scanning());
        for spec in [
            QuerySpec { tokens: vec!["ai".into()], ..Default::default() },
            QuerySpec { tokens: vec!["a".into()], ..Default::default() },
        ] {
            assert_split_matches_two_pass(&scan, &spec, 1000);
        }
    }

    /// total 边界:All 计数先撞 TOTAL_CAP(100k)而 Folders 未撞时,split 必须
    /// 继续扫到 Folders 也到顶/库尾,两个 total 与旧双路各自的截断口径一致。
    #[test]
    fn split_total_cap_matches_two_pass() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        for i in 0..120_000u64 {
            let attrs = if i % 2 == 0 { 0x10 } else { 0 };
            s.insert(1000 + i, 5, attrs, format!("a_{i:06}.txt").as_bytes());
        }
        s.compact();
        let spec = QuerySpec { tokens: vec!["a".into()], ..Default::default() };
        assert_split_matches_two_pass(&s, &spec, 1000);
        let (all_slots, folder_slots, total_all, total_folders, aborted) =
            query_volume_split_cancel(&s, &spec, 1000, 1000, None);
        assert!(!aborted);
        assert_eq!(total_all, TOTAL_CAP, "All 计数应在 100k 截断");
        assert_eq!(total_folders, 60_000, "Folders 计数未撞上限,应精确");
        assert_eq!(all_slots.len(), 1000);
        assert_eq!(folder_slots.len(), 1000);
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

    /// MAJOR 回归:空卷首扫完成(VolumeReady → compact)后新建文件,
    /// ≥2 字符纯 ASCII 查询曾被空倒排快路径静默归零。空 store 不得建倒排,
    /// 插入不得被 scan 模式挡住,查询必须回落 memmem 并命中。
    #[test]
    fn empty_compact_then_insert_is_searchable() {
        let mut s = VolumeStore::new(false);
        assert!(s.is_scanning());
        s.compact();
        assert!(!s.is_scanning(), "空卷 compact 后必须退出 scan 模式");
        assert!(s.is_empty());
        assert!(s.ngram_candidates(b"alpha").is_none(), "空 store 无倒排");

        assert!(s.insert(10, 5, 0, b"alpha.txt"));
        // 每个查询形态都必须命中,且与强制 memmem 路径逐字段一致
        for q in ["alpha", "al", "ph", "alpha.txt"] {
            let spec = QuerySpec { tokens: vec![q.into()], ..Default::default() };
            let (slots, total, _) = query_volume(&s, &spec, 100);
            assert_eq!(total, 1, "查询 {q:?} 应命中空卷 compact 后新建的文件");
            assert_eq!(s.name(slots[0]), b"alpha.txt");
            assert_equivalent(&s, &spec);
        }
        // 继续新建:倒排要到下次 compact 才建,查询仍走 memmem 兜底
        assert!(s.insert(11, 5, 0, b"beta.log"));
        let spec = QuerySpec { tokens: vec!["be".into()], ..Default::default() };
        let (_, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
        assert_equivalent(&s, &spec);
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

    /// 区分大小写：只命中原始字节一致的名字；默认模式保持大小写不敏感。
    #[test]
    fn case_sensitive_matches_raw_names_only() {
        let mut s = VolumeStore::new(false);
        for (i, name) in ["README", "readme", "ReadMe", "report.txt", "ÄI", "äi"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        let run = |token: &str, case_sensitive: bool| {
            let spec = QuerySpec {
                tokens: vec![token.to_string()],
                case_sensitive,
                ..Default::default()
            };
            let (_, total, _) = query_volume(&s, &spec, 100);
            total
        };
        assert_eq!(run("README", true), 1);
        assert_eq!(run("readme", true), 1);
        assert_eq!(run("ReadMe", true), 1);
        assert_eq!(run("read", true), 1, "只应命中全小写的 readme");
        assert_eq!(run("read", false), 3, "默认大小写不敏感仍命中三例");
        // 非 ASCII 本来就是字节精确；fold_ascii 不折叠 Ä/ä。
        assert_eq!(run("ÄI", true), 1);
        assert_eq!(run("äi", true), 1);
    }

    fn whole_word_store() -> VolumeStore {
        let mut s = VolumeStore::new(false);
        for (i, name) in [
            "foo",
            "foo.txt",
            "my-foo-bar",
            "foobar",
            "xfoo",
            "(foo)",
            "foo_bar",
            "FOO",
        ]
        .iter()
        .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        s
    }

    #[test]
    fn whole_word_requires_boundaries() {
        let s = whole_word_store();
        let spec = QuerySpec {
            tokens: vec!["foo".into()],
            whole_word: true,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 6);
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        for expected in [&b"foo"[..], b"foo.txt", b"my-foo-bar", b"(foo)", b"foo_bar", b"FOO"] {
            assert!(names.contains(&expected), "缺少 {:?}: {names:?}", expected);
        }
        for unexpected in [&b"foobar"[..], b"xfoo"] {
            assert!(!names.contains(&unexpected), "不应命中 {:?}", unexpected);
        }

        // 区分大小写 + 全词：FOO 不再命中。
        let spec = QuerySpec {
            tokens: vec!["foo".into()],
            whole_word: true,
            case_sensitive: true,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 5);
    }

    #[test]
    fn whole_word_multi_token_is_and() {
        let mut s = VolumeStore::new(false);
        for (i, name) in ["foo-bar", "bar-foo", "foo bar baz", "foobar bar"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["foo".into(), "bar".into()],
            whole_word: true,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 3, "foobar 里的 foo 不是整词，应被排除");
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        assert!(!names.contains(&&b"foobar bar"[..]));
    }

    /// 严格匹配必须前置到 cap 之前：全盘大量非整词命中不能把整词命中挤掉。
    #[test]
    fn whole_word_filters_before_candidate_cap() {
        let mut s = VolumeStore::new(false);
        for i in 0..3000u64 {
            s.insert(1000 + i, 0, 0, format!("foobar{i}.txt").as_bytes());
        }
        s.insert(9000, 0, 0, b"foo");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["foo".into()],
            whole_word: true,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 10);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"foo");
    }

    #[test]
    fn case_sensitive_works_in_scan_mode() {
        // 未 compact：ngram 不可用，走 memmem 全扫。
        let mut s = VolumeStore::new(false);
        s.insert(10, 0, 0, b"README");
        s.insert(11, 0, 0, b"readme");
        assert!(s.is_scanning());
        let spec = QuerySpec {
            tokens: vec!["README".into()],
            case_sensitive: true,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
    }

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
        assert!(!s.is_scanning(), "非空 compact 后必须退出 scan 模式");

        // 空 store 回归:删光存活项再 compact(空卷 compact 路径),之后
        // 插入的条目必须仍可查;倒排保持 None → 查询走 memmem 兜底。
        for frn in alive.drain(..) {
            s.remove(frn);
        }
        s.compact();
        assert!(!s.is_scanning(), "空卷 compact 后也必须退出 scan 模式");
        assert!(s.is_empty());
        for _ in 0..64 {
            let frn = next_frn;
            next_frn += 1;
            let name = random_name(&mut rng);
            let attrs = if rng.below(5) == 0 { 0x10 } else { 0 };
            s.insert(frn, 0, attrs, name.as_bytes());
            alive.push(frn);
        }
        assert!(s.reindex_slots().is_empty(), "无倒排时插入不进 overlay");
        query_batch(&s, &mut rng);

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
