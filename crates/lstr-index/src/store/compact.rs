//! 可暂停的整卷 compact（大卷独立后台线程，小卷引擎线程同步跑完）。
//!
//! 背景：`VolumeStore::compact` 是同步全量重建。实测 D 卷 148 万条最坏 3.6s，
//! 其中全量噪声重判占 74%（已改为按 `ancestry_dirty` 触发）、ngram 重建
//! 约 400ms、列重建约 100ms。即使在引擎线程上切成 4ms 片，累计仍会占用
//! 查询线程数秒；大卷现在改为「fork 快照 + 独立线程重排 + 完成后换血」，
//! 查询线程只保留 channel 轮询与 O(1) 收尾。
//!
//! 本任务把 compact 拆成可恢复的阶段，每次 `advance` 最多跑 `budget`：
//! 解析未消解父链 → 收集 live → 按需全量判噪 → FRN 排序 → 重映射 →
//! 重建列 → 重建 offs → 原子换血 → 分片重建 ngram。除「判噪 / 解析父链」
//! 阶段会原地改 attrs/parent/anchors（与旧 compact 语义一致，查询可照常
//! 读取）外，构建阶段只读旧列、结果写在任务自己的缓冲里，最后 O(1) 换血。
//!
//! 调用方（Engine）必须在任务运行期间冻结同卷修改类消息（同分段保存），
//! 并在换血后 bump generation、替换拼音缓存。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::*;

/// 每处理多少条检查一次时间预算。
const CHECK_EVERY: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Resolve,
    Live,
    Classify,
    SortFrn,
    Remap,
    Names,
    Columns,
    Offs,
    Install,
    Ngram,
    Done,
}

pub(crate) struct CompactTask {
    phase: Phase,
    /// 换血是否已发生（持久;abort 时据此补代次/拼音收尾）。
    swapped: bool,
    /// 任务开始时的旧列长度（remap 按旧 slot 下标寻址）。
    old_len: u32,
    /// 解析父链阶段快照（运行期 store 冻结，unresolved 稳定）。
    resolve_pairs: Vec<(u32, u64)>,
    resolve_cursor: usize,
    resolve_still: HashMap<u32, u64>,
    resolved_any: bool,
    live_cursor: u32,
    live: Vec<u32>,
    classify_cursor: usize,
    anchor_cursor: u32,
    by_frn: Vec<(u64, u32)>,
    remap: Vec<u32>,
    remap_cursor: usize,
    names: Vec<u8>,
    names_lc: Vec<u8>,
    name_off: Vec<u32>,
    mask: Vec<u32>,
    names_cursor: usize,
    parent: Vec<u32>,
    attrs: Vec<u16>,
    mtime: Vec<u32>,
    size: Vec<u64>,
    frn: Vec<u64>,
    cols_cursor: usize,
    offs: Vec<u32>,
    offs_sorted: Vec<u32>,
    offs_base: Vec<u32>,
    offs_delta: Vec<u32>,
    offs_cursor: usize,
    offs_base_pos: usize,
    offs_delta_pos: usize,
    ngram: Option<NgramBuildTask>,
    /// 已推进片数与累计占用时间（诊断用：wall 时长包含片间处理消息/等调度）。
    slices: u32,
    busy: Duration,
}

impl CompactTask {
    /// `None` = 无事可做（与 `compact` 的早退条件一致）。
    pub(crate) fn new(store: &VolumeStore) -> Option<Self> {
        if !store.is_scanning()
            && store.delta.is_empty()
            && store.dead_count == 0
            && store.renames == 0
        {
            return None;
        }
        Some(Self {
            phase: Phase::Resolve,
            swapped: false,
            old_len: store.name_off.len() as u32,
            resolve_pairs: Vec::new(),
            resolve_cursor: 0,
            resolve_still: HashMap::new(),
            resolved_any: false,
            live_cursor: 0,
            live: Vec::new(),
            classify_cursor: 0,
            anchor_cursor: 0,
            by_frn: Vec::new(),
            remap: Vec::new(),
            remap_cursor: 0,
            names: Vec::new(),
            names_lc: Vec::new(),
            name_off: Vec::new(),
            mask: Vec::new(),
            names_cursor: 0,
            parent: Vec::new(),
            attrs: Vec::new(),
            mtime: Vec::new(),
            size: Vec::new(),
            frn: Vec::new(),
            cols_cursor: 0,
            offs: Vec::new(),
            offs_sorted: Vec::new(),
            offs_base: Vec::new(),
            offs_delta: Vec::new(),
            offs_cursor: 0,
            offs_base_pos: 0,
            offs_delta_pos: 0,
            ngram: None,
            slices: 0,
            busy: Duration::ZERO,
        })
    }

    /// 换血是否已发生（持久标志,abort 收尾用）。
    pub(crate) fn installed(&self) -> bool {
        self.swapped
    }

    /// 实际占用（片内耗时累计），不含片间处理消息/等调度。
    pub(crate) fn busy(&self) -> Duration {
        self.busy
    }

