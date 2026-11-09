//! 引擎编排:多卷状态、消息处理、跨卷查询、状态汇总。
//!
//! 所有权模型:lstr 的搜索 worker 线程独占 Engine;管道线程把 EngineMsg
//! 塞进 channel,worker 在查询间隙 try_recv 应用(渐进构建期可搜)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::pinyin::PinyinCache;
use crate::query::{QueryOutcome, QuerySpec, Hit};
use crate::snapshot::{self, SnapshotHeader};
use crate::store::{VolumeStore, NO_PARENT, ROOT_FRN};
use crate::usn::{Op, Rec};
use crate::usnwatch::UsnWatchState;

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
    /// USN 实时增量层状态(worker `UsnWatch` 帧上报;非 Off 才显示)。
    pub usn_state: Option<UsnWatchState>,
    /// 游标年龄(秒):自引擎上次推进该卷位点(journal 批次/Ready)以来;
    /// 尚无位点信息时为 None。UI 拼成「游标 Ns 前」。
    pub cursor_age_secs: Option<u64>,
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
    /// 索引代次:compact/重扫/重置后 +1,用于让旧 `Hit` 失效(H8)。
    pub generation: u32,
    /// 快照有未落盘变更。
    dirty: bool,
    /// 后台对账(DirSync)进行中:逐批 patch 现有 store,期间禁止落盘
    /// (SEEN 位是临时态,写进快照会让下次启动误删条目)。
    pub syncing: bool,
    /// 连续落盘失败次数(指数退避)。
    save_backoff: u32,
    /// 下次允许落盘的时间:失败时按指数退避往后推,成功后加节流间隔
    /// (见 save_interval)。journal 每来一小批就整卷重写 120MB 会把查询线程
    /// 饿死(实测搜索要等几秒),必须节流。
    save_retry_at: Option<Instant>,
    /// USN 实时增量层状态(usn 后端 worker 经 UsnWatch 帧上报;默认 Off,
    /// 旧后端 worker 从不上报 → UI 不显示)。
    pub usn_watch: UsnWatchState,
    /// 游标上次推进时间(journal 批次 / VolumeReady):UI 的「游标 age」。
    cursor_at: Option<Instant>,
}

#[derive(Debug)]
pub enum EngineMsg {
    VolumeMeta { guid: u128, letter: char, journal_id: u64, next_usn: i64, est_records: u64 },
    ScanBegin { guid: u128, resume_frn: u64 },
    ScanProgress { guid: u128, frn: u64, est: u64, count: u32 },
    Records { guid: u128, recs: Vec<Rec>, next_usn: i64 },
    /// 后台对账:已有快照时逐批 patch 现有条目,而不是清空重扫。
    DirSyncBegin { guid: u128 },
    DirSyncEnd { guid: u128, ok: bool },
    /// 运行期 watcher:某父目录的最新子项快照(替换该目录子项集合)。
    DirChildren { guid: u128, parent_frn: u64, recs: Vec<Rec> },
    /// USN 实时增量层每卷状态上报(usn 后端 worker)。
    UsnWatch { guid: u128, state: UsnWatchState },
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
    /// 勾选要索引的盘符(大写单字母,如 `'C'`);空 vec = 全部卷。
    /// 只影响启动时加载哪些 `vol_*.bin` 快照:未选中卷的旧快照留在磁盘上,
    /// 重新勾选后下次启动可直接复用,无需重扫。
    pub volumes: Vec<char>,
}

