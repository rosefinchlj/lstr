//! 卷内列式存储。
//!
//! 设计要点(对应 docs/index-engine-plan.md §4.2):
//! - 名字区 UTF-8 精确副本 + ASCII 折叠副本,**等长共享偏移**(与 NTFS 大小写
//!   语义一致;非 ASCII 不折叠——中文无大小写)。
//! - 名字以 `\0` 结尾打包:子串匹配天然不会跨条目(needle 不含 \0)。
//! - slot 即列下标,append-only;改名/删除=旧 slot 置 DEAD 位+新 slot,
//!   因此 name_off 恒升序,「命中偏移→slot」可二分。
//! - scan 模式(scan_mode=true):parent 列暂存父 FRN(u64),一切查询走打包
//!   buffer 扫描;volume 完成后 compact():解析父 slot、噪声分类、列重建。
//! - journal 模式(scan_mode=false):frn 有序数组二分 + delta HashMap 定位;
//!   delta 攒批,超阈值或空闲时 compact 归并(保留既有排序,只做 O(n) 归并)。
//!   scan_mode 与 base_len 解耦:空卷 compact 后 base_len 仍为 0,但已 ready。

use std::collections::HashMap;

use crate::usn::Rec;

/// 内部位:已删除(等待 compact 回收)。
pub const ATTR_DEAD: u16 = 0x8000;
/// 内部位:命中噪声排除规则(等待 compact 回收)。
pub const ATTR_NOISE: u16 = 0x4000;
/// 内部位掩码:对外不可见的 slot。
pub const ATTR_INTERNAL: u16 = ATTR_DEAD | ATTR_NOISE;
/// 对账扫描的「本次见过」临时位(仅内存,不落盘)。DirSync 结束时未被标记的
/// 存活槽就是离线期间消失的条目,标 DEAD;这与 DEAD/NOISE 不同,不参与
/// [`ATTR_INTERNAL`] 的可见性过滤,也不会被持久化(落盘前会清掉)。
pub const ATTR_SEEN: u16 = 0x2000;
/// 卷根子项标记(持久化)。根记录被过滤器丢弃,顶层条目的 parent 只能记为
/// [`NO_PARENT`];watcher 刷新卷根时需要能把「根子项」与「父未消解」区分开,
/// 否则根目录删除无法实时处理(会被当成未消解条目而不敢删)。
pub const ATTR_ROOT_CHILD: u16 = 0x1000;
/// parent 列的「卷根/未解析」标记。
pub const NO_PARENT: u32 = u32::MAX;
/// NTFS 卷根目录的固定 FRN。根记录本身会被 USN 过滤器丢弃,但顶层条目的
/// parent 就是它;若按普通父项去 find_slot,每条顶层条目都会进 unresolved,
/// 而 unresolved>=4096 又让 needs_compact 恒真 —— 实测每轮 maintain 全量
/// compact 250 万条,CPU 单核打满、查询被饿死。根目录必须当作 NO_PARENT。
pub const ROOT_FRN: u64 = 5;

/// compact 触发阈值:delta 新 slot 数(同样用作 reindex overlay 阈值)。
pub(crate) const COMPACT_DELTA: usize = 32_768;
/// compact 触发阈值:死 slot 占比(分子)。
const COMPACT_DEAD: u32 = 4096;

/// 小卷跳过 n-gram 倒排的阈值(名字区字节数)。trigram 平坦表固定
/// 94³×8B≈6.6MB/卷,小卷(如 1.7 万条)的倒排几乎全是这个固定表开销;
/// 而小卷名字区用 memmem SIMD 扫描本就极快(<8MB 约 <3ms),不值得为它
/// 常驻 6.6MB 表。低于阈值不建倒排,查询回落 memmem(等价性有测试保证)。
/// 测试构建恒为 0:始终建倒排,保证「快路径≡memmem」等价性测试覆盖 ngram。
#[cfg(not(test))]
const NGRAM_MIN_BYTES: usize = 8 * 1024 * 1024;
#[cfg(test)]
const NGRAM_MIN_BYTES: usize = 0;

/// n-gram 字母表大小:可打印非空格 ASCII(0x21..=0x7E)。
/// 空格与非 ASCII 视为分隔符(切分 run):全字母表 token 的子串出现必然
/// 完整落在某个 run 内,因此按 run 提取 n-gram 对这类 token 精确
/// (≤3 字符零假阳性/零假阴性;含字母表外字节的 token 走 memmem 兜底)。
const NG_ALPHA: usize = 94;
/// bigram 链数 = 94²。
const NG_BIGRAM: usize = NG_ALPHA * NG_ALPHA;
/// trigram 链数 = 94³(平坦表,约 6.6MB,换 O(1) 查找)。
const NG_TRIGRAM: usize = NG_ALPHA * NG_ALPHA * NG_ALPHA;

#[inline]
fn ng_alpha(b: u8) -> Option<u32> {
    // then_some 是急切求值,b-0x21 会先算;必须用 then 保持惰性
    (b >= 0x21 && b <= 0x7E).then(|| (b - 0x21) as u32)
}

#[inline]
fn varint_len(mut v: u32) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// 一条倒排链在 postings 缓冲里的起点与命中 slot 数(字节终点可由
/// 「解码 len 条」推出,不冗余存储)。
#[derive(Clone, Copy)]
struct NgList {
    start: u32,
    len: u32,
}

/// 倒排链解码迭代器:varint(delta) 累加还原 slot 升序序列,按条数终止。
struct NgDecode<'a> {
    buf: &'a [u8],
    pos: usize,
    remaining: u32,
    acc: u32,
}

impl Iterator for NgDecode<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let b = self.buf[self.pos];
            self.pos += 1;
            v |= ((b & 0x7F) as u32) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        self.acc = self.acc.wrapping_add(v);
        Some(self.acc)
    }
}

/// n-gram 倒排索引(加速 ≥2 字符子串查询,替代 memmem 全量扫描)。
///
/// 构建时机 = compact/from_parts(列一致有序态);覆盖 slot 0..base_len,
/// 之后新增/改名的 slot 由 store.reindex overlay 承载(查询时 memmem 验证)。
/// 链内 slot 升序(compact 后 slot 序 ≡ name_off 序),delta varint 编码。
/// bigram/trigram 键空间独立,各用一个 postings 缓冲(构建期两个回填闭包
/// 各借各的缓冲,免共享可变借用)。
pub struct NgramIndex {
    /// bigram 倒排链字节(varint delta;链首项 delta = slot 本身)。
    postings_b: Vec<u8>,
    /// trigram 倒排链字节。
    postings_t: Vec<u8>,
    /// [c1*94+c2] → 链。
    bigram: Vec<NgList>,
    /// [(c1*94+c2)*94+c3] → 链。
    trigram: Vec<NgList>,
}