    pub(crate) fn slices(&self) -> u32 {
        self.slices
    }

    /// 推进直至阶段完成或预算耗尽。返回 true = 全部完成。
    pub(crate) fn advance(&mut self, store: &mut VolumeStore, budget: Duration) -> bool {
        let t_slice = Instant::now();
        let deadline = t_slice + budget;
        self.slices += 1;
        let out = self.advance_inner(store, deadline);
        self.busy += t_slice.elapsed();
        out
    }

    fn advance_inner(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        loop {
            let done = match self.phase {
                Phase::Resolve => self.step_resolve(store, deadline),
                Phase::Live => self.step_live(store, deadline),
                Phase::Classify => self.step_classify(store, deadline),
                Phase::SortFrn => self.step_sort(store),
                Phase::Remap => self.step_remap(store, deadline),
                Phase::Names => self.step_names(store, deadline),
                Phase::Columns => self.step_columns(store, deadline),
                Phase::Offs => self.step_offs(store, deadline),
                Phase::Install => self.step_install(store),
                Phase::Ngram => self.step_ngram(store, deadline),
                Phase::Done => return true,
            };
            if !done {
                return false;
            }
            if Instant::now() >= deadline && self.phase != Phase::Done {
                return false;
            }
        }
    }

    fn step_resolve(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.resolve_pairs.is_empty() && self.resolve_cursor == 0 {
            self.resolve_pairs = store
                .unresolved
                .iter()
                .map(|(&s, &f)| (s, f))
                .collect();
        }
        let mut n = 0usize;
        while self.resolve_cursor < self.resolve_pairs.len() {
            let (slot, pfrn) = self.resolve_pairs[self.resolve_cursor];
            self.resolve_cursor += 1;
            match store.find_slot(pfrn) {
                Some(p) => {
                    self.resolved_any = true;
                    store.parent[slot as usize] = p;
                    if !store.exclude.is_empty() {
                        store.classify_chain(slot);
                    }
                }
                None => {
                    self.resolve_still.insert(slot, pfrn);
                }
            }
            n += 1;
            if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                return false;
            }
        }
        store.unresolved = std::mem::take(&mut self.resolve_still);
        self.phase = Phase::Live;
        true
    }

    fn step_live(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.live.is_empty() && self.live_cursor == 0 {
            self.live = Vec::with_capacity(store.name_off.len());
        }
        let total = store.name_off.len() as u32;
        let mut n = 0usize;
        while self.live_cursor < total {
            let s = self.live_cursor;
            self.live_cursor += 1;
            if store.visible(s) {
                self.live.push(s);
            }
            n += 1;
            if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                return false;
            }
        }
        self.phase = Phase::Classify;
        true
    }

    fn step_classify(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        // scan_mode 的 store 不走本任务（VolumeReady 用同步 compact），但这里
        // 仍保守覆盖：扫描态一律全量判噪。
        let reclassify = !store.exclude.is_empty()
            && (store.is_scanning() || self.resolved_any || store.ancestry_dirty);
        if self.classify_cursor == 0 && self.anchor_cursor == 0 && !self.live.is_empty() {
            // 进入本阶段的第一片：无需判噪则直接跳过。
            if !reclassify {
                self.phase = Phase::SortFrn;
                return true;
            }
        }
        if reclassify {
            let mut n = 0usize;
            while self.classify_cursor < self.live.len() {
                let s = self.live[self.classify_cursor];
                self.classify_cursor += 1;
                store.classify_chain(s);
                n += 1;
                if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                    return false;
                }
            }
            // 段名噪声目录锚点（与旧 compact 的后置扫描逐条一致）。
            let total = store.name_off.len() as u32;
            while self.anchor_cursor < total {
                let s = self.anchor_cursor;
                self.anchor_cursor += 1;
                let a = store.attrs[s as usize];
                if (a & ATTR_INTERNAL) != 0 || (a & 0x10) == 0 {
                    continue;
                }
                if store.exclude.is_segment(store.name_lc(s)) {
                    store.noise_anchors.insert(store.frn[s as usize]);
                }
                n += 1;
                if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                    return false;
                }
            }
            self.live.retain(|&s| store.visible(s));
        }
        self.phase = Phase::SortFrn;
        true
    }

    fn step_sort(&mut self, store: &mut VolumeStore) -> bool {
        self.by_frn = self
            .live
            .iter()
            .map(|&s| (store.frn[s as usize], s))
            .collect();
        self.by_frn.sort_unstable_by_key(|&(f, _)| f);
        self.phase = Phase::Remap;
        true
    }

    fn step_remap(&mut self, _store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.remap.is_empty() {
            // 按旧 slot 下标寻址：必须覆盖旧列全部槽位（含未 live 的槽）。
            self.remap = vec![u32::MAX; self.old_len as usize];
        }
        let mut n = 0usize;
        while self.remap_cursor < self.by_frn.len() {
            let (_, old) = self.by_frn[self.remap_cursor];
            self.remap[old as usize] = self.remap_cursor as u32;
            self.remap_cursor += 1;
            n += 1;
            if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                return false;
            }
        }
        self.phase = Phase::Names;
        true
    }

    fn step_names(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.name_off.is_empty() && self.names.is_empty() {
            self.names.reserve(store.names.len());
            self.name_off.reserve(self.by_frn.len());
            self.mask.reserve(self.by_frn.len());
        }
        let mut n = 0usize;
        while self.names_cursor < self.by_frn.len() {
            let (_, old) = self.by_frn[self.names_cursor];
            self.names_cursor += 1;
            let start = store.name_off[old as usize] as usize;
            let end = slice_end(&store.names, start);
            self.name_off.push(self.names.len() as u32);
            self.names.extend_from_slice(&store.names[start..end]);
            self.names.push(0);
            let name = &store.names[start..end];
            self.names_lc.extend_from_slice(&fold_case(name));
            self.names_lc.push(0);
            self.mask.push(mask_of_bytes(name));
            n += 1;
            if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                return false;
            }
        }
        debug_assert_eq!(self.names.len(), self.names_lc.len());
        self.phase = Phase::Columns;
        true
    }

    fn step_columns(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.parent.is_empty() && self.cols_cursor == 0 {
            self.parent.reserve(self.by_frn.len());
            self.attrs.reserve(self.by_frn.len());
            self.mtime.reserve(self.by_frn.len());
            self.size.reserve(self.by_frn.len());
            self.frn.reserve(self.by_frn.len());
        }
        let mut n = 0usize;
        while self.cols_cursor < self.by_frn.len() {
            let (_, old) = self.by_frn[self.cols_cursor];
            self.cols_cursor += 1;
            let p = store.parent[old as usize];
            self.parent
                .push(if p == NO_PARENT { NO_PARENT } else { self.remap[p as usize] });
            self.attrs.push(store.attrs[old as usize] & !ATTR_INTERNAL);
            self.mtime.push(store.mtime[old as usize]);
            // size 列与 mtime 同语义:按旧 slot 取值,写入新 slot 序。
            // 漏掉这一列会让 `size:` 过滤指向别的文件(UI 由逐条 find_meta
            // 覆盖,肉眼不易发现),且错位列会被快照持久化。
            self.size.push(store.size[old as usize]);
            self.frn.push(store.frn[old as usize]);
            n += 1;
            if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                return false;
            }
        }
        self.phase = Phase::Offs;
        true
    }

    fn step_offs(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.offs_cursor == 0 && self.offs.is_empty() && self.offs_sorted.is_empty() {
            if store.renames > 0 {
                // 改名导致 base 名序失效：整体重排（罕见路径，排序本身不可切）。
                let mut sorted = self.live.clone();
                sorted.sort_unstable_by(|&a, &b| {
                    slice_at(&store.names_lc, store.name_off[a as usize] as usize)
                        .cmp(slice_at(&store.names_lc, store.name_off[b as usize] as usize))
                });
                self.offs_sorted = sorted;
            } else {
                // delta 按新列名序排序（≤ COMPACT_DELTA，毫秒级）。
                self.offs_delta = store
                    .delta
                    .iter()
                    .filter(|&&s| self.remap[s as usize] != u32::MAX)
                    .map(|&s| self.remap[s as usize])
                    .collect();
                self.offs_delta.sort_unstable_by(|&a, &b| {
                    slice_at(&self.names_lc, self.name_off[a as usize] as usize)
                        .cmp(slice_at(&self.names_lc, self.name_off[b as usize] as usize))
                });
                // base 旧名序过滤 + 重映射。
                self.offs_base = if store.base_len > 0 && !store.offs.is_empty() {
                    store
                        .offs
                        .iter()
                        .filter(|&&s| self.remap[s as usize] != u32::MAX)
                        .map(|&s| self.remap[s as usize])
                        .collect()
                } else {
                    Vec::new()
                };
            }
        }
        let mut n = 0usize;
        if !self.offs_sorted.is_empty() || store.renames > 0 {
            while self.offs_cursor < self.offs_sorted.len() {
                let old = self.offs_sorted[self.offs_cursor];
                self.offs_cursor += 1;
                self.offs.push(self.remap[old as usize]);
                n += 1;
                if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                    return false;
                }
            }
        } else if !self.offs_base.is_empty() {
            // 两条已排序序列 O(n) 归并（与旧 compact 同口径）。
            self.offs.reserve(self.offs_base.len() + self.offs_delta.len());
            while self.offs_base_pos < self.offs_base.len()
                && self.offs_delta_pos < self.offs_delta.len()
            {
                let a = self.offs_base[self.offs_base_pos];
                let b = self.offs_delta[self.offs_delta_pos];
                let na = slice_at(&self.names_lc, self.name_off[a as usize] as usize);
                let nb = slice_at(&self.names_lc, self.name_off[b as usize] as usize);
                if na <= nb {
                    self.offs.push(a);
                    self.offs_base_pos += 1;
                } else {
                    self.offs.push(b);
                    self.offs_delta_pos += 1;
                }
                n += 1;
                if n % CHECK_EVERY == 0 && Instant::now() >= deadline {
                    return false;
                }
            }
        } else {
            self.offs = std::mem::take(&mut self.offs_delta);
        }
        self.phase = Phase::Install;
        true
    }

    fn step_install(&mut self, store: &mut VolumeStore) -> bool {
        // O(1) 换血：列先全部构建完成，这里只做移动。
        if self.offs_base_pos < self.offs_base.len() || self.offs_delta_pos < self.offs_delta.len() {
            self.offs.extend_from_slice(&self.offs_base[self.offs_base_pos..]);
            self.offs.extend_from_slice(&self.offs_delta[self.offs_delta_pos..]);
        }
        store.names = std::sync::Arc::new(std::mem::take(&mut self.names));
        store.names_lc = std::sync::Arc::new(std::mem::take(&mut self.names_lc));
        store.name_off = std::mem::take(&mut self.name_off);
        store.offs = std::mem::take(&mut self.offs);
        store.frn = std::mem::take(&mut self.frn);
        store.parent = std::mem::take(&mut self.parent);
        store.attrs = std::mem::take(&mut self.attrs);
        store.mtime = std::mem::take(&mut self.mtime);
        store.size = std::mem::take(&mut self.size);
        store.char_mask = std::mem::take(&mut self.mask);
        store.off_index = store
            .name_off
            .iter()
            .enumerate()
            .map(|(i, &o)| (o, i as u32))
            .collect();
        store.delta.clear();
        store.delta.shrink_to_fit();
        store.delta_frn.clear();
        store.delta_frn.shrink_to_fit();
        // unresolved 的 slot id 重映射（被噪声淘汰的条目随 remap 消失）。
        let old_unresolved = std::mem::take(&mut store.unresolved);
        store.unresolved = old_unresolved
            .iter()
            .filter_map(|(&old, &f)| {
                let new = self.remap[old as usize];
                (new != u32::MAX).then_some((new, f))
            })
            .collect();
        store.base_len = store.name_off.len() as u32;
        store.dead_count = 0;
        store.renames = 0;
        store.ancestry_dirty = false;
        store.scan_mode = false;
        // 换血后的派生索引：清空旧倒排（分片重建），待回填队列重建。
        store.ngram = None;
        store.ngram_base_len = store.name_off.len() as u32;
        store.reindex.clear();
        store.reindex_mark.clear();
        store.reindex_mark.resize((store.name_off.len() + 63) / 64, 0);
        store.rebuild_mtime_pending();
        store.rebuild_size_pending();
        if store.children_built {
            store.rebuild_children();
        }
        self.swapped = true;
        self.phase = Phase::Ngram;
        true
    }

    fn step_ngram(&mut self, store: &mut VolumeStore, deadline: Instant) -> bool {
        if self.ngram.is_none() {
            if store.name_off.is_empty() || store.names_lc.len() < NGRAM_MIN_BYTES {
                self.phase = Phase::Done;
                return true;
            }
            self.ngram = Some(NgramBuildTask::new(store.name_off.len()));
        }
        let task = self.ngram.as_mut().expect("ngram task");
        if task.advance(&store.names_lc, &store.name_off, deadline) {
            store.ngram = task.take_index();
            self.ngram = None;
            self.phase = Phase::Done;
        }
        self.phase == Phase::Done
    }
}

