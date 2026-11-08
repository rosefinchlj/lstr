//! 属性测试:随机 USN 操作流(新建/改名/移动/删除)对拍暴力模型。
//!
//! 模型 = `HashMap<frn, (parent_frn, name, is_dir)>`,路径沿父链重算;
//! store 的可见条目、路径、查询结果必须与模型完全一致。
//! 过程中穿插 compact 与快照 roundtrip,验证增量与持久化的一致性。

use std::collections::HashMap;

use lstr_index::query::{query_volume, KindFilter, QuerySpec};
use lstr_index::snapshot;
use lstr_index::store::VolumeStore;

struct Rng(u64);
impl Rng {
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

use std::cell::RefCell;
thread_local! {
    static TRACE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}
fn trace(msg: String) {
    TRACE.with(|t| t.borrow_mut().push(msg));
}

#[derive(Clone, Debug, PartialEq)]
struct ModelEntry {
    parent_frn: u64,
    name: String,
    is_dir: bool,
}

struct Model {
    entries: HashMap<u64, ModelEntry>,
    next_frn: u64,
}

impl Model {
    fn new() -> Self {
        // 根:FRN 5,名字在 store 里用 "volroot" 占位(真实卷根是 "\",路径拼装时跳过)
        let mut entries = HashMap::new();
        entries.insert(5u64, ModelEntry { parent_frn: 0, name: "volroot".into(), is_dir: true });
        Self { entries, next_frn: 10 }
    }

    fn path(&self, frn: u64) -> Option<String> {
        // 与 store_path_of 同一规则:走父链直到父缺失(根的 parent_frn=0),
        // 根段名 "volroot" 与 store 一并出现。
        let mut parts: Vec<String> = Vec::new();
        let mut cur = frn;
        for _ in 0..64 {
            let Some(e) = self.entries.get(&cur) else { break };
            parts.push(e.name.clone());
            cur = e.parent_frn;
        }
        parts.reverse();
        Some(parts.join("\\"))
    }

    fn depth_of(&self, frn: u64) -> Option<usize> {
        self.path(frn).map(|p| p.split('\\').count())
    }

    fn rand_name(&mut self, rng: &mut Rng) -> String {
        const ALPHA: &[u8] = b"abcdefghij0123456789_";
        let len = 3 + rng.below(8);
        (0..len)
            .map(|_| ALPHA[rng.below(ALPHA.len())] as char)
            .collect()
    }
}

/// 生成一步随机操作,同时应用到模型与 store。
/// journal=false 时只做新建(scan 流:FRN 互异、目录先行由调用方保证)。
fn apply_random_op(rng: &mut Rng, model: &mut Model, store: &mut VolumeStore, journal: bool) {
    let frns: Vec<u64> = model.entries.keys().copied().collect();
    let roll = if journal { rng.below(100) } else { 0 };
    if roll < 35 || model.entries.len() < 3 {
        // 新建(文件或目录)
        let frn = model.next_frn;
        model.next_frn += 1;
        let mut dirs: Vec<u64> = model.entries.iter().filter(|(_, e)| e.is_dir).map(|(&f, _)| f).collect();
        if rng.below(100) < 25 {
            // 新建目录:限制父深度,避免链无限加深
            dirs.retain(|&f| model.depth_of(f).map(|d| d <= 12).unwrap_or(false));
        }
        let parent = dirs[rng.below(dirs.len())];
        let name = model.rand_name(rng);
        let is_dir = rng.below(100) < 25;
        trace(format!("CREATE frn={frn} parent={parent} name={name} dir={is_dir}"));
        store.insert(frn, parent, if is_dir { 0x10 } else { 0 }, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: parent, name, is_dir });
    } else if journal && roll < 60 {
        // 改名/移动既有条目;目录不得移入自身子树(NTFS 也禁止,且会造成环)
        let frn = frns[rng.below(frns.len())];
        if frn == 5 {
            return;
        }
        let subtree: std::collections::HashSet<u64> =
            collect_subtree(&model.entries, frn).into_iter().collect();
        let mut dirs: Vec<u64> = model.entries.iter().filter(|(_, e)| e.is_dir).map(|(&f, _)| f).collect();
        dirs.retain(|f| !subtree.contains(f));
        if dirs.is_empty() {
            return;
        }
        let parent = dirs[rng.below(dirs.len())];
        let name = model.rand_name(rng);
        let is_dir = model.entries[&frn].is_dir;
        trace(format!("MOVE frn={frn} -> parent={parent} name={name}"));
        store.insert(frn, parent, if is_dir { 0x10 } else { 0 }, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: parent, name, is_dir });
    } else if journal {
        // 删除
        let frn = frns[rng.below(frns.len())];
        if frn == 5 {
            return;
        }
        // 目录删除会把整个子树从模型里带走;store 靠 journal 级联记录(每子一条)
        // 这里按「逐条删除子树」模拟真实 journal 行为
        let subtree: Vec<u64> = collect_subtree(&model.entries, frn);
        trace(format!("DELETE subtree_of={frn} n={} frns={:?}", subtree.len(), &subtree[..subtree.len().min(6)]));
        for f in &subtree {
            store.remove(*f);
            model.entries.remove(f);
        }
    }
}

