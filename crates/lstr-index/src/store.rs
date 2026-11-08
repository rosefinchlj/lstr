//! 卷内列式存储。
//!
//! 设计要点(对应 docs/index-engine-plan.md §4.2):
//! - 名字区 UTF-8 精确副本 + ASCII 折叠副本,**等长共享偏移**(与 NTFS 大小写
//!   语义一致;非 ASCII 不折叠——中文无大小写)。
//! - 名字以 `\0` 结尾打包:子串匹配天然不会跨条目(needle 不含 \0)。
//! - slot 即列下标,append-only;改名/删除=旧 slot 置 DEAD 位+新 slot,
//!   因此 name_off 恒升序,「命中偏移→slot」可二分。
//! - scan 模式(base_len==0):parent 列暂存父 FRN(u64),一切查询走打包
//!   buffer 扫描;volume 完成后 compact():解析父 slot、噪声分类、列重建。
//! - journal 模式(base_len>0):frn 有序数组二分 + delta HashMap 定位;
//!   delta 攒批,超阈值或空闲时 compact 归并(保留既有排序,只做 O(n) 归并)。

use std::collections::HashMap;

/// 内部位:已删除(等待 compact 回收)。
pub const ATTR_DEAD: u16 = 0x8000;
/// 内部位:命中噪声排除规则(等待 compact 回收)。
pub const ATTR_NOISE: u16 = 0x4000;
/// 内部位掩码:对外不可见的 slot。
pub const ATTR_INTERNAL: u16 = ATTR_DEAD | ATTR_NOISE;
/// parent 列的「卷根/未解析」标记。
pub const NO_PARENT: u32 = u32::MAX;

/// compact 触发阈值:delta 新 slot 数。
const COMPACT_DELTA: usize = 32_768;
/// compact 触发阈值:死 slot 占比(分子)。
const COMPACT_DEAD: u32 = 4096;

pub struct VolumeStore {
    pub names: Vec<u8>,
    pub names_lc: Vec<u8>,
    pub name_off: Vec<u32>,
    /// slot 按小写名序排序;仅 compact 后有效,scan 模式为空。
    pub offs: Vec<u32>,
    /// slot → FRN;compact 后按 FRN 升序(即 frn_sorted),scan 模式为追加序。
    pub frn: Vec<u64>,
    /// slot → 父 slot。scan 模式下此列语义为「父 FRN」(见 parent_frn)。
    pub parent: Vec<u32>,
    pub attrs: Vec<u16>,
    /// (缓冲偏移, slot) 升序追加;memmem 命中 → slot 的映射。
    /// 就地改名后旧条目成为陈项,由 slot_at_offset 的「偏移==当前 name_off」校验过滤。
    off_index: Vec<(u32, u32)>,
    /// compact 后的 slot 数(= offs.len());之后的新 slot 全在 delta。
    base_len: u32,
    /// 自上次 compact 以来的就地改名次数(>0 时 base offs 名序失效,需整体重排)。
    renames: u32,
    /// compact 后新增的 slot(base_len..)。
    delta: Vec<u32>,
    /// delta slot 的 FRN 索引(有界,受 COMPACT_DELTA 约束)。
    delta_frn: HashMap<u64, u32>,
    dead_count: u32,
    /// 插入时父未解析的 slot → 父 FRN;compact 时重试解析并补分类。
    unresolved: HashMap<u32, u64>,
    /// 噪声锚点:compact 时被整树剔除的「顶层噪声目录」FRN。
    /// 之后到达的子文件按锚点立即判噪,避免父链断裂导致泄漏。
    noise_anchors: std::collections::HashSet<u64>,
    /// 噪声排除开关(关=不分类、不回收)。
    pub noise_enabled: bool,
}

pub fn fold_ascii(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|&b| if b.is_ascii_uppercase() { b + 32 } else { b })
        .collect()
}

fn slice_at(buf: &[u8], off: usize) -> &[u8] {
    let end = buf[off..].iter().position(|&b| b == 0).unwrap_or(buf.len() - off) + off;
    &buf[off..end]
}

impl VolumeStore {
    pub fn new(noise_enabled: bool) -> Self {
        Self {
            names: Vec::with_capacity(4 << 20),
            names_lc: Vec::with_capacity(4 << 20),
            name_off: Vec::with_capacity(1 << 16),
            off_index: Vec::with_capacity(1 << 16),
            offs: Vec::new(),
            frn: Vec::with_capacity(1 << 16),
            parent: Vec::new(),
            attrs: Vec::new(),
            base_len: 0,
            delta: Vec::new(),
            delta_frn: HashMap::new(),
            dead_count: 0,
            renames: 0,
            unresolved: HashMap::new(),
            noise_anchors: std::collections::HashSet::new(),
            noise_enabled,
        }
    }