/// 分片构建 n-gram 倒排：与 [`NgramIndex::build`] 两遍算法逐字节一致，
/// 只是把「每卷一遍」拆成每次 `advance` 处理若干 slot。
struct NgramBuildTask {
    n: u32,
    pass: u8,
    cursor: u32,
    stamp_b: Vec<u32>,
    stamp_t: Vec<u32>,
    len_b: Vec<u32>,
    len_t: Vec<u32>,
    size_b: Vec<u32>,
    size_t: Vec<u32>,
    off_b: Vec<u32>,
    off_t: Vec<u32>,
    postings_b: Vec<u8>,
    postings_t: Vec<u8>,
    cur_b: Vec<u32>,
    cur_t: Vec<u32>,
    index: Option<NgramIndex>,
}

impl NgramBuildTask {
    fn new(n: usize) -> Self {
        Self {
            n: n as u32,
            pass: 0,
            cursor: 0,
            stamp_b: vec![0u32; NG_BIGRAM],
            stamp_t: vec![0u32; NG_TRIGRAM],
            len_b: vec![0u32; NG_BIGRAM],
            len_t: vec![0u32; NG_TRIGRAM],
            size_b: vec![0u32; NG_BIGRAM],
            size_t: vec![0u32; NG_TRIGRAM],
            off_b: Vec::new(),
            off_t: Vec::new(),
            postings_b: Vec::new(),
            postings_t: Vec::new(),
            cur_b: Vec::new(),
            cur_t: Vec::new(),
            index: None,
        }
    }