fn collect_subtree(entries: &HashMap<u64, ModelEntry>, root: u64) -> Vec<u64> {
    let mut out = vec![root];
    let mut seen: std::collections::HashSet<u64> = [root].into_iter().collect();
    let mut i = 0;
    while i < out.len() {
        let cur = out[i];
        let mut children: Vec<u64> = entries
            .iter()
            .filter(|(&f, e)| e.parent_frn == cur && f != cur)
            .map(|(&f, _)| f)
            .collect();
        children.retain(|f| seen.insert(*f));
        out.extend(children);
        i += 1;
    }
    out
}

/// store 与模型全量对拍:存在性、名字、目录位、路径。
fn verify(model: &Model, store: &VolumeStore) {
    let mut live_slots = 0usize;
    let mut seen_frns: std::collections::HashSet<u64> = Default::default();
    for slot in 0..store.name_off.len() as u32 {
        if !store.visible(slot) {
            continue;
        }
        live_slots += 1;
        let frn0 = store.frn[slot as usize];
        assert!(seen_frns.insert(frn0), "duplicate live slot for frn {frn0}");
        let frn = store.frn[slot as usize];
        let Some(entry) = model.entries.get(&frn) else {
            panic!("store has frn {frn} not in model");
        };
        assert_eq!(store.name(slot), entry.name.as_bytes(), "name mismatch frn={frn}");
        assert_eq!(store.is_dir(slot), entry.is_dir, "dir flag mismatch frn={frn}");
        let sp = store_path_of(store, slot);
        let mp = model.path(frn);
        if mp.as_deref() != Some(sp.as_str()) {
            TRACE.with(|t| {
                let t = t.borrow();
                let needle = format!("frn={frn}");
                let hist: Vec<String> = t
                    .iter()
                    .filter(|l| l.contains(&needle))
                    .cloned()
                    .collect();
                panic!(
                    "path mismatch frn={frn}\n  store={sp:?}\n  model={mp:?}\n  history({}):\n    {}",
                    hist.len(),
                    hist.join("\n    ")
                );
            });
        }
    }
    assert_eq!(live_slots, model.entries.len(), "live slot count mismatch");
}

fn store_path_of(store: &VolumeStore, slot: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = slot;
    for _ in 0..64 {
        parts.push(String::from_utf8_lossy(store.name(cur)).into_owned());
        cur = store.parent[cur as usize];
        if cur == lstr_index::store::NO_PARENT {
            break;
        }
    }
    parts.reverse();
    parts.join("\\")
}