    /// 从快照列构建(已处于 compact 后的有序态)。
    pub fn from_parts(
        names: Vec<u8>,
        offs: Vec<u32>,
        frn: Vec<u64>,
        parent: Vec<u32>,
        attrs: Vec<u16>,
        name_off: Vec<u32>,
        noise_enabled: bool,
    ) -> Self {
        let names_lc = fold_ascii(&names);
        let base_len = name_off.len() as u32;
        let off_index: Vec<(u32, u32)> =
            name_off.iter().enumerate().map(|(i, &o)| (o, i as u32)).collect();
        Self {
            names,
            names_lc,
            name_off,
            off_index,
            offs,
            frn,
            parent,
            attrs,
            base_len,
            delta: Vec::new(),
            delta_frn: HashMap::new(),
            dead_count: 0,
            renames: 0,
            unresolved: HashMap::new(),
            noise_anchors: std::collections::HashSet::new(),
            noise_enabled,
        }
    }

    pub fn len(&self) -> usize {
        self.name_off.len()
    }

    pub fn is_empty(&self) -> bool {
        self.name_off.is_empty()
    }

    /// 是否仍在 scan 模式(未完成首次 compact)。
    pub fn is_scanning(&self) -> bool {
        self.base_len == 0
    }

    pub fn name(&self, slot: u32) -> &[u8] {
        slice_at(&self.names, self.name_off[slot as usize] as usize)
    }

    pub fn name_lc(&self, slot: u32) -> &[u8] {
        slice_at(&self.names_lc, self.name_off[slot as usize] as usize)
    }

    pub fn is_dir(&self, slot: u32) -> bool {
        self.attrs[slot as usize] & 0x10 != 0
    }

    /// 对外可见(未删、非噪声)。
    pub fn visible(&self, slot: u32) -> bool {
        self.attrs[slot as usize] & ATTR_INTERNAL == 0
    }

    /// 存活(未删)。NOISE 槽位对 USN 定位仍然存在(父链解析需要),只是查询不可见。
    fn alive(&self, slot: u32) -> bool {
        self.attrs[slot as usize] & ATTR_DEAD == 0
    }

    /// memmem 命中偏移 → slot。陈项(旧偏移)返回的 slot 与当前 name_off 不符,
    /// 由调用方以「返回 slot 的当前偏移 == 命中所在项起点」校验过滤。
    pub fn slot_at_offset(&self, m: usize) -> Option<u32> {
        let idx = self.off_index.partition_point(|&(o, _)| (o as usize) <= m);
        let (off, slot) = *self.off_index.get(idx.checked_sub(1)?)?;
        // 陈项过滤:条目必须仍指向该 slot 的当前名字起点(就地改名会留下陈项)
        if off as usize != self.name_off[slot as usize] as usize {
            return None;
        }
        if m < off as usize + self.name(slot).len() {
            Some(slot)
        } else {
            None
        }
    }

    /// FRN → slot。scan 模式下无索引(不需要:查询按名字,插入是纯追加)。
    pub fn find_slot(&self, frn: u64) -> Option<u32> {
        if let Some(&s) = self.delta_frn.get(&frn) {
            if self.alive(s) {
                return Some(s);
            }
            return None;
        }
        if self.base_len > 0 {
            if let Ok(i) = self.frn[..self.base_len as usize].binary_search(&frn) {
                let s = i as u32;
                if self.alive(s) {
                    return Some(s);
                }
            }
        }
        None
    }