    fn advance(&mut self, names_lc: &[u8], name_off: &[u32], deadline: Instant) -> bool {
        if self.pass == 0 {
            let stamp_b = &mut self.stamp_b;
            let stamp_t = &mut self.stamp_t;
            let len_b = &mut self.len_b;
            let len_t = &mut self.len_t;
            let size_b = &mut self.size_b;
            let size_t = &mut self.size_t;
            let mut done = 0usize;
            while self.cursor < self.n {
                let slot = self.cursor;
                self.cursor += 1;
                let name = slice_at(names_lc, name_off[slot as usize] as usize);
                let s1 = slot + 1;
                for_each_ngram(
                    name,
                    &mut |k| {
                        if stamp_b[k] != s1 {
                            size_b[k] += varint_len(ng_delta(stamp_b[k], slot)) as u32;
                            stamp_b[k] = s1;
                            len_b[k] += 1;
                        }
                    },
                    &mut |k| {
                        if stamp_t[k] != s1 {
                            size_t[k] += varint_len(ng_delta(stamp_t[k], slot)) as u32;
                            stamp_t[k] = s1;
                            len_t[k] += 1;
                        }
                    },
                );
                done += 1;
                if done % CHECK_EVERY == 0 && Instant::now() >= deadline {
                    return false;
                }
            }
            // 前缀和 + 分配 postings（一趟线性，毫秒级）。
            let mut total = 0u32;
            self.off_b.push(0);
            for &s in &self.size_b {
                total += s;
                self.off_b.push(total);
            }
            let mut total = 0u32;
            self.off_t.push(0);
            for &s in &self.size_t {
                total += s;
                self.off_t.push(total);
            }
            self.postings_b = vec![0u8; self.off_b[NG_BIGRAM] as usize];
            self.postings_t = vec![0u8; self.off_t[NG_TRIGRAM] as usize];
            self.cur_b = self.off_b[..NG_BIGRAM].to_vec();
            self.cur_t = self.off_t[..NG_TRIGRAM].to_vec();
            self.stamp_b.fill(0);
            self.stamp_t.fill(0);
            self.pass = 1;
            self.cursor = 0;
            return false;
        }
        let stamp_b = &mut self.stamp_b;
        let stamp_t = &mut self.stamp_t;
        let cur_b = &mut self.cur_b;
        let cur_t = &mut self.cur_t;
        let postings_b = &mut self.postings_b;
        let postings_t = &mut self.postings_t;
        let mut done = 0usize;
        while self.cursor < self.n {
            let slot = self.cursor;
            self.cursor += 1;
            let name = slice_at(names_lc, name_off[slot as usize] as usize);
            let s1 = slot + 1;
            for_each_ngram(
                name,
                &mut |k| {
                    if stamp_b[k] != s1 {
                        let delta = ng_delta(stamp_b[k], slot);
                        cur_b[k] += ng_write(postings_b, cur_b[k] as usize, delta) as u32;
                        stamp_b[k] = s1;
                    }
                },
                &mut |k| {
                    if stamp_t[k] != s1 {
                        let delta = ng_delta(stamp_t[k], slot);
                        cur_t[k] += ng_write(postings_t, cur_t[k] as usize, delta) as u32;
                        stamp_t[k] = s1;
                    }
                },
            );
            done += 1;
            if done % CHECK_EVERY == 0 && Instant::now() >= deadline {
                return false;
            }
        }
        let bigram = (0..NG_BIGRAM)
            .map(|k| NgList { start: self.off_b[k], len: self.len_b[k] })
            .collect();
        let trigram = (0..NG_TRIGRAM)
            .map(|k| NgList { start: self.off_t[k], len: self.len_t[k] })
            .collect();
        self.index = Some(NgramIndex {
            postings_b: std::mem::take(&mut self.postings_b),
            postings_t: std::mem::take(&mut self.postings_t),
            bigram,
            trigram,
        });
        true
    }

