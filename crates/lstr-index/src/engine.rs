//! 引擎编排:多卷状态、消息处理、跨卷查询、状态汇总。
//!
//! 所有权模型:lstr 的搜索 worker 线程独占 Engine;管道线程把 EngineMsg
//! 塞进 channel,worker 在查询间隙 try_recv 应用(渐进构建期可搜)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::pinyin::PinyinCache;
use crate::query::{QueryOutcome, QuerySpec, Hit};
use crate::sidecar::{self, SidecarOp};
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
    /// 旁车日志:自上次全量保存以来的增量操作(按序,回放语义与运行期 apply
    /// 完全一致)。稳态变更先记这里,由 maintain 以 ~5s 节流原子写进旁车文件
    /// (几 KB,毫秒级);全量保存成功后清空。见 [`crate::sidecar`]。
    sidecar_ops: Vec<SidecarOp>,
    /// 旁车基线:当前日志所基于的全量快照游标 (journal_id, next_usn)。
    /// 加载/全量保存成功时更新;旁车文件基线与之不符即视为陈旧并丢弃。
    sidecar_base: (u64, i64),
    /// 旁车日志覆盖到的游标(= 最后一条已捕获批次的位点)。可与 next_usn
    /// 短暂不同:日志溢出停捕期间位点继续推进,旁车文件以 sidecar_cursor
    /// 为准 —— 游标永不超前于可回放内容,缺口由 journal 回放补齐。
    sidecar_cursor: i64,
    /// 内存日志与旁车文件不一致(有待写盘)。
    sidecar_pending: bool,
    /// 下次允许旁车写盘的时间(节流;失败按同间隔重试,写盘毫秒级,退避无必要)。
    sidecar_retry_at: Option<Instant>,
    /// 捕获开关:全量保存成功后武装;对账/重扫/重置窗口内关闭 —— 这些变更
    /// 的持久化走全量快照,不进旁车(关闭期间的位点推进由 journal 回放兜底)。
    capture_on: bool,
    /// USN 实时增量层状态(usn 后端 worker 经 UsnWatch 帧上报;默认 Off,
    /// 旧后端 worker 从不上报 → UI 不显示)。
    pub usn_watch: UsnWatchState,
    /// 游标上次推进时间(journal 批次 / VolumeReady):UI 的「游标 age」。
    cursor_at: Option<Instant>,
}

impl VolumeState {
    /// 旁车捕获是否可用:就绪卷(非对账/非扫描)且捕获已武装。武装在每次
    /// 全量保存成功后恢复;对账/重扫窗口内的变更走全量快照持久化。
    fn sidecar_capture_ready(&self) -> bool {
        self.capture_on
            && self.complete
            && !self.syncing
            && self.store.as_ref().map(|s| !s.is_scanning()).unwrap_or(false)
    }

    /// 旁车日志当前的变更条数(溢出判断)。
    fn sidecar_rec_count(&self) -> usize {
        self.sidecar_ops.iter().map(SidecarOp::recs_len).sum()
    }

    /// 捕获一批变更:进内存日志并标记待写盘;日志已满则停捕并请求立即
    /// 全量保存(游标停在最后一条已捕获批次,缺口由 journal 回放兜底)。
    fn capture(&mut self, op: SidecarOp, cursor: Option<i64>) {
        if self.sidecar_rec_count() < SIDECAR_MAX_RECS {
            self.sidecar_ops.push(op);
            if let Some(usn) = cursor {
                self.sidecar_cursor = usn;
            }
            self.sidecar_pending = true;
        } else {
            self.save_retry_at = None;
        }
    }
}

/// 对账/首扫 Records 分批 apply 的默认块大小(条)。
///
/// 依据:dirsync/dirscan 单条消息 4000 条、空闲一轮最多 400 条消息,整轮消化
/// 会把同线程上的查询堵住数百 ms~秒级(实测对账期查询 execute p50=218ms、
/// p90=512ms;稳态 0.4ms)。按 ~512 条(约 0.2-1ms 工作量)切块,块间让宿主
/// 线程回去服务查询,打字期间的单次等待从「一整轮」降到「一块」。
const DEFAULT_APPLY_CHUNK: usize = 512;

/// 就绪卷两次**全量**快照的最小间隔(秒)。
///
/// 依据(2026-11 实测):USN 稳态下就绪卷每 ~60s 全量保存一次,C 卷 117 万条
/// 每次占引擎线程 1.6-2s,查询每分钟撞上一次 1-3 秒停顿。稳态增量改由旁车
/// 游标承载(sidecar.rs,毫秒级写盘)后,全量快照只需做阶段归档,10 分钟一次
/// 足以把崩溃后的 journal 回放窗口控制在分钟级;对账收尾/优雅退出仍立即全量。
const FULL_SAVE_INTERVAL_SECS: u64 = 600;

/// 旁车写盘节流:USN 轮询 500ms 一批,变更按序积压在内存日志,合并 5s 原子
/// 写一次(几 KB)。崩溃最多丢一个节流窗口的日志 —— 游标以旁车文件里的为准,
/// 缺口由 journal 回放补齐,不存在丢数据。
const SIDECAR_SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// 旁车日志容量上限(变更条数):超出即触发立即全量保存并停捕(内存有界)。
/// 稳态远达不到;突发批量变更(如解压/同步盘)时回落全量保存的既有路径。
const SIDECAR_MAX_RECS: usize = 8192;

/// 渐进 apply 缓冲:对账/首扫的 Records 不再一口气整条消息打完,而是按块
/// 消化,块间让宿主线程回去服务查询。按卷分组,保证同卷记录的消费顺序与
/// 到达顺序一致(跨卷本就无序,与现行为一致)。
struct PendingApply {
    guid: u128,
    /// 待 apply 的剩余记录(FIFO)。
    recs: std::collections::VecDeque<Rec>,
    /// 缓冲内最后一条消息的 journal 位点:缓冲消化完时写入卷游标,
    /// 与一次性路径「每条消息更新一次」的最终状态等价。
    next_usn: i64,
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
    /// 对账/首扫 Records 的分批缓冲(见 [`PendingApply`])。
    pending_applies: Vec<PendingApply>,
    /// 分批块大小(条);0 = 关闭分批(一次性 apply,与旧路径等价)。
    apply_chunk_records: usize,
    /// 让路钩子:分批块间调用;true = 本线程有等待中的查询,apply 立即收手,
    /// 剩余块由宿主稍后继续消化。宿主(search worker)注入,复用 0294602
    /// 「打字时查询优先于索引维护」的判定,把优先级检查扩展到 apply 中段。
    query_yield: Option<Box<dyn Fn() -> bool + Send>>,
    /// 让路发生次数(诊断/基准断言:批间让路必须真的发生过)。
    apply_yields: u64,
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
            pending_applies: Vec::new(),
            apply_chunk_records: DEFAULT_APPLY_CHUNK,
            query_yield: None,
            apply_yields: 0,
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

    /// 从快照 header 构造加载态 VolumeState(旁车基线 = 快照游标,捕获武装)。
    fn volume_state_from_snapshot(
        guid: u128,
        h: &SnapshotHeader,
        store: Option<VolumeStore>,
        next_usn: i64,
        sidecar_ops: Vec<SidecarOp>,
        sidecar_base: (u64, i64),
        sidecar_cursor: i64,
    ) -> VolumeState {
        VolumeState {
            guid,
            letter: h.letter,
            journal_id: h.journal_id,
            next_usn,
            est_records: h.est_records,
            resume_frn: h.resume_frn,
            complete: h.complete,
            store,
            generation: 0,
            dirty: false,
            syncing: false,
            save_backoff: 0,
            save_retry_at: None,
            sidecar_ops,
            sidecar_base,
            sidecar_cursor,
            sidecar_pending: false,
            sidecar_retry_at: None,
            capture_on: true,
            usn_watch: UsnWatchState::Off,
            cursor_at: None,
        }
    }

    /// 把一条旁车操作回放进 store:语义与运行期 apply 完全一致(Journal =
    /// `apply_rec(syncing=false)` 逐条;Children = DirChildren 同款替换语义)。
    /// 回放输入是「刚加载的快照 + 前序旁车操作」,与捕获时的 store 状态同构,
    /// 故结果确定性一致。
    fn replay_sidecar_op(store: &mut VolumeStore, op: &SidecarOp) {
        match op {
            SidecarOp::Journal { recs } => {
                for rec in recs {
                    Self::apply_rec(store, rec, false);
                }
            }
            SidecarOp::Children { parent_frn, recs } => {
                if *parent_frn == ROOT_FRN {
                    store.apply_root_children(recs);
                } else {
                    let slot = store.find_slot(*parent_frn).unwrap_or(NO_PARENT);
                    store.apply_children(slot, recs);
                }
            }
        }
    }