    /// 追加式插入(scan 模式主路径 / journal upsert)。
    /// 返回 false 表示 FRN 已存在且名字/父都没变(无操作)。
    ///
    /// scan 模式约定(worker 保证):ENUM 流 FRN 互异、目录先行 —— 因此
    /// 父(必为目录)要么已插入(经 dir-only 索引解析),要么登记 unresolved。
    pub fn insert(
        &mut self,
        frn: u64,
        parent_frn: u64,
        attrs: u32,
        name: &[u8],
    ) -> bool {
        let scanning = self.is_scanning();
        if !scanning {
            if let Some(old) = self.find_slot(frn) {
                let same_name = self.name(old) == name;
                let new_p = if parent_frn == 0 {
                    NO_PARENT
                } else {
                    self.find_slot(parent_frn).unwrap_or(NO_PARENT)
                };
                let old_parent = self.parent[old as usize];
                if same_name && old_parent == new_p {
                    // 完全一致:仅同步属性位
                    self.attrs[old as usize] =
                        (self.attrs[old as usize] & ATTR_INTERNAL) | (attrs as u16 & !ATTR_INTERNAL);
                    return false;
                }
                // 就地更新:slot 身份不变 —— 目录改名后子项的父指针仍然有效
                // (NTFS 目录改名不级联发子记录,死槽+新槽会让子项永远指向旧身份)。
                // 名字变化 = 名字区追加新字节 + name_off 重指 + off_index 追加;
                // 旧字节成为幽灵,由 slot_at_offset 的「偏移==当前 name_off」校验过滤。
                if !same_name {
                    self.renames += 1;
                    self.name_off[old as usize] = self.names.len() as u32;
                    self.names.extend_from_slice(name);
                    self.names.push(0);
                    self.names_lc.extend_from_slice(&fold_ascii(name));
                    self.names_lc.push(0);
                    self.off_index.push((self.name_off[old as usize], old));
                }
                self.parent[old as usize] = new_p;
                self.attrs[old as usize] =
                    (self.attrs[old as usize] & ATTR_INTERNAL) | (attrs as u16 & !ATTR_INTERNAL);
                if self.noise_enabled {
                    // 父/名变了,重新判噪(结果可能解除或加深)
                    self.attrs[old as usize] &= !ATTR_NOISE;
                    self.classify_chain(old);
                }
                return true;
            }
        }
        let slot = self.name_off.len() as u32;
        let off = self.names.len() as u32;
        self.name_off.push(off);
        self.off_index.push((off, slot));
        self.names.extend_from_slice(name);
        self.names.push(0);
        self.names_lc.extend_from_slice(&fold_ascii(name));
        self.names_lc.push(0);
        self.frn.push(frn);
        let anchored = self.noise_enabled && self.noise_anchors.contains(&parent_frn);
        let p = if parent_frn == 0 || anchored {
            NO_PARENT
        } else {
            self.find_slot(parent_frn).unwrap_or(NO_PARENT)
        };
        self.parent.push(p);
        self.attrs.push((attrs as u16) & !ATTR_INTERNAL);
        self.delta.push(slot);
        // 索引策略:scan 模式只索引目录(父查找只需要目录,内存省 78%);
        // journal 模式索引全部(改名/删除要按 FRN 定位)。
        if scanning {
            if attrs & 0x10 != 0 {
                self.delta_frn.insert(frn, slot);
            }
        } else {
            self.delta_frn.insert(frn, slot);
        }
        // 立即按父链做噪声分类;父未解析则登记,compact 时重试
        if parent_frn != 0 && !anchored && p == NO_PARENT {
            self.unresolved.insert(slot, parent_frn);
        }
        if self.noise_enabled {
            if anchored {
                self.attrs[slot as usize] |= ATTR_NOISE;
            } else {
                self.classify_chain(slot);
            }
        }
        true
    }

    pub fn remove(&mut self, frn: u64) -> bool {
        self.noise_anchors.remove(&frn);
        if let Some(slot) = self.find_slot(frn) {
            if self.attrs[slot as usize] & ATTR_DEAD == 0 {
                self.attrs[slot as usize] |= ATTR_DEAD;
                self.dead_count += 1;
                self.delta_frn.remove(&frn);
                return true;
            }
        }
        false
    }

    /// 沿父链做噪声分类(可达部分)。父未解析时由 compact 全量重分类兜底。
    fn classify_chain(&mut self, slot: u32) {
        let mut hops = 0;
        let mut cur = slot;
        let mut self_noise = false;
        loop {
            let name_lc = self.name_lc(cur);
            if crate::noise::is_noise_segment(name_lc) {
                self_noise = true;
                break;
            }
            if self.noise_anchors.contains(&self.frn[cur as usize]) {
                self_noise = true;
                break;
            }
            if self.attrs[cur as usize] & ATTR_NOISE != 0 {
                // 祖先已判噪(其父链可能已断),子孙必然同判
                self_noise = true;
                break;
            }
            let p = self.parent[cur as usize];
            if p == NO_PARENT || hops >= 64 {
                break;
            }
            cur = p;
            hops += 1;
        }
        if self_noise {
            self.attrs[slot as usize] |= ATTR_NOISE;
        }
    }