pub struct Engine {
    cfg: EngineConfig,
    pub volumes: Vec<VolumeState>,
    pinyin: Vec<PinyinCache>,
    phase: Phase,
    phase_since: Instant,
    /// 卷代次分配器(只增不减,跨卷唯一)。
    next_generation: u32,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        let mut engine = Self {
            cfg,
            volumes: Vec::new(),
            pinyin: Vec::new(),
            phase: Phase::Loading,
            phase_since: Instant::now(),
            next_generation: 1,
        };
        engine.load_snapshots();
        engine.recompute_phase();
        engine
    }

    /// 给第 i 卷分配新代次:此后所有指向旧代次的 `Hit` 都失效。
    fn bump_generation(&mut self, i: usize) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        self.volumes[i].generation = generation;
    }

    fn load_snapshots(&mut self) {
        let dir = self.cfg.index_dir.clone();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // 上次写快照中途崩溃/被杀留下的 temp 文件(vol_<pid>.tmp):清掉,
            // 避免长期堆积;若仍有并发写者,它会在 rename 前重建/覆盖。
            if name.strip_prefix("vol_").is_some_and(|s| s.ends_with(".tmp")) {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            let Some(hex) = name
                .strip_prefix("vol_")
                .and_then(|s| s.strip_suffix(".bin"))
            else {
                continue;
            };
            let Ok(guid) = u128::from_str_radix(hex, 16) else { continue };
            if let Some(snap) = snapshot::load(&dir, guid, self.cfg.exclude_noise) {
                let h = &snap.header;
                // 配置勾选了盘符时只加载选中的卷:不 push VolumeState、不建
                // PinyinCache;快照文件保留,重新勾选后可直接复用(不重扫)。
                if !self.cfg.volumes.is_empty() && !self.cfg.volumes.contains(&h.letter) {
                    snapshot::debug_log(&format!(
                        "skip {guid:032x} letter={} not selected",
                        h.letter
                    ));
                    continue;
                }
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
                    generation: 0,
                    dirty: false,
                    syncing: false,
                    save_backoff: 0,
                    save_retry_at: None,
                    usn_watch: UsnWatchState::Off,
                    cursor_at: None,
                });
                let i = self.volumes.len() - 1;
                self.bump_generation(i);
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
                generation: 0,
                dirty: false,
                syncing: false,
                save_backoff: 0,
                save_retry_at: None,
                usn_watch: UsnWatchState::Off,
                cursor_at: None,
            });
            self.pinyin.push(PinyinCache::empty());
            let i = self.volumes.len() - 1;
            self.bump_generation(i);
            return Some(i);
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
                    {
                        let v = &mut self.volumes[i];
                        v.resume_frn = resume_frn;
                        v.complete = false;
                        if v.store.is_none() {
                            v.store = Some(VolumeStore::new(self.cfg.exclude_noise));
                        }
                    }
                    // 重扫会重建/原地更新 slot,旧 Hit 必须失效(H8)
                    self.bump_generation(i);
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
            EngineMsg::Records { guid, recs, next_usn } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                let syncing = self.volumes[i].syncing;
                if let Some(store) = &mut self.volumes[i].store {
                    for rec in &recs {
                        match rec.op {
                            Op::Upsert => {
                                store.insert_meta(rec.frn, rec.parent_frn, rec.attrs, rec.name.as_bytes(), rec.mtime);
                                // 后台对账期间记「见过」,DirSyncEnd 据此删离线消失项。
                                if syncing {
                                    store.mark_seen(rec.frn);
                                }
                            }
                            Op::Remove => {
                                store.remove(rec.frn);
                            }
                        }
                    }
                    self.volumes[i].dirty = true;
                }
                // M2:记录批次的 journal 位点,重连握手从最新位点续传。
                // 仅对就绪卷生效:扫描中的卷若提前写入 next_usn,
                // 空/半 store 会被 worker 误判成可增量,丢文件。
                if self.volumes[i].complete {
                    self.volumes[i].next_usn = next_usn;
                    // 游标推进时间戳:设置页/横幅的「游标 age」依据。
                    self.volumes[i].cursor_at = Some(Instant::now());
                }
            }
            EngineMsg::UsnWatch { guid, state } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    if self.volumes[i].usn_watch != state {
                        snapshot::debug_log(&format!(
                            "usn watch {} → {}",
                            self.volumes[i].letter,
                            state.label()
                        ));
                        self.volumes[i].usn_watch = state;
                    }
                }
                // 未知 guid(卷已移除/未出现在握手)直接忽略:不凭空建卷。
            }
            EngineMsg::VolumeReady { guid, journal_id, next_usn, count } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                let compacted = {
                    let v = &mut self.volumes[i];
                    v.journal_id = journal_id;
                    v.next_usn = next_usn;
                    v.complete = true;
                    v.resume_frn = 0;
                    v.dirty = true;
                    v.cursor_at = Some(Instant::now());
                    // 扫完的卷立即落一次盘(不等 60s 节流):这是最值钱的快照。
                    v.save_retry_at = None;
                    let mut compacted = false;
                    if let Some(store) = &mut v.store {
                        if store.is_scanning() || store.needs_compact() {
                            store.compact();
                            compacted = true;
                        }
                        let _ = count.max(store.len() as u32);
                    }
                    compacted
                };
                if compacted {
                    // compact remap 后旧 Hit 的 slot 已失效(H8)
                    self.bump_generation(i);
                }
                self.pinyin[i] = self
                    .volumes[i]
                    .store
                    .as_ref()
                    .map(|s| PinyinCache::rebuild(s))
                    .unwrap_or_else(PinyinCache::empty);
            }
            EngineMsg::DirSyncBegin { guid } => {
                if let Some(i) = self.volume_idx(guid, true, '?') {
                    if self.volumes[i].store.is_none() {
                        self.volumes[i].store = Some(VolumeStore::new(self.cfg.exclude_noise));
                        self.pinyin[i] = PinyinCache::empty();
                        self.bump_generation(i);
                    }
                    self.volumes[i].syncing = true;
                    snapshot::debug_log(&format!("dirsync begin {guid:032x}"));
                }
            }
            EngineMsg::DirSyncEnd { guid, ok } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                if !ok {
                    // 中途停止:只清临时位,绝不删除(否则会把没扫到的条目全判死)。
                    if let Some(store) = &mut self.volumes[i].store {
                        store.clear_seen();
                    }
                    self.volumes[i].syncing = false;
                    snapshot::debug_log(&format!(
                        "dirsync end {guid:032x} aborted, kept snapshot as-is"
                    ));
                    return;
                }
                let removed = {
                    let Some(store) = &mut self.volumes[i].store else {
                        self.volumes[i].syncing = false;
                        return;
                    };
                    let n = store.remove_unseen();
                    if store.needs_compact() {
                        store.compact();
                    }
                    n
                };
                self.bump_generation(i);
                if let Some(store) = &self.volumes[i].store {
                    self.pinyin[i] = PinyinCache::rebuild(store);
                }
                self.volumes[i].syncing = false;
                self.volumes[i].dirty = true;
                // 对账结果立即落盘:这是本轮启动后最值钱的快照。
                self.volumes[i].save_retry_at = None;
                self.recompute_phase();
                let (count, letter) = self
                    .volumes
                    .get(i)
                    .map(|v| (v.store.as_ref().map(|s| s.len()).unwrap_or(0), v.letter))
                    .unwrap_or((0, '?'));
                snapshot::debug_log(&format!(
                    "dirsync end {guid:032x} letter={letter} removed={removed} count={count}"
                ));
            }
            EngineMsg::DirChildren { guid, parent_frn, recs } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                let (added, removed) = {
                    let Some(store) = &mut self.volumes[i].store else {
                        return;
                    };
                    let parent_slot = if parent_frn == ROOT_FRN {
                        NO_PARENT
                    } else {
                        store.find_slot(parent_frn).unwrap_or(NO_PARENT)
                    };
                    if parent_frn == ROOT_FRN {
                        store.apply_root_children(&recs)
                    } else {
                        store.apply_children(parent_slot, &recs)
                    }
                };
                self.volumes[i].dirty = true;
                if added > 0 || removed > 0 {
                    snapshot::debug_log(&format!(
                        "watch {guid:032x} parent={parent_frn} added={added} removed={removed}"
                    ));
                }
            }
            EngineMsg::JournalReset { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    {
                        let v = &mut self.volumes[i];
                        v.store = None;
                        v.complete = false;
                        v.next_usn = 0;
                        v.journal_id = 0;
                        v.dirty = false;
                    }
                    // 索引已清空,旧 Hit 一律失效(H8)
                    self.bump_generation(i);
                    self.pinyin[i] = PinyinCache::empty();
                    let _ = snapshot::snapshot_path(&self.cfg.index_dir, guid);
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&self.cfg.index_dir, guid));
                }
            }
            EngineMsg::VolumeGone { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    // swap_remove:O(1) 收尾。代次跨卷唯一,旧 Hit 的 (vol,generation)
                    // 要么越界、要么代次不符,由访问器的校验拒绝(H8)。
                    self.volumes.swap_remove(i);
                    self.pinyin.swap_remove(i);
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
        let mut volumes: Vec<VolumeProgress> = self
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
                VolumeProgress {
                    letter: v.letter,
                    complete: v.complete,
                    pct,
                    count,
                    // Off 是「worker 从未上报」的缺省:不显示,避免旧后端 UI 噪音。
                    usn_state: (v.usn_watch != UsnWatchState::Off).then_some(v.usn_watch),
                    cursor_age_secs: v.cursor_at.map(|t| t.elapsed().as_secs()),
                }
            })
            .collect();
        // 内部顺序来自 read_dir / swap_remove(会乱),UI 要按盘符 C D E F
        // 升序显示,所以在出口做一次确定性排序。
        volumes.sort_by_key(|v| v.letter);
        IndexStatus { phase: self.phase, volumes, since: self.phase_since }
    }

    /// 握手载荷:每卷 (guid, journal_id, next_usn, resume_frn, complete)。
    ///
    /// - complete=true:next_usn 是 journal 位点,worker 走增量;jid/usn 无效时它自己退全量。
    /// - complete=false 且 store 已建:带上次扫描的起点高水位与 resume_frn,
    ///   worker 从 resume_frn 续扫(中断续扫),不用每次重启从头全量。
    /// - 其它:全 0,要求 worker 从头全量。
    pub fn handshake_state(&self) -> Vec<(u128, u64, i64, u64, bool)> {
        self.volumes
            .iter()
            .map(|v| {
                if v.complete {
                    (v.guid, v.journal_id, v.next_usn, 0, true)
                } else if v.store.is_some() && v.journal_id > 0 && v.next_usn > 0 {
                    (v.guid, v.journal_id, v.next_usn, v.resume_frn, false)
                } else {
                    (v.guid, 0, 0, 0, false)
                }
            })
            .collect()
    }

    pub fn query(&self, spec: &QuerySpec) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        // 查询取消:UI 每发新搜索就刷新 CANCEL_GEN;这里取快照,热循环发现
        // 代次变化即中止(旧查询如单字符 "a" 命中 200 万条要 600ms+,
        // 不中止会把后续击键堵在队列里)。
        let cancel_my = crate::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed);
        for (i, v) in self.volumes.iter().enumerate() {
            if crate::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed) != cancel_my {
                out.aborted = true;
                break;
            }
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
            let generation = v.generation;
            // H5:仅过滤器(无名字 token)的查询没有名字锚点,候选池容易被
            // 前 2000 个可见项占满(尤其 `path:C:\dir` 还要后置过滤),
            // 放大 cap 让收集阶段多留余量;total 仍精确。
            let cap = if spec.tokens.is_empty() {
                self.cfg.candidate_cap.saturating_mul(8)
            } else {
                self.cfg.candidate_cap
            };
            let (slots, total, capped, aborted) =
                crate::query::query_volume_cancel(store, spec, cap, Some(cancel_my));
            if aborted {
                out.aborted = true;
                break;
            }
            out.total += total;
            out.capped |= capped;
            let mut hits: Vec<Hit> = slots
                .into_iter()
                .map(|slot| Hit { vol: i as u32, slot, generation })
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
                        hits.push(Hit { vol: i as u32, slot, generation });
                    }
                }
            }
            // 深路径前缀后置过滤
            if let Some(prefix) = &spec.path_prefix {
                hits.retain(|h| {
                    self.path_of(h)
                        .map(|p| p.to_lowercase().starts_with(prefix))
                        .unwrap_or(false)
                });
            }
            out.hits.append(&mut hits);
        }
        out
    }

    /// 校验 Hit 仍指向有效卷/有效 slot(H8:裸索引在 compact/卷移除后必须拒绝)。
    fn store_for_hit(&self, hit: &Hit) -> Option<&VolumeStore> {
        let v = self.volumes.get(hit.vol as usize)?;
        if v.generation != hit.generation {
            return None;
        }
        let store = v.store.as_ref()?;
        if hit.slot as usize >= store.len() {
            return None;
        }
        Some(store)
    }

    pub fn entry_name(&self, hit: &Hit) -> String {
        self.store_for_hit(hit)
            .map(|s| String::from_utf8_lossy(s.name(hit.slot)).into_owned())
            .unwrap_or_default()
    }

    pub fn is_dir(&self, hit: &Hit) -> bool {
        self.store_for_hit(hit).map(|s| s.is_dir(hit.slot)).unwrap_or(false)
    }

    pub fn visible(&self, hit: &Hit) -> bool {
        self.store_for_hit(hit).map(|s| s.visible(hit.slot)).unwrap_or(false)
    }

    /// 沿父链拼全路径(盘符 + 路径)。Hit 失效/卷已移除时返回 None。
    pub fn path_of(&self, hit: &Hit) -> Option<String> {
        let store = self.store_for_hit(hit)?;
        let v = &self.volumes[hit.vol as usize];
        let mut parts: Vec<String> = Vec::new();
        let mut cur = hit.slot;
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

    /// 拼音 (initials, full),无缓存/Hit 失效则 None。
    pub fn pinyin_of(&self, hit: &Hit) -> Option<(&str, &str)> {
        self.store_for_hit(hit)?;
        self.pinyin.get(hit.vol as usize)?.get(hit.slot)
    }

    /// 空闲维护:需要的卷 compact + 落盘脏快照。返回是否做了事(调用方可据此让路)。
    pub fn maintain(&mut self) -> bool {
        let mut did = false;
        // 先归并(compact 会 remap,旧 Hit 失效),再落盘
        for i in 0..self.volumes.len() {
            if self.volumes[i].syncing {
                continue; // 对账期间不 compact/落盘:SEEN 位是临时态
            }
            let compacted = {
                let Some(store) = &mut self.volumes[i].store else { continue };
                if store.is_scanning() {
                    continue; // scan 模式的 compact 由 VolumeReady 触发
                }
                if store.needs_compact() {
                    let started = Instant::now();
                    let len = store.len();
                    store.compact();
                    let ms = started.elapsed().as_millis();
                    if ms > 50 {
                        crate::snapshot::debug_log(&format!(
                            "compact vol {} len={} took {}ms",
                            self.volumes[i].letter, len, ms
                        ));
                    }
                    true
                } else {
                    false
                }
            };
            if compacted {
                self.bump_generation(i);
                if let Some(store) = &self.volumes[i].store {
                    self.pinyin[i] = PinyinCache::rebuild(store);
                }
                self.volumes[i].dirty = true;
                did = true;
            }
        }
        // 落盘:成功清 dirty;失败保留 dirty 并按指数退避重试(H6)。
        // 每轮最多落一个卷:一次 maintain 写 3×120MB 会把同一线程上的查询请求
        // 卡住数秒(用户可见的「搜索要好几秒,但显示只要 0.08s」)。
        let now = Instant::now();
        for i in 0..self.volumes.len() {
            if self.volumes[i].syncing {
                continue; // 对账期间不落盘
            }
            if !self.volumes[i].dirty {
                continue;
            }
            if let Some(retry_at) = self.volumes[i].save_retry_at {
                if now < retry_at {
                    continue;
                }
            }
            if self.save_volume(i) {
                did = true;
                break;
            }
        }
        did
    }

    /// 落盘单个卷。成功返回 true 并清 dirty;失败保留 dirty、指数退避(H6)。
    fn save_volume(&mut self, i: usize) -> bool {
        let save_result = {
            let v = &self.volumes[i];
            let Some(store) = &v.store else { return false };
            if v.syncing {
                // SEEN 位是内存临时态,落盘会让下次启动把未见条目误删。
                return false;
            }
            if store.is_scanning() && v.resume_frn == 0 {
                // 还没有任何扫描进度时落盘只会反复写空卷;等扫描推进后再保存
                // 部分快照(ZCode 9d93786 的行为),让重启后能先搜到已扫到的部分。
                return false;
            }
            let header = SnapshotHeader {
                guid: v.guid,
                letter: v.letter,
                complete: v.complete,
                exclude_noise: self.cfg.exclude_noise,
                journal_id: v.journal_id,
                next_usn: v.next_usn,
                resume_frn: v.resume_frn,
                est_records: v.est_records,
            };
            let started = Instant::now();
            let result = snapshot::save_report(
                &self.cfg.index_dir,
                &header,
                &mut self.volumes[i].store.as_mut().unwrap(),
            );
            let ms = started.elapsed().as_millis();
            if ms > 100 {
                crate::snapshot::debug_log(&format!(
                    "save vol {} took {}ms",
                    self.volumes[i].letter, ms
                ));
            }
            result
        };
        match save_result {
            Ok(report) => {
                if report.compacted {
                    // save 内部可能 compact(未归并 delta/扫描中/待处理改名)→ slot 重排。
                    // 拼音缓存以 slot 为键,必须同步重建,并把旧 Hit 代次作废,
                    // 否则下一次查询会拿旧 slot 越界 panic(m04259)。
                    self.bump_generation(i);
                    if let Some(store) = &self.volumes[i].store {
                        self.pinyin[i] = PinyinCache::rebuild(store);
                    }
                }
                let next_at = Instant::now() + Self::save_interval(&self.volumes[i]);
                let v = &mut self.volumes[i];
                v.dirty = false;
                v.save_backoff = 0;
                v.save_retry_at = Some(next_at);
                true
            }
            Err(err) => {
                eprintln!("lstr-index: save snapshot failed: {err}");
                crate::snapshot::debug_log(&format!("save {} failed: {err}", self.volumes[i].guid));
                let v = &mut self.volumes[i];
                v.save_backoff = v.save_backoff.saturating_add(1);
                // 1s, 2s, 4s … 上限 60s;dirty 保留,下一轮 maintain 再试
                let secs = (1u64 << v.save_backoff.min(6)).min(60);
                v.save_retry_at = Some(Instant::now() + Duration::from_secs(secs));
                false
            }
        }
    }

    /// 两次落盘的最小间隔:就绪卷只随 journal 增量变化,60s 一次足够;
    /// 扫描中的卷保留 20s 粒度作为断点(崩溃后最多重扫 20s)。
    fn save_interval(v: &VolumeState) -> Duration {
        let scanning = v.store.as_ref().map(|s| s.is_scanning()).unwrap_or(false);
        if scanning {
            Duration::from_secs(20)
        } else {
            Duration::from_secs(60)
        }
    }

    /// 退出前把所有脏快照落盘(忽略节流)。失败留给下次启动的重扫/增量流程。
    pub fn flush_all(&mut self) {
        for i in 0..self.volumes.len() {
            if self.volumes[i].syncing {
                continue; // 对账中途退出:让下次启动重新对账,不落半态
            }
            if self.volumes[i].dirty {
                self.volumes[i].save_retry_at = None;
                let _ = self.save_volume(i);
            }
        }
    }

    pub fn cfg(&self) -> &EngineConfig {
        &self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lstr-engine-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn engine_with(dir: &std::path::Path) -> Engine {
        engine_with_volumes(dir, Vec::new())
    }

    /// 指定勾选盘符(空 = 全部)构造引擎,用于验证快照加载过滤。
    fn engine_with_volumes(dir: &std::path::Path, volumes: Vec<char>) -> Engine {
        Engine::new(EngineConfig {
            index_dir: dir.to_path_buf(),
            exclude_noise: true,
            candidate_cap: 1000,
            volumes,
        })
    }

    fn rec(frn: u64, parent: u64, attrs: u32, name: &str) -> Rec {
        Rec { frn, parent_frn: parent, attrs, op: Op::Upsert, name: name.to_string(), mtime: 0 }
    }

    /// 走一遍 worker 的握手序列:VolumeMeta → ScanBegin → Records → VolumeReady。
    fn feed_ready(engine: &mut Engine, guid: u128, letter: char) {
        const DIR: u32 = 0x0010;
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter,
            journal_id: 7,
            next_usn: 100,
            est_records: 3,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![
                rec(10, 0, DIR, "docs"),
                rec(11, 10, 0, "alpha.txt"),
                rec(12, 10, 0, "beta.txt"),
            ],
            next_usn: 100,
        });
        engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 3 });
    }

    #[test]
    fn h6_dirty_cleared_after_successful_save() {
        let dir = temp_index_dir("h6");
        let guid = 0x1111u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        // VolumeReady 置 dirty → 第一次 maintain 落盘
        assert!(engine.maintain(), "first maintain should save the dirty volume");
        assert!(snapshot::snapshot_path(&dir, guid).is_file());

        // 落盘成功后 dirty 必须清空,否则 maintain 会永远重写快照(H6)
        assert!(!engine.maintain(), "dirty must be cleared after successful save");

        // 快照能被新引擎读回(计数/内容一致)
        let engine2 = engine_with(&dir);
        assert_eq!(engine2.volumes.len(), 1);
        assert_eq!(engine2.volumes[0].store.as_ref().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// journal 每 500ms 一批时,不能每批都整卷重写快照(会卡查询线程);
    /// 退出路径的 flush_all 必须无视节流。
    #[test]
    fn snapshot_saves_are_throttled_but_flushable() {
        let dir = temp_index_dir("throttle");
        let guid = 0x2222u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        assert!(engine.maintain(), "first maintain should save the completed volume");

        // journal 又推来一条记录 → dirty,但节流窗口未到不应重写整卷。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(13, 10, 0, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine.volumes[0].dirty);
        assert!(!engine.maintain(), "saves must be throttled while the interval is pending");

        // 节流窗口到期后才再次落盘。
        engine.volumes[0].save_retry_at = None;
        assert!(engine.maintain(), "save should run once the throttle expires");
        assert!(!engine.volumes[0].dirty);

        // 退出前 flush_all 忽略节流,保证最后一次 journal 增量不丢。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(14, 10, 0, "delta.txt")],
            next_usn: 300,
        });
        engine.flush_all();
        assert!(!engine.volumes[0].dirty);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// m04259: save 内部 compact(未归并 delta / 改名 / 扫描中)会重排 slot,
    /// engine 的拼音缓存必须随之重建,否则下一次查询按旧 slot 索引 → 越界 panic。
    #[test]
    fn pinyin_cache_is_rebuilt_after_save_time_compact() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("pinyin-save-compact");
        let guid = 0x4444u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        // VolumeReady 已 compact 过,这次落盘不会 compact。
        assert!(engine.maintain());

        // 删掉一条(dead→compact 时丢弃)+ 就地改名(renames>0,触发 save 内部 compact)。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![Rec {
                frn: 12,
                parent_frn: 10,
                attrs: 0,
                op: Op::Remove,
                name: String::new(),
                mtime: 0,
            }],
            next_usn: 150,
        });
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(11, 10, 0, "alpaca.txt")],
            next_usn: 160,
        });
        assert!(engine.volumes[0]
            .store
            .as_ref()
            .unwrap()
            .has_pending_rename());

        // 跳过落盘节流窗口(第一次 maintain 刚保存过)。
        engine.volumes[0].save_retry_at = None;
        assert!(engine.maintain(), "save-time compact should still write the snapshot");

        let store = engine.volumes[0].store.as_ref().unwrap();
        assert_eq!(store.len(), 2, "dead entry dropped by save-time compact");
        assert!(
            engine.pinyin[0]
                .slots
                .iter()
                .all(|&slot| (slot as usize) < store.len()),
            "pinyin cache must be rebuilt after save-time compact"
        );

        // 查询(ASCII token 会走拼音候选)不得 panic,total 仍能找到改名后的条目。
        let mut spec = QuerySpec::default();
        spec.tokens = vec!["alp".to_string()];
        let outcome = engine.query(&spec);
        assert!(outcome.total >= 1, "renamed entry should still be found");
        assert!(!outcome.aborted);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn h6_partial_scan_snapshot_saved_after_progress() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("h6partial");
        let guid = 0x31u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 3,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(10, 0, DIR, "docs"), rec(11, 10, 0, "alpha.txt")],
            next_usn: 100,
        });

        // 尚无扫描进度:不落盘,避免反复写空卷。
        assert!(!engine.maintain());
        assert!(!snapshot::snapshot_path(&dir, guid).is_file());

        // 有进度后保存部分快照;重启时 store 可先用于搜索,但仍标记未完成。
        engine.handle(EngineMsg::ScanProgress { guid, frn: 50, est: 100, count: 2 });
        assert!(engine.maintain());
        assert!(snapshot::snapshot_path(&dir, guid).is_file());
        let engine2 = engine_with(&dir);
        assert_eq!(engine2.volumes.len(), 1);
        assert!(!engine2.volumes[0].complete);
        assert_eq!(engine2.volumes[0].store.as_ref().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M2:批次位点只对「已就绪」的卷生效。扫描中的卷提前写 next_usn 会让
    /// 空/半 store 被 worker 误判成可增量;就绪后则必须推进到最新位点,否则
    /// 重连只能从扫描结束位置续传(重放巨量 journal)。
    #[test]
    fn m2_next_usn_only_tracks_ready_volumes() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("m2");
        let guid = 0x55u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 2,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        // 扫描中:即使批次带位点也不能落进握手状态
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(10, 0, DIR, "docs")],
            next_usn: 555,
        });
        assert_eq!(engine.handshake_state(), vec![(guid, 7, 100, 0, false)]);
        // M9:有扫描进度后,未完成卷带 resume_frn 回传,worker 可续扫而非从头全量
        engine.handle(EngineMsg::ScanProgress { guid, frn: 50, est: 100, count: 1 });
        assert_eq!(engine.handshake_state(), vec![(guid, 7, 100, 50, false)]);
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count: 1,
        });
        // 就绪后:批次位点推进,握手拿到的是最新位点而非扫描结束位点
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(11, 10, 0, "a.txt")],
            next_usn: 900,
        });
        assert_eq!(engine.handshake_state(), vec![(guid, 7, 900, 0, true)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn h8_stale_hits_invalidate_without_panicking() {
        let dir = temp_index_dir("h8");
        let g1 = 0x21u128;
        let g2 = 0x22u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, g1, 'C');
        feed_ready(&mut engine, g2, 'D');

        let spec = QuerySpec { tokens: vec!["alpha".into()], ..Default::default() };
        let out = engine.query(&spec);
        assert_eq!(out.hits.len(), 2, "each volume has one alpha.txt");
        let hit = *out
            .hits
            .iter()
            .find(|h| engine.volumes[h.vol as usize].guid == g1)
            .expect("hit on first volume");
        assert!(engine.path_of(&hit).is_some());

        // 重扫:store 原地更新 + 代次递增 → 旧 Hit 失效,但访问不得 panic
        engine.handle(EngineMsg::ScanBegin { guid: g1, resume_frn: 0 });
        assert!(engine.path_of(&hit).is_none());
        assert_eq!(engine.entry_name(&hit), "");
        assert!(!engine.is_dir(&hit));

        // 卷移除:swap_remove 后旧 Hit 要么越界、要么代次不符,一律拒绝
        feed_ready(&mut engine, g1, 'C');
        let out = engine.query(&spec);
        let hit = *out
            .hits
            .iter()
            .find(|h| engine.volumes[h.vol as usize].guid == g1)
            .expect("hit on first volume");
        assert!(engine.path_of(&hit).is_some());
        engine.handle(EngineMsg::VolumeGone { guid: g1 });
        assert!(engine.path_of(&hit).is_none());

        // 伪造/过期代次同样拒绝
        let stale = Hit { vol: 0, slot: 0, generation: hit.generation.wrapping_add(1) };
        assert!(engine.path_of(&stale).is_none());
        assert!(engine.pinyin_of(&stale).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 步:启动加载快照后,worker 只用 DirSync 逐批 patch;本轮没见到的条目
    /// 视为离线期间被删除,DirSyncEnd(ok=true) 后消失;中途停止(ok=false)
    /// 绝不能删条目。
    #[test]
    fn dir_sync_patches_snapshot_and_removes_vanished_entries() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("dirsync");
        let guid = 0x3333u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.volumes[0].complete);
        assert_eq!(engine.volumes[0].store.as_ref().unwrap().len(), 3);

        // 模拟启动对账:docs/alpha 仍在,beta.txt 已被删除,新增 gamma.txt。
        engine.handle(EngineMsg::DirSyncBegin { guid });
        assert!(engine.volumes[0].syncing);
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![
                rec(10, 0, DIR, "docs"),
                rec(11, 10, 0, "alpha.txt"),
                rec(13, 10, 0, "gamma.txt"),
            ],
            next_usn: 100,
        });
        // 对账期间不允许落盘(SEEN 位是临时态)
        assert!(engine.volumes[0].dirty);
        assert!(!engine.maintain(), "maintain must not save while syncing");

        engine.handle(EngineMsg::DirSyncEnd { guid, ok: false });
        assert!(!engine.volumes[0].syncing);
        {
            let store = engine.volumes[0].store.as_ref().unwrap();
            assert_eq!(store.len(), 4, "aborted sync must not delete anything");
            assert!(store.find_slot(12).is_some(), "beta must survive an aborted sync");
        }

        // 完整对账:beta 未见到 → 删除;gamma 新增;docs/alpha 保留。
        engine.handle(EngineMsg::DirSyncBegin { guid });
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![
                rec(10, 0, DIR, "docs"),
                rec(11, 10, 0, "alpha.txt"),
                rec(13, 10, 0, "gamma.txt"),
            ],
            next_usn: 100,
        });
        engine.handle(EngineMsg::DirSyncEnd { guid, ok: true });
        assert!(!engine.volumes[0].syncing);
        {
            let store = engine.volumes[0].store.as_ref().unwrap();
            assert!(store.find_slot(12).is_none(), "beta.txt vanished offline → removed");
            assert!(store.find_slot(10).is_some());
            assert!(store.find_slot(11).is_some());
            assert!(store.find_slot(13).is_some(), "gamma.txt added offline → present");
        }

        // 对账结果应能落盘并被下一次启动读回
        assert!(engine.maintain());
        let engine2 = engine_with(&dir);
        assert_eq!(engine2.volumes.len(), 1);
        let store2 = engine2.volumes[0].store.as_ref().unwrap();
        assert_eq!(store2.len(), 3);
        assert!(store2.find_slot(12).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 状态输出的卷顺序必须是盘符升序(C D E F),不能依赖内部
    /// read_dir / swap_remove 的偶然顺序。
    #[test]
    fn status_volumes_sorted_by_letter() {
        let dir = temp_index_dir("status-order");
        let mut engine = engine_with(&dir);
        // 刻意先喂 F 再喂 C:内部 push 顺序与期望显示顺序相反。
        feed_ready(&mut engine, 0x61u128, 'F');
        feed_ready(&mut engine, 0x62u128, 'C');

        let letters: Vec<char> = engine.status().volumes.iter().map(|v| v.letter).collect();
        assert_eq!(letters, vec!['C', 'F']);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// UsnWatch 状态上报(usn 后端):只更新已知卷、变化才记;状态经 status()
    /// 暴露给 UI,非 Off 才显示。Off 缺省下 UI 不显示,旧后端零噪音。
    #[test]
    fn usn_watch_state_updates_and_surfaces_in_status() {
        let dir = temp_index_dir("usn-watch");
        let guid = 0x81u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        // 缺省 Off:status 不带 usn_state(旧后端/未上报时 UI 无此段)
        // 游标 age 在 VolumeReady 后即存在(扫描完成的 Ready 也算一次推进)。
        let st = engine.status();
        assert_eq!(st.volumes.len(), 1);
        assert!(st.volumes[0].usn_state.is_none());
        assert!(st.volumes[0].cursor_age_secs.is_some());

        // worker 上报进入轮询 → Watching
        engine.handle(EngineMsg::UsnWatch { guid, state: UsnWatchState::Watching });
        let st = engine.status();
        assert_eq!(st.volumes[0].usn_state, Some(UsnWatchState::Watching));

        // 游标推进(就绪卷的 Records)→ cursor age 出现且为刚发生
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(20, 10, 0, "usn-new.txt")],
            next_usn: 120,
        });
        let st = engine.status();
        let age = st.volumes[0].cursor_age_secs.expect("cursor age after records");
        assert!(age <= 5, "刚推进的游标 age 应接近 0,实测 {age}s");

        // 退避 → 上报;Disabled 未知 guid 被忽略,不建新卷
        engine.handle(EngineMsg::UsnWatch { guid, state: UsnWatchState::Backoff });
        let st = engine.status();
        assert_eq!(st.volumes[0].usn_state, Some(UsnWatchState::Backoff));
        engine.handle(EngineMsg::UsnWatch { guid: 0xdead, state: UsnWatchState::Disabled });
        assert_eq!(engine.volumes.len(), 1, "未知 guid 不得凭空建卷");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// index.volumes 非空时,未选中卷的快照不加载(文件保留);
    /// 空 vec = 全部加载。
    #[test]
    fn load_snapshots_skips_unselected_volumes() {
        let dir = temp_index_dir("vol-filter");
        let guid_c = 0x71u128;
        let guid_f = 0x72u128;
        let mut engine = engine_with_volumes(&dir, Vec::new());
        feed_ready(&mut engine, guid_c, 'C');
        feed_ready(&mut engine, guid_f, 'F');
        engine.flush_all();
        assert!(snapshot::snapshot_path(&dir, guid_c).is_file());
        assert!(snapshot::snapshot_path(&dir, guid_f).is_file());

        // 只勾选 C:F 的 VolumeState / PinyinCache 不建,但快照文件必须保留。
        let only_c = engine_with_volumes(&dir, vec!['C']);
        let letters: Vec<char> = only_c.volumes.iter().map(|v| v.letter).collect();
        assert_eq!(letters, vec!['C']);
        assert_eq!(only_c.pinyin.len(), 1, "skip 卷不应建 PinyinCache");
        assert!(
            snapshot::snapshot_path(&dir, guid_f).is_file(),
            "未选中卷的快照文件必须保留"
        );

        // 空 = 全部:两卷都加载。
        let all = engine_with_volumes(&dir, Vec::new());
        let mut letters: Vec<char> = all.volumes.iter().map(|v| v.letter).collect();
        letters.sort();
        assert_eq!(letters, vec!['C', 'F']);
        assert_eq!(all.pinyin.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