    fn load_snapshots(&mut self) {
        let dir = self.cfg.index_dir.clone();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // 上次写快照/旁车中途崩溃/被杀留下的 temp 文件(vol_*.tmp / cur_*.tmp):
            // 清掉,避免长期堆积;若仍有并发写者,它会在 rename 前重建/覆盖。
            if name.ends_with(".tmp") {
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
                // 旁车:基线与快照游标完全一致才回放(回放语义与运行期 apply
                // 一致);基线不符(陈旧)/损坏 → 删除,恢复回落「快照游标 +
                // journal 回放」(journal 已回绕则 dirsync,语义自洽)。
                let mut next_usn = h.next_usn;
                let mut sidecar_ops: Vec<SidecarOp> = Vec::new();
                let sidecar_base = (h.journal_id, h.next_usn);
                let mut sidecar_cursor = h.next_usn;
                let mut store = Some(snap.store);
                match sidecar::load(&dir, guid) {
                    Ok(Some(sc)) if sc.base == sidecar_base && sc.next_usn >= h.next_usn => {
                        let s = store.as_mut().expect("snapshot store");
                        for op in &sc.ops {
                            Self::replay_sidecar_op(s, op);
                        }
                        snapshot::debug_log(&format!(
                            "sidecar {guid:032x}: replayed {} ops, cursor {} → {}",
                            sc.ops.len(),
                            h.next_usn,
                            sc.next_usn
                        ));
                        next_usn = sc.next_usn;
                        sidecar_cursor = sc.next_usn;
                        sidecar_ops = sc.ops;
                    }
                    Ok(Some(sc)) => {
                        snapshot::debug_log(&format!(
                            "sidecar {guid:032x}: stale base {:?} != snapshot {:?}, discarded",
                            sc.base, sidecar_base
                        ));
                        sidecar::remove(&dir, guid);
                    }
                    Ok(None) => {}
                    Err(()) => {
                        snapshot::debug_log(&format!(
                            "sidecar {guid:032x}: corrupt, discarded (recover via journal replay)"
                        ));
                        sidecar::remove(&dir, guid);
                    }
                }
                self.volumes.push(Self::volume_state_from_snapshot(
                    guid,
                    h,
                    store,
                    next_usn,
                    sidecar_ops,
                    sidecar_base,
                    sidecar_cursor,
                ));
                let i = self.volumes.len() - 1;
                self.bump_generation(i);
                // 回放了旁车增量的卷:这些变更不在全量快照里,须保持 dirty,
                // 由全量保存(降频)最终归档。
                self.volumes[i].dirty = !self.volumes[i].sidecar_ops.is_empty();
                // 加载后不立即全量保存:游标在快照/旁车里已持久;稳态走旁车,
                // 全量按 save_interval 降频(见常量注释)。节流从加载时刻起算。
                let interval = if self.volumes[i].complete {
                    Duration::from_secs(FULL_SAVE_INTERVAL_SECS)
                } else {
                    Duration::from_secs(20)
                };
                self.volumes[i].save_retry_at = Some(Instant::now() + interval);
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
                sidecar_ops: Vec::new(),
                sidecar_base: (0, 0),
                sidecar_cursor: 0,
                sidecar_pending: false,
                sidecar_retry_at: None,
                capture_on: false,
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
                    // 就绪卷不接受游标对 (journal_id, next_usn) 覆盖:Full 计划
                    // 下该值是扫描起点高水位,与随后的 ScanBegin 之间存在「游标
                    // 超前于内容」的窄落盘窗口;增量计划的 VolumeMeta 本就是握手
                    // 回显,跳过不改变语义。未就绪卷(首扫/重扫窗口)正常接受。
                    if !v.complete {
                        v.journal_id = journal_id;
                        v.next_usn = next_usn;
                    }
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
                        // 重扫窗口不进旁车:store 将整卷重建,断点由 20s 全量
                        // 快照承载。日志清零;旧旁车文件仍与旧快照自洽,留着
                        // (基线守卫在加载时兜底)。
                        v.capture_on = false;
                        v.sidecar_ops = Vec::new();
                        v.sidecar_pending = false;
                        v.sidecar_cursor = v.next_usn;
                        v.sidecar_base = (v.journal_id, v.next_usn);
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
                // 对账(dirsync)/首扫(scan 模式)期间的大批 Records 分批 apply:
                // 一轮 idle 最多 400 条消息 × 4000 条记录,整轮消化会把同线程上
                // 的查询堵住数百 ms~秒级。按块消化,块间查让路钩子(0294602 的
                // 打字优先扩展),钩子触发时剩余留待宿主下轮继续。
                // 条件取「!complete」而非存储扫描态(H-1):首扫第一次部分保存
                // 必做 compact,之后 is_scanning() 翻 false,按存储态判定会让
                // 扫描剩余阶段退回一次性 apply,让路优化失效。
                let chunked = self.apply_chunk_records > 0
                    && (self.volumes[i].syncing || !self.volumes[i].complete);
                if chunked {
                    match self.pending_applies.iter_mut().find(|p| p.guid == guid) {
                        Some(buf) => {
                            // 同卷追加:消息顺序即消费顺序,保序
                            buf.recs.extend(recs);
                            buf.next_usn = next_usn;
                        }
                        None => {
                            self.pending_applies.push(PendingApply {
                                guid,
                                recs: recs.into(),
                                next_usn,
                            });
                        }
                    }
                    // 立即连续消化:无钩子(或无等待查询)时一口气消化完,
                    // 最终状态与一次性 apply 完全一致;有等待查询则收手。
                    self.drain_pending_applies();
                } else {
                    let syncing = self.volumes[i].syncing;
                    if let Some(store) = &mut self.volumes[i].store {
                        for rec in &recs {
                            Self::apply_rec(store, rec, syncing);
                        }
                        self.volumes[i].dirty = true;
                    }
                    // 旁车捕获:就绪卷的稳态变更按 apply 同序记进内存日志,
                    // 由 maintain 以 ~5s 节流原子写盘 —— 全量快照因此降频到
                    // FULL_SAVE_INTERVAL,查询不再每分钟撞上整卷重写。
                    if self.volumes[i].sidecar_capture_ready() {
                        self.volumes[i].capture(
                            SidecarOp::Journal { recs: recs.clone() },
                            Some(next_usn),
                        );
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
                // 首扫的分批缓冲先消化完:compact / 拼音缓存重建的前提是
                // 记录已全部 apply,分批路径必须先补齐再走原逻辑。
                self.drain_pending_for(guid);
                let compacted = {
                    let v = &mut self.volumes[i];
                    v.journal_id = journal_id;
                    v.next_usn = next_usn;
                    v.complete = true;
                    v.resume_frn = 0;
                    v.dirty = true;
                    v.cursor_at = Some(Instant::now());
                    // 扫完的卷立即落一次盘(不等节流):这是最值钱的快照。
                    // 保存成功前关闭旁车捕获(重扫内容不在旧旁车基线里)。
                    v.capture_on = false;
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
                    // 对账窗口不进旁车:patch/删除语义由 DirSyncEnd 后的全量
                    // 保存承载(关闭捕获,保存成功后重新武装)。
                    self.volumes[i].capture_on = false;
                    snapshot::debug_log(&format!("dirsync begin {guid:032x}"));
                }
            }
            EngineMsg::DirSyncEnd { guid, ok } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                // 分批缓冲先消化完(与一次性路径等价):ok=true 时 remove_unseen
                // 依赖「所有记录已 apply 并标记 SEEN」;ok=false 时一次性路径同样
                // 会把已到达的记录 apply 进 store(随后只清 SEEN、不删除)。
                self.drain_pending_for(guid);
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
                // 旁车捕获:替换语义整条记录(parent_frn 存 FRN,回放时重解析)
                if self.volumes[i].sidecar_capture_ready() {
                    self.volumes[i]
                        .capture(SidecarOp::Children { parent_frn, recs: recs.clone() }, None);
                }
                if added > 0 || removed > 0 {
                    snapshot::debug_log(&format!(
                        "watch {guid:032x} parent={parent_frn} added={added} removed={removed}"
                    ));
                }
            }
            EngineMsg::JournalReset { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    // 索引将清空:未消化的分批缓冲随旧卷状态一起丢弃
                    self.pending_applies.retain(|p| p.guid != guid);
                    {
                        let v = &mut self.volumes[i];
                        v.store = None;
                        v.complete = false;
                        v.next_usn = 0;
                        v.journal_id = 0;
                        v.dirty = false;
                        v.capture_on = false;
                        v.sidecar_ops = Vec::new();
                        v.sidecar_pending = false;
                        v.sidecar_base = (0, 0);
                        v.sidecar_cursor = 0;
                    }
                    // 索引已清空,旧 Hit 一律失效(H8)
                    self.bump_generation(i);
                    self.pinyin[i] = PinyinCache::empty();
                    let dir = self.cfg.index_dir.clone();
                    let _ = snapshot::snapshot_path(&dir, guid);
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&dir, guid));
                    sidecar::remove(&dir, guid);
                }
            }
            EngineMsg::VolumeGone { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    // 未消化的分批缓冲随卷丢弃
                    self.pending_applies.retain(|p| p.guid != guid);
                    // swap_remove:O(1) 收尾。代次跨卷唯一,旧 Hit 的 (vol,generation)
                    // 要么越界、要么代次不符,由访问器的校验拒绝(H8)。
                    self.volumes.swap_remove(i);
                    self.pinyin.swap_remove(i);
                    let dir = self.cfg.index_dir.clone();
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&dir, guid));
                    sidecar::remove(&dir, guid);
                }
            }
            EngineMsg::VolumeAppeared { guid, letter } => {
                let _ = self.volume_idx(guid, true, letter);
            }
        }
        self.recompute_phase();
    }

    /// 注入查询让路钩子(见 [`Engine::query_yield`])。宿主(search worker)
    /// 在线程内检查「是否有等待中的查询请求」并注入;测试/基准可注入任意判定。
    pub fn set_query_yield_hook(&mut self, hook: Option<Box<dyn Fn() -> bool + Send>>) {
        self.query_yield = hook;
    }

    /// 调整分批块大小(条);0 = 关闭分批(一次性 apply)。测试/基准用;
    /// 生产路径用默认值([`DEFAULT_APPLY_CHUNK`])。
    pub fn set_apply_chunk_records(&mut self, n: usize) {
        self.apply_chunk_records = n;
    }

    /// 是否还有未消化完的分批记录(宿主在消化完之前不应消费后续消息,
    /// 引擎内部已保证同卷顺序,这里供调度方判断何时让路)。
    pub fn has_pending_apply(&self) -> bool {
        self.pending_applies.iter().any(|p| !p.recs.is_empty())
    }

    /// 指定卷是否还有未消化完的分批记录(B-1 落盘守卫:缓冲非空时
    /// resume_frn 可能已超前于 store 内容,落盘即持久化裂缝快照)。
    fn has_pending_apply_for(&self, guid: u128) -> bool {
        self.pending_applies.iter().any(|p| p.guid == guid && !p.recs.is_empty())
    }

    /// 分批让路发生次数:批间检查到「有线程内等待中的查询」而收手的次数
    /// (诊断/基准断言:批间让路必须真的发生过)。
    pub fn apply_yield_count(&self) -> u64 {
        self.apply_yields
    }

    /// 让路钩子是否触发(有线程内等待中的查询)。无钩子 = false。
    fn query_pending(&self) -> bool {
        self.query_yield.as_ref().map(|f| f()).unwrap_or(false)
    }

    /// 逐条 apply 语义(一次性路径与分批路径共用,保证最终状态一致):
    /// Upsert = insert_meta,对账期间再记「本轮见过」(DirSyncEnd 据此删
    /// 离线消失项);Remove = 打 DEAD 位(等 compact 回收)。
    fn apply_rec(store: &mut VolumeStore, rec: &Rec, syncing: bool) {
        match rec.op {
            Op::Upsert => {
                store.insert_meta(rec.frn, rec.parent_frn, rec.attrs, rec.name.as_bytes(), rec.mtime);
                if syncing {
                    store.mark_seen(rec.frn);
                }
            }
            Op::Remove => {
                store.remove(rec.frn);
            }
        }
    }

    /// 消化一块分批记录:从第一个非空缓冲取至多 [`Engine::apply_chunk_records`]
    /// 条,逐条按一次性路径同款语义 apply;缓冲消化完即收尾——游标推进到
    /// 缓冲内最后一条消息的位点(与一次性路径的最终状态等价)。
    /// 返回实际 apply 的条数;0 = 没有待消化内容。
    fn apply_one_chunk(&mut self) -> usize {
        let Some(pi) = self.pending_applies.iter().position(|p| !p.recs.is_empty()) else {
            return 0;
        };
        let guid = self.pending_applies[pi].guid;
        let chunk = self.apply_chunk_records.max(1);
        let batch: Vec<Rec> = {
            let buf = &mut self.pending_applies[pi];
            buf.recs.drain(..buf.recs.len().min(chunk)).collect()
        };
        let applied = batch.len();
        let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
            // 卷已移除(热插拔兜底,正常不会走到):缓冲随卷丢弃
            self.pending_applies.remove(pi);
            return applied;
        };
        let syncing = self.volumes[i].syncing;
        if let Some(store) = &mut self.volumes[i].store {
            for rec in &batch {
                Self::apply_rec(store, rec, syncing);
            }
            self.volumes[i].dirty = true;
        }
        if self.pending_applies[pi].recs.is_empty() {
            let next_usn = self.pending_applies[pi].next_usn;
            // 与一次性路径等价的游标推进:消化完这批消息后写最后位点
            // (仅就绪卷生效,见 Records 处理的 M2 注释)。
            if self.volumes[i].complete {
                self.volumes[i].next_usn = next_usn;
                self.volumes[i].cursor_at = Some(Instant::now());
            }
            self.pending_applies.remove(pi);
        }
        applied
    }

    /// 消化一个分批块;消费前先查让路钩子,「有线程内等待中的查询」时收手
    /// 返回 0,剩余由宿主稍后继续(宿主服务完查询再调本方法续传)。
    /// 返回本块实际 apply 的条数;0 = 无待消化内容或本次让路。
    pub fn advance_pending_apply(&mut self) -> usize {
        if !self.has_pending_apply() {
            return 0;
        }
        if self.query_pending() {
            self.apply_yields += 1;
            return 0;
        }
        self.apply_one_chunk()
    }

    /// 连续消化直到清空或让路钩子触发(handle 内部驱动;无钩子或无等待查询时
    /// 一口气消化完,与一次性 apply 等价)。
    fn drain_pending_applies(&mut self) {
        while self.has_pending_apply() {
            if self.query_pending() {
                self.apply_yields += 1;
                return;
            }
            self.apply_one_chunk();
        }
    }

    /// 把指定卷的分批缓冲全部消化完(无视让路钩子):控制消息
    /// (DirSyncEnd/VolumeReady)的语义建立在「本卷记录已全部 apply」之上,
    /// 必须先补齐再走原逻辑,保证最终状态与一次性 apply 完全一致。
    fn drain_pending_for(&mut self, guid: u128) {
        loop {
            let Some(pi) = self.pending_applies.iter().position(|p| p.guid == guid) else {
                return;
            };
            if self.pending_applies[pi].recs.is_empty() {
                self.pending_applies.remove(pi);
                return;
            }
            self.apply_one_chunk();
        }
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

    /// 空闲维护:需要的卷 compact + 落盘(旁车优先,全量降频)。
    /// 返回是否做了事(调用方可据此让路)。
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
        let now = Instant::now();
        // 旁车写盘:几 KB 原子写(毫秒级),不与查询抢窗口;每卷独立节流。
        // 旁车内容 = 快照游标之后的增量,游标/变更由此持久化(见 sidecar.rs)。
        for i in 0..self.volumes.len() {
            let v = &mut self.volumes[i];
            if v.syncing || !v.sidecar_pending || !v.sidecar_capture_ready() {
                continue;
            }
            if let Some(at) = v.sidecar_retry_at {
                if now < at {
                    continue;
                }
            }
            if self.write_sidecar(i) {
                did = true;
            }
        }
        // 全量保存:成功清 dirty;失败保留 dirty 并按指数退避重试(H6)。
        // 每轮最多落一个卷:一次 maintain 写 3×120MB 会把同一线程上的查询请求
        // 卡住数秒(用户可见的「搜索要好几秒,但显示只要 0.08s」)。
        // 就绪卷在有线程内等待中的查询时推迟启动 —— 一次全量写 120MB(实测
        // 1.6-2s),让路钩子复用:把「查询等保存」变成「保存等查询」。
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
            let ready = self.volumes[i].complete
                && self.volumes[i]
                    .store
                    .as_ref()
                    .map(|s| !s.is_scanning())
                    .unwrap_or(false);
            if ready && self.query_pending() {
                continue;
            }
            if self.save_volume(i) {
                did = true;
                break;
            }
        }
        did
    }

    /// 写旁车文件(内存日志 → 原子小文件)。成功清 pending 并进入节流窗口;
    /// 失败保留 pending(内存日志是权威副本),下轮再试(写盘毫秒级,固定
    /// 间隔重试即可,无需退避)。
    fn write_sidecar(&mut self, i: usize) -> bool {
        let (guid, letter, base, cursor, ops) = {
            let v = &self.volumes[i];
            (v.guid, v.letter, v.sidecar_base, v.sidecar_cursor, v.sidecar_ops.clone())
        };
        let started = Instant::now();
        let result = sidecar::save(&self.cfg.index_dir, guid, base, cursor, &ops);
        let ms = started.elapsed().as_millis();
        if ms > 100 {
            crate::snapshot::debug_log(&format!(
                "sidecar vol {letter} took {}ms ({} ops)",
                ms,
                ops.len()
            ));
        }
        let v = &mut self.volumes[i];
        match result {
            Ok(()) => {
                v.sidecar_pending = false;
                v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
                true
            }
            Err(err) => {
                eprintln!("lstr-index: save sidecar failed: {err}");
                crate::snapshot::debug_log(&format!("sidecar {guid:032x} failed: {err}"));
                v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
                false
            }
        }
    }

    /// 落盘单个卷。成功返回 true 并清 dirty;失败保留 dirty、指数退避(H6)。
    fn save_volume(&mut self, i: usize) -> bool {
        // B-1:分批缓冲未消化完(首扫+让路压住)时禁止落盘 —— ScanProgress 已
        // 推进 resume_frn,而 FRN < resume_frn 的一段还压在缓冲里,此刻保存
        // 会持久化「游标超前于内容」的裂缝快照,崩溃重启后该段永久无声丢失。
        // 保留 dirty、不动 save_retry_at:缓冲清空后的下一轮 maintain 即可保存。
        if self.has_pending_apply_for(self.volumes[i].guid) {
            return false;
        }
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
                let dir = self.cfg.index_dir.clone();
                let v = &mut self.volumes[i];
                v.dirty = false;
                v.save_backoff = 0;
                v.save_retry_at = Some(next_at);
                // 全量快照已覆盖全部变更:清旁车日志、基线推进到本次游标、
                // 删旁车文件(基线守卫兜底删除失败的情形,双保险)。
                v.sidecar_base = (v.journal_id, v.next_usn);
                v.sidecar_cursor = v.next_usn;
                v.sidecar_ops = Vec::new();
                v.sidecar_pending = false;
                v.sidecar_retry_at = None;
                v.capture_on = true;
                sidecar::remove(&dir, v.guid);
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

    /// 两次**全量**落盘的最小间隔:扫描中的卷保留 20s 粒度作为断点(崩溃后
    /// 最多重扫 20s);就绪卷降频到 [`FULL_SAVE_INTERVAL_SECS`] —— 稳态增量由
    /// 旁车游标承载(毫秒级写盘),全量快照只做阶段归档(见常量注释)。
    fn save_interval(v: &VolumeState) -> Duration {
        let scanning = v.store.as_ref().map(|s| s.is_scanning()).unwrap_or(false);
        if scanning {
            Duration::from_secs(20)
        } else {
            Duration::from_secs(FULL_SAVE_INTERVAL_SECS)
        }
    }

    /// 退出前收尾(忽略节流)。失败留给下次启动的重扫/增量流程。
    ///
    /// 就绪卷的稳态变更以旁车收尾(毫秒级):游标+增量持久化语义与稳态完全
    /// 一致,退出不必等整卷重写 —— 旧实现每卷全量保存 1.6-2s,四卷退出要
    /// 卡 6s+;全量归档由下一会话的降频保存完成(dirty 保留,加载即恢复)。
    /// 旁车已自洽(无待写变更)的卷直接跳过,不整卷重写;旁车写盘失败则
    /// 回落全量保存,退出前变更必须以任一自洽形式落盘。
    /// 对账收尾/扫描断点(捕获未武装/扫描中)仍全量落盘。
    pub fn flush_all(&mut self) {
        for i in 0..self.volumes.len() {
            if self.volumes[i].syncing {
                continue; // 对账中途退出:让下次启动重新对账,不落半态
            }
            if !self.volumes[i].dirty {
                continue;
            }
            // B-1:分批缓冲未消化完时宁可不存 —— 落盘会持久化「resume_frn
            // 超前于 store 内容」的裂缝快照;重启从上一份自洽快照续扫即可。
            if self.has_pending_apply_for(self.volumes[i].guid) {
                continue;
            }
            if self.volumes[i].sidecar_capture_ready() {
                if self.volumes[i].sidecar_pending {
                    self.volumes[i].sidecar_retry_at = None;
                    if self.write_sidecar(i) {
                        continue;
                    }
                    // 旁车写盘失败:落到下面的整卷保存,变更不丢
                } else {
                    // 无待写旁车变更:盘上旁车本就自洽,跳过整卷重写
                    // (每卷 1.6-2s 的退出卡顿);dirty 保留,全量归档由下一
                    // 会话的降频保存完成。
                    continue;
                }
            }
            self.volumes[i].save_retry_at = None;
            let _ = self.save_volume(i);
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

    /// attrs=0 的便捷版(旁车测试用)。
    fn rec_file(frn: u64, parent: u64, name: &str) -> Rec {
        rec(frn, parent, 0, name)
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

    /// journal 每 500ms 一批时,不能每批都整卷重写快照(会卡查询线程):
    /// 节流窗口内的稳态变更走旁车(毫秒级),全量快照按 FULL_SAVE_INTERVAL
    /// 降频;退出路径的 flush_all 必须无视节流。
    #[test]
    fn snapshot_saves_are_throttled_but_flushable() {
        let dir = temp_index_dir("throttle");
        let guid = 0x2222u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        assert!(engine.maintain(), "first maintain should save the completed volume");
        let snap_path = snapshot::snapshot_path(&dir, guid);
        let before = std::fs::read(&snap_path).unwrap();
        assert!(sidecar::load(&dir, guid).unwrap().is_none(), "全量保存后旁车应不存在");

        // journal 又推来一条记录 → dirty;节流窗口未到:只写旁车,不重写整卷。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine.volumes[0].dirty);
        assert!(engine.maintain(), "旁车写盘是毫秒级动作,节流窗口内也应执行");
        assert!(sidecar::path(&dir, guid).is_file(), "旁车应承载稳态变更");
        assert_eq!(
            std::fs::read(&snap_path).unwrap(),
            before,
            "全量快照在节流窗口内不得被重写"
        );

        // 全量节流窗口到期(模拟:清 retry_at)→ 整卷保存,旁车随成功删除。
        engine.volumes[0].save_retry_at = None;
        assert!(engine.maintain(), "full save should run once the throttle expires");
        assert!(!engine.volumes[0].dirty);
        assert!(sidecar::load(&dir, guid).unwrap().is_none(), "全量保存成功后旁车应删除");
        assert_ne!(std::fs::read(&snap_path).unwrap(), before, "快照应已重写");

        // 退出前 flush_all:就绪卷稳态变更以旁车收尾(毫秒级,退出不等
        // 整卷重写);最后一次 journal 增量不丢,全量归档留给下一会话。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(14, 10, "delta.txt")],
            next_usn: 300,
        });
        engine.flush_all();
        let sc = sidecar::load(&dir, guid).unwrap().unwrap();
        assert_eq!(sc.next_usn, 300, "最后一次增量经旁车持久");
        assert!(engine.volumes[0].dirty, "全量归档由下一会话的降频保存完成");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旁车游标/增量日志:稳态变更只写旁车(全量快照不动),重启后
    /// 「快照 + 旁车回放」恢复出与退出时刻一致的 store 与游标;
    /// 旁车日志跨重启延续累积,直到下一次全量保存归档。
    #[test]
    fn sidecar_carries_cursor_and_delta_without_full_save() {
        let dir = temp_index_dir("sidecar");
        let guid = 0xa1u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain(), "首次全量保存");
        let snap_path = snapshot::snapshot_path(&dir, guid);
        let before = std::fs::read(&snap_path).unwrap();

        // 稳态批次:捕获进旁车,全量快照不动
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine.volumes[0].sidecar_pending);
        assert!(engine.maintain());
        assert!(!engine.volumes[0].sidecar_pending);
        assert_eq!(std::fs::read(&snap_path).unwrap(), before, "全量快照不得被重写");
        let sc = sidecar::load(&dir, guid).unwrap().expect("sidecar written");
        assert_eq!(sc.base, (7, 100));
        assert_eq!(sc.next_usn, 200);
        assert_eq!(sc.ops.len(), 1);

        // 重启:快照 + 旁车回放 → 游标推进、条目恢复、日志延续
        let mut engine2 = engine_with(&dir);
        assert_eq!(engine2.handshake_state(), vec![(guid, 7, 200, 0, true)]);
        let store2 = engine2.volumes[0].store.as_ref().unwrap();
        assert!(store2.find_slot(13).is_some(), "旁车增量应已回放");
        assert_eq!(engine2.volumes[0].sidecar_ops.len(), 1, "日志须保留以延续累积");
        assert!(engine2.volumes[0].dirty, "旁车内容不在全量快照里,须标记 dirty");

        // 继续稳态:第二批次并入同一份旁车(节流窗口内每批合并写一次)
        engine2.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(14, 10, "delta.txt")],
            next_usn: 300,
        });
        assert!(engine2.maintain(), "sidecar write should run");
        let sc = sidecar::load(&dir, guid).unwrap().unwrap();
        assert_eq!(sc.next_usn, 300);
        assert_eq!(sc.ops.len(), 2, "重启后旁车日志延续累积");
        assert_eq!(std::fs::read(&snap_path).unwrap(), before, "全量快照仍不得被重写");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// watcher 目录刷新(DirChildren 替换语义)同样经旁车恢复:删除的子项
    /// 在回放时同样消失,与运行期 apply 结果一致。
    #[test]
    fn dir_children_changes_recover_via_sidecar() {
        let dir = temp_index_dir("sidecar-children");
        let guid = 0xa2u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());

        // 目录 10 的子项刷新:beta.txt 消失、child.txt 新增
        engine.handle(EngineMsg::DirChildren {
            guid,
            parent_frn: 10,
            recs: vec![rec_file(11, 10, "alpha.txt"), rec_file(15, 10, "child.txt")],
        });
        assert!(engine.volumes[0].dirty);
        assert!(engine.maintain(), "sidecar write should run");
        let sc = sidecar::load(&dir, guid).unwrap().unwrap();
        assert_eq!(sc.ops.len(), 1, "DirChildren 应被捕获");

        let engine2 = engine_with(&dir);
        let store2 = engine2.volumes[0].store.as_ref().unwrap();
        assert!(store2.find_slot(15).is_some(), "新增子项应恢复");
        assert!(store2.find_slot(12).is_none(), "替换语义:未出现在刷新里的子项应标死");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏旁车:加载失败即删除,回落「快照游标 + journal 回放」,绝不半态。
    #[test]
    fn corrupt_sidecar_falls_back_to_snapshot() {
        let dir = temp_index_dir("sidecar-corrupt");
        let guid = 0xa3u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine.maintain());

        std::fs::write(sidecar::path(&dir, guid), b"junk-not-a-sidecar").unwrap();
        let engine2 = engine_with(&dir);
        assert_eq!(engine2.handshake_state(), vec![(guid, 7, 100, 0, true)], "回落快照游标");
        assert!(
            engine2.volumes[0].store.as_ref().unwrap().find_slot(13).is_none(),
            "损坏旁车不回放:增量由重启后的 journal 回放补齐"
        );
        assert!(!sidecar::path(&dir, guid).exists(), "损坏旁车应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 基线不符(陈旧)旁车:全量保存后残留的旧文件必须被识别并丢弃 ——
    /// 它的内容已包含在更新的快照里,回放会双重施加。
    #[test]
    fn stale_sidecar_base_is_discarded() {
        let dir = temp_index_dir("sidecar-stale");
        let guid = 0xa4u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());

        // 伪造旧基线的旁车(base=(7,42) != 快照游标 (7,100))
        sidecar::save(
            &dir,
            guid,
            (7, 42),
            500,
            &[SidecarOp::Journal { recs: vec![rec_file(99, 10, "stale.txt")] }],
        )
        .unwrap();
        let engine2 = engine_with(&dir);
        assert_eq!(engine2.handshake_state(), vec![(guid, 7, 100, 0, true)]);
        assert!(
            engine2.volumes[0].store.as_ref().unwrap().find_slot(99).is_none(),
            "陈旧旁车不得回放"
        );
        assert!(!sidecar::path(&dir, guid).exists(), "陈旧旁车应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旁车日志溢出:停捕并请求立即全量保存(内存有界,回落全量路径)。
    #[test]
    fn sidecar_overflow_triggers_full_save() {
        let dir = temp_index_dir("sidecar-overflow");
        let guid = 0xa5u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        let snap_path = snapshot::snapshot_path(&dir, guid);
        let before = std::fs::read(&snap_path).unwrap();

        // 连续灌入超过 SIDECAR_MAX_RECS 条变更
        for k in 0..(SIDECAR_MAX_RECS / 100 + 2) {
            let base = 1000u64 + k as u64 * 100;
            let recs: Vec<Rec> = (0..100u64)
                .map(|j| rec_file(base + j, 10, &format!("bulk{k:03}_{j:03}.txt")))
                .collect();
            engine.handle(EngineMsg::Records { guid, recs, next_usn: 200 + k as i64 });
        }
        assert!(
            engine.volumes[0].sidecar_rec_count() <= SIDECAR_MAX_RECS + 100,
            "溢出后停止捕获,内存日志有界"
        );
        assert_eq!(engine.volumes[0].save_retry_at, None, "溢出应请求立即全量保存");

        assert!(engine.maintain(), "溢出后全量保存应立即执行");
        assert!(!engine.volumes[0].dirty);
        assert!(sidecar::load(&dir, guid).unwrap().is_none(), "全量保存后旁车删除");
        assert_ne!(std::fs::read(&snap_path).unwrap(), before, "快照应已重写");
        assert!(engine.volumes[0].sidecar_ops.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 让路钩子复用到全量保存:有线程内等待中的查询时,就绪卷的全量保存
    /// 推迟到下一轮空闲(旁车写盘不受影响 —— 毫秒级);无等待查询时照常执行。
    #[test]
    fn full_save_yields_to_pending_query() {
        let dir = temp_index_dir("save-yield");
        let guid = 0xa6u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        let snap_path = snapshot::snapshot_path(&dir, guid);
        let before = std::fs::read(&snap_path).unwrap();

        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook = force.clone();
        engine.set_query_yield_hook(Some(Box::new(move || {
            hook.load(std::sync::atomic::Ordering::Relaxed)
        })));

        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        engine.volumes[0].save_retry_at = None; // 模拟全量窗口已过

        // 有等待中的查询:全量保存推迟,快照不动(旁车照常,毫秒级)
        force.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(engine.maintain(), "旁车写盘不应被让路阻塞");
        assert_eq!(std::fs::read(&snap_path).unwrap(), before, "全量保存应让路");
        assert!(engine.volumes[0].dirty, "dirty 保留,下一轮再全量");

        // 查询服务完:全量保存执行
        force.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(engine.maintain());
        assert!(!engine.volumes[0].dirty);
        assert_ne!(std::fs::read(&snap_path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 加载后不立即全量保存:快照/旁车里游标已持久,稳态首变走旁车,
    /// 全量按 10 分钟降频(避免每次启动都撞一次整卷重写)。
    #[test]
    fn loaded_volume_sidecars_instead_of_full_save() {
        let dir = temp_index_dir("sidecar-loaded");
        let guid = 0xa7u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        let snap_path = snapshot::snapshot_path(&dir, guid);
        let before = std::fs::read(&snap_path).unwrap();

        let mut engine2 = engine_with(&dir);
        engine2.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine2.maintain());
        assert_eq!(std::fs::read(&snap_path).unwrap(), before, "加载后稳态首变只写旁车");
        assert!(sidecar::path(&dir, guid).is_file());
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

    /// 就绪卷收到带高水位游标的 VolumeMeta(Full 计划的扫描起点高水位):
    /// 游标对 (journal_id, next_usn) 不得被覆盖 —— 否则到随后的 ScanBegin
    /// 之间存在「游标超前于内容」的窄落盘窗口。增量计划的 VolumeMeta 本就是
    /// 握手回显,跳过不改变语义;未就绪卷(重扫窗口)仍正常接受。
    #[test]
    fn volume_meta_does_not_overwrite_cursor_of_ready_volume() {
        let dir = temp_index_dir("meta-cursor");
        let guid = 0xb3u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C'); // jid=7 usn=100,complete
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 900,
        });
        assert_eq!(engine.handshake_state(), vec![(guid, 7, 900, 0, true)]);

        // Full 计划的 VolumeMeta(高水位 5000 + 新 journal_id):就绪卷跳过
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 9,
            next_usn: 5000,
            est_records: 10,
        });
        let v = &engine.volumes[0];
        assert_eq!((v.journal_id, v.next_usn), (7, 900), "就绪卷游标不得被覆盖");
        assert_eq!(engine.handshake_state(), vec![(guid, 7, 900, 0, true)]);

        // 未就绪卷(ScanBegin 之后的重扫窗口)仍接受覆盖:首扫/续扫语义不变
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 9,
            next_usn: 5000,
            est_records: 10,
        });
        let v = &engine.volumes[0];
        assert_eq!((v.journal_id, v.next_usn), (9, 5000), "未就绪卷仍接受游标");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// flush_all:就绪卷旁车写盘失败时必须回落全量保存 —— 退出前变更要以
    /// 任一自洽形式落盘,不能因旁车失败而整卷丢掉。
    #[test]
    fn flush_all_falls_back_to_full_save_when_sidecar_fails() {
        let dir = temp_index_dir("flush-sidecar-fail");
        let guid = 0xb4u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain()); // 首次全量保存,武装旁车捕获
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine.volumes[0].sidecar_pending);

        // 阻断旁车写盘:把 tmp 路径占成目录,File::create 必失败
        let tmp = dir.join(format!("cur_{guid:032x}.tmp"));
        std::fs::create_dir_all(&tmp).unwrap();
        engine.flush_all();

        // 回落全量:快照已含 gamma,重启直接读到;dirty 随全量成功清除
        let engine2 = engine_with(&dir);
        assert!(
            engine2.volumes[0].store.as_ref().unwrap().find_slot(13).is_some(),
            "旁车失败必须回落全量保存,变更不得丢"
        );
        assert!(!engine.volumes[0].dirty, "全量保存成功后 dirty 清");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// flush_all:就绪卷捕获已武装且无待写旁车变更时,跳过整卷全量保存
    /// (旁车文件在盘上本就自洽;每卷 1.6-2s 的退出整卷重写是纯浪费),
    /// dirty 保留给下一会话的降频保存归档;恢复语义不受影响。
    #[test]
    fn flush_all_skips_full_save_when_sidecar_already_persisted() {
        let dir = temp_index_dir("flush-no-pending");
        let guid = 0xb5u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        let snap_path = snapshot::snapshot_path(&dir, guid);
        let before = std::fs::read(&snap_path).unwrap();

        // 稳态变更已经旁车落盘(无待写):dirty 仍真(未归档),旁车自洽
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(13, 10, "gamma.txt")],
            next_usn: 200,
        });
        assert!(engine.maintain(), "旁车写盘");
        assert!(!engine.volumes[0].sidecar_pending);
        assert!(engine.volumes[0].dirty);

        engine.flush_all();
        assert_eq!(
            std::fs::read(&snap_path).unwrap(),
            before,
            "无待写旁车变更:退出不得整卷重写"
        );
        assert!(engine.volumes[0].dirty, "dirty 保留,全量归档留给下一会话");

        // 恢复语义:快照 + 旁车回放仍恢复出 gamma
        let engine2 = engine_with(&dir);
        assert!(engine2.volumes[0].store.as_ref().unwrap().find_slot(13).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 分批 apply 的正确性:分 1 批 vs 分 N 批,最终 store 完全一致 ----------

    /// 最终状态指纹:(frn, 小写名, 父 frn, attrs, mtime) 按 slot 序。
    /// 一次性 apply 与分批 apply 的逐条调用序列完全相同,故列内容(含
    /// 未 compact 回收的 DEAD 槽)必须逐行一致。
    fn store_rows(engine: &Engine, vol: usize) -> Vec<(u64, Vec<u8>, u64, u16, u32)> {
        let store = engine.volumes[vol].store.as_ref().unwrap();
        (0..store.len() as u32)
            .map(|slot| {
                let p = store.parent[slot as usize];
                let parent_frn = if p == crate::store::NO_PARENT { 0 } else { store.frn[p as usize] };
                (
                    store.frn[slot as usize],
                    store.name_lc(slot).to_vec(),
                    parent_frn,
                    store.attrs[slot as usize] & !crate::store::ATTR_INTERNAL,
                    store.mtime[slot as usize],
                )
            })
            .collect()
    }

    /// 同一记录流分别喂给两个引擎:engine_a 一次性(分批关闭,单条大
    /// Records);engine_b 分数十条小消息 + 64 条/块,让路钩子中段强制触发
    /// (证明分批可中断、可恢复)。卷 Ready 后逐列对比,并校验游标、
    /// 拼音缓存有效性与查询结果一致。
    #[test]
    fn chunked_first_scan_apply_matches_one_shot() {
        let dir = temp_index_dir("chunked-scan");
        let guid = 0x91u128;
        let mut engine_a = engine_with(&dir);
        let mut engine_b = engine_with(&dir);
        engine_a.set_apply_chunk_records(0);
        engine_b.set_apply_chunk_records(64);
        // 让路开关:开始放行(正常分批消化),中段强制让路(缓冲堆积),
        // 消化阶段再放行(证明中断后可恢复)。
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let force_hook = force.clone();
        engine_b.set_query_yield_hook(Some(Box::new(move || {
            force_hook.load(std::sync::atomic::Ordering::Relaxed)
        })));

        // 确定性记录流:目录先行 + 文件(部分父未消解)+ 重发(改名/属性/无变化)
        let mut recs: Vec<Rec> = Vec::new();
        for d in 0..40u64 {
            recs.push(rec(100 + d, 0, 0x0010, &format!("dir{d:03}")));
        }
        for f in 0..460u64 {
            let parent = 100 + (f % 40);
            let name = format!("file{f:04}{}", [".txt", ".dat", ".md"][(f % 3) as usize]);
            recs.push(rec(200 + f, parent, 0, &name));
        }
        // 后段重发:改名 / mtime / 完全一致(FRN 复用与就地更新路径)
        for f in 0..60u64 {
            recs.push(rec(200 + f, 100 + (f % 40), 0, &format!("renamed{f:04}.txt")));
        }

        for e in [&mut engine_a, &mut engine_b] {
            e.handle(EngineMsg::VolumeMeta {
                guid,
                letter: 'C',
                journal_id: 7,
                next_usn: 100,
                est_records: recs.len() as u64,
            });
            e.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        }
        // a:一条大 Records(一次性)
        engine_a.handle(EngineMsg::Records { guid, recs: recs.clone(), next_usn: 100 });
        // b:同流切成数十条不等长小消息(分批引擎),前半段正常分批消化
        let mut chunks = Vec::new();
        let base = recs.len() / 20 + 1;
        let mut rest = recs.as_slice();
        while !rest.is_empty() {
            let n = rest.len().min(base + (chunks.len() % 3) * 3);
            chunks.push(&rest[..n]);
            rest = &rest[n..];
        }
        for (k, part) in chunks.iter().enumerate() {
            engine_b.handle(EngineMsg::Records {
                guid,
                recs: part.to_vec(),
                next_usn: 100 + k as i64,
            });
            if k + 1 == chunks.len() / 2 {
                // 中段起强制让路:后续消息整条留在缓冲(模拟查询持续等待)
                force.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        assert!(chunks.len() >= 10);
        assert!(
            engine_b.apply_yield_count() > 0,
            "让路钩子必须真的触发过"
        );
        assert!(
            engine_b.has_pending_apply(),
            "钩子触发期间部分记录应留在缓冲"
        );
        // 强制让路期间 advance 不得消费(把控制权完整交还宿主)
        assert_eq!(engine_b.advance_pending_apply(), 0);
        assert!(engine_b.has_pending_apply());
        // 放行后逐块消化剩余缓冲直到清空(中断后可恢复)
        force.store(false, std::sync::atomic::Ordering::Relaxed);
        while engine_b.has_pending_apply() {
            assert!(engine_b.advance_pending_apply() > 0, "钩子已放行,必须持续推进");
        }
        assert!(!engine_b.has_pending_apply());

        for e in [&mut engine_a, &mut engine_b] {
            e.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 0 });
        }
        assert_eq!(store_rows(&engine_a, 0), store_rows(&engine_b, 0), "最终 store 必须逐列一致");
        assert_eq!(
            engine_a.handshake_state(),
            engine_b.handshake_state(),
            "游标状态必须一致"
        );
        // 拼音缓存有效(6b60e46 的越界回归护栏):所有 slot 都在界内
        let len_b = engine_b.volumes[0].store.as_ref().unwrap().len();
        assert!(len_b > 500);
        assert!(engine_b.pinyin[0].slots.iter().all(|&s| s < len_b as u32));
        // 查询结果一致(含 ASCII 拼音候选路径)
        let mut spec = QuerySpec::default();
        spec.tokens = vec!["file0001".to_string()];
        let out_a = engine_a.query(&spec);
        let out_b = engine_b.query(&spec);
        assert_eq!(out_a.total, out_b.total);
        assert_eq!(out_a.hits.len(), out_b.hits.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 对账(dirsync)路径:分 1 批 vs 分 N 批,含新增/改名/删除/属性更新;
    /// DirSyncEnd(ok=true) 删除离线消失项的语义必须完全一致。中途中止
    /// (ok=false)时缓冲先消化、只清 SEEN 不删除,同样与一次性等价。
    #[test]
    fn chunked_dirsync_apply_matches_one_shot() {
        let dir = temp_index_dir("chunked-dirsync");
        let guid = 0x92u128;
        let mut engine_a = engine_with(&dir);
        let mut engine_b = engine_with(&dir);
        engine_a.set_apply_chunk_records(0);
        engine_b.set_apply_chunk_records(2);
        // 让路开关:第一轮放行(正常分批消化),中止轮强制让路,
        // 用于验证 DirSyncEnd 时缓冲会被强制消化(控制消息语义前提)。
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let force_hook = force.clone();
        engine_b.set_query_yield_hook(Some(Box::new(move || {
            force_hook.load(std::sync::atomic::Ordering::Relaxed)
        })));

        feed_ready(&mut engine_a, guid, 'C'); // docs/alpha.txt/beta.txt
        feed_ready(&mut engine_b, guid, 'C');

        // 完整对账流:alpha 更新(mtime)、gamma 新增、beta 删除、alpha 改名
        let sync_recs = vec![
            rec(10, 0, 0x0010, "docs"),
            Rec { frn: 11, parent_frn: 10, attrs: 0, op: Op::Upsert, name: "alpha.txt".into(), mtime: 1_700_000_000 },
            rec(13, 10, 0, "gamma.txt"),
            Rec { frn: 12, parent_frn: 10, attrs: 0, op: Op::Remove, name: String::new(), mtime: 0 },
            rec(11, 10, 0, "zeta.txt"),
        ];
        // a:一次性 apply
        engine_a.handle(EngineMsg::DirSyncBegin { guid });
        engine_a.handle(EngineMsg::Records { guid, recs: sync_recs.clone(), next_usn: 300 });
        engine_a.handle(EngineMsg::DirSyncEnd { guid, ok: true });
        // b:同流分批,全程强制让路(缓冲堆积),DirSyncEnd 前缓冲未消化完
        // —— 验证控制消息会先强制消化缓冲再走原逻辑(ok=true 删除语义的前提)
        engine_b.handle(EngineMsg::DirSyncBegin { guid });
        force.store(true, std::sync::atomic::Ordering::Relaxed);
        for part in sync_recs.chunks(2) {
            engine_b.handle(EngineMsg::Records { guid, recs: part.to_vec(), next_usn: 300 });
        }
        assert!(
            engine_b.has_pending_apply(),
            "强制让路下,DirSyncEnd 前缓冲必须未消化完"
        );
        engine_b.handle(EngineMsg::DirSyncEnd { guid, ok: true });
        force.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(
            engine_b.apply_yield_count() > 0,
            "分批对账中让路钩子必须真的触发过"
        );
        assert!(!engine_b.has_pending_apply());
        assert_eq!(
            store_rows(&engine_a, 0),
            store_rows(&engine_b, 0),
            "对账后的 store 必须逐列一致(beta 删、gamma 增、alpha 改名)"
        );
        for e in [&engine_a, &engine_b] {
            let s = e.volumes[0].store.as_ref().unwrap();
            assert!(s.find_slot(12).is_none(), "beta 离线消失 → 删除");
            assert!(s.find_slot(13).is_some(), "gamma 新增");
            assert_eq!(s.name(s.find_slot(11).unwrap()), b"zeta.txt");
            let len = s.len();
            assert!(
                e.pinyin[0].slots.iter().all(|&slot| (slot as usize) < len),
                "拼音缓存必须全部指向有效 slot"
            );
        }

        // 中途中止(ok=false):缓冲未消化的记录也要 apply(与一次性一致),
        // 随后只清 SEEN、绝不删除。
        let abort_recs = vec![rec(14, 10, 0, "mid.txt"), rec(15, 10, 0, "late.txt")];
        for (e, split) in [(&mut engine_a, false), (&mut engine_b, true)] {
            e.handle(EngineMsg::DirSyncBegin { guid });
            if !split {
                e.handle(EngineMsg::Records { guid, recs: abort_recs.clone(), next_usn: 400 });
            } else {
                // 强制让路:两条消息整条留在缓冲,DirSyncEnd 时强制消化
                force.store(true, std::sync::atomic::Ordering::Relaxed);
                e.handle(EngineMsg::Records { guid, recs: abort_recs[..1].to_vec(), next_usn: 400 });
                e.handle(EngineMsg::Records { guid, recs: abort_recs[1..].to_vec(), next_usn: 400 });
                assert!(
                    e.has_pending_apply(),
                    "强制让路下中止前的记录必须还在缓冲"
                );
            }
            e.handle(EngineMsg::DirSyncEnd { guid, ok: false });
            force.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        assert_eq!(
            store_rows(&engine_a, 0),
            store_rows(&engine_b, 0),
            "中止路径的最终 store 也必须逐列一致"
        );
        for e in [&engine_a, &engine_b] {
            let s = e.volumes[0].store.as_ref().unwrap();
            assert!(s.find_slot(14).is_some() && s.find_slot(15).is_some());
            assert!(
                s.attrs.iter().all(|a| a & crate::store::ATTR_SEEN == 0),
                "中止后不得残留 SEEN 临时位"
            );
            assert!(!e.has_pending_apply());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H-1 回归:首扫第一次部分保存必做 compact(save 的列一致契约),
    /// compact 后 base_len 写满、store.is_scanning() 翻 false;分批条件若挂在
    /// 存储扫描态上,首扫剩余阶段会退回一次性 apply,让路优化失效。条件按
    /// 「!complete」判定后,部分保存之后的 Records 仍走分批路径。
    #[test]
    fn chunked_survives_partial_save_compact() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("h1-chunked-after-save");
        let guid = 0xb2u128;
        let mut engine = engine_with(&dir);
        engine.set_apply_chunk_records(64);
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook = force.clone();
        engine.set_query_yield_hook(Some(Box::new(move || {
            hook.load(std::sync::atomic::Ordering::Relaxed)
        })));
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 10,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        // 第一批正常消化 → dirty;推进断点后做一次部分保存(内部必 compact)。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(10, 0, DIR, "docs"), rec(11, 10, 0, "alpha.txt")],
            next_usn: 100,
        });
        engine.handle(EngineMsg::ScanProgress { guid, frn: 50, est: 100, count: 2 });
        assert!(engine.maintain(), "首扫中途的部分保存应发生");
        assert!(snapshot::snapshot_path(&dir, guid).is_file());
        // 前提确认:compact 已把存储态翻出「扫描中」—— 旧条件在这里即失效。
        assert!(
            !engine.volumes[0].store.as_ref().unwrap().is_scanning(),
            "前提:部分保存后 base_len 写满,is_scanning() 翻 false"
        );

        // 首扫未完(complete=false):压住让路钩子,后续 Records 仍必须进
        // 分批缓冲而不是一次性 apply 把查询线程堵住。
        force.store(true, std::sync::atomic::Ordering::Relaxed);
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(12, 10, 0, "beta.txt"), rec(13, 10, 0, "gamma.txt")],
            next_usn: 100,
        });
        assert!(
            engine.has_pending_apply(),
            "部分保存后首扫剩余 Records 仍应走分批路径(让路时留在缓冲)"
        );
        // 放行消化,语义不受影响:记录最终落进 store
        force.store(false, std::sync::atomic::Ordering::Relaxed);
        while engine.has_pending_apply() {
            assert!(engine.advance_pending_apply() > 0);
        }
        let store = engine.volumes[0].store.as_ref().unwrap();
        assert!(store.find_slot(12).is_some() && store.find_slot(13).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- B-1:首扫分批缓冲未消化完时,保存路径必须跳过该卷 ----------

    /// 首扫 + 打字(让路钩子压住分批缓冲)+ 20s 断点保存窗口:resume_frn 已随
    /// ScanProgress 推进,而 FRN < resume_frn 的一段记录还压在 pending_applies
    /// 里没进 store —— 此刻落盘会持久化「游标超前于内容」的裂缝快照,崩溃重启
    /// 后该段永久无声丢失。缓冲清空前:maintain / flush_all 都不得生成快照,
    /// dirty 保留待下轮。
    #[test]
    fn save_skipped_while_first_scan_buffer_pending() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("b1-pending-skip");
        let guid = 0xb1u128;
        let mut engine = engine_with(&dir);
        engine.set_apply_chunk_records(64);
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook = force.clone();
        engine.set_query_yield_hook(Some(Box::new(move || {
            hook.load(std::sync::atomic::Ordering::Relaxed)
        })));
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 10,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        // 第一批正常消化(dirty=true),模拟断点已推进;第二批起压住缓冲。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(10, 0, DIR, "docs"), rec(11, 10, 0, "alpha.txt")],
            next_usn: 100,
        });
        engine.handle(EngineMsg::ScanProgress { guid, frn: 50, est: 100, count: 2 });
        force.store(true, std::sync::atomic::Ordering::Relaxed);
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(12, 10, 0, "beta.txt"), rec(13, 10, 0, "gamma.txt")],
            next_usn: 100,
        });
        assert!(engine.has_pending_apply(), "让路期间缓冲必须未消化完");
        assert!(engine.volumes[0].dirty);

        // 断点保存路径:缓冲非空 → 跳过,不生成快照(否则 resume_frn=50 与
        // store(缺 frn=12/13 段)一起持久化,重启后该段永久丢失)。
        assert!(!engine.maintain(), "缓冲未消化完时 maintain 不得落盘");
        assert!(
            !snapshot::snapshot_path(&dir, guid).is_file(),
            "缓冲未消化完时不得生成快照文件"
        );
        assert!(engine.volumes[0].dirty, "dirty 保留,下轮再试");
        assert_eq!(engine.volumes[0].save_retry_at, None, "不被退避推后:缓冲清空后立即可存");

        // 退出收尾同样宁可不存(重启从上一份自洽快照续扫)。
        engine.flush_all();
        assert!(
            !snapshot::snapshot_path(&dir, guid).is_file(),
            "flush_all 也不得保存裂缝快照"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓冲消化完后再保存:快照内容与 store 逐列一致(复用 chunked 等价测试
    /// 的指纹比较),resume_frn 断点正确持久化。
    #[test]
    fn save_after_buffer_drained_matches_store() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("b1-drained-save");
        let guid = 0xb8u128;
        let mut engine = engine_with(&dir);
        engine.set_apply_chunk_records(2); // 小块,确保缓冲真的分多轮消化
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hook = force.clone();
        engine.set_query_yield_hook(Some(Box::new(move || {
            hook.load(std::sync::atomic::Ordering::Relaxed)
        })));
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 10,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![
                rec(10, 0, DIR, "docs"),
                rec(11, 10, 0, "alpha.txt"),
                rec(12, 10, 0, "beta.txt"),
                rec(13, 10, 0, "gamma.txt"),
            ],
            next_usn: 100,
        });
        engine.handle(EngineMsg::ScanProgress { guid, frn: 50, est: 100, count: 4 });
        assert!(engine.has_pending_apply());
        assert!(!engine.maintain(), "缓冲压着:不落盘");

        // 放行并清空缓冲(宿主在查询间隙逐块消化的等价驱动)
        force.store(false, std::sync::atomic::Ordering::Relaxed);
        while engine.has_pending_apply() {
            assert!(engine.advance_pending_apply() > 0);
        }
        assert!(engine.maintain(), "缓冲清空后部分快照应正常落盘");
        assert!(snapshot::snapshot_path(&dir, guid).is_file());
        assert!(!engine.volumes[0].dirty);

        // 重启加载:快照内容与实际 store 逐列一致,断点正确恢复
        let engine2 = engine_with(&dir);
        assert_eq!(engine2.volumes.len(), 1);
        assert!(!engine2.volumes[0].complete);
        assert_eq!(engine2.volumes[0].resume_frn, 50, "resume_frn 须随快照持久化");
        assert_eq!(
            store_rows(&engine, 0),
            store_rows(&engine2, 0),
            "快照内容必须与实际 store 逐列一致"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- timing 基准:对账期间查询延迟(分批前 vs 分批后) ----------
    //
    // 结构:引擎线程持续 apply dirsync 记录流(250 万条,公式化生成、零随机),
    // 查询线程模拟打字(周期性发单字符 "a" 查询,撞 candidate_cap)。
    // 查询延迟 = 查询完成时刻 - 发送时刻(含排队等待 = 用户感知)。
    // 对账收尾(DirSyncEnd:remove_unseen + 拼音重建)两种模式成本相同,
    // 在计时窗口外补发,不参与采样。
    //
    // - one-shot(分批前,旧行为):分批关闭;apply 以 400 条消息为一整轮,
    //   查询只能在轮间隙被服务(复刻 0294602 之前的调度)。
    // - chunked(分批后):512 条/块,块间让路钩子接「查询待处理」标志;
    //   消息间隙与块间发现等待中的查询立即收手(0294602 的 apply 中段扩展)。
    //
    // 断言取结构性判定而非绝对时间(避免 CI/测试机抖动 flaky):
    // 1. 分批后批间让路发生次数 > 0;
    // 2. 分批后 p90 至少比分批前好一倍(实测差 1-2 个数量级);
    // 3. 分批后 p50/p90 落在目标内(p50<30ms、p90<100ms,实测余量 20 倍+);
    // 4. 分批后查询最大延迟 < apply 总时长(查询绝不等待整个 apply)。
    // p50/p90/max 实测数字打印到输出(--nocapture 可见),提交信息引用。

    /// 两卷规模:2000 目录 + 2,498,000 文件 = 250 万条。
    const BENCH_DIRS: u64 = 2000;
    const BENCH_FILES: u64 = 2_498_000;

    fn bench_rec(frn: u64, parent: u64, name: String, attrs: u32, mtime: i64) -> Rec {
        Rec { frn, parent_frn: parent, attrs, op: Op::Upsert, name, mtime }
    }

    /// 首扫构建流(目录先行 + 文件;名字 1/5 含 'a',单字符查询撞 cap 的痛点形态)。
    fn bench_build_msgs(guid: u128) -> Vec<EngineMsg> {
        let mut msgs = vec![
            EngineMsg::VolumeMeta {
                guid,
                letter: 'C',
                journal_id: 7,
                next_usn: 100,
                est_records: BENCH_DIRS + BENCH_FILES,
            },
            EngineMsg::ScanBegin { guid, resume_frn: 0 },
        ];
        let mut recs: Vec<Rec> = Vec::with_capacity((BENCH_DIRS + BENCH_FILES) as usize);
        for d in 0..BENCH_DIRS {
            recs.push(bench_rec(10 + d, crate::store::ROOT_FRN, format!("dir{d:05}"), 0x0010, 0));
        }
        for f in 0..BENCH_FILES {
            let prefix = ["zzz", "yyy", "mmm", "qqq", "data"][f as usize % 5];
            let parent = if f % 10 == 9 { 10 + (f / 10 % BENCH_DIRS) } else { crate::store::ROOT_FRN };
            recs.push(bench_rec(
                10 + BENCH_DIRS + f,
                parent,
                format!("{prefix}_{f:08}.bin"),
                0,
                1_600_000_000 + (f % 1_000_000) as i64,
            ));
        }
        for chunk in recs.chunks(50_000) {
            msgs.push(EngineMsg::Records { guid, recs: chunk.to_vec(), next_usn: 100 });
        }
        msgs.push(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 0 });
        msgs
    }

    /// 对账回放流:DirSyncBegin + 同卷记录重发(98% 无变化快路径 + 1% 改名 +
    /// 1% mtime 变化 + 1% 新增)按 dirscan 同款 4000 条/批打包。
    /// 注意:**不含 DirSyncEnd** —— 收尾(remove_unseen/拼音重建,数百 ms 且
    /// 与分批无关)是两种模式共同的固定成本,由调用方在计时窗口外补发,
    /// 不参与查询延迟采样。
    fn bench_dirsync_msgs(guid: u128) -> Vec<EngineMsg> {
        let mut msgs = vec![EngineMsg::DirSyncBegin { guid }];
        let base = 10 + BENCH_DIRS; // 文件起始 frn
        let mut new_frn = base + BENCH_FILES;
        let mut recs: Vec<Rec> = Vec::with_capacity((BENCH_FILES + BENCH_DIRS) as usize);
        for d in 0..BENCH_DIRS {
            recs.push(bench_rec(10 + d, crate::store::ROOT_FRN, format!("dir{d:05}"), 0x0010, 0));
        }
        for f in 0..BENCH_FILES {
            let frn = base + f;
            let parent = if f % 10 == 9 { 10 + (f / 10 % BENCH_DIRS) } else { crate::store::ROOT_FRN };
            match f % 100 {
                0 => recs.push(bench_rec(new_frn, parent, format!("new_{f:08}.bin"), 0, 1_700_000_000)),
                // FRN 复用改名(new 的名字被下一轮复用的情形抽象)
                1 => recs.push(bench_rec(frn, parent, format!("renamed_{f:08}.bin"), 0, 1_600_000_000 + (f % 1_000_000) as i64)),
                2 => recs.push(bench_rec(frn, parent, format!("data_{f:08}.bin"), 0, 1_700_000_000)),
                f0 => {
                    let prefix = ["zzz", "yyy", "mmm", "qqq", "data"][f0 as usize % 5];
                    recs.push(bench_rec(frn, parent, format!("{prefix}_{f:08}.bin"), 0, 1_600_000_000 + (f % 1_000_000) as i64));
                }
            }
            if f % 100 == 0 {
                new_frn += 1;
            }
        }
        for chunk in recs.chunks(4000) {
            msgs.push(EngineMsg::Records { guid, recs: chunk.to_vec(), next_usn: 100 });
        }
        msgs
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "release-profile 基准(250 万条合成记录)")]
    fn bench_dirsync_apply_yields_to_queries() {
        use std::collections::VecDeque;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        const GUID: u128 = 0xbeef;
        // 与生产一致的单字符查询形态:candidate_cap=100(engine.log 里
        // 「单字符 a 撞 100 条结果上限」的痛点),total 会计到 TOTAL_CAP。
        let make_engine = || {
            Engine::new(EngineConfig {
                // 目录不存在 → 不读也不写快照,基准零磁盘 IO
                index_dir: std::env::temp_dir().join("lstr-bench-nonexistent"),
                exclude_noise: true,
                candidate_cap: 100,
                volumes: Vec::new(),
            })
        };

        // 两种模式共用:查询线程按 20ms 节奏发查询并打时间戳;
        // 返回 (延迟样本 ms, apply 总时长 ms, 让路次数, 对账后 store 条数)。
        // `tail` 在计时窗口外补发(对账收尾,不参与采样)。
        let run = |mut engine: Engine, msgs: Vec<EngineMsg>, tail: Option<EngineMsg>, query_flag: Option<Arc<AtomicBool>>| -> (Vec<f64>, f64, u64, usize) {
            let (q_tx, q_rx) = std::sync::mpsc::channel::<(Instant, QuerySpec)>();
            let done = Arc::new(AtomicBool::new(false));
            let done2 = done.clone();
            let flag2 = query_flag.clone();
            let spec = || QuerySpec { tokens: vec!["a".to_string()], ..Default::default() };
            let q_thread = std::thread::Builder::new()
                .name("bench-query".to_string())
                .spawn(move || {
                    // 10ms 节奏:比真实打字密,为了让延迟分布有足够样本
                    // (Windows sleep 粒度 ~15ms,实际节奏 ≈30-70ms/条)
                    while !done2.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(10));
                        if let Some(f) = &flag2 {
                            f.store(true, Ordering::Relaxed);
                        }
                        let _ = q_tx.send((Instant::now(), spec()));
                    }
                })
                .unwrap();
            let latencies = Mutex::new(Vec::new());
            let t0 = Instant::now();
            let mut queue = VecDeque::from(msgs);
            let mut iter = 0usize;
            // 剩最后 2 条记录消息(≈6ms)就停止注入查询:之后的收尾不采样,
            // 也切断「服务积压期间查询线程持续注入」的雪球效应(生产里打字会停)。
            let stop_sending = |queue: &VecDeque<EngineMsg>, engine: &Engine, done: &Arc<AtomicBool>| {
                if queue.len() <= 2 && !engine.has_pending_apply() {
                    done.store(true, Ordering::Relaxed);
                }
            };
            loop {
                if queue.is_empty() && !engine.has_pending_apply() {
                    break;
                }
                stop_sending(&queue, &engine, &done);
                let it_start = Instant::now();
                // 分批缓冲:块间让路(钩子接查询标志)
                while engine.has_pending_apply() {
                    if engine.advance_pending_apply() == 0 {
                        break;
                    }
                }
                // 消息消化:分批后打字优先(有等待查询/缓冲未清即收手);
                // one-shot 保持旧行为:400 条消息一整轮,期间不看查询。
                let mut applied = 0usize;
                while let Some(msg) = queue.pop_front() {
                    engine.handle(msg);
                    applied += 1;
                    stop_sending(&queue, &engine, &done);
                    if applied >= 400 {
                        break;
                    }
                    if let Some(f) = &query_flag {
                        if f.load(Ordering::Relaxed) || engine.has_pending_apply() {
                            break;
                        }
                    }
                }
                // 服务查询:延迟 = 完成时刻 - 发送时刻(含排队等待)
                let mut served = 0usize;
                let mut served_max_lat = 0f64;
                while let Ok((sent, s)) = q_rx.try_recv() {
                    let _ = engine.query(&s);
                    let lat = sent.elapsed().as_secs_f64() * 1000.0;
                    served_max_lat = served_max_lat.max(lat);
                    latencies.lock().unwrap().push(lat);
                    served += 1;
                    if let Some(f) = &query_flag {
                        // 无论是否还有查询,都放行 apply(防让路标志滞留空等)
                        f.store(false, Ordering::Relaxed);
                    }
                }
                iter += 1;
                if std::env::var("LSTR_BENCH_TRACE").is_ok() {
                    println!(
                        "[trace] iter={iter} applied={applied} served={served} dur={:.1}ms lat_max={served_max_lat:.1}ms",
                        it_start.elapsed().as_secs_f64() * 1000.0
                    );
                }
            }
            done.store(true, Ordering::Relaxed);
            q_thread.join().unwrap();
            // 计时窗口外补发对账收尾(remove_unseen/拼音重建),与生产语义一致
            if let Some(m) = tail {
                engine.handle(m);
            }
            let total = t0.elapsed().as_secs_f64() * 1000.0;
            let store_len = engine.volumes[0].store.as_ref().map(|s| s.len()).unwrap_or(0);
            (latencies.into_inner().unwrap(), total, engine.apply_yield_count(), store_len)
        };

        // ---- 分批前(one-shot,旧行为)----
        let mut engine_a = make_engine();
        engine_a.set_apply_chunk_records(0); // 关闭分批 = 旧行为
        for m in bench_build_msgs(GUID) {
            engine_a.handle(m);
        }
        let (lat_a, total_a, _, len_a) = run(
            engine_a,
            bench_dirsync_msgs(GUID),
            Some(EngineMsg::DirSyncEnd { guid: GUID, ok: true }),
            None,
        );
        let mut lat_a = lat_a;
        lat_a.sort_by(|x, y| x.partial_cmp(y).unwrap());

        // ---- 分批后(chunked + 让路钩子)----
        let mut engine_b = make_engine();
        engine_b.set_apply_chunk_records(512);
        let flag = Arc::new(AtomicBool::new(false));
        let flag_hook = flag.clone();
        engine_b.set_query_yield_hook(Some(Box::new(move || {
            flag_hook.load(Ordering::Relaxed)
        })));
        for m in bench_build_msgs(GUID) {
            engine_b.handle(m);
        }
        let (lat_b, total_b, yields_b, len_b) = run(
            engine_b,
            bench_dirsync_msgs(GUID),
            Some(EngineMsg::DirSyncEnd { guid: GUID, ok: true }),
            Some(flag),
        );
        let mut lat_b = lat_b;
        lat_b.sort_by(|x, y| x.partial_cmp(y).unwrap());

        // 两模式最终 store 规模必须一致(对账语义未被分批改变)
        assert_eq!(len_a, len_b, "分批前后对账结果条数必须一致");
        assert!(lat_a.len() >= 15 && lat_b.len() >= 15, "查询样本太少,统计无意义: {} / {}", lat_a.len(), lat_b.len());

        let (p50_a, p90_a, max_a) = (percentile(&lat_a, 0.5), percentile(&lat_a, 0.9), *lat_a.last().unwrap());
        let (p50_b, p90_b, max_b) = (percentile(&lat_b, 0.5), percentile(&lat_b, 0.9), *lat_b.last().unwrap());
        println!("[bench] one-shot(分批前): apply={total_a:.0}ms queries={} p50={p50_a:.1}ms p90={p90_a:.1}ms max={max_a:.1}ms", lat_a.len());
        println!("[bench] chunked (分批后): apply={total_b:.0}ms queries={} p50={p50_b:.1}ms p90={p90_b:.1}ms max={max_b:.1}ms yields={yields_b}", lat_b.len());

        // 结构性断言(避免绝对时间抖动 flaky):
        // - 批间让路必须真的发生过;
        // - 分批后 p90 至少比分批前好一倍(实测差 1-2 个数量级);
        // - 分批后的 p50/p90 必须落在任务目标内(p50<30ms、p90<100ms,
        //   实测留有 20 倍以上余量);
        // - 查询最大延迟必须小于 apply 总时长(查询绝不等待整个 apply)。
        assert!(yields_b > 0, "批间让路必须真的发生过");
        assert!(
            p90_b < p90_a / 2.0,
            "分批后 p90 {p90_b:.1}ms 应明显低于分批前 p90 {p90_a:.1}ms"
        );
        assert!(
            p50_b < 30.0 && p90_b < 100.0,
            "分批后查询延迟应达标(p50<30ms、p90<100ms): p50={p50_b:.1}ms p90={p90_b:.1}ms"
        );
        assert!(
            max_b < total_b,
            "查询最大延迟必须小于 apply 总时长(结构性)"
        );
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("lstr-bench-nonexistent"));
    }

    // ---------- timing 基准:稳态保存期间查询延迟(全量保存 vs 旁车) ----------
    //
    // 结构:引擎持有一卷 ~117 万条(与实测 C 卷同规模),查询线程按 300ms
    // 节奏发查询;引擎线程每轮:注入一条稳态增量 → maintain() → 排队查询
    // 全部服务完。延迟 = 完成时刻 - 发送时刻(含排队等待 = 用户感知)。
    //
    // 查询形态用稀有 token("steady",只命中注入的稳态文件):单字符查询在
    // 117 万条上评估本身就要 ~450ms(TOTAL_CAP 内逐槽过滤),会把两种模式都
    // 淹没在评估成本里;保存阻塞是「引擎线程被保存占住」与查询形态无关,
    // 稀有 token 让评估降到 ~10ms,排队等待(保存阻塞)成为唯一变量。
    //
    // - full-save(旧行为):清 save_retry_at 模拟「每 60s 一次全量保存」,
    //   查询排队等整卷重写(实测 1.6-2s)。
    // - sidecar(新行为):正常 maintain —— 全量降频,稳态只写几 KB 旁车。
    //
    // 断言取结构性判定(避免绝对时间抖动 flaky):
    // 1. sidecar 模式最大延迟 < full-save 模式 p50(整个分布移出保存窗口);
    // 2. sidecar 模式 p90 < 30ms(打字无感)。
    // 数字打印到输出(--nocapture 可见),提交信息引用。

    const SAVE_BENCH_DIRS: u64 = 2000;
    const SAVE_BENCH_FILES: u64 = 1_168_000;

    fn save_bench_engine(dir: &std::path::Path) -> Engine {
        let guid = 0xcafeu128;
        let mut engine = Engine::new(EngineConfig {
            index_dir: dir.to_path_buf(),
            exclude_noise: true,
            candidate_cap: 100,
            volumes: Vec::new(),
        });
        engine.set_apply_chunk_records(0); // 构建期一次性 apply,加速
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: SAVE_BENCH_DIRS + SAVE_BENCH_FILES,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let mut recs = Vec::with_capacity((SAVE_BENCH_DIRS + SAVE_BENCH_FILES) as usize);
        for d in 0..SAVE_BENCH_DIRS {
            recs.push(bench_rec(10 + d, crate::store::ROOT_FRN, format!("dir{d:05}"), 0x0010, 0));
        }
        for f in 0..SAVE_BENCH_FILES {
            let prefix = ["zzz", "yyy", "mmm", "qqq", "data"][f as usize % 5];
            recs.push(bench_rec(
                10 + SAVE_BENCH_DIRS + f,
                if f % 10 == 9 { 10 + (f / 10 % SAVE_BENCH_DIRS) } else { crate::store::ROOT_FRN },
                format!("{prefix}_{f:08}.bin"),
                0,
                1_600_000_000 + (f % 1_000_000) as i64,
            ));
        }
        for chunk in recs.chunks(50_000) {
            engine.handle(EngineMsg::Records { guid, recs: chunk.to_vec(), next_usn: 100 });
        }
        engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 0 });
        engine
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "release-profile 基准(117 万条,真实落盘)")]
    fn bench_save_blocks_queries_full_save_vs_sidecar() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!("lstr-save-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let run = |full_save_each_round: bool| -> (Vec<f64>, f64) {
            let mut engine = save_bench_engine(&dir);
            // 预置一轮:VolumeReady 的 dirty 已由首次 maintain 消化(计时窗外)
            engine.maintain();
            let guid = 0xcafeu128;
            let (q_tx, q_rx) = std::sync::mpsc::channel::<(Instant, QuerySpec)>();
            let done = Arc::new(AtomicBool::new(false));
            let done2 = done.clone();
            let q_thread = std::thread::Builder::new()
                .name("save-bench-query".to_string())
                .spawn(move || {
                    let spec = || QuerySpec { tokens: vec!["steady".to_string()], ..Default::default() };
                    // 300ms 节奏(打字间距量级):评估 ~10ms/条,稳态无积压;
                    // full-save 模式下保存期到达的查询只能排队等保存结束。
                    while !done2.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(300));
                        let _ = q_tx.send((Instant::now(), spec()));
                    }
                })
                .unwrap();
            let latencies = Mutex::new(Vec::new());
            let t0 = Instant::now();
            let deadline = t0 + Duration::from_secs(9);
            let mut usn = 100i64;
            let mut round = 0usize;
            while Instant::now() < deadline {
                let round_start = Instant::now();
                // 空闲期:紧密服务到达的查询(对应生产循环「空闲 → recv_timeout
                // 出队服务」;这里的轮询间隙 5ms,远小于生产 150ms,避免把空闲
                // 排队混进保存阻塞的信号里)。
                while round_start.elapsed() < Duration::from_millis(500) {
                    while let Ok((sent, spec)) = q_rx.try_recv() {
                        let _ = engine.query(&spec);
                        latencies
                            .lock()
                            .unwrap()
                            .push(sent.elapsed().as_secs_f64() * 1000.0);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                // 维护轮次(500ms 节拍,与真实 journal 轮询一致):注入稳态增量
                // → maintain(旧模式=整卷重写,新模式=毫秒级旁车)。
                usn += 1;
                let recs: Vec<Rec> = (0..4)
                    .map(|j| {
                        bench_rec(
                            900_000 + (round * 4 + j) as u64,
                            10,
                            format!("steady_{round:03}_{j}.txt"),
                            0,
                            0,
                        )
                    })
                    .collect();
                engine.handle(EngineMsg::Records { guid, recs, next_usn: usn });
                if full_save_each_round {
                    // 模拟旧节流(60s 全量)被压缩到每轮:清 retry_at 强制整卷重写
                    engine.volumes[0].save_retry_at = None;
                }
                engine.maintain();
                round += 1;
            }
            done.store(true, Ordering::Relaxed);
            q_thread.join().unwrap();
            (latencies.into_inner().unwrap(), t0.elapsed().as_secs_f64() * 1000.0)
        };

        let (lat_a, total_a) = run(true);
        let mut lat_a = lat_a;
        lat_a.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let (lat_b, total_b) = run(false);
        let mut lat_b = lat_b;
        lat_b.sort_by(|x, y| x.partial_cmp(y).unwrap());

        assert!(lat_a.len() >= 8 && lat_b.len() >= 8, "样本不足: {} / {}", lat_a.len(), lat_b.len());
        let (p50_a, p90_a, max_a) = (percentile(&lat_a, 0.5), percentile(&lat_a, 0.9), *lat_a.last().unwrap());
        let (p50_b, p90_b, max_b) = (percentile(&lat_b, 0.5), percentile(&lat_b, 0.9), *lat_b.last().unwrap());
        println!(
            "[bench] full-save(旧行为,每轮全量): {} queries p50={p50_a:.1}ms p90={p90_a:.1}ms max={max_a:.1}ms dur={total_a:.0}ms",
            lat_a.len()
        );
        println!(
            "[bench] sidecar (新行为,降频全量): {} queries p50={p50_b:.1}ms p90={p90_b:.1}ms max={max_b:.1}ms dur={total_b:.0}ms",
            lat_b.len()
        );
        // 结构性断言(避免绝对时间抖动 flaky):
        // - sidecar 模式 p90 至少比 full-save 模式好一倍(实测差一个数量级);
        // - sidecar 模式 p90 上限 200ms(查询评估本身 ~60ms,余量 3 倍);
        // - sidecar 模式最大延迟 < full-save 模式 p50(整个分布移出保存窗口)。
        assert!(
            p90_b < p90_a / 2.0,
            "sidecar 模式 p90 {p90_b:.1}ms 应显著低于 full-save 模式 p90 {p90_a:.1}ms"
        );
        assert!(p90_b < 60.0, "sidecar 模式 p90 {p90_b:.1}ms 应落在打字无感区间");
        assert!(
            max_b < p50_a,
            "sidecar 模式 max {max_b:.1}ms 应显著低于 full-save 模式 p50 {p50_a:.1}ms"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