    /// 是否有未归并的 delta。
    pub fn delta_is_empty(&self) -> bool {
        self.delta.is_empty()
    }

    /// 是否需要 compact。
    pub fn needs_compact(&self) -> bool {
        self.delta.len() >= COMPACT_DELTA
            || self.dead_count >= COMPACT_DEAD
            || self.unresolved.len() >= 4096
    }

    pub fn pending_work(&self) -> usize {
        if self.needs_compact() {
            1
        } else {
            0
        }
    }

    /// 全量重建:解析父链、噪声分类、按 FRN 序重建列、归并排序。
    /// scan 模式的首次调用完成「scan → ready」转换。
    /// 无事可做时早退(delta 空、无死项、非 scan),重复调用 O(1)。
    pub fn compact(&mut self) {
        if !self.is_scanning()
            && self.delta.is_empty()
            && self.unresolved.is_empty()
            && self.dead_count == 0
            && self.renames == 0
        {
            return;
        }
        // 0. 重试解析 journal 期未解析的父链,再补一次分类
        if !self.unresolved.is_empty() {
            let pairs: Vec<(u32, u64)> = self.unresolved.iter().map(|(&s, &f)| (s, f)).collect();
            let mut still: HashMap<u32, u64> = HashMap::new();
            for (slot, pfrn) in pairs {
                match self.find_slot(pfrn) {
                    Some(p) => {
                        self.parent[slot as usize] = p;
                        if self.noise_enabled {
                            self.classify_chain(slot);
                        }
                    }
                    None => {
                        still.insert(slot, pfrn);
                    }
                }
            }
            self.unresolved = still;
        }
        // 1. 收集活 slot(live)
        let n = self.name_off.len();
        let mut live: Vec<u32> = Vec::with_capacity(n);
        for s in 0..n as u32 {
            if self.visible(s) {
                live.push(s);
            }
        }
        // 3. 噪声分类(全量重走;insert 时已分类的会重算,结果一致)
        if self.noise_enabled {
            for &s in &live {
                self.classify_chain(s);
            }
            // 记录噪声锚点:将被剔除的 slot 中,父不是「同为噪声被剔除」的顶层者
            let live_set: std::collections::HashSet<u32> = live.iter().copied().collect();
            for s in 0..self.name_off.len() as u32 {
                if self.visible(s) || self.attrs[s as usize] & ATTR_NOISE == 0 {
                    continue;
                }
                let p = self.parent[s as usize];
                let parent_dropped = p != NO_PARENT
                    && !live_set.contains(&p)
                    && self.alive(p)
                    && self.attrs[p as usize] & ATTR_NOISE != 0;
                if !parent_dropped {
                    self.noise_anchors.insert(self.frn[s as usize]);
                }
            }
            // 分类产生的 NOISE 位重新过滤 live
            live.retain(|&s| self.visible(s));
        }
        // 4. 新 id:按 FRN 序分配(此后 frn 列天然有序)
        let mut by_frn: Vec<(u64, u32)> = live.iter().map(|&s| (self.frn[s as usize], s)).collect();
        // base 部分已按 frn 有序,但重排序一次最简单(整型排序 845万 ~0.5s,发生在空闲/完成时)
        by_frn.sort_unstable_by_key(|&(f, _)| f);
        // old slot → new id 用 Vec(NO_PARENT 哨兵),比 HashMap 省 400MB 峰值
        let mut remap: Vec<u32> = vec![u32::MAX; self.name_off.len()];
        for (new_id, &(_, old)) in by_frn.iter().enumerate() {
            remap[old as usize] = new_id as u32;
        }
        // 5. 重建名字区(按新 id 序)
        let mut names = Vec::with_capacity(self.names.len());
        let mut name_off = Vec::with_capacity(by_frn.len());
        for &(_, old) in &by_frn {
            name_off.push(names.len() as u32);
            let start = self.name_off[old as usize] as usize;
            let end = slice_end(&self.names, start);
            names.extend_from_slice(&self.names[start..end]);
            names.push(0);
        }
        let names_lc = fold_ascii(&names);
        // 6. parent 重映射
        let mut parent = Vec::with_capacity(by_frn.len());
        for &(_, old) in &by_frn {
            let p = self.parent[old as usize];
            parent.push(if p == NO_PARENT { NO_PARENT } else { remap[p as usize] });
        }
        // 7. attrs/frn
        let mut attrs = Vec::with_capacity(by_frn.len());
        let mut frn = Vec::with_capacity(by_frn.len());
        for &(_, old) in &by_frn {
            attrs.push(self.attrs[old as usize] & !ATTR_INTERNAL);
            frn.push(self.frn[old as usize]);
        }
        // 8. offs:发生过就地改名时 base 名序失效 → 整体重排;
        //    否则旧有序列过滤重映射 + delta 按名排序后 O(n) 归并
        let mut offs: Vec<u32> = Vec::with_capacity(by_frn.len());
        if self.renames > 0 {
            // live 里是旧 slot id,按旧列名序重排,再映射到新 id
            // (单测里 frn 序恰同旧序掩盖过这里的遗漏,属性测试抓出)
            let mut sorted_old = live.clone();
            sorted_old.sort_unstable_by(|&a, &b| {
                slice_at(&self.names_lc, self.name_off[a as usize] as usize)
                    .cmp(slice_at(&self.names_lc, self.name_off[b as usize] as usize))
            });
            offs = sorted_old
                .into_iter()
                .map(|old| remap[old as usize])
                .collect();
        }
        let mut delta_sorted: Vec<u32> = self
            .delta
            .iter()
            .filter(|&&s| remap[s as usize] != u32::MAX)
            .map(|&s| remap[s as usize])
            .collect();
        delta_sorted.sort_unstable_by(|&a, &b| {
            slice_at(&names_lc, name_off[a as usize] as usize)
                .cmp(slice_at(&names_lc, name_off[b as usize] as usize))
        });
        if self.renames > 0 {
            // 已整体重排
        } else if self.base_len > 0 && !self.offs.is_empty() {
            // 归并两个有序序列
            let base: Vec<u32> = self
                .offs
                .iter()
                .filter(|&&s| remap[s as usize] != u32::MAX)
                .map(|&s| remap[s as usize])
                .collect();
            offs.reserve(base.len() + delta_sorted.len());
            let (mut i, mut j) = (0usize, 0usize);
            while i < base.len() && j < delta_sorted.len() {
                let a = slice_at(&names_lc, name_off[base[i] as usize] as usize);
                let b = slice_at(&names_lc, name_off[delta_sorted[j] as usize] as usize);
                if a <= b {
                    offs.push(base[i]);
                    i += 1;
                } else {
                    offs.push(delta_sorted[j]);
                    j += 1;
                }
            }
            offs.extend_from_slice(&base[i..]);
            offs.extend_from_slice(&delta_sorted[j..]);
        } else {
            offs = delta_sorted;
        }
        // 9. 换血(off_index 重建为新鲜项;幽灵与陈项清零)
        let off_index: Vec<(u32, u32)> =
            name_off.iter().enumerate().map(|(i, &o)| (o, i as u32)).collect();
        self.names = names;
        self.names_lc = names_lc;
        self.name_off = name_off;
        self.off_index = off_index;
        self.offs = offs;
        self.frn = frn;
        self.parent = parent;
        self.attrs = attrs;
        self.delta.clear();
        self.delta.shrink_to_fit();
        self.delta_frn.clear();
        self.delta_frn.shrink_to_fit();
        // unresolved 的 slot id 重映射(被噪声淘汰的条目随 remap 消失)
        self.unresolved = self
            .unresolved
            .iter()
            .filter_map(|(&old, &f)| {
                let new = remap[old as usize];
                (new != u32::MAX).then_some((new, f))
            })
            .collect();
        self.base_len = self.name_off.len() as u32;
        self.dead_count = 0;
        self.renames = 0;
    }
}

