//! 候选生成:精确 / 前缀 / 子串(打包 buffer memmem)+ 拼音缓存。
//!
//! 语义对齐现有链路(方案 §4.3):
//! - 候选 = 名字匹配(多 token AND);
//! - 收集封顶 candidate_cap,total 照常精确统计(全扫计数);
//! - 精确/前缀不单独走二分:memmem 一次扫出全部命中,按「命中起点/长度」
//!   判定精确与前缀(ready 模式的二分加速留作 P4 杠杆);
//! - scan 模式(未 compact,无 offs)同样适用,渐进搜索白拿;
//! - 名字区 `\0` 结尾保证命中不跨条目,仍按起点校验兜底。

use crate::store::{fold_ascii, VolumeStore, ATTR_INTERNAL};

pub const FILE_ATTRIBUTE_DIRECTORY_U16: u16 = 0x0010;

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
}

#[derive(Clone, Debug, Default)]
pub struct QueryOutcome {
    pub hits: Vec<Hit>,
    pub total: usize,
    /// 因 candidate_cap 提前截断(total 仍精确,仅收集截断)。
    pub capped: bool,
}

/// 判断 slot 是否通过 kind/ext/组 过滤(不含路径)。
pub fn passes_filters(store: &VolumeStore, slot: u32, spec: &QuerySpec) -> bool {
    let attrs = store.attrs[slot as usize];
    if attrs & ATTR_INTERNAL != 0 {
        return false;
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
    let tokens: Vec<Vec<u8>> = spec
        .tokens
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| fold_ascii(t.as_bytes()))
        .collect();
    if tokens.is_empty() {
        return (Vec::new(), 0, false);
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
    for off in finder.find_iter(&store.names_lc) {
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
        if slots.len() < cap {
            slots.push(slot);
        } else {
            capped = true;
        }
    }
    (slots, total, capped)
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