impl NgramIndex {
    /// 两遍构建:先计数+精确测编码长度(决定 postings 容量,避免倍增虚胖),
    /// 再按每链游标回填。stamp 存「最近一次命中 slot+1」,同一条目内去重,
    /// 0 值同时表示「本条目尚未写过该 n-gram」(delta 前驱 = stamp-1)。
    ///
    /// 注意:不过滤 DEAD/NOISE 槽(未 compact 的快照可能带内部位,死槽原地
    /// 复活后仍需可查);查询端以 attrs 过滤兜底,多余的 posting 可忽略。
    pub fn build(names_lc: &[u8], name_off: &[u32]) -> Self {
        let n = name_off.len();
        let mut stamp_b = vec![0u32; NG_BIGRAM];
        let mut stamp_t = vec![0u32; NG_TRIGRAM];
        let mut len_b = vec![0u32; NG_BIGRAM];
        let mut len_t = vec![0u32; NG_TRIGRAM];
        let mut size_b = vec![0u32; NG_BIGRAM];
        let mut size_t = vec![0u32; NG_TRIGRAM];
        // pass 1:每链命中数 + varint 字节数(两遍同序遍历,链内 slot 升序)
        for slot in 0..n as u32 {
            let name = slice_at(names_lc, name_off[slot as usize] as usize);
            for_each_ngram(name, &mut |k| {
                if stamp_b[k] != slot + 1 {
                    size_b[k] += varint_len(ng_delta(stamp_b[k], slot)) as u32;
                    stamp_b[k] = slot + 1;
                    len_b[k] += 1;
                }
            }, &mut |k| {
                if stamp_t[k] != slot + 1 {
                    size_t[k] += varint_len(ng_delta(stamp_t[k], slot)) as u32;
                    stamp_t[k] = slot + 1;
                    len_t[k] += 1;
                }
            });
        }
        // 前缀和 → 链起点;游标 = 起点副本(pass 2 回填用)
        let mut off_b = Vec::with_capacity(NG_BIGRAM + 1);
        let mut off_t = Vec::with_capacity(NG_TRIGRAM + 1);
        let mut total = 0u32;
        off_b.push(0);
        for &s in &size_b {
            total += s;
            off_b.push(total);
        }
        let mut total = 0u32;
        off_t.push(0);
        for &s in &size_t {
            total += s;
            off_t.push(total);
        }
        let mut postings_b = vec![0u8; off_b[NG_BIGRAM] as usize];
        let mut cur_b = off_b[..NG_BIGRAM].to_vec();
        let total_t = off_t[NG_TRIGRAM];
        let mut postings_t = vec![0u8; total_t as usize];
        let mut cur_t = off_t[..NG_TRIGRAM].to_vec();
        // pass 2:重提取并按链游标回填(stamp 重置:0 = 本条目未写过)
        stamp_b.fill(0);
        stamp_t.fill(0);
        for slot in 0..n as u32 {
            let name = slice_at(names_lc, name_off[slot as usize] as usize);
            for_each_ngram(name, &mut |k| {
                if stamp_b[k] != slot + 1 {
                    let delta = ng_delta(stamp_b[k], slot);
                    cur_b[k] += ng_write(&mut postings_b, cur_b[k] as usize, delta) as u32;
                    stamp_b[k] = slot + 1;
                }
            }, &mut |k| {
                if stamp_t[k] != slot + 1 {
                    let delta = ng_delta(stamp_t[k], slot);
                    cur_t[k] += ng_write(&mut postings_t, cur_t[k] as usize, delta) as u32;
                    stamp_t[k] = slot + 1;
                }
            });
        }
        let bigram = (0..NG_BIGRAM)
            .map(|k| NgList { start: off_b[k], len: len_b[k] })
            .collect();
        let trigram = (0..NG_TRIGRAM)
            .map(|k| NgList { start: off_t[k], len: len_t[k] })
            .collect();
        Self { postings_b, postings_t, bigram, trigram }
    }

    /// 解码一条 bigram 链(升序 slot)追加到 out。
    fn decode_b(&self, r: NgList, out: &mut Vec<u32>) {
        out.extend(NgDecode {
            buf: &self.postings_b,
            pos: r.start as usize,
            remaining: r.len,
            acc: 0,
        });
    }

    /// 解码一条 trigram 链(升序 slot)追加到 out。
    fn decode_t(&self, r: NgList, out: &mut Vec<u32>) {
        out.extend(NgDecode {
            buf: &self.postings_t,
            pos: r.start as usize,
            remaining: r.len,
            acc: 0,
        });
    }

    /// 把 trigram 链 r 与升序 acc 归并求交(acc 原地收缩)。
    fn intersect_t(&self, r: NgList, acc: &mut Vec<u32>) {
        let mut merged = Vec::with_capacity(acc.len().min(r.len as usize));
        let mut it = NgDecode {
            buf: &self.postings_t,
            pos: r.start as usize,
            remaining: r.len,
            acc: 0,
        };
        let mut next = it.next();
        let mut i = 0;
        while i < acc.len() {
            let a = acc[i];
            while let Some(x) = next {
                if x < a {
                    next = it.next();
                } else {
                    break;
                }
            }
            match next {
                Some(x) if x == a => {
                    merged.push(a);
                    next = it.next();
                    i += 1;
                }
                Some(_) => i += 1,
                None => break,
            }
        }
        *acc = merged;
    }

    /// 诊断/内存预算:postings + 两张链表字节数。
    pub fn heap_bytes(&self) -> usize {
        self.postings_b.len()
            + self.postings_t.len()
            + self.bigram.len() * std::mem::size_of::<NgList>()
            + self.trigram.len() * std::mem::size_of::<NgList>()
    }
}

/// 链内 delta:首项为 slot 本身,其余为与前一命中 slot 的差(≥1)。
#[inline]
fn ng_delta(stamp: u32, slot: u32) -> u32 {
    if stamp == 0 {
        slot
    } else {
        slot - (stamp - 1)
    }
}

/// 在 buf[p..] 原地写 varint,返回写入字节数。
#[inline]
fn ng_write(buf: &mut [u8], p: usize, mut v: u32) -> usize {
    let mut n = 0;
    while v >= 0x80 {
        buf[p + n] = v as u8 | 0x80;
        v >>= 7;
        n += 1;
    }
    buf[p + n] = v as u8;
    n + 1
}

/// trigram 平坦表键(调用方保证字节在字母表内)。
#[inline]
fn ng_trigram_key(c1: u8, c2: u8, c3: u8) -> usize {
    ((ng_alpha(c1).unwrap() * NG_ALPHA as u32 + ng_alpha(c2).unwrap()) * NG_ALPHA as u32
        + ng_alpha(c3).unwrap()) as usize
}

/// 遍历名字里全部 bigram/trigram 键(按 run 切分;分隔符重置 run)。
/// `b` 收 bigram 键 c1*94+c2,`t` 收 trigram 键 (c1*94+c2)*94+c3。
#[inline]
fn for_each_ngram(name: &[u8], b: &mut impl FnMut(usize), t: &mut impl FnMut(usize)) {
    let (mut p1, mut p2) = (None::<u32>, None::<u32>);
    for &byte in name {
        match ng_alpha(byte) {
            None => {
                p1 = None;
                p2 = None;
            }
            Some(c) => {
                if let Some(x) = p2 {
                    b((x * NG_ALPHA as u32 + c) as usize);
                }
                if let (Some(x), Some(y)) = (p1, p2) {
                    t(((x * NG_ALPHA as u32 + y) * NG_ALPHA as u32 + c) as usize);
                }
                p1 = p2;
                p2 = Some(c);
            }
        }
    }
}

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
    /// slot → 最后修改时间(unix 秒;0=未知)。目录枚举(首扫/watcher 刷新)写入,
    /// USN journal 路径没有时间信息(保持 0)。内置时间筛选依赖这一列。
    pub mtime: Vec<u32>,
    /// mtime 仍为 0 的 slot 队列(USN 增量插入/恢复名时登记)。引擎空闲时
    /// 限量探测真实 mtime 回填(见 `Engine::maintain`),查询路径不做 FS IO。
    /// compact/快照加载后由 [`Self::rebuild_mtime_pending`] 重建。
    pub mtime_pending: Vec<u32>,
    /// slot → 小写名里出现的 ASCII 字母位图(bit0=a .. bit25=z)。
    /// 单字符查询用它把 memmem 全量扫描降为按位快速筛(短串是打字延迟的主因)。
    pub char_mask: Vec<u32>,
    /// (缓冲偏移, slot) 升序追加;memmem 命中 → slot 的映射。
    /// 就地改名后旧条目成为陈项,由 slot_at_offset 的「偏移==当前 name_off」校验过滤。
    off_index: Vec<(u32, u32)>,
    /// compact 后的 slot 数(= offs.len());之后的新 slot 全在 delta。
    base_len: u32,
    /// 是否仍在 scan 模式(未完成首次 compact)。与 `base_len` 解耦:空卷
    /// compact 后 `base_len` 仍为 0,但 scan 模式必须结束(否则空卷上新建
    /// 的文件永不进 reindex overlay,≥2 字符查询被空倒排静默归零)。
    scan_mode: bool,
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
    /// n-gram 倒排(覆盖 0..ngram_base_len;scan 模式为 None,查询退 memmem)。
    ngram: Option<NgramIndex>,
    /// 倒排构建时的 slot 总数;之后的追加/改名进 reindex overlay。
    ngram_base_len: u32,
    /// 倒排构建后的新增 slot 与改名的 base slot(查询时逐个 memmem 验证)。
    /// 追加按 slot 升序进入;base 改名以 reindex_mark 去重。
    reindex: Vec<u32>,
    /// base slot 改名去重位图(slot < ngram_base_len 才置位)。
    reindex_mark: Vec<u64>,
    /// 父 slot → 子 slot 列表(惰性索引,仅 watch 模式用)。
    /// [`Self::apply_children`] 首次调用时才构建([`Self::rebuild_children`]),
    /// 之后随 insert/compact 精确维护。USN 模式不调 apply_children → 永不构建,
    /// 零内存/零维护开销;watch 模式借此把目录刷新从 O(全表) 降为 O(子项数)。
    children: HashMap<u32, Vec<u32>>,
    /// children 索引是否已构建(惰性门闸)。
    children_built: bool,
}

