//! 引擎编排:多卷状态、消息处理、跨卷查询、状态汇总。
//!
//! 所有权模型:lstr 的搜索 worker 线程独占 Engine;管道线程把 EngineMsg
//! 塞进 channel,worker 在查询间隙 try_recv 应用(渐进构建期可搜)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::pinyin::PinyinCache;
use crate::query::{QueryOutcome, QuerySpec, Hit};
use crate::snapshot::{self, SnapshotHeader};
use crate::store::{VolumeStore, ATTR_INTERNAL};
use crate::usn::{Op, Rec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 尚无任何卷数据(worker 未连上)。
    Loading,
    /// 至少一卷在构建/增量同步。
    Scanning,
    /// 全部卷就绪。
    Ready,
    /// worker 不可用(拒绝提权/未运行),走 Everything。
    Fallback,
}

#[derive(Clone, Debug)]
pub struct VolumeProgress {
    pub letter: char,
    pub complete: bool,
    /// 0.0~1.0,未知时为 None。
    pub pct: Option<f32>,
    pub count: u32,
}

#[derive(Clone, Debug)]
pub struct IndexStatus {
    pub phase: Phase,
    pub volumes: Vec<VolumeProgress>,
    /// 状态起始时间(UI 据此决定是否显示「构建中」横幅)。
    pub since: Instant,
}

pub struct VolumeState {
    pub guid: u128,
    pub letter: char,
    pub journal_id: u64,
    pub next_usn: i64,
    pub est_records: u64,
    /// 扫描断点(ENUM 的 StartFileReferenceNumber)。
    pub resume_frn: u64,
    pub complete: bool,
    pub store: Option<VolumeStore>,
    /// 快照有未落盘变更。
    dirty: bool,
    /// 上次落盘时间(节流:扫描期频繁保存会拖慢构建)。
    last_save: Option<Instant>,
}

#[derive(Debug)]
pub enum EngineMsg {
    VolumeMeta { guid: u128, letter: char, journal_id: u64, next_usn: i64, est_records: u64 },
    ScanBegin { guid: u128, resume_frn: u64 },
    ScanProgress { guid: u128, frn: u64, est: u64, count: u32 },
    Records { guid: u128, recs: Vec<Rec> },
    VolumeReady { guid: u128, journal_id: u64, next_usn: i64, count: u32 },
    JournalReset { guid: u128 },
    VolumeGone { guid: u128 },
    VolumeAppeared { guid: u128, letter: char },
    WorkerLost,
}

pub struct EngineConfig {
    pub index_dir: PathBuf,
    pub exclude_noise: bool,
    pub candidate_cap: usize,
}