    fn take_index(&mut self) -> Option<NgramIndex> {
        self.index.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::ExcludeRules;
    use crate::query::{query_volume, QuerySpec};

    fn rules() -> ExcludeRules {
        ExcludeRules::build(&[
            "node_modules".into(),
            "d:/programdata".into(),
            "temp".into(),
        ])
    }

    /// 先同步 compact 到 ready 态，再施加一轮 churn（delta/删除/改名/迟到子项），
    /// 返回处于「待 compact」状态的 store。两个 store 用同一序列构造，逐字节等价。
    fn churned_store() -> VolumeStore {
        const DIR: u32 = 0x10;
        let mut s = VolumeStore::new(rules(), 'D');
        // 初始树：普通目录 + 两个噪声树 + 一个锚定路径树。
        s.insert(100, ROOT_FRN, DIR, b"docs");
        s.insert(101, 100, 0, b"alpha.txt");
        s.insert(102, 100, 0, b"beta.txt");
        s.insert(110, ROOT_FRN, DIR, b"node_modules");
        // 噪声目录本身可见，子项应判噪。
        s.insert(111, 110, 0, b"pkg.js");
        s.insert(120, ROOT_FRN, DIR, b"ProgramData");
        s.insert(121, 120, DIR, b"Vendor");
        s.insert(122, 121, 0, b"ucrtbase.dll");
        s.insert(130, ROOT_FRN, DIR, b"temp");
        s.insert(131, 130, 0, b"scratch.tmp");
        // 父项迟到：子项先插入（父 FRN 尚不存在）→ unresolved。
        s.insert(140, 141, DIR, b"late_child");
        s.insert(141, ROOT_FRN, DIR, b"late_dir");
        s.compact();

        // churn：新增 delta、删除、文件改名、目录改名、新的迟到子项。
        s.insert(200, ROOT_FRN, 0, b"gamma.txt");
        s.insert(201, 141, 0, b"late_file.txt");
        s.remove(101);
        s.remove(102);
        s.insert(101, 100, 0, b"alpha_renamed.txt"); // 文件改名（就地）
        s.insert(141, ROOT_FRN, DIR, b"late_dir_moved"); // 目录改名
        s.insert(210, 9999, 0, b"orphan.txt"); // 永久解析不到父
        s
    }

    fn assert_same_cols(a: &VolumeStore, b: &VolumeStore) {
        assert_eq!(a.names, b.names, "names");
        assert_eq!(a.names_lc, b.names_lc, "names_lc");
        assert_eq!(a.name_off, b.name_off, "name_off");
        assert_eq!(a.offs, b.offs, "offs");
        assert_eq!(a.frn, b.frn, "frn");
        assert_eq!(a.parent, b.parent, "parent");
        assert_eq!(a.attrs, b.attrs, "attrs");
        assert_eq!(a.mtime, b.mtime, "mtime");
        assert_eq!(a.size, b.size, "size");
        assert_eq!(a.char_mask, b.char_mask, "char_mask");
        assert_eq!(a.base_len, b.base_len, "base_len");
        assert_eq!(a.dead_count, b.dead_count, "dead_count");
        assert_eq!(a.renames, b.renames, "renames");
        assert_eq!(a.scan_mode, b.scan_mode, "scan_mode");
        assert_eq!(a.ancestry_dirty, b.ancestry_dirty, "ancestry_dirty");
        assert_eq!(a.unresolved, b.unresolved, "unresolved");
        assert_eq!(a.noise_anchors, b.noise_anchors, "noise_anchors");
    }

    fn assert_same_queries(a: &VolumeStore, b: &VolumeStore) {
        for tok in ["txt", "ucrtbase", "js", "late", "alpha", "gamma", "orphan", "node", "a"] {
            let spec = QuerySpec {
                tokens: vec![tok.to_string()],
                ..Default::default()
            };
            assert_eq!(
                query_volume(a, &spec, 100),
                query_volume(b, &spec, 100),
                "查询 {tok} 结果不一致"
            );
        }
    }

    #[test]
    fn paged_compact_matches_synchronous() {
        let mut sync_store = churned_store();
        let mut paged = churned_store();
        sync_store.compact();

        let mut task = CompactTask::new(&paged).expect("有待归并工作");
        let mut slices = 0usize;
        // 极小预算：强制分片，验证状态机可恢复。
        while !task.advance(&mut paged, Duration::from_micros(50)) {
            slices += 1;
            assert!(slices < 100_000, "分片 compact 必须收敛");
        }
        assert!(slices > 0, "极小预算下必须分片执行");
        assert_same_cols(&sync_store, &paged);
        assert_same_queries(&sync_store, &paged);
        assert!(paged.ngram.is_some(), "测试构建强制建倒排（阈值 1）");
    }

    /// P1 回归:分片 compact 必须重排 size 列。同步 `compact` 会按新 slot 序
    /// 重建 size,而 `CompactTask` 曾漏掉这一列 —— slot 按 FRN 重排后
    /// `size[slot]` 指向别的文件,`size:` 过滤静默命中错误条目(UI 由逐条
    /// find_meta 覆盖,肉眼不易发现),错位列还会被快照持久化。
    #[test]
    fn paged_compact_permutes_size_column() {
        let mut sync_store = churned_store();
        let mut paged = churned_store();
        // 灌可区分的真实 size(u64::MAX 是「待回填」标记,用它测不出错位)。
        for (i, s) in sync_store.size.iter_mut().enumerate() {
            *s = (i as u64 + 1) * 4096;
        }
        for (i, s) in paged.size.iter_mut().enumerate() {
            *s = (i as u64 + 1) * 4096;
        }

        sync_store.compact();
        let mut task = CompactTask::new(&paged).expect("有待归并工作");
        while !task.advance(&mut paged, Duration::from_micros(50)) {}

        assert_eq!(sync_store.size, paged.size, "size 列必须随 slot 重排");
        assert_eq!(paged.size.len(), paged.len(), "size 列必须与 live 列等长");
    }

    /// 逐名折叠与整缓冲折叠必须逐字节一致（分片构建 names_lc 的前提）。
    #[test]
    fn per_name_fold_matches_whole_buffer() {
        let names: Vec<&[u8]> = vec![
            b"alpha.TXT",
            b"ucrtbase.dll",
            "报告.DOCX".as_bytes(),
            "ÄI.txt".as_bytes(),
            b"a",
        ];
        let mut whole = Vec::new();
        let mut per = Vec::new();
        for n in &names {
            whole.extend_from_slice(n);
            whole.push(0);
            per.extend_from_slice(&fold_case(n));
            per.push(0);
        }
        assert_eq!(fold_case(&whole), per);
    }

    /// 分类门限：稳态 compact（无祖先链变化）不得再全量重判。
    #[test]
    fn steady_state_compact_skips_reclassify() {
        let mut s = churned_store();
        s.compact();
        assert!(!s.ancestry_dirty);
        // 纯删除/新增（祖先链不变）→ 第二次 compact 不应重判。
        s.insert(300, ROOT_FRN, 0, b"another.txt");
        s.remove(200);
        s.compact();
        assert!(!s.ancestry_dirty, "稳态 compact 后 dirty 必须清空");

        // 目录改名 → 下一次 compact 必须重判。
        s.insert(141, ROOT_FRN, 0x10, b"late_dir_into_node_modules");
        assert!(s.ancestry_dirty, "目录改名必须置脏");
    }

    /// 目录改名成噪声段名：子项必须在下一次 compact 被剔除。
    /// （分类门限若漏触发，这里会留下 leak.txt —— 909c86e 的同类泄漏。）
    #[test]
    fn dir_rename_into_noise_segment_reclassifies_descendants() {
        let mut s = VolumeStore::new(rules(), 'D');
        s.insert(300, ROOT_FRN, 0x10, b"x");
        s.insert(301, 300, 0, b"leak.txt");
        s.compact();
        s.insert(300, ROOT_FRN, 0x10, b"node_modules"); // 目录就地改名
        assert!(s.ancestry_dirty, "目录改名必须置脏");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["leak".to_string()],
            ..Default::default()
        };
        assert_eq!(query_volume(&s, &spec, 100).1, 0, "噪声目录的子孙必须被剔除");
        let dir = QuerySpec {
            tokens: vec!["node_modules".to_string()],
            ..Default::default()
        };
        assert_eq!(query_volume(&s, &dir, 100).1, 1, "噪声目录自身保持可见");
    }