fn slice_end(buf: &[u8], off: usize) -> usize {
    off + buf[off..].iter().position(|&b| b == 0).unwrap_or(buf.len() - off)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(s: &mut VolumeStore, frn: u64, parent: u64, attrs: u32, name: &str) {
        s.insert(frn, parent, attrs, name.as_bytes());
    }

    #[test]
    fn insert_find_remove_roundtrip() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "vol-root");
        insert(&mut s, 10, 5, 0x10, "users");
        insert(&mut s, 11, 10, 0, "readme.txt");
        s.compact(); // 新 id 按 FRN 序:5→0, 10→1, 11→2
        assert_eq!(s.find_slot(10), Some(1));
        assert_eq!(s.find_slot(11), Some(2));
        assert!(s.is_dir(1));
        assert!(!s.is_dir(2));
        s.remove(11);
        assert_eq!(s.find_slot(11), None);
        s.compact();
        assert_eq!(s.len(), 2);
        assert_eq!(s.find_slot(11), None);
        assert_eq!(s.find_slot(10), Some(1));
    }

    #[test]
    fn rename_creates_new_slot_and_keeps_order() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0, "alpha.txt");
        insert(&mut s, 11, 5, 0, "beta.txt");
        s.compact();
        let offs_before: Vec<&[u8]> = s.offs.iter().map(|&x| s.name(x)).collect();
        assert_eq!(offs_before, vec![&b"alpha.txt"[..], &b"beta.txt"[..], &b"root"[..]]);
        // 改名 alpha → zeta:就地更新,slot 身份不变(子项父指针持续有效)
        let slot_before = s.find_slot(10).unwrap();
        s.insert(10, 5, 0, b"zeta.txt");
        assert_eq!(s.find_slot(10), Some(slot_before));
        assert_eq!(s.name(slot_before), b"zeta.txt");
        s.compact();
        let offs_after: Vec<&[u8]> = s.offs.iter().map(|&x| s.name(x)).collect();
        assert_eq!(offs_after, vec![&b"beta.txt"[..], &b"root"[..], &b"zeta.txt"[..]]);
        assert_eq!(s.find_slot(10), Some(slot_before));
        assert_eq!(s.name(s.find_slot(10).unwrap()), b"zeta.txt");
    }

    #[test]
    fn in_place_rename_keeps_slot_identity_and_offindex_sorted() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 1, 0, 0, "aaa");
        insert(&mut s, 2, 0, 0, "bbb");
        s.compact();
        s.insert(2, 0, 0, b"ccc"); // 就地改名
        s.insert(3, 0, 0, b"ddd");
        // off_index 始终按偏移升序(追加保证)
        for i in 1..s.off_index.len() {
            assert!(s.off_index[i].0 > s.off_index[i - 1].0, "off_index order broken at {i}");
        }
        // 改名后的 slot:当前 name_off 指向新字节,旧字节成幽灵
        let slot = s.find_slot(2).unwrap();
        assert_eq!(s.name(slot), b"ccc");
        assert_eq!(s.slot_at_offset(s.name_off[slot as usize] as usize), Some(slot));
        s.compact();
        // compact 后幽灵清零:off_index 与 name_off 一一对应
        assert_eq!(s.off_index.len(), s.name_off.len());
        for (i, &(off, slot)) in s.off_index.iter().enumerate() {
            assert_eq!(off, s.name_off[i]);
            assert_eq!(slot as usize, i);
        }
        let names: Vec<&[u8]> = s.offs.iter().map(|&x| s.name(x)).collect();
        assert_eq!(names, vec![&b"aaa"[..], &b"ccc"[..], &b"ddd"[..]]);
    }

    #[test]
    fn parent_resolution_across_compact() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0x10, "dir");
        insert(&mut s, 11, 10, 0, "file.txt");
        s.compact();
        let dir_slot = s.find_slot(10).unwrap();
        let file_slot = s.find_slot(11).unwrap();
        assert_eq!(s.parent[file_slot as usize], dir_slot);
        // journal 模式再插一个,父链照样解析
        insert(&mut s, 12, 10, 0, "file2.txt");
        let f2 = s.find_slot(12).unwrap();
        assert_eq!(s.parent[f2 as usize], dir_slot);
    }

    #[test]
    fn noise_excluded_subtree_dropped_at_compact() {
        let mut s = VolumeStore::new(true);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0x10, "node_modules");
        insert(&mut s, 11, 10, 0, "dep.txt");
        insert(&mut s, 12, 5, 0, "keep.txt");
        s.compact();
        assert_eq!(s.find_slot(10), None);
        assert_eq!(s.find_slot(11), None);
        assert_eq!(s.find_slot(12), Some(s.find_slot(12).unwrap()));
        assert_eq!(s.len(), 2); // root + keep.txt
    }

    #[test]
    fn scan_mode_substring_then_prefix_consistent() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "prog");
        insert(&mut s, 10, 5, 0, "myprog.txt");
        insert(&mut s, 11, 5, 0, "Program Files"); // 大小写折叠命中 "prog"
        insert(&mut s, 12, 5, 0, "other.txt");
        // scan 模式:query 层走 memmem;这里只验证 name_lc 折叠正确
        assert_eq!(s.name_lc(2), b"program files");
        s.compact();
        assert_eq!(s.offs.len(), 4);
        // 有序性:sorted by lowercase name
        let names: Vec<&[u8]> = s.offs.iter().map(|&x| s.name_lc(x)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }
}