/// 查询对拍:模型线性扫 vs store 查询,路径集合与 total 一致。
fn verify_query(model: &Model, store: &VolumeStore, needle: &str) {
    let spec = QuerySpec {
        tokens: vec![needle.into()],
        kind: KindFilter::All,
        ..Default::default()
    };
    let (slots, total, _) = query_volume(store, &spec, 100_000);
    let mut model_hits: Vec<String> = Vec::new();
    for (frn, e) in &model.entries {
        if e.name.to_lowercase().contains(needle) {
            if let Some(p) = model.path(*frn) {
                model_hits.push(p);
            }
        }
    }
    model_hits.sort();
    let mut store_hits: Vec<String> = slots
        .iter()
        .map(|&s| store_path_of(store, s))
        .collect();
    store_hits.sort();
    assert_eq!(store_hits, model_hits, "query hits mismatch needle={needle}");
    assert_eq!(total, model_hits.len(), "query total mismatch needle={needle}");
}

#[test]
fn random_op_stream_matches_model() {
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let mut model = Model::new();
    let mut store = VolumeStore::new(false);
    store.insert(5, 0, 0x10, b"volroot");
    // 种子结构
    for _ in 0..20 {
        apply_random_op(&mut rng, &mut model, &mut store, false);
    }
    store.compact();

    let needles = ["a", "e", "0", "1", "ab", "zz"];
    for step in 0..3000usize {
        apply_random_op(&mut rng, &mut model, &mut store, true);
        if step % 300 == 299 {
            store.compact();
        }
        if step % 97 == 0 {
            verify(&model, &store);
            let needle = needles[rng.below(needles.len())];
            verify_query(&model, &store, needle);
        }
        if step == 1500 {
            // 快照 roundtrip 后继续增量
            let dir = std::env::temp_dir().join(format!("lstr-prop-{}", std::process::id()));
            let header = snapshot::SnapshotHeader {
                guid: 0xABC,
                letter: 'C',
                complete: true,
                journal_id: 1,
                next_usn: 100,
                resume_frn: 0,
                est_records: 0,
            };
            snapshot::save(&dir, &header, &mut store).unwrap();
            let loaded = snapshot::load(&dir, 0xABC, false).unwrap();
            store = loaded.store;
            let _ = std::fs::remove_dir_all(&dir);
            verify(&model, &store);
        }
    }
    store.compact();
    verify(&model, &store);
    for needle in needles {
        verify_query(&model, &store, needle);
    }
}

#[test]
fn scan_mode_append_stream_matches_model() {
    // scan 模式:纯追加(ENUM 语义,FRN 互异),compact 后与模型一致
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);
    let mut model = Model::new();
    let mut store = VolumeStore::new(false);
    store.insert(5, 0, 0x10, b"volroot");
    // 目录先行(worker 的 ENUM 分两段发送),再文件;期间随时可查(渐进)
    let mut created: Vec<u64> = vec![5];
    for _ in 0..200 {
        let frn = model.next_frn;
        model.next_frn += 1;
        let parent = created[rng.below(created.len())];
        let name = model.rand_name(&mut rng);
        store.insert(frn, parent, 0x10, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: parent, name, is_dir: true });
        created.push(frn);
        if rng.below(30) == 0 {
            verify_query(&model, &store, "a");
        }
    }
    for _ in 0..600 {
        let frn = model.next_frn;
        model.next_frn += 1;
        let parent = created[rng.below(created.len())];
        let name = model.rand_name(&mut rng);
        store.insert(frn, parent, 0, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: parent, name, is_dir: false });
        if rng.below(50) == 0 {
            verify_query(&model, &store, "a");
        }
    }
    store.compact();
    verify(&model, &store);
}