    /// 判噪目录被 compact 剔除后，迟到子项靠锚点立即判噪（909c86e 回归）。
    #[test]
    fn late_child_of_removed_noise_dir_is_noise() {
        let mut s = VolumeStore::new(rules(), 'D');
        s.insert(999, ROOT_FRN, 0x10, b"ProgramData");
        s.insert(1000, 999, 0x10, b"Vendor");
        s.insert(1001, 1000, 0, b"inside.dll");
        s.compact();
        // Vendor 整棵子树被剔除，但 FRN 锚点保留。
        assert!(s.noise_anchors.contains(&1000), "被判噪目录必须记入锚点");
        assert!(!s.noise_anchors.is_empty(), "锚点集合不得为空");
        // 迟到子项：父槽已不在，只能靠锚点。
        s.insert(2000, 1000, 0, b"late_inside.dll");
        let spec = QuerySpec {
            tokens: vec!["late_inside".to_string()],
            ..Default::default()
        };
        assert_eq!(
            query_volume(&s, &spec, 100).1,
            0,
            "已剔除噪声目录的迟到子项必须立即判噪"
        );
        // 噪声树内新增目录/文件不再触发全量重判（构建产物高频 churn 场景）。
        assert!(!s.ancestry_dirty, "新增噪声子项不得置脏");
    }