/// 名字里出现的 ASCII 字母位图(bit0=a .. bit25=z);非字母字节忽略。
pub(crate) fn mask_of_bytes(name: &[u8]) -> u32 {
    let mut m = 0u32;
    for &b in name {
        let c = b.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            m |= 1u32 << (c - b'a');
        }
    }
    m
}

/// 大小写折叠(大小写不敏感匹配用):ASCII 字母转小写;非 ASCII 做 Unicode
/// 「简单折叠」——仅当 char::to_lowercase 恰好产出单个字符且 UTF-8 字节长与
/// 原字符相同时才替换(Ä→ä、全角 Ａ→ａ、西里尔/希腊字母等),否则保留原字节。
/// 折叠前后**字节长度不变**是硬约束(names 与 names_lc 共享 name_off 偏移);
/// 长度会变的折叠(ß→ss、İ→i̇)不做。旧 fold_ascii 只折 ASCII,Ä/ä 不互配,
/// 与 rank/pin 的 Unicode lowercase 跨层不一致(A-6),这里补齐常见等长情形。
pub fn fold_case(bytes: &[u8]) -> Vec<u8> {
    // 快路径:纯 ASCII 逐字节折叠(绝大多数名字),与旧行为逐字节一致。
    if bytes.is_ascii() {
        return bytes
            .iter()
            .map(|&b| if b.is_ascii_uppercase() { b + 32 } else { b })
            .collect();
    }
    let Ok(s) = std::str::from_utf8(bytes) else {
        return bytes.to_vec(); // 非法 UTF-8 原样(名字区理论上均合法)
    };
    let mut out = String::with_capacity(bytes.len());
    for ch in s.chars() {
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase());
            continue;
        }
        let mut lower = ch.to_lowercase();
        match lower.next() {
            Some(lc) if lower.next().is_none() && lc.len_utf8() == ch.len_utf8() => out.push(lc),
            _ => out.push(ch),
        }
    }
    out.into_bytes()
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
            mtime: Vec::new(),
            mtime_pending: Vec::new(),
            char_mask: Vec::new(),
            base_len: 0,
            scan_mode: true,
            delta: Vec::new(),
            delta_frn: HashMap::new(),
            dead_count: 0,
            renames: 0,
            unresolved: HashMap::new(),
            noise_anchors: std::collections::HashSet::new(),
            noise_enabled,
            ngram: None,
            ngram_base_len: 0,
            reindex: Vec::new(),
            reindex_mark: Vec::new(),
            children: HashMap::new(),
            children_built: false,
        }
    }

    /// 从快照列构建(已处于 compact 后的有序态)。`unresolved` = 保存时
    /// 仍未消解的 (slot, parent_frn):父项要到后续 journal 事件补进来才能修复路径,
    /// 不落盘会让这些条目在重启后永久退化为 NO_PARENT(路径只剩文件名)。
    pub fn from_parts(
        names: Vec<u8>,
        offs: Vec<u32>,
        frn: Vec<u64>,
        parent: Vec<u32>,
        attrs: Vec<u16>,
        mtime: Vec<u32>,
        name_off: Vec<u32>,
        unresolved: Vec<(u32, u64)>,
        noise_enabled: bool,
    ) -> Self {
        let names_lc = fold_case(&names);
        let base_len = name_off.len() as u32;
        let off_index: Vec<(u32, u32)> =
            name_off.iter().enumerate().map(|(i, &o)| (o, i as u32)).collect();
        let char_mask: Vec<u32> = name_off
            .iter()
            .map(|&o| mask_of_bytes(slice_at(&names_lc, o as usize)))
            .collect();
        let mut s = Self {
            names,
            names_lc,
            name_off,
            off_index,
            offs,
            frn,
            parent,
            attrs,
            mtime,
            mtime_pending: Vec::new(),
            char_mask,
            base_len,
            scan_mode: false,
            delta: Vec::new(),
            delta_frn: HashMap::new(),
            dead_count: 0,
            renames: 0,
            // 旧快照里根父条目也被登记过(旧实现),加载时直接丢弃:卷根就是 NO_PARENT,
        // 没有可解析的目标,留着只会白占内存。
        unresolved: unresolved
            .into_iter()
            .filter(|&(_, parent_frn)| parent_frn != 0 && parent_frn != ROOT_FRN)
            .collect(),
            noise_anchors: std::collections::HashSet::new(),
            noise_enabled,
            ngram: None,
            ngram_base_len: 0,
            reindex: Vec::new(),
            reindex_mark: Vec::new(),
            children: HashMap::new(),
            children_built: false,
        };
        // 加载即建倒排:快照格式不变(不持久化倒排),重建耗时有界(实测见
        // bench_wide_query_phase_profile),避免版本升级触发全量重扫。
        s.rebuild_ngram();
        // 快照里的 mtime=0 条目(USN 增量写入的)同样需要回填:重建待探测
        // 队列,否则重启后这些条目仍永久过不了时间筛选。
        s.rebuild_mtime_pending();
        s
    }

    pub fn len(&self) -> usize {
        self.name_off.len()
    }

    /// 登记「mtime 未知」的 slot(USN 插入/恢复名)。查询路径不做 FS IO,
    /// 真实 mtime 由引擎空闲维护回填。
    #[inline]
    fn note_mtime_pending(&mut self, slot: u32, mtime: u32, name: &[u8]) {
        if mtime == 0 && !name.is_empty() {
            self.mtime_pending.push(slot);
        }
    }

    /// 按当前列重建待回填队列(O(n) 一次)。compact 重排 slot、快照加载
    /// (from_parts)后必须调用:旧 slot 编号在重排后失效,只能按 mtime 列重扫。
    /// 重复登记无害:引擎 drain 时会因 mtime 已非 0 而丢弃。
    pub fn rebuild_mtime_pending(&mut self) {
        let pending: Vec<u32> = (0..self.name_off.len() as u32)
            .filter(|&slot| self.mtime[slot as usize] == 0 && !self.name(slot).is_empty())
            .collect();
        self.mtime_pending = pending;
    }

    /// 取走最多 `max` 个待回填 slot(从尾部切,避免大规模前缀 drain 的
    /// memmove;处理顺序无关)。未取走的留在队列里,下一轮继续。
    pub fn take_mtime_pending(&mut self, max: usize) -> Vec<u32> {
        let n = max.min(self.mtime_pending.len());
        if n == 0 {
            return Vec::new();
        }
        self.mtime_pending.split_off(self.mtime_pending.len() - n)
    }

    /// compact 后已归并的 slot 数。快照按「base(FRN 序)+ delta(FRN 排序后归并)」
    /// 流式重建列,保存期间不 compact(见 snapshot 的分段保存)。
    pub fn base_len(&self) -> u32 {
        self.base_len
    }

    /// 自上次 compact 以来就地改名过的 base slot(reindex 里小于 base_len 的项)。
    /// 这些 slot 在 `offs` 里的位置已失效,快照视图需把它们移出 base 序列,
    /// 与 delta/复活槽一起按当前名重排后归并。
    pub fn renamed_base_slots(&self) -> Vec<u32> {
        self.reindex
            .iter()
            .copied()
            .filter(|&s| s < self.base_len)
            .collect()
    }

    /// 各列堆字节数(诊断/内存预算断言用)。不含未收缩的 Vec 容量余量。
    pub fn heap_bytes(&self) -> usize {
        let v = |x: &Vec<u8>| x.len();
        v(&self.names)
            + v(&self.names_lc)
            + self.name_off.len() * 4
            + self.off_index.len() * 8
            + self.offs.len() * 4
            + self.frn.len() * 8
            + self.parent.len() * 4
            + self.attrs.len() * 2
            + self.mtime.len() * 4
            + self.mtime_pending.len() * 4
            + self.char_mask.len() * 4
            + self.ngram_heap_bytes()
    }

    /// 内存分项诊断(MB):逐列列出占用,定位最肥的列供精准瘦身。
    /// 不含 Vec 容量余量(与 heap_bytes 同口径)。
    pub fn memory_breakdown_mb(&self) -> String {
        let mb = |n: usize| n as f64 / (1024.0 * 1024.0);
        format!(
            "names={:.1} names_lc={:.1} name_off={:.1} off_index={:.1} offs={:.1} frn={:.1} parent={:.1} attrs={:.1} mtime={:.1} char_mask={:.1} mtime_pending={:.1} ngram={:.1}",
            mb(self.names.len()),
            mb(self.names_lc.len()),
            mb(self.name_off.len() * 4),
            mb(self.off_index.len() * 8),
            mb(self.offs.len() * 4),
            mb(self.frn.len() * 8),
            mb(self.parent.len() * 4),
            mb(self.attrs.len() * 2),
            mb(self.mtime.len() * 4),
            mb(self.char_mask.len() * 4),
            mb(self.mtime_pending.len() * 4),
            mb(self.ngram_heap_bytes()),
        )
    }

    /// 倒排堆字节:postings + 链表 + overlay。
    pub fn ngram_heap_bytes(&self) -> usize {
        self.ngram.as_ref().map_or(0, NgramIndex::heap_bytes)
            + self.reindex.len() * 4
            + self.reindex_mark.len() * 8
    }

    /// 重建倒排(compact/from_parts 尾部调用;列处于一致有序态)。
    /// pub(crate):bench 测构建耗时用。
    ///
    /// 空名字区不建索引(保持 None):空卷 compact 后新建的文件没有 base
    /// 倒排可用,查询必须回落 memmem;若给空 store 造 `Some(空倒排)`,
    /// 2/3 字符链命中集恒空且永不走兜底,查询会静默归零。
    /// 先释放旧索引再构建,避免新旧两份 postings 同时驻留(峰值内存翻倍)。
    pub(crate) fn rebuild_ngram(&mut self) {
        self.ngram_base_len = self.name_off.len() as u32;
        self.reindex.clear();
        self.reindex_mark.clear();
        self.reindex_mark.resize((self.name_off.len() + 63) / 64, 0);
        self.ngram = None;
        if self.name_off.is_empty() {
            return;
        }
        // 小卷不建倒排:省 trigram 固定表(~6.6MB/卷),memmem 扫描已足够快。
        if self.names_lc.len() < NGRAM_MIN_BYTES {
            return;
        }
        self.ngram = Some(NgramIndex::build(&self.names_lc, &self.name_off));
    }

    #[inline]
    fn reindex_marked(&self, slot: u32) -> bool {
        self.reindex_mark
            .get((slot / 64) as usize)
            .is_some_and(|&w| w & (1u64 << (slot % 64)) != 0)
    }

    #[inline]
    fn mark_reindexed(&mut self, slot: u32) {
        if let Some(w) = self.reindex_mark.get_mut((slot / 64) as usize) {
            *w |= 1u64 << (slot % 64);
        }
    }

    /// n-gram 快路径候选:gen(已小写折叠)全部字节在字母表内且有索引时,
    /// 返回升序候选 slot(链内 slot 序 ≡ name_off 序 ≡ memmem 命中序)。
    /// None = 无可用倒排(scan 模式/空 store/倒排未建)/token 不可索引
    /// → 调用方走 memmem 兜底。
    ///
    /// - 2/3 字符:单链即精确命中集(子串出现 ⇔ n-gram 在链上);
    /// - ≥4 字符:全部连续 trigram 链求交(最短链起步)= 超集,调用方逐个
    ///   memmem 验证。
    pub(crate) fn ngram_candidates(&self, gen: &[u8]) -> Option<Vec<u32>> {
        // 三层防御:空 store / scan 模式 / 倒排缺失任一都不得走倒排路径 ——
        // 空倒排的 2/3 字符链返回 `Some(vec![])`,会让查询静默归零且永不
        // 回落 memmem(空卷首扫完成后的新建文件因此查不到)。
        if self.name_off.is_empty() || self.is_scanning() || self.ngram.is_none() {
            return None;
        }
        let idx = self.ngram.as_ref()?;
        if gen.len() < 2 || gen.iter().any(|&b| ng_alpha(b).is_none()) {
            return None;
        }
        let mut out: Vec<u32> = Vec::new();
        if gen.len() == 2 {
            let k = (ng_alpha(gen[0]).unwrap() * NG_ALPHA as u32 + ng_alpha(gen[1]).unwrap()) as usize;
            idx.decode_b(idx.bigram[k], &mut out);
        } else if gen.len() == 3 {
            let k = ng_trigram_key(gen[0], gen[1], gen[2]);
            idx.decode_t(idx.trigram[k], &mut out);
        } else {
            let mut refs: Vec<NgList> = gen
                .windows(3)
                .map(|w| idx.trigram[ng_trigram_key(w[0], w[1], w[2])])
                .collect();
            refs.sort_unstable_by_key(|r| r.len);
            idx.decode_t(refs[0], &mut out);
            for &r in &refs[1..] {
                if out.is_empty() {
                    break;
                }
                idx.intersect_t(r, &mut out);
            }
        }
        Some(out)
    }

    /// overlay:倒排构建后的新增/改名 slot(查询端逐个 memmem 验证)。
    pub(crate) fn reindex_slots(&self) -> &[u32] {
        &self.reindex
    }

    /// base 倒排对该 slot 是否已失效(改名后旧名 posting 是假阳性,须跳过,
    /// 由 overlay 用新名覆盖)。
    #[inline]
    pub(crate) fn base_posting_stale(&self, slot: u32) -> bool {
        self.reindex_marked(slot)
    }

    pub fn is_empty(&self) -> bool {
        self.name_off.is_empty()
    }

    /// 是否仍在 scan 模式(未完成首次 compact)。
    /// 读显式 `scan_mode`,不看 `base_len`:空卷 compact 后 base_len 仍为 0,
    /// 若以 `base_len == 0` 判定,空卷会永远停留在 scanning。
    pub fn is_scanning(&self) -> bool {
        self.scan_mode
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

    /// 同 [`Self::find_slot`],但允许命中 DEAD slot(用于 FRN 复用时的原地复活)。
    fn find_slot_any(&self, frn: u64) -> Option<u32> {
        if let Some(&s) = self.delta_frn.get(&frn) {
            return Some(s);
        }
        if self.base_len > 0 {
            if let Ok(i) = self.frn[..self.base_len as usize].binary_search(&frn) {
                return Some(i as u32);
            }
        }
        None
    }

    /// 后台对账:标记「本轮扫描见到过」的槽位。仅内存位,不落盘。
    pub fn mark_seen(&mut self, frn: u64) {
        if let Some(slot) = self.find_slot(frn) {
            self.attrs[slot as usize] |= ATTR_SEEN;
        }
    }

    /// 后台对账中途中止:只清掉 SEEN 临时位,不删除任何条目。
    pub fn clear_seen(&mut self) {
        for a in self.attrs.iter_mut() {
            *a &= !ATTR_SEEN;
        }
    }

    /// 构建/重建 parent→children 索引(惰性:首次 apply_children 或 compact 后)。
    /// 遍历 parent 列把每个非根 slot 挂到其父下;含 DEAD slot(使用端过滤),
    /// compact 后 parent 列仅存活槽,重建即精确。
    fn rebuild_children(&mut self) {
        self.children.clear();
        for slot in 0..self.parent.len() as u32 {
            let p = self.parent[slot as usize];
            if p != NO_PARENT {
                self.children.entry(p).or_default().push(slot);
            }
        }
        self.children_built = true;
    }

    /// slot 的父从 old_p 变为 new_p 时精确移动 children 索引(仅已构建时调用)。
    /// 从旧父列表移除、挂到新父下,保证索引无重复/无陈旧。
    fn move_child(&mut self, slot: u32, old_p: u32, new_p: u32) {
        if old_p == new_p {
            return;
        }
        if old_p != NO_PARENT {
            if let Some(v) = self.children.get_mut(&old_p) {
                v.retain(|&s| s != slot);
            }
        }
        if new_p != NO_PARENT {
            self.children.entry(new_p).or_default().push(slot);
        }
    }

    /// watcher 目录刷新:用 `recs` 替换 `parent_slot` 下的子项集合。
    /// 返回 (新增/更新数, 标死数)。
    ///
    /// 只有父 slot 可解析时才做删除;`NO_PARENT`(卷根,或父项尚未消解)
    /// 无法与其它未消解条目区分,只做 upsert —— 卷根删除交给下次启动对账。
    ///
    /// 非根刷新走 children 惰性索引(O(子项数) 而非 O(全表));首次调用
    /// 构建索引,之后由 insert_meta/compact 精确维护。
    pub fn apply_children(&mut self, parent_slot: u32, recs: &[Rec]) -> (usize, usize) {
        if parent_slot == NO_PARENT {
            self.clear_seen();
            let mut added = 0usize;
            for r in recs {
                if self.insert_meta(r.frn, r.parent_frn, r.attrs, r.name.as_bytes(), r.mtime) {
                    added += 1;
                }
                if let Some(slot) = self.find_slot(r.frn) {
                    self.attrs[slot as usize] |= ATTR_SEEN;
                }
            }
            self.clear_seen();
            return (added, 0);
        }
        if !self.children_built {
            self.rebuild_children();
        }
        // 先快照现有子项:下面 insert_meta 会改 children,不能边借边改。
        let existing: Vec<u32> = self.children.get(&parent_slot).cloned().unwrap_or_default();
        for &slot in &existing {
            if self.parent[slot as usize] == parent_slot
                && self.attrs[slot as usize] & ATTR_DEAD == 0
            {
                self.attrs[slot as usize] &= !ATTR_SEEN;
            }
        }
        let mut added = 0usize;
        for r in recs {
            if self.insert_meta(r.frn, r.parent_frn, r.attrs, r.name.as_bytes(), r.mtime) {
                added += 1;
            }
            if let Some(slot) = self.find_slot(r.frn) {
                self.attrs[slot as usize] |= ATTR_SEEN;
            }
        }
        // 重新快照(可能新增子项);未 SEEN 的活子项标 DEAD。
        let now: Vec<u32> = self.children.get(&parent_slot).cloned().unwrap_or_default();
        let mut removed = 0usize;
        for slot in now {
            let a = self.attrs[slot as usize];
            if self.parent[slot as usize] == parent_slot && a & ATTR_DEAD == 0 {
                if a & ATTR_SEEN == 0 {
                    self.attrs[slot as usize] = a | ATTR_DEAD;
                    removed += 1;
                } else {
                    self.attrs[slot as usize] = a & !ATTR_SEEN;
                }
            }
        }
        self.dead_count = self.dead_count.saturating_add(removed as u32);
        (added, removed)
    }

    /// watcher 刷新卷根子项:根记录不在索引里,顶层条目的 parent 一律是
    /// [`NO_PARENT`],必须靠 [`ATTR_ROOT_CHILD`] 与「父未消解」条目区分开;
    /// 否则不敢删除,根目录下的删除要等下次启动对账。
    pub fn apply_root_children(&mut self, recs: &[Rec]) -> (usize, usize) {
        for slot in 0..self.attrs.len() {
            let a = self.attrs[slot];
            if a & ATTR_ROOT_CHILD != 0 && a & ATTR_DEAD == 0 {
                self.attrs[slot] = a & !ATTR_SEEN;
            }
        }
        let mut added = 0usize;
        for r in recs {
            if self.insert_meta(r.frn, r.parent_frn, r.attrs, r.name.as_bytes(), r.mtime) {
                added += 1;
            }
            if let Some(slot) = self.find_slot(r.frn) {
                self.attrs[slot as usize] |= ATTR_SEEN | ATTR_ROOT_CHILD;
            }
        }
        let mut removed = 0usize;
        for slot in 0..self.attrs.len() {
            let a = self.attrs[slot];
            if a & ATTR_ROOT_CHILD != 0 && a & ATTR_DEAD == 0 {
                if a & ATTR_SEEN == 0 {
                    self.attrs[slot] = a | ATTR_DEAD;
                    removed += 1;
                } else {
                    self.attrs[slot] = a & !ATTR_SEEN;
                }
            }
        }
        self.dead_count = self.dead_count.saturating_add(removed as u32);
        (added, removed)
    }

    /// 后台对账结束:把本轮没见过且仍存活的槽位标 DEAD(离线期间被删除/移走),
    /// 清掉所有 SEEN 位,返回新标死的条数。槽位不立即回收,交给 compact 重排。
    pub fn remove_unseen(&mut self) -> usize {
        let mut dead = 0usize;
        for slot in 0..self.attrs.len() {
            let a = self.attrs[slot as usize];
            if a & ATTR_SEEN == 0 {
                if a & ATTR_DEAD == 0 {
                    self.attrs[slot as usize] = a | ATTR_DEAD;
                    dead += 1;
                }
            } else {
                self.attrs[slot as usize] = a & !ATTR_SEEN;
            }
        }
        self.dead_count = self.dead_count.saturating_add(dead as u32);
        // 删除改变了存活集:compact 会重排并回收死槽;调用方(DirSyncEnd)随后触发。
        dead
    }

    /// 追加式插入(scan 模式主路径 / journal upsert)。
    /// 返回 false 表示 FRN 已存在且名字/父都没变(无操作)。
    ///
    /// scan 模式约定(worker 保证):ENUM 流 FRN 互异、目录先行 —— 因此
    /// 父(必为目录)要么已插入(经 dir-only 索引解析),要么登记 unresolved。
    /// 插入(不带 mtime 的便捷入口;测试/journal 旧路径用)。
    pub fn insert(
        &mut self,
        frn: u64,
        parent_frn: u64,
        attrs: u32,
        name: &[u8],
    ) -> bool {
        self.insert_meta(frn, parent_frn, attrs, name, 0)
    }

    pub fn insert_meta(
        &mut self,
        frn: u64,
        parent_frn: u64,
        attrs: u32,
        name: &[u8],
        mtime: i64,
    ) -> bool {
        let mtime = mtime.clamp(0, u32::MAX as i64) as u32;
        let scanning = self.is_scanning();
        if !scanning {
            // 命中活槽 → 就地更新;只命中死槽 → 原地复活(FRN 复用 / OLD 当删除的
            // 旧语义遗留),保证 slot 身份不变,子项 parent 指针继续有效。
            let hit = self
                .find_slot(frn)
                .or_else(|| self.find_slot_any(frn).filter(|&s| !self.alive(s)));
            if let Some(old) = hit {
                if !self.alive(old) {
                    self.attrs[old as usize] &= !ATTR_DEAD;
                    self.dead_count = self.dead_count.saturating_sub(1);
                    if old >= self.base_len {
                        self.delta_frn.insert(frn, old);
                    }
                }
                let same_name = self.name(old) == name;
                let new_p = if parent_frn == 0 {
                    NO_PARENT
                } else {
                    self.find_slot(parent_frn).unwrap_or(NO_PARENT)
                };
                let old_parent = self.parent[old as usize];
                if same_name && old_parent == new_p {
                    // 完全一致:仅同步属性位
                    self.attrs[old as usize] = (self.attrs[old as usize] & ATTR_INTERNAL)
                        | (attrs as u16 & !ATTR_INTERNAL)
                        | if parent_frn == ROOT_FRN { ATTR_ROOT_CHILD } else { 0 };
                    self.mtime[old as usize] = mtime;
                    self.note_mtime_pending(old, mtime, name);
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
                    self.names_lc.extend_from_slice(&fold_case(name));
                    self.names_lc.push(0);
                    self.off_index.push((self.name_off[old as usize], old));
                    self.char_mask[old as usize] = mask_of_bytes(name);
                    // base 槽改名:旧名 posting 失效 → 标记跳过 + 进 overlay
                    // (查询端按新名 memmem 验证);delta 槽建槽时已在 overlay。
                    if (old as usize) < self.ngram_base_len as usize && !self.reindex_marked(old) {
                        self.mark_reindexed(old);
                        self.reindex.push(old);
                    }
                }
                if self.children_built {
                    self.move_child(old, old_parent, new_p);
                }
                self.parent[old as usize] = new_p;
                self.attrs[old as usize] = (self.attrs[old as usize] & ATTR_INTERNAL)
                    | (attrs as u16 & !ATTR_INTERNAL)
                    | if parent_frn == ROOT_FRN { ATTR_ROOT_CHILD } else { 0 };
                self.mtime[old as usize] = mtime;
                self.note_mtime_pending(old, mtime, name);
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
        self.names_lc.extend_from_slice(&fold_case(name));
        self.names_lc.push(0);
        self.frn.push(frn);
        let anchored = self.noise_enabled && self.noise_anchors.contains(&parent_frn);
        let p = if parent_frn == 0 || anchored {
            NO_PARENT
        } else {
            self.find_slot(parent_frn).unwrap_or(NO_PARENT)
        };
        self.parent.push(p);
        if self.children_built && p != NO_PARENT {
            self.children.entry(p).or_default().push(slot);
        }
        let mut a = (attrs as u16) & !ATTR_INTERNAL;
        if parent_frn == ROOT_FRN {
            a |= ATTR_ROOT_CHILD;
        }
        self.attrs.push(a);
        self.mtime.push(mtime);
        self.note_mtime_pending(slot, mtime, name);
        self.char_mask.push(mask_of_bytes(name));
        self.delta.push(slot);
        // 倒排 overlay:索引构建后的新增槽不进倒排(重建留给 compact),
        // 查询时逐个 memmem 验证;scan 模式或倒排未建(空 store)时无可用
        // 倒排,查询整体走 memmem 兜底,登记 overlay 没有意义。
        if !scanning && self.ngram.is_some() {
            self.reindex.push(slot);
        }
        // 索引策略:scan 模式只索引目录(父查找只需要目录,内存省 78%);
        // journal 模式索引全部(改名/删除要按 FRN 定位)。
        if scanning {
            if attrs & 0x10 != 0 {
                self.delta_frn.insert(frn, slot);
            }
        } else {
            self.delta_frn.insert(frn, slot);
        }
        // 立即按父链做噪声分类;父未解析则登记,compact 时重试。
        // 根目录父(FRN 5)不算未解析:它永远不在索引里,登记只会让 compact 死循环。
        if parent_frn != 0 && parent_frn != ROOT_FRN && !anchored && p == NO_PARENT {
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
                // 保留 delta_frn 映射:find_slot 仍按 alive 过滤,但 FRN 复用/
                // 同 FRN 复活时 find_slot_any 能定位到这个死槽并原地复用。
                return true;
            }
        }
        false
    }

    /// 沿父链做噪声分类(可达部分)。父未解析时由 compact 全量重分类兜底。
    ///
    /// 语义(A-2):噪声目录**自身保留可见**(搜 node_modules/temp 能找到目录),
    /// 只排除其子孙。因此从**父槽**开始向上查、跳过 slot 自身:slot 自己是噪声段
    /// 不判噪,但它的子项分类时会查到它(名字是噪声段)而判噪。子孙泄漏由
    /// 「祖先 NOISE 位」与 compact 重建的噪声锚点双重兜底。
    fn classify_chain(&mut self, slot: u32) {
        let mut hops = 0;
        let mut cur = self.parent[slot as usize];
        let mut self_noise = false;
        while cur != NO_PARENT && hops < 64 {
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
            cur = self.parent[cur as usize];
            hops += 1;
        }
        if self_noise {
            self.attrs[slot as usize] |= ATTR_NOISE;
        }
    }

    /// 是否有未归并的 delta。
    /// 自上次 compact 以来是否有就地改名。
    /// 改名只在名字区追加新字节、name_off 重指,旧字节成幽灵 —— 此时
    /// names 里的条目数 > name_off.len(),快照的「count==names 条目数」契约
    /// 会被打破,所以落盘前必须 compact 清幽灵。
    pub fn has_pending_rename(&self) -> bool {
        self.renames > 0
    }

    /// 快照要落盘的未消解父链接 (slot, parent_frn)。落盘前由 save 先 compact,
    /// 此时 slot 已是最终编号。
    pub fn unresolved_pairs(&self) -> Vec<(u32, u64)> {
        self.unresolved.iter().map(|(&slot, &p)| (slot, p)).collect()
    }

    pub fn delta_is_empty(&self) -> bool {
        self.delta.is_empty()
    }

    /// 是否需要 compact。
    ///
    /// unresolved 不计入触发条件:未解析父链只有在 delta 带来新记录时才可能被
    /// 修好,单凭 unresolved 会让「每轮 maintain 全量 compact」变成活锁。
    ///
    /// reindex overlay 同样计入:就地改名只进 overlay、不进 delta/死计数,
    /// 批量改名(daemon 同步/解压/构建产物)能把 overlay 顶到 250 万级,
    /// 使逐击键的 overlay 排序 + memmem 随改名数线性劣化。达到 delta 同量级
    /// 就 compact 重建倒排(overlay 随之清空),把查询成本拉回常数。
    pub fn needs_compact(&self) -> bool {
        self.delta.len() >= COMPACT_DELTA
            || self.dead_count >= COMPACT_DEAD
            || self.reindex.len() >= COMPACT_DELTA
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
    /// 未解析父链不算「有工作」:它只可能被 delta 里的新父项修好。
    pub fn compact(&mut self) {
        if !self.is_scanning()
            && self.delta.is_empty()
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
            // 噪声锚点(A-2):自身可见、名字是噪声段的**目录**。其未来子项
            // (journal 增量,父可能尚未解析)按锚点立即判噪,杜绝子孙泄漏。
            // 噪声目录自身保留可搜,故锚点是「保留下来的噪声目录本身」,
            // 而非旧语义的「被剔除的顶层噪声」。
            self.noise_anchors.clear();
            for s in 0..self.name_off.len() as u32 {
                let a = self.attrs[s as usize];
                if (a & ATTR_INTERNAL) != 0 || (a & 0x10) == 0 {
                    continue; // 只看可见目录
                }
                if crate::noise::is_noise_segment(self.name_lc(s)) {
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
        let names_lc = fold_case(&names);
        // 6. parent 重映射
        let mut parent = Vec::with_capacity(by_frn.len());
        for &(_, old) in &by_frn {
            let p = self.parent[old as usize];
            parent.push(if p == NO_PARENT { NO_PARENT } else { remap[p as usize] });
        }
        // 7. attrs/mtime/frn
        let mut attrs = Vec::with_capacity(by_frn.len());
        let mut mtime = Vec::with_capacity(by_frn.len());
        let mut mask = Vec::with_capacity(by_frn.len());
        let mut frn = Vec::with_capacity(by_frn.len());
        for &(_, old) in &by_frn {
            attrs.push(self.attrs[old as usize] & !ATTR_INTERNAL);
            mtime.push(self.mtime[old as usize]);
            mask.push(mask_of_bytes(slice_at(&self.names, self.name_off[old as usize] as usize)));
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
        // 9. 换血(off_index 重建为新鲜项;幽灵与陈项清零)。
        // 噪声剔除后保留集远小于旧容量,收缩避免 names 双缓冲虚胖 3 倍。
        let mut names = names;
        let mut names_lc = names_lc;
        names.shrink_to_fit();
        names_lc.shrink_to_fit();
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
        self.mtime = mtime;
        self.char_mask = mask;
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
        // 10. 列已一致有序:重建倒排,清空 overlay;scan 模式结束 —— 空卷
        //     (live 为空)同样结束,否则空卷会永远停留在 scanning。
        self.rebuild_ngram();
        self.scan_mode = false;
        // slot 已整体重排,旧的待回填编号全部失效;按新列重建一次。
        self.rebuild_mtime_pending();
        // children 索引若已构建(watch 模式),slot 重排后按新 parent 列重建。
        if self.children_built {
            self.rebuild_children();
        }
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

    /// 顶层条目的父是卷根 FRN 5(根记录被 USN 过滤掉)。必须当作 NO_PARENT:
    /// 若登记进 unresolved,unresolved 会长期 >=4096,needs_compact 恒真,
    /// maintain 每轮全量 compact 250 万条(实测 CPU 单核打满、查询被饿死)。
    #[test]
    fn root_parent_never_becomes_unresolved() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 100, 5, 0x10, "Users");
        insert(&mut s, 101, 100, 0, "readme.txt");
        assert_eq!(s.parent[s.find_slot(100).unwrap() as usize], NO_PARENT);
        assert!(s.unresolved_pairs().is_empty());
        assert!(!s.needs_compact());
        s.compact();
        assert!(s.unresolved_pairs().is_empty());
        // 反复 compact 不应再有任何工作(旧实现在这里会永远重建)
        s.compact();
        assert!(!s.needs_compact());
    }

    /// mtime 待回填队列:插入(含 USN 的 mtime=0)登记;已知 mtime 不登记;
    /// compact 重排 slot 后按 mtime 列重建,旧队列不得残留失效编号。
    #[test]
    fn mtime_pending_rebuilds_after_compact() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 100, 5, 0x10, "dir");
        s.compact(); // ready:后续 insert 走增量更新/追加路径
        s.insert_meta(101, 100, 0, b"unknown.txt", 0);
        s.insert_meta(102, 100, 0, b"known.txt", 1_700_000_000);
        assert_eq!(s.mtime_pending.len(), 2, "dir + unknown.txt");

        // 后续 USN 记录带来真实 mtime:就地更新(不再登记新 pending),
        // 旧 pending 项由引擎 drain 按 mtime != 0 丢弃。
        s.insert_meta(101, 100, 0, b"unknown.txt", 1_700_000_001);
        assert_eq!(s.mtime_pending.len(), 2);
        s.compact();
        assert_eq!(s.mtime_pending.len(), 1, "compact 重建后只剩 mtime=0 的 dir");
        let slot = s.mtime_pending[0];
        assert_eq!(s.mtime[slot as usize], 0);
        assert_eq!(s.name(slot), b"dir");

        // 新插入的未知 mtime 条目继续登记。
        s.insert_meta(103, 100, 0, b"new.txt", 0);
        assert_eq!(s.mtime_pending.len(), 2);
    }

    /// 空卷首扫完成:compact 后即使 live 为空也必须退出 scan 模式,且不建
    /// 空倒排。旧实现两者都错:`is_scanning()==base_len==0` 恒真,插入不进
    /// overlay,`Some(空倒排)` 让 2/3 字符查询静默返回 0。
    #[test]
    fn empty_compact_exits_scan_mode_and_skips_empty_ngram() {
        let mut s = VolumeStore::new(false);
        assert!(s.is_scanning());
        s.compact();
        assert!(!s.is_scanning(), "空卷 compact 后 scan 模式必须结束");
        assert!(s.is_empty());
        assert!(s.ngram.is_none(), "空名字区不得构建倒排");
        assert!(s.ngram_candidates(b"ab").is_none(), "无倒排时必须回落 memmem");
        // 空 store 生长期:插入可用,但不进 overlay(没有可维护的倒排)
        assert!(s.insert(10, 0, 0, b"alpha.txt"));
        assert!(s.reindex_slots().is_empty());
        assert!(s.ngram_candidates(b"alp").is_none());
        s.compact();
        assert!(!s.is_scanning());
        assert!(!s.needs_compact());
    }

    /// MINOR-2:就地改名不进 delta/死计数,只堆 reindex overlay。批量改名
    /// 达到 COMPACT_DELTA 时必须触发 compact,否则 overlay 无界增长会把每次
    /// 查询拖成 O(改名数) 的排序 + memmem。
    #[test]
    fn rename_storm_overlay_triggers_compact() {
        let mut s = VolumeStore::new(false);
        for i in 0..COMPACT_DELTA as u64 {
            s.insert(1000 + i, 0, 0, format!("f{i}.txt").as_bytes());
        }
        s.compact();
        assert!(!s.needs_compact());
        for slot in 0..COMPACT_DELTA as u64 {
            let frn = s.frn[slot as usize];
            s.insert(frn, 0, 0, format!("g{slot}_con.txt").as_bytes());
        }
        assert!(s.delta_is_empty(), "就地改名不进 delta");
        assert_eq!(s.dead_count, 0);
        assert!(s.has_pending_rename());
        assert_eq!(s.reindex_slots().len(), COMPACT_DELTA);
        assert!(s.needs_compact(), "overlay 达到 COMPACT_DELTA 必须触发 compact");
        s.compact();
        assert!(!s.has_pending_rename());
        assert!(s.reindex_slots().is_empty());
        assert!(!s.needs_compact());
    }

    /// watcher 刷新卷根:根目录的删除必须靠 ATTR_ROOT_CHILD 识别,
    /// 不能把「父未消解」条目一起删掉。
    #[test]
    fn root_child_refresh_removes_root_level_deletes_only() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 100, 5, 0x10, "folder");
        insert(&mut s, 101, 5, 0, "keep.txt");
        insert(&mut s, 200, 99, 0, "orphan.txt"); // 父未消解,不是根子项
        // 真实 watcher 只会在首扫完成(compact 过)后运行;这里先归并成就绪态。
        s.compact();
        assert!(s.attrs[s.find_slot(100).unwrap() as usize] & ATTR_ROOT_CHILD != 0);
        let recs = vec![
            Rec { frn: 101, parent_frn: 5, attrs: 0, op: crate::usn::Op::Upsert, name: "keep.txt".into(), mtime: 0 },
            Rec { frn: 102, parent_frn: 5, attrs: 0, op: crate::usn::Op::Upsert, name: "new.txt".into(), mtime: 0 },
        ];
        let (added, removed) = s.apply_root_children(&recs);
        assert_eq!(removed, 1);
        assert_eq!(added, 1);
        assert_eq!(s.find_slot(100), None);
        assert!(s.find_slot(101).is_some());
        assert!(s.find_slot(102).is_some());
        // 孤儿条目不受根刷新影响
        assert!(s.find_slot(200).is_some());
        s.compact();
        assert!(s.find_slot(200).is_some());
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
    fn rename_keeps_slot_and_rebuilds_offs_order() {
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

    /// H3:改名 = 原地更新,slot 身份不变 → 子项 parent 指针继续有效。
    #[test]
    fn rename_keeps_slot_and_children_pointers() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0x10, "old-dir");
        insert(&mut s, 11, 10, 0, "child.txt");
        s.compact();
        let dir = s.find_slot(10).unwrap();
        // 旧语义(OLD→Remove)会先打 DEAD;新语义只有 NEW→Upsert
        assert!(s.insert(10, 5, 0x10, b"new-dir"));
        assert_eq!(s.find_slot(10), Some(dir));
        assert_eq!(s.name(dir), b"new-dir");
        assert_eq!(s.parent[s.find_slot(11).unwrap() as usize], dir);
        // 父链上的名字已更新,子项无需级联记录
        assert_eq!(s.name(s.parent[s.find_slot(11).unwrap() as usize]), b"new-dir");
        s.compact();
        assert_eq!(s.len(), 3);
    }

    /// H3:命中 DEAD slot 时原地复活(FRN 复用 / 旧语义遗留的删+建),
    /// 不追加重复 slot。
    #[test]
    fn dead_slot_revived_in_place() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0, "ghost.txt");
        s.compact();
        let slot = s.find_slot(10).unwrap();
        assert!(s.remove(10));
        assert_eq!(s.find_slot(10), None);
        let len_before = s.len();
        // 内容未变的复活返回 false(只有名字/父变化才算 update),但必须原地复用 slot
        s.insert(10, 5, 0, b"ghost.txt");
        assert_eq!(s.len(), len_before, "复活不应追加新 slot");
        assert_eq!(s.find_slot(10), Some(slot));
        assert!(s.visible(slot));
        s.compact();
        assert_eq!(s.len(), 2);
    }

    /// H3:delta 槽删除后同 FRN 再出现同样原地复活(delta_frn 保留死映射)。
    #[test]
    fn dead_delta_slot_revived_in_place() {
        let mut s = VolumeStore::new(false);
        insert(&mut s, 5, 0, 0x10, "root");
        s.compact();
        insert(&mut s, 20, 5, 0, "tmp.txt");
        let slot = s.find_slot(20).unwrap();
        assert!(s.remove(20));
        assert_eq!(s.find_slot(20), None);
        let len_before = s.len();
        s.insert(20, 5, 0, b"tmp.txt");
        assert_eq!(s.len(), len_before);
        assert_eq!(s.find_slot(20), Some(slot));
        assert!(s.visible(slot));
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
    fn noise_dir_visible_but_subtree_dropped_at_compact() {
        // A-2:噪声目录自身保留可搜,子孙整树剔除。
        let mut s = VolumeStore::new(true);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0x10, "node_modules");
        insert(&mut s, 11, 10, 0, "dep.txt");
        insert(&mut s, 12, 5, 0, "keep.txt");
        s.compact();
        assert!(s.find_slot(10).is_some(), "噪声目录自身应保留可见");
        assert_eq!(s.find_slot(11), None, "噪声子孙应被剔除");
        assert!(s.find_slot(12).is_some());
        assert_eq!(s.len(), 3); // root + node_modules + keep.txt
        let spec = |t: &str| crate::query::QuerySpec { tokens: vec![t.into()], ..Default::default() };
        let (_, total, _) = crate::query::query_volume(&s, &spec("node_modules"), 100);
        assert_eq!(total, 1, "搜 node_modules 应找到目录本身");
        let (_, total, _) = crate::query::query_volume(&s, &spec("dep"), 100);
        assert_eq!(total, 0, "噪声内容不可搜");
    }

    #[test]
    fn packages_source_is_searchable() {
        // A-2:packages 是常见 monorepo 源码目录(pnpm/lerna workspace),不再误排。
        let mut s = VolumeStore::new(true);
        insert(&mut s, 5, 0, 0x10, "root");
        insert(&mut s, 10, 5, 0x10, "packages");
        insert(&mut s, 11, 10, 0, "app.js");
        s.compact();
        let spec = crate::query::QuerySpec { tokens: vec!["app".into()], ..Default::default() };
        let (slots, total, _) = crate::query::query_volume(&s, &spec, 100);
        assert_eq!(total, 1, "packages/ 下的源码应可搜到");
        assert_eq!(s.name(slots[0]), b"app.js");
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

    #[test]
    fn fold_case_unicode_equal_length() {
        // ASCII 与旧行为逐字节一致
        assert_eq!(fold_case(b"Hello.TXT"), b"hello.txt");
        // 等长 Unicode 简单折叠:Ä→ä(2字节)、全角 Ａ→ａ(3字节)、西里尔 А→а
        assert_eq!(fold_case("Ä".as_bytes()), "ä".as_bytes());
        assert_eq!(fold_case("ＡＢＣ".as_bytes()), "ａｂｃ".as_bytes());
        assert_eq!(fold_case("Привет".as_bytes()), "привет".as_bytes());
        // 长度会变的折叠不做,保留原字节(ß→ss 会变长)
        assert_eq!(fold_case("ß".as_bytes()), "ß".as_bytes());
        // 中文不受影响,仅 ASCII 部分折叠
        assert_eq!(fold_case("报告.DOCX".as_bytes()), "报告.docx".as_bytes());
        // 硬约束:任意输入折叠前后字节长度不变(names/names_lc 共享偏移)
        for s in ["ÄI", "äi", "ＡＢＣ", "Привет", "ß", "报告.docx", "İstanbul", "aÄbＢc"] {
            assert_eq!(fold_case(s.as_bytes()).len(), s.len(), "长度必须不变: {s}");
        }
    }

    #[test]
    fn apply_children_uses_index_and_tracks_moves() {
        use crate::usn::{Op, Rec};
        let rec = |frn: u64, parent_frn: u64, name: &str| Rec {
            frn,
            parent_frn,
            attrs: 0,
            op: Op::Upsert,
            name: name.to_string(),
            mtime: 0,
        };
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0x10, b"dirA");
        s.insert(20, 5, 0x10, b"dirB");
        s.compact();
        assert!(!s.children_built, "USN 路径(未调 apply_children)不应构建索引");
        let a = s.find_slot(10).unwrap();
        let b = s.find_slot(20).unwrap();
        // 首次 apply_children 构建索引并加两个子项
        let (added, removed) =
            s.apply_children(a, &[rec(100, 10, "c1.txt"), rec(101, 10, "c2.txt")]);
        assert_eq!((added, removed), (2, 0));
        assert!(s.children_built, "首次调用应构建索引");
        let c1 = s.find_slot(100).unwrap();
        let c2 = s.find_slot(101).unwrap();
        assert_eq!(s.parent[c1 as usize], a);
        assert_eq!(s.parent[c2 as usize], a);
        // 刷新只留 c1 → c2 标死
        let (added, removed) = s.apply_children(a, &[rec(100, 10, "c1.txt")]);
        assert_eq!(added, 0);
        assert_eq!(removed, 1, "c2 应被标死");
        assert!(s.find_slot(101).is_none());
        assert!(s.find_slot(100).is_some());
        // 把 c1 移到 dirB(insert_meta 就地改父),索引应同步移动
        s.insert_meta(100, 20, 0, b"c1.txt", 0);
        assert_eq!(s.parent[c1 as usize], b);
        // 刷新 dirA 为空:不应误删已移走的 c1
        let (_, removed) = s.apply_children(a, &[]);
        assert_eq!(removed, 0, "c1 已属 dirB,刷新 dirA 不应删它");
        assert!(s.find_slot(100).is_some());
        // 刷新 dirB 保留 c1
        let (_, removed) = s.apply_children(b, &[rec(100, 20, "c1.txt")]);
        assert_eq!(removed, 0);
        assert!(s.find_slot(100).is_some());
        // compact 重建索引后仍正确
        s.compact();
        let b2 = s.find_slot(20).unwrap();
        let c1b = s.find_slot(100).unwrap();
        assert_eq!(s.parent[c1b as usize], b2);
        let (_, removed) = s.apply_children(b2, &[]);
        assert_eq!(removed, 1, "compact 后索引重建,刷新 dirB 为空应删 c1");
        assert!(s.find_slot(100).is_none());
    }
}