pub struct Engine {
    cfg: EngineConfig,
    pub volumes: Vec<VolumeState>,
    pinyin: Vec<PinyinCache>,
    phase: Phase,
    phase_since: Instant,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        let mut engine = Self {
            cfg,
            volumes: Vec::new(),
            pinyin: Vec::new(),
            phase: Phase::Loading,
            phase_since: Instant::now(),
        };
        engine.load_snapshots();
        engine.recompute_phase();
        engine
    }

    fn load_snapshots(&mut self) {
        let dir = self.cfg.index_dir.clone();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(hex) = name
                .strip_prefix("vol_")
                .and_then(|s| s.strip_suffix(".bin"))
            else {
                continue;
            };
            let Ok(guid) = u128::from_str_radix(hex, 16) else { continue };
            if let Some(snap) = snapshot::load(&dir, guid, self.cfg.exclude_noise) {
                let h = &snap.header;
                snapshot::debug_log(&format!(
                    "loaded {guid:032x} letter={} count={} complete={}",
                    h.letter, snap.store.len(), h.complete
                ));
                self.volumes.push(VolumeState {
                    guid,
                    letter: h.letter,
                    journal_id: h.journal_id,
                    next_usn: h.next_usn,
                    est_records: h.est_records,
                    resume_frn: h.resume_frn,
                    complete: h.complete,
                    store: Some(snap.store),
                    dirty: false,
                    last_save: Some(Instant::now()),
                });
            }
        }
        self.pinyin = self
            .volumes
            .iter()
            .map(|v| v.store.as_ref().map(|s| PinyinCache::rebuild(s)).unwrap_or_else(PinyinCache::empty))
            .collect();
    }

    fn volume_idx(&mut self, guid: u128, create: bool, letter: char) -> Option<usize> {
        if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
            return Some(i);
        }
        if create {
            self.volumes.push(VolumeState {
                guid,
                letter,
                journal_id: 0,
                next_usn: 0,
                est_records: 0,
                resume_frn: 0,
                complete: false,
                store: None,
                dirty: false,
                last_save: None,
            });
            self.pinyin.push(PinyinCache::empty());
            return Some(self.volumes.len() - 1);
        }
        None
    }

    pub fn handle(&mut self, msg: EngineMsg) {
        match msg {
            EngineMsg::WorkerLost => {
                self.phase = Phase::Fallback;
                self.phase_since = Instant::now();
            }
            EngineMsg::VolumeMeta { guid, letter, journal_id, next_usn, est_records } => {
                if let Some(i) = self.volume_idx(guid, true, letter) {
                    let v = &mut self.volumes[i];
                    v.letter = letter;
                    v.journal_id = journal_id;
                    v.next_usn = next_usn;
                    v.est_records = est_records;
                }
            }
            EngineMsg::ScanBegin { guid, resume_frn } => {
                if let Some(i) = self.volume_idx(guid, true, '?') {
                    let v = &mut self.volumes[i];
                    v.resume_frn = resume_frn;
                    v.complete = false;
                    if v.store.is_none() {
                        v.store = Some(VolumeStore::new(self.cfg.exclude_noise));
                    }
                }
            }
            EngineMsg::ScanProgress { guid, frn, est, count } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    let v = &mut self.volumes[i];
                    v.resume_frn = frn;
                    v.est_records = est;
                    if let Some(s) = &v.store {
                        let _ = count.max(s.len() as u32);
                    }
                }
            }
            EngineMsg::Records { guid, recs } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                if let Some(store) = &mut self.volumes[i].store {
                    for rec in &recs {
                        match rec.op {
                            Op::Upsert => {
                                store.insert(rec.frn, rec.parent_frn, rec.attrs, rec.name.as_bytes());
                            }
                            Op::Remove => {
                                store.remove(rec.frn);
                            }
                        }
                    }
                    self.volumes[i].dirty = true;
                }
            }
            EngineMsg::VolumeReady { guid, journal_id, next_usn, count } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                let v = &mut self.volumes[i];
                v.journal_id = journal_id;
                v.next_usn = next_usn;
                v.complete = true;
                v.resume_frn = 0;
                v.dirty = true;
                v.last_save = None;
                if let Some(store) = &mut v.store {
                    if store.is_scanning() || store.needs_compact() {
                        store.compact();
                    }
                    let _ = count.max(store.len() as u32);
                }
                self.pinyin[i] = v
                    .store
                    .as_ref()
                    .map(|s| PinyinCache::rebuild(s))
                    .unwrap_or_else(PinyinCache::empty);
            }
            EngineMsg::JournalReset { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    let v = &mut self.volumes[i];
                    v.store = None;
                    v.complete = false;
                    v.next_usn = 0;
                    v.journal_id = 0;
                    v.dirty = false;
                    self.pinyin[i] = PinyinCache::empty();
                    let _ = snapshot::snapshot_path(&self.cfg.index_dir, guid);
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&self.cfg.index_dir, guid));
                }
            }
            EngineMsg::VolumeGone { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    self.volumes.remove(i);
                    self.pinyin.remove(i);
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&self.cfg.index_dir, guid));
                }
            }
            EngineMsg::VolumeAppeared { guid, letter } => {
                let _ = self.volume_idx(guid, true, letter);
            }
        }
        self.recompute_phase();
    }

    fn recompute_phase(&mut self) {
        let new = if self.volumes.is_empty() {
            Phase::Loading
        } else if self.volumes.iter().any(|v| !v.complete) {
            Phase::Scanning
        } else {
            Phase::Ready
        };
        if new != self.phase && self.phase != Phase::Fallback {
            self.phase = new;
            self.phase_since = Instant::now();
        } else if new == Phase::Ready && self.phase != Phase::Ready {
            self.phase = new;
            self.phase_since = Instant::now();
        }
    }

    pub fn status(&self) -> IndexStatus {
        let volumes = self
            .volumes
            .iter()
            .map(|v| {
                let count = v.store.as_ref().map(|s| s.len()).unwrap_or(0) as u32;
                let pct = if v.complete {
                    Some(1.0)
                } else if v.est_records > 0 {
                    Some((v.resume_frn as f32 / v.est_records as f32).clamp(0.0, 0.99))
                } else {
                    None
                };
                VolumeProgress { letter: v.letter, complete: v.complete, pct, count }
            })
            .collect();
        IndexStatus { phase: self.phase, volumes, since: self.phase_since }
    }

    /// 握手载荷:每卷 (guid, journal_id, next_usn)。未完成的卷 next_usn=0(要求全量)。
    pub fn handshake_state(&self) -> Vec<(u128, u64, i64)> {
        self.volumes
            .iter()
            .map(|v| {
                if v.complete {
                    (v.guid, v.journal_id, v.next_usn)
                } else {
                    (v.guid, 0, 0)
                }
            })
            .collect()
    }

    pub fn query(&self, spec: &QuerySpec) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        for (i, v) in self.volumes.iter().enumerate() {
            // 路径前缀带盘符 → 只查对应卷
            if let Some(prefix) = &spec.path_prefix {
                if prefix.len() >= 2 && prefix.as_bytes()[1] == b':' {
                    let want = prefix.as_bytes()[0].to_ascii_lowercase();
                    if want != v.letter.to_ascii_lowercase() as u8 {
                        continue;
                    }
                }
            }
            let Some(store) = &v.store else { continue };
            let (slots, total, capped) =
                crate::query::query_volume(store, spec, self.cfg.candidate_cap);
            out.total += total;
            out.capped |= capped;
            let mut hits: Vec<Hit> = slots
                .into_iter()
                .map(|slot| Hit { vol: i as u32, slot })
                .collect();
            // 拼音候选(仅 ASCII 查询;名字里不含查询词时靠拼音命中)
            if !spec.tokens.is_empty() && spec.tokens.iter().all(|t| t.is_ascii()) {
                let tokens: Vec<Vec<u8>> = spec
                    .tokens
                    .iter()
                    .map(|t| crate::store::fold_ascii(t.as_bytes()))
                    .collect();
                for slot in self.pinyin[i].search(store, &tokens, self.cfg.candidate_cap) {
                    if !hits.iter().any(|h| h.slot == slot)
                        && crate::query::passes_filters(store, slot, spec)
                    {
                        out.total += 1;
                        hits.push(Hit { vol: i as u32, slot });
                    }
                }
            }
            // 深路径前缀后置过滤
            if let Some(prefix) = &spec.path_prefix {
                hits.retain(|h| {
                    self.path_of(h.vol, h.slot)
                        .map(|p| p.to_lowercase().starts_with(prefix))
                        .unwrap_or(false)
                });
            }
            out.hits.append(&mut hits);
        }
        out
    }

    pub fn entry_name(&self, vol: u32, slot: u32) -> String {
        self.volumes[vol as usize]
            .store
            .as_ref()
            .map(|s| String::from_utf8_lossy(s.name(slot)).into_owned())
            .unwrap_or_default()
    }

    pub fn is_dir(&self, vol: u32, slot: u32) -> bool {
        self.volumes[vol as usize]
            .store
            .as_ref()
            .map(|s| s.is_dir(slot))
            .unwrap_or(false)
    }

    pub fn visible(&self, vol: u32, slot: u32) -> bool {
        self.volumes[vol as usize]
            .store
            .as_ref()
            .map(|s| s.attrs[slot as usize] & ATTR_INTERNAL == 0)
            .unwrap_or(false)
    }

    /// 沿父链拼全路径(盘符 + 路径)。
    pub fn path_of(&self, vol: u32, slot: u32) -> Option<String> {
        let v = &self.volumes[vol as usize];
        let store = v.store.as_ref()?;
        let mut parts: Vec<String> = Vec::new();
        let mut cur = slot;
        let mut hops = 0;
        while cur != crate::store::NO_PARENT && hops < 64 {
            // 父已删除(目录被删,子项删除记录还在路上):链不完整,不给路径
            if store.attrs[cur as usize] & crate::store::ATTR_DEAD != 0 {
                return None;
            }
            let s = store.name(cur);
            parts.push(String::from_utf8_lossy(s).into_owned());
            cur = store.parent[cur as usize];
            hops += 1;
        }
        if hops >= 64 {
            return None;
        }
        parts.reverse();
        Some(format!("{}:\\{}", v.letter, parts.join("\\")))
    }

    /// 拼音 (initials, full),无缓存则 None。
    pub fn pinyin_of(&self, vol: u32, slot: u32) -> Option<(&str, &str)> {
        self.pinyin
            .get(vol as usize)?
            .get(slot)
    }

    /// 空闲维护:需要的卷 compact + 落盘脏快照(节流)。
    /// 保存前必须归并:快照格式假设列一致(offs 覆盖全部 slot),
    /// 否则载入被拒 → 无谓重扫(实测抓出:扫描期保存 offs 短 200/2907)。
    pub fn maintain(&mut self) -> bool {
        let mut did = false;
        let now = Instant::now();
        // 先挑出待保存的卷(借用两段式:挑 → 处理)
        let mut pending: Vec<usize> = Vec::new();
        for (i, v) in self.volumes.iter().enumerate() {
            let due = match v.last_save {
                None => true,
                Some(t) => now.duration_since(t) >= Duration::from_secs(10),
            };
            if v.dirty && due {
                pending.push(i);
            }
        }
        for i in pending {
            let Some(store) = &mut self.volumes[i].store else { continue };
            // 有未归并的 delta(无论模式)先 compact,保证列一致
            if !store.delta_is_empty() {
                store.compact();
                self.pinyin[i] = PinyinCache::rebuild(store);
            }
            self.save_volume(i);
            did = true;
        }
        did
    }

    fn save_volume(&mut self, i: usize) {
        let (header, has_store) = {
            let v = &self.volumes[i];
            let Some(store) = &v.store else { return };
            if store.is_scanning() && v.resume_frn == 0 {
                return; // 没有断点意义的空扫描不落盘
            }
            (
                SnapshotHeader {
                    guid: v.guid,
                    letter: v.letter,
                    complete: v.complete,
                    journal_id: v.journal_id,
                    next_usn: v.next_usn,
                    resume_frn: v.resume_frn,
                    est_records: v.est_records,
                },
                true,
            )
        };
        if !has_store {
            return;
        }
        let v = &mut self.volumes[i];
        let store = v.store.as_mut().unwrap();
        match snapshot::save(&self.cfg.index_dir, &header, store) {
            Ok(_) => {
                self.volumes[i].dirty = false;
                self.volumes[i].last_save = Some(Instant::now());
            }
            Err(err) => {
                eprintln!("lstr-index: save snapshot failed: {err}");
                crate::snapshot::debug_log(&format!("save {} failed: {err}", header.guid));
            }
        }
    }

    pub fn cfg(&self) -> &EngineConfig {
        &self.cfg
    }
}