    /// 锚定规则目录迟到插入：先来的子项靠 unresolved + 重判归位。
    #[test]
    fn late_anchored_dir_marks_earlier_child_noise() {
        let mut s = VolumeStore::new(rules(), 'D');
        // 子项先到（父 FRN 999 尚不存在）→ unresolved。
        s.insert(400, 999, 0, b"leak_early.txt");
        s.insert(999, ROOT_FRN, 0x10, b"ProgramData");
        // 根级锚定目录自身保持可见、不产生锚点（锚点只给「判噪后被剔除的目录」）;
        // 迟到子项靠 compact 的父链解析 + resolved_any 重判归位。
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["leak_early".to_string()],
            ..Default::default()
        };
        assert_eq!(
            query_volume(&s, &spec, 100).1,
            0,
            "锚定目录的迟到子项必须判噪"
        );
    }
}

/// 真实快照基准（手工跑）：验证分片 compact 在 D 卷 148 万条上的单次停顿。
/// `LSTR_BENCH_INDEX_DIR=<索引目录> LSTR_BENCH_CONFIG=<config.toml>
///  LSTR_BENCH_GUID=<vol_<guid>.bin 的 guid 十六进制>
///  cargo test -p lstr-index --release bench_paged_compact_real -- --ignored --nocapture`
#[cfg(test)]
mod bench_paged_real {
    use super::*;
    use crate::snapshot;