#[test]
fn noise_subtree_never_visible() {
    // 关键时序:噪声目录先被 compact 剔除(留下锚点),之后子文件才到达。
    // 锚点机制必须让迟到子文件立即判噪,否则会永久泄漏可见条目。
    let mut model = Model::new();
    let mut store = VolumeStore::new(true);
    store.insert(5, 0, 0x10, b"volroot");
    store.insert(100, 5, 0x10, b"project");
    model.entries.insert(100, ModelEntry { parent_frn: 5, name: "project".into(), is_dir: true });
    store.insert(101, 100, 0x10, b"node_modules");
    model.entries.insert(101, ModelEntry { parent_frn: 100, name: "node_modules".into(), is_dir: true });
    store.compact();
    // 此时 node_modules 已被剔除,仅留锚点
    assert_eq!(store.find_slot(101), None);
    // 迟到的子文件与子目录(journal 增量)
    for i in 0..500u64 {
        let frn = 1000 + i;
        let name = format!("dep{i}.js");
        store.insert(frn, 101, 0, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: 101, name, is_dir: false });
    }
    store.insert(2000, 101, 0x10, b"lodash");
    model.entries.insert(2000, ModelEntry { parent_frn: 101, name: "lodash".into(), is_dir: true });
    store.insert(2001, 2000, 0, b"index.js");
    model.entries.insert(2001, ModelEntry { parent_frn: 2000, name: "index.js".into(), is_dir: false });
    // 立即不可见(锚点判噪,孙代沿父链)
    let spec = QuerySpec {
        tokens: vec!["dep".into()],
        kind: KindFilter::All,
        ..Default::default()
    };
    let (_, total, _) = query_volume(&store, &spec, 100_000);
    assert_eq!(total, 0);
    let spec = QuerySpec {
        tokens: vec!["index".into()],
        kind: KindFilter::All,
        ..Default::default()
    };
    let (_, total, _) = query_volume(&store, &spec, 100_000);
    assert_eq!(total, 0);
    store.compact();
    // compact 后整棵子树物理消失,project 保留
    for frn in std::iter::once(101u64).chain((1000..1500).chain([2000, 2001])) {
        assert_eq!(store.find_slot(frn), None, "frn {frn} should be noise-dropped");
    }
    assert!(store.find_slot(100).is_some());
    assert_eq!(store.len(), 2); // volroot + project
}

#[test]
fn mid_scan_compact_then_more_inserts_stay_consistent() {
    // 复现真实时序:scan 流中途 delta≥32768 触发 maintain() 的 compact
    // (base_len 归位 → store 切换到 journal 模式),后续继续插入。
    let mut rng = Rng::new(0xCAFE_F00D_1234_5678);
    let mut model = Model::new();
    let mut store = VolumeStore::new(false);
    store.insert(5, 0, 0x10, b"volroot");
    model.entries.insert(5, ModelEntry { parent_frn: 0, name: "volroot".into(), is_dir: true });

    let mut dirs: Vec<u64> = vec![5];
    let mut next_frn = 100u64;
    let mut step = 0usize;
    // 第一段:目录流,期间主动 compact(模拟 maintain)
    for _ in 0..40_000 {
        let frn = next_frn;
        next_frn += 1;
        let parent = dirs[rng.below(dirs.len())];
        let name = model.rand_name(&mut rng);
        store.insert(frn, parent, 0x10, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: parent, name, is_dir: true });
        dirs.push(frn);
        step += 1;
        if step == 20_000 {
            store.compact(); // mid-scan compact
        }
    }
    // 第二段:文件流(journal 模式)
    for _ in 0..60_000 {
        let frn = next_frn;
        next_frn += 1;
        let parent = dirs[rng.below(dirs.len())];
        let name = model.rand_name(&mut rng);
        store.insert(frn, parent, 0, name.as_bytes());
        model.entries.insert(frn, ModelEntry { parent_frn: parent, name, is_dir: false });
    }
    store.compact();
    // offs 必须覆盖全部活 slot(列数一致即覆盖,verify 再对拍内容)
    assert_eq!(store.offs.len(), store.name_off.len(), "offs vs columns");
    verify(&model, &store);
}