    fn exclude_patterns(config_path: &str) -> Vec<String> {
        let text = std::fs::read_to_string(config_path).expect("read config");
        let mut out = Vec::new();
        let mut in_rule = false;
        let mut pattern: Option<String> = None;
        let mut exclude = false;
        for line in text.lines() {
            let line = line.trim();
            if line == "[[rank.noise_rules]]" {
                if let (true, Some(p)) = (exclude, pattern.take()) {
                    out.push(p);
                }
                in_rule = true;
                pattern = None;
                exclude = false;
                continue;
            }
            if !in_rule {
                continue;
            }
            if line.starts_with('[') {
                if let (true, Some(p)) = (exclude, pattern.take()) {
                    out.push(p);
                }
                in_rule = false;
                continue;
            }
            if let Some(rest) = line.strip_prefix("pattern") {
                let rest = rest.trim_start().trim_start_matches('=').trim();
                if let Some(q @ ('"' | '\'')) = rest.chars().next() {
                    if let Some(end) = rest[1..].find(q) {
                        pattern = Some(rest[1..1 + end].to_string());
                    }
                }
            } else if let Some(rest) = line.strip_prefix("exclude") {
                exclude = rest.contains("true");
            }
        }
        if let (true, Some(p)) = (exclude, pattern.take()) {
            out.push(p);
        }
        out
    }

    #[test]
    #[ignore]
    fn bench_paged_compact_real() {
        let Some(dir) = std::env::var_os("LSTR_BENCH_INDEX_DIR") else { return };
        let Some(cfg) = std::env::var_os("LSTR_BENCH_CONFIG") else { return };
        let Some(guid_hex) = std::env::var_os("LSTR_BENCH_GUID") else { return };
        let dir = std::path::PathBuf::from(dir);
        let guid = u128::from_str_radix(
            guid_hex.to_string_lossy().trim_start_matches("0x"),
            16,
        )
        .expect("guid hex");
        let rules =
            crate::noise::ExcludeRules::build(&exclude_patterns(&cfg.to_string_lossy()));
        let mut loaded = snapshot::load(&dir, guid, rules.hash(), &rules).expect("snapshot");
        let store = &mut loaded.store;
        // 走进稳态：先同步压一次（含锚点重建），再制造一轮 churn。
        store.compact();
        for _ in 0..2 {
            let frns: Vec<u64> = store.frn.iter().take(5000).copied().collect();
            for frn in frns {
                store.remove(frn);
            }
            if store.needs_compact() {
                store.compact();
            }
        }
        let frns: Vec<u64> = store.frn.iter().take(5000).copied().collect();
        for frn in frns {
            store.remove(frn);
        }
        let mut task = CompactTask::new(store).expect("task");
        let mut slices = 0usize;
        let mut max_ms = 0u128;
        let mut over = 0usize;
        let t0 = std::time::Instant::now();
        loop {
            let t = std::time::Instant::now();
            let done = task.advance(store, Duration::from_millis(4));
            let ms = t.elapsed().as_millis();
            max_ms = max_ms.max(ms);
            if ms > 10 {
                over += 1;
            }
            slices += 1;
            if done {
                break;
            }
            assert!(slices < 1_000_000);
        }
        println!(
            "paged compact (steady): wall={}ms busy={}ms slices={slices} max_slice={max_ms}ms over10ms={over}",
            t0.elapsed().as_millis(),
            task.busy().as_millis()
        );
        assert!(max_ms <= 50, "单片停顿必须远小于整卷 compact");

        // 冷路径：刚加载的快照 ancestry_dirty=true，首轮 compact 含全量判噪。
        let mut loaded = snapshot::load(&dir, guid, rules.hash(), &rules).expect("reload");
        let store = &mut loaded.store;
        let frns: Vec<u64> = store.frn.iter().take(5000).copied().collect();
        for frn in frns {
            store.remove(frn);
        }
        let mut task = CompactTask::new(store).expect("task");
        let mut slices = 0usize;
        let mut max_ms = 0u128;
        let mut over = 0usize;
        let t0 = std::time::Instant::now();
        loop {
            let t = std::time::Instant::now();
            let done = task.advance(store, Duration::from_millis(4));
            let ms = t.elapsed().as_millis();
            max_ms = max_ms.max(ms);
            if ms > 10 {
                over += 1;
            }
            slices += 1;
            if done {
                break;
            }
            assert!(slices < 1_000_000);
        }
        println!(
            "paged compact (cold+reclassify): wall={}ms busy={}ms slices={slices} max_slice={max_ms}ms over10ms={over}",
            t0.elapsed().as_millis(),
            task.busy().as_millis()
        );
        assert!(max_ms <= 50, "首轮含判噪也不得出现整卷级停顿");
    }
}
