//! 引擎编排:多卷状态、消息处理、跨卷查询、状态汇总。
//!
//! 所有权模型:lstr 的搜索 worker 线程独占 Engine;管道线程把 EngineMsg
//! 塞进 channel,worker 在查询间隙 try_recv 应用(渐进构建期可搜)。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pinyin::PinyinCache;
use crate::query::{Hit, KindFilter, QueryOutcome, QuerySpec};
use crate::sidecar::{self, SidecarOp};
use crate::snapshot::{self, SnapshotHeader};
use crate::store::{CompactTask, VolumeStore, NO_PARENT, ROOT_FRN};
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
    /// worker 不可用(拒绝提权/未运行)，查询返回空结果。
    Fallback,
}

#[derive(Clone, Debug)]
pub struct VolumeProgress {
    pub letter: char,
    pub complete: bool,
    /// 0.0~1.0,未知时为 None。
    pub pct: Option<f32>,
    pub count: u32,
    /// 该卷索引堆内存占用(MB,含列与 n-gram 倒排),供设置页评估
    /// 「哪个盘占内存多」以决定是否取消勾选。0 = 尚未加载。
    pub heap_mb: u32,
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
    /// 旁车日志是否已溢出停捕。溢出本身有界(停捕后不再进日志),此处只做
    /// 一次性「安排尽快全量保存」的闩:否则每个后续批次都会把 save_retry_at
    /// 清成「立即」,高频变更卷会连续整卷重写(实测 E 卷 1 分钟内连发多次)。
    sidecar_overflowed: bool,
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
    /// 在飞旁车写任务的序号:写盘已交给 `lstr-sidecar-write` 线程,该笔的
    /// ack 回来前不再为空追加(避免日志乱序);`None` 表示没有在飞写。
    sidecar_inflight: Option<u64>,
    /// P2-14:旁车日志世代。全量保存成功/清空日志时 +1,使在飞写任务的
    /// ack 失效(watch/dir 模式 base 恒 (0,0),不能只靠基线判断陈旧)。
    sidecar_epoch: u64,
    /// 下次允许旁车写盘的时间(节流;失败按同间隔重试,写盘毫秒级,退避无必要)。
    sidecar_retry_at: Option<Instant>,
    /// 内存日志里已追加进文件的前缀条数:追加式写盘只写
    /// `sidecar_ops[sidecar_flushed..]`,不再每 5s 重写整份日志。
    /// 加载(回放完毕)/重建(save)/清日志时与 ops 同步。
    sidecar_flushed: usize,
    /// watcher 目录刷新的日志聚合窗口(每卷独立):解压/构建期间目录事件可达
    /// 数百条/秒,逐条落日志既慢又刷屏,这里按秒级窗口合成一行。
    watch_dirs: u32,
    watch_added: u64,
    watch_removed: u64,
    watch_log_at: Option<Instant>,
    /// P1-4：自上次全量保存/compact 以来新建或改名的 slot，待下一轮 maintain
    /// 局部刷进拼音缓存。此前只有全量保存（最长 30min）才刷新，中文名新建/
    /// 改名后拼音搜不到或搜错。
    pinyin_dirty: Vec<u32>,
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

    /// 捕获一批变更:进内存日志并标记待写盘;日志已满则停捕并**只安排一次**
    /// 全量保存(游标停在最后一条已捕获批次,缺口由 journal 回放兜底)。
    ///
    /// 溢出闩必须只触发一次:旧实现每个后续批次都写 `save_retry_at = None`,
    /// 把 600s 的全量节流反复抵消;高频变更卷(实测 E 卷,构建/同步盘)
    /// 在日志清空后几秒又溢出,于是 1 分钟内连发多次 200-350ms 的整卷保存。
    /// 这里改为:首次溢出把保存排到 [`SIDECAR_OVERFLOW_SAVE_DELAY`] 之后
    /// (若原本就已到期则保持立即),之后若再次溢出不再改写截止时间。
    fn capture(&mut self, op: SidecarOp, cursor: Option<i64>) {
        if self.sidecar_rec_count() < SIDECAR_MAX_RECS {
            self.sidecar_ops.push(op);
            if let Some(usn) = cursor {
                self.sidecar_cursor = usn;
            }
            self.sidecar_pending = true;
        } else if !self.sidecar_overflowed {
            self.sidecar_overflowed = true;
            let due = Instant::now() + SIDECAR_OVERFLOW_SAVE_DELAY;
            if self.save_retry_at.map_or(true, |at| at > due) {
                self.save_retry_at = Some(due);
            }
            snapshot::debug_log(&format!(
                "sidecar {:032x}: overflow (>{} recs), full save scheduled in {}s",
                self.guid,
                SIDECAR_MAX_RECS,
                SIDECAR_OVERFLOW_SAVE_DELAY.as_secs()
            ));
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

/// P1-4 拼音缓存待刷新队列的单卷上限。
///
/// 稳态 watch 变更量很小；极端 churn（解压/构建）时队列可能暴涨，超出部分
/// 丢弃即可——下一次全量保存会整体刷新，不会长期失真。
const PINYIN_DIRTY_CAP: usize = 65_536;

/// 整卷 compact 前需要的连续空闲时长。大卷 compact 现在跑在独立线程上,
/// 不再占用查询线程;但重排本身会抢 CPU、放大内存带宽,用户连续打字时仍
/// 让它等一个安静窗口,避免查询 P99 被后台任务扰动。留 800ms 覆盖
/// 「停顿一下继续打」的常见节奏。
const COMPACT_IDLE_GAP: Duration = Duration::from_millis(800);

/// 小于该条数的卷直接同步 compact(毫秒级),不为小卷起线程。
/// 阈值以上才交给真正的后台线程:实测 C 卷 123 万条一次重排 2.9–3.6s,
/// 这是唯一会明显拖慢查询的量级。20k 条的整卷重排约 10–20ms,与一次
/// 慢查询同量级,同步做完比线程调度更划算(也让小卷测试保持确定性)。
const COMPACT_BACKGROUND_MIN_LEN: usize = 20_000;

/// 后台 compact 线程每片的时间预算。片间检查取消标志(控制帧重建同卷时
/// 立即收手),不再需要把片切到 4ms —— 查询线程已经完全不受它影响。
const COMPACT_BACKGROUND_SLICE: Duration = Duration::from_millis(25);

/// 后台 compact 结束后,单轮 maintain 最多重放多少条冻结期间积压的消息。
/// 大卷 compact 3s 期间 watcher/USN 可能积压数千条;一次性回放会把查询
/// 线程重新堵住数百 ms。分批回放,宿主在查询间隙继续推进。
const DEFERRED_REPLAY_PER_ROUND: usize = 200;

/// 每轮 maintain 每卷最多回填的 mtime 条数。USN 记录不带 mtime,增量条目
/// 插入后为 0;`FindFirstFileW` 实测约 22µs/条,128 条 ≈ 3ms/轮,宿主空闲
/// 循环间隔数百 ms,可接受且不会把查询线程堵住(查询路径本身不做 FS IO)。
pub const MTIME_BACKFILL_PER_ROUND: usize = 128;

/// 就绪卷两次**全量**快照的最小间隔(秒)。
///
/// 依据(2026-11 实测):USN 稳态下就绪卷每 ~60s 全量保存一次,C 卷 117 万条
/// 每次占引擎线程 1.6-2s,查询每分钟撞上一次 1-3 秒停顿。稳态增量改由旁车
/// 游标承载(sidecar.rs,毫秒级写盘)后,全量快照只需做阶段归档。
///
/// 2026-11-14:600s→1800s。旁车改写为追加式后,全量快照不再是「唯一的变更
/// 载体」,只是归档点;实测 C 卷 65.7MB、D 卷 78MB 的整文件重写是磁盘写入的
/// 绝对大头(C 卷每 6.7 分钟一次 ≈ 14GB/天)。放宽到 30 分钟把这份写入压到
/// 约 1/3,代价只是「旁车文件损坏时」的 journal 回放窗口变长(默认
/// 32MB journal 在本机 ~14MB/h 的变更量下可覆盖约 2 小时,余量充足);
/// 正常路径(旁车完好)崩溃恢复完全不受影响。对账收尾/优雅退出仍立即全量。
const FULL_SAVE_INTERVAL_SECS: u64 = 1800;

/// 旁车写盘节流:USN 轮询 500ms 一批,变更按序积压在内存日志,合并 5s 追加
/// 写一次(只写新增块,几 KB)。崩溃最多丢一个节流窗口的日志 —— 游标以旁车
/// 文件里的为准,缺口由 journal 回放补齐,不存在丢数据。
const SIDECAR_SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// 旁车日志容量上限(变更条数):超出即触发全量保存并停捕(内存有界)。
///
/// 2026-11-14:8192→32768。上限决定「多久全量重写一次整卷快照」——旧值在
/// C 卷(~20 条/s 变更)下 6.7 分钟就撞顶,每撞一次写 65MB。32k 条约占
/// 2.5MB 内存、文件约 6.5MB,启动回放仍是毫秒~百毫秒级,把全量间隔拉到
/// 20 分钟以上;真正的兜底仍由 FULL_SAVE_INTERVAL 与溢出闩共同保证。
const SIDECAR_MAX_RECS: usize = 32768;

/// 旁车日志溢出后,全量保存的最小延迟:溢出本身有界(停捕),无需秒级抢救;
/// 60s 上限避免高频变更卷把「溢出→整卷保存→几秒后再溢出」打成连环保存。
/// 期间的位点缺口由 journal 回放兜底(crash 安全语义不变)。
const SIDECAR_OVERFLOW_SAVE_DELAY: Duration = Duration::from_secs(60);

/// watcher 目录事件的日志聚合窗口:解压/构建时事件可达数百条/秒,逐条落日志
/// 既刷屏又制造大量小写(旧实现每条一次 open+write+close)。窗口内合并成
/// 「多少个目录、增删多少条」一行,诊断信息不丢。
const WATCH_LOG_INTERVAL: Duration = Duration::from_secs(5);

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

/// 排除规则表(共享槽)。设置页「立即重建」时改槽内容 → 引擎在建新 store/
/// 落快照指纹时取到新表;测试用 from_static 静态表。
#[derive(Clone)]
pub struct NoiseRules {
    slot: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    static_raw: Vec<String>,
}

impl NoiseRules {
    /// 静态表(测试/简单构造)。
    pub fn from_static(raw: Vec<String>) -> Self {
        Self { slot: None, static_raw: raw }
    }

    /// 内置默认段名(node_modules 等,与 noise::NOISE_SEGMENTS 同源)。
    pub fn builtin() -> Self {
        Self::from_static(crate::noise::builtin_patterns())
    }

    /// 共享槽(生产:设置页运行期改表,「立即重建」时生效)。
    pub fn from_slot(slot: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        Self { slot: Some(slot), static_raw: Vec::new() }
    }

    /// 当前原始模式表。
    pub fn raw(&self) -> Vec<String> {
        match &self.slot {
            Some(s) => s.lock().map(|g| g.clone()).unwrap_or_default(),
            None => self.static_raw.clone(),
        }
    }

    /// 当前规则集(归一化)。
    pub fn rules(&self) -> crate::noise::ExcludeRules {
        crate::noise::ExcludeRules::build(&self.raw())
    }
}

impl Default for NoiseRules {
    fn default() -> Self {
        Self::from_static(Vec::new())
    }
}

pub struct EngineConfig {
    pub index_dir: PathBuf,
    /// 排除规则表(共享槽/静态表,见 [`NoiseRules`])。
    pub noise: NoiseRules,
    pub candidate_cap: usize,
    /// 勾选要索引的盘符(大写单字母,如 `'C'`);空 vec = 全部卷。
    /// 只影响启动时加载哪些 `vol_*.bin` 快照:未选中卷的旧快照留在磁盘上,
    /// 重新勾选后下次启动可直接复用,无需重扫。
    pub volumes: Vec<char>,
}

/// 一次遍历同时产出的「文件夹路 + 文件路」结果(取代旧的双路两次全引擎查询)。
///
/// 语义与两次独立查询逐字节对齐:
/// - `folder_hits` = 旧 `search_split(folders=true)` 的引擎候选(Folders 前缀);
/// - `file_hits` = 旧 `search_split(folders=false)` 的 All 前缀里的非文件夹项;
/// - `total_folders` = 旧文件夹路的精确计数(TOTAL_CAP 封顶);
/// - `total_all` = 旧 All 路的精确计数(TOTAL_CAP 封顶);
///   旧 files 路的总数 = total_all - total_folders;
/// - `exact_hits` = 旧 `Engine::query(exact spec)` 的候选(仅
///   `query_split_with_exact` 填):同名 offs 二分结果 + 拼音补集,顺序一致。
#[derive(Clone, Debug, Default)]
pub struct SplitOutcome {
    pub file_hits: Vec<Hit>,
    pub folder_hits: Vec<Hit>,
    pub total_all: usize,
    pub total_folders: usize,
    pub exact_hits: Vec<Hit>,
    /// 前缀补集(A-3):名字以查询词开头的命中,offs 二分直接拿。独立于
    /// file/folder_hits,不并入主候选也不计入 total——保证宽查询的前缀命中
    /// 不被字母序靠前的海量子串命中挤出候选窗,去重交给 finalize。
    pub prefix_hits: Vec<Hit>,
    pub aborted: bool,
}

/// 前缀补集单卷收集上限:补集只是把前缀命中送进排序池,rank 会再按分
/// 挑选,200 条足够覆盖常见宽查询;builtin 侧再截到展示预算。
const PREFIX_SUPPLEMENT_CAP: usize = 200;

/// 投递给旁车写线程的一笔写任务。
struct SidecarJob {
    seq: u64,
    dir: PathBuf,
    guid: u128,
    letter: char,
    base: (u64, i64),
    cursor: i64,
    /// 投递时内存旁车日志的总条数(ack 回来后作为已落盘前缀)。
    total: usize,
    ops: Vec<SidecarOp>,
    /// P2-14:投递时的日志世代;ack 回来时世代不符则丢弃。
    epoch: u64,
    /// P2-14:true 表示删除该卷旁车文件(与 append 在同一线程串行,
    /// 避免全量保存清理与在飞 append 的 open/create 竞态)。
    remove: bool,
}

/// 旁车写线程回来的完成回执。
struct SidecarDone {
    seq: u64,
    guid: u128,
    base: (u64, i64),
    total: usize,
    result: Result<(), String>,
    /// P2-14:投递时的日志世代,用于识别已被全量保存清空/换基线的陈旧 ack。
    epoch: u64,
}

/// 旁车写盘线程:把 open / write / `sync_data` 全部搬离搜索线程。
///
/// 背景(评审 P1-1):旧实现在 maintain 中同步调 `sidecar::append`,其内
/// `f.sync_data()` 在杀软/文件过滤驱动在场时单次可卡 1–12s(engine.log 实测
/// 139 条 `sidecar vol ... took`,max 11938ms;紧邻的 `idle maintenance` max
/// 15259ms)——用户看到「输入后列表卡几秒」。内存旁车日志是权威副本,写失败
/// 下一轮重试即可,所以整笔写盘交给这个线程,搜索线程只做入队与收 ack。
struct SidecarWriter {
    tx: Option<mpsc::Sender<SidecarJob>>,
    done: Receiver<SidecarDone>,
    join: Option<std::thread::JoinHandle<()>>,
    next_seq: u64,
}

impl SidecarWriter {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<SidecarJob>();
        let (done_tx, done) = mpsc::channel::<SidecarDone>();
        let join = std::thread::Builder::new()
            .name("lstr-sidecar-write".to_string())
            .spawn(move || {
                for job in rx {
                    if job.remove {
                        // P2-14:删除也走这个线程,保证与 append 串行。
                        sidecar::remove(&job.dir, job.guid);
                        let _ = done_tx.send(SidecarDone {
                            seq: job.seq,
                            guid: job.guid,
                            base: job.base,
                            total: 0,
                            result: Ok(()),
                            epoch: job.epoch,
                        });
                        continue;
                    }
                    let started = Instant::now();
                    let result = sidecar::append(
                        &job.dir,
                        job.guid,
                        job.base,
                        job.cursor,
                        &job.ops,
                    )
                    .map_err(|e| e.to_string());
                    let ms = started.elapsed().as_millis();
                    if ms > 100 {
                        // 诊断日志:异步后不再直接卡搜索线程,但慢盘/AV 环境
                        // 仍需能看出「写盘本身慢」而不是「搜索慢」。
                        snapshot::debug_log(&format!(
                            "sidecar vol {} took {ms}ms ({} ops, async)",
                            job.letter,
                            job.ops.len()
                        ));
                    }
                    let _ = done_tx.send(SidecarDone {
                        seq: job.seq,
                        guid: job.guid,
                        base: job.base,
                        total: job.total,
                        result,
                        epoch: job.epoch,
                    });
                }
            })
            .expect("spawn lstr-sidecar-write thread");
        Self {
            tx: Some(tx),
            done,
            join: Some(join),
            next_seq: 1,
        }
    }
}

impl Drop for SidecarWriter {
    fn drop(&mut self) {
        // 先关队列让写线程收尾(已入队的任务仍会执行),再等它退出:搜索引擎
        // 被丢弃时最后一笔旁车不能丢。worker 退出走 `process::exit` 不跑
        // Drop,那条路径靠 `flush_all` 末尾的 `wait_sidecar_idle` 兜。
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
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
    /// 元数据回填探针:路径 → (size, mtime)（None = 文件已删/无权限）。
    /// None 时用生产实现 `win::find_meta`;测试注入假探针，避免依赖真实磁盘。
    meta_probe: Option<Box<dyn Fn(&str) -> Option<(u64, i64)> + Send>>,
    /// 让路发生次数(诊断/基准断言:批间让路必须真的发生过)。
    apply_yields: u64,
    /// 正在进行的可中断全量保存(一次一个卷)。保存期间 store 冻结:
    /// 修改类 EngineMsg 进 deferred,查询继续按冻结视图服务。
    save_task: Option<snapshot::SaveTask>,
    /// 保存期间到达的修改/控制消息(FIFO),保存收尾后按原序重放。
    deferred_msgs: std::collections::VecDeque<EngineMsg>,
    /// 最近一次查询开始的时间。整卷 compact 现在跑在独立线程,但重排会抢
    /// CPU/内存带宽;空闲维护仍等一个安静窗口再启动,避免打字期间扰动查询。
    last_query_at: std::cell::Cell<Instant>,
    /// 允许 compact 前需要连续空闲多久;测试可调到 0。
    compact_idle_gap: Duration,
    /// 运行中的真后台 compact(同一时刻至多一卷;期间修改类消息进 deferred)。
    /// 查询线程只做 channel 轮询/换血,不再执行任何 compact 切片。
    bg_compact: Option<BackgroundCompact>,
    /// 旁车日志异步写盘线程(见 [`SidecarWriter`] 的说明)。
    sidecar_writer: SidecarWriter,
}

/// 一次后台 compact:在独立线程里对 fork 出的 store 快照重排,完成后把
/// 新列/拼音缓存经 channel 送回引擎线程做 O(1) 换血。
struct BackgroundCompact {
    guid: u128,
    /// 完成时按 guid 重新定位,不信任旧下标(卷可能已被移除/重排)。
    started: Instant,
    cancel: Arc<AtomicBool>,
    rx: Receiver<CompactOutcome>,
}

struct CompactOutcome {
    guid: u128,
    store: VolumeStore,
    pinyin: PinyinCache,
    wall: Duration,
    busy: Duration,
    slices: u32,
}

/// compact 启动方式:小卷同步跑完(毫秒级),大卷交给后台线程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompactStart {
    /// 不需要 compact,或已有后台任务在跑。
    Skipped,
    /// 小卷已在当前线程同步压完。
    Inline,
    /// 已交给真后台线程,完成后由 maintain 换血。
    Background,
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
            meta_probe: None,
            apply_yields: 0,
            save_task: None,
            deferred_msgs: std::collections::VecDeque::new(),
            // 初始化为「久远」:启动后的空闲维护立刻允许 compact,
            // 不需要先等一个空闲窗口。
            last_query_at: std::cell::Cell::new(
                Instant::now() - std::time::Duration::from_secs(60),
            ),
            compact_idle_gap: COMPACT_IDLE_GAP,
            bg_compact: None,
            sidecar_writer: SidecarWriter::spawn(),
        };
        let t_load = Instant::now();
        engine.load_snapshots();
        engine.recompute_phase();
        snapshot::debug_log(&format!(
            "engine: loaded {} volumes in {}ms",
            engine.volumes.len(),
            t_load.elapsed().as_millis()
        ));
        engine
    }

    /// 测试/基准用:调整 compact 的空闲门限。
    #[cfg(test)]
    pub fn set_compact_idle_gap(&mut self, gap: Duration) {
        self.compact_idle_gap = gap;
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
        // 加载时回放的 ops 已经在旁车里(文件即权威副本),追加游标直接到末尾,
        // 否则第一次 maintain 会把整份日志再写一遍。
        let sidecar_flushed = sidecar_ops.len();
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
            sidecar_overflowed: false,
            sidecar_ops,
            sidecar_flushed,
            sidecar_base,
            sidecar_cursor,
            sidecar_pending: false,
            sidecar_inflight: None,
            sidecar_epoch: 0,
            sidecar_retry_at: None,
            capture_on: true,
            usn_watch: UsnWatchState::Off,
            cursor_at: None,
            watch_dirs: 0,
            watch_added: 0,
            watch_removed: 0,
            watch_log_at: None,
            pinyin_dirty: Vec::new(),
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
            if let Some(snap) = snapshot::load(&dir, guid, self.noise_hash(), &self.noise_rules()) {
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
                            "sidecar {guid:032x}: invalid or incompatible, discarded \
                             (recover via journal replay)"
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
                // 内存分项诊断:每次启动加载快照后落一次,定位最肥的列供瘦身。
                if let Some(store) = &self.volumes[i].store {
                    snapshot::debug_log(&format!(
                        "mem vol {} entries={} {}",
                        self.volumes[i].letter,
                        store.len(),
                        store.memory_breakdown_mb()
                    ));
                }
            }
        }
        // 拼音缓存是启动加载的第二大头(仅在内存里建索引,不读盘)。
        // 与快照解析分开计时:冷启动 13s 这类报告要能区分「读盘/解析」和
        // 「建拼音」,否则只能猜。
        let t_pinyin = Instant::now();
        self.pinyin = self
            .volumes
            .iter()
            .map(|v| v.store.as_ref().map(|s| PinyinCache::rebuild(s)).unwrap_or_else(PinyinCache::empty))
            .collect();
        let pinyin_ms = t_pinyin.elapsed().as_millis();
        if pinyin_ms > 50 {
            snapshot::debug_log(&format!("engine: pinyin cache built in {pinyin_ms}ms"));
        }
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
                sidecar_overflowed: false,
                sidecar_ops: Vec::new(),
                sidecar_flushed: 0,
                sidecar_base: (0, 0),
                sidecar_cursor: 0,
                sidecar_pending: false,
                sidecar_inflight: None,
            sidecar_epoch: 0,
                sidecar_retry_at: None,
                capture_on: false,
                usn_watch: UsnWatchState::Off,
                cursor_at: None,
                watch_dirs: 0,
                watch_added: 0,
                watch_removed: 0,
                watch_log_at: None,
                pinyin_dirty: Vec::new(),
            });
            self.pinyin.push(PinyinCache::empty());
            let i = self.volumes.len() - 1;
            self.bump_generation(i);
            return Some(i);
        }
        None
    }

    /// 当前排除规则集(共享槽实时取,建 store/判定用)。
    fn noise_rules(&self) -> crate::noise::ExcludeRules {
        self.cfg.noise.rules()
    }

    /// 排除规则内容哈希(快照指纹)。
    fn noise_hash(&self) -> u64 {
        self.noise_rules().hash()
    }

    /// 排除规则变更后的卷重置:`letters = None` 重置全部,否则仅这些盘。
    /// 丢弃 store(旧 NOISE 位烙在列里)、清拼音、标记未就绪并失效代次;
    /// 随后 Rescan 掩码触发逐卷重扫。若 Rescan 丢失,卷处于未就绪态,
    /// 下次重连握手也会按未完成卷重新扫描(自愈)。返回受影响盘符列表。
    pub fn reset_volumes_for_noise(&mut self, letters: Option<&[char]>) -> Vec<char> {
        let mut affected: Vec<usize> = Vec::new();
        for (i, v) in self.volumes.iter().enumerate() {
            let hit = match letters {
                None => true,
                // 盘符大小写无关(worker 上报盘符可能是小写)
                Some(ls) => ls.iter().any(|l| {
                    l.to_ascii_uppercase() == v.letter.to_ascii_uppercase()
                }),
            };
            if hit && (v.store.is_some() || v.complete) {
                affected.push(i);
            }
        }
        let mut out = Vec::with_capacity(affected.len());
        for i in affected {
            let letter = self.volumes[i].letter;
            // 该卷的后台 compact 若还在跑,必须先中止:fork 基于旧噪声规则,
            // 清空 store 之后它若完成换血会把旧列复活(complete=false 的僵尸
            // 卷),要等到下一次 ScanBegin 才纠正。
            if self.compact_guid() == Some(self.volumes[i].guid) {
                self.abort_compact("noise rules reset");
            }
            let v = &mut self.volumes[i];
            v.store = None;
            v.complete = false;
            v.capture_on = false;
            v.sidecar_ops = Vec::new();
            v.sidecar_flushed = 0;
            v.sidecar_pending = false;
            v.sidecar_overflowed = false;
            // P2-14:清空日志即换代,在飞写的 ack 不得再把 flushed 抬回来。
            v.sidecar_epoch = v.sidecar_epoch.wrapping_add(1);
            self.pinyin[i] = PinyinCache::empty();
            self.bump_generation(i);
            out.push(letter);
        }
        out
    }

    /// 已收到 ScanBegin(存在 store)的卷盘符:「立即重建」的重发自愈判定用。
    pub fn letters_with_store(&self) -> Vec<char> {
        self.volumes
            .iter()
            .filter(|v| v.store.is_some())
            .map(|v| v.letter)
            .collect()
    }

    pub fn handle(&mut self, msg: EngineMsg) {
        // 保存期间 store 必须冻结(快照按启动时的冻结视图写列);修改类消息
        // 按到达顺序排队,保存收尾后原序重放。查询不走 EngineMsg,不受影响。
        if let Some(saved_guid) = self.save_guid() {
            match &msg {
                // 纯状态帧:不碰 store,立即生效(UI 的 USN 状态不因保存停滞)。
                EngineMsg::UsnWatch { .. } | EngineMsg::WorkerLost => {}
                // 会重建/清空 store 的控制帧:中止本次保存,先按序重放已排队
                // 消息,再处理它自己(否则旧视图会被写进已被取代的卷)。
                EngineMsg::ScanBegin { guid, .. }
                | EngineMsg::JournalReset { guid }
                | EngineMsg::VolumeGone { guid }
                    if *guid == saved_guid =>
                {
                    self.abort_save("control message superseded frozen store");
                    self.flush_deferred();
                }
                _ => {
                    self.deferred_msgs.push_back(msg);
                    return;
                }
            }
        }
        // 后台 compact 期间同样冻结修改类消息(与保存一致):任务持有 fork 快照,
        // 换血前引擎列的 slot/排序必须保持不变。控制帧重建/清空同卷时中止任务。
        // 注意冻结是全局的(不止 compact 的那一卷):300 万条的卷后台重排约 2~3s,
        // 这段时间所有卷的增量都排在 deferred_msgs,换血后 200 条/轮按序重放。
        // 取舍:保住 slot 稳定性,代价是 USN 更新最多延迟一个 compact 窗口。
        if let Some(compact_guid) = self.compact_guid() {
            match &msg {
                EngineMsg::UsnWatch { .. } | EngineMsg::WorkerLost => {}
                EngineMsg::ScanBegin { guid, .. }
                | EngineMsg::JournalReset { guid }
                | EngineMsg::VolumeGone { guid }
                    if *guid == compact_guid =>
                {
                    self.abort_compact("control message superseded background compact");
                    self.flush_deferred();
                }
                _ => {
                    self.deferred_msgs.push_back(msg);
                    return;
                }
            }
        }
        match msg {
            EngineMsg::WorkerLost => {
                self.phase = Phase::Fallback;
                self.phase_since = Instant::now();
            }
            EngineMsg::VolumeMeta { guid, letter, journal_id, next_usn, est_records } => {
                self.clear_fallback();
                if let Some(i) = self.volume_idx(guid, true, letter) {
                    let v = &mut self.volumes[i];
                    v.letter = letter;
                    // 锚定路径模式的盘符过滤依赖盘符:store 已存在时同步
                    if let Some(s) = &mut v.store {
                        s.set_letter(letter);
                    }
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
                self.clear_fallback();
                if let Some(i) = self.volume_idx(guid, true, '?') {
                    let rules = self.noise_rules();
                    {
                        let v = &mut self.volumes[i];
                        v.resume_frn = resume_frn;
                        v.complete = false;
                        if resume_frn == 0 {
                            // 从头重扫(显式 Rescan/journal 重建):旧 store 的
                            // NOISE 位按旧规则烙在列里,必须整卷丢弃重建,
                            // 否则排除规则变更不会生效。
                            v.store = None;
                            self.pinyin[i] = PinyinCache::empty();
                        }
                        if v.store.is_none() {
                            v.store = Some(VolumeStore::new(rules, v.letter));
                        }
                        // 重扫窗口不进旁车:store 将整卷重建,断点由 20s 全量
                        // 快照承载。日志清零;旧旁车文件仍与旧快照自洽,留着
                        // (基线守卫在加载时兜底)。
                        v.capture_on = false;
                        v.sidecar_ops = Vec::new();
                        v.sidecar_flushed = 0;
                        v.sidecar_pending = false;
                        v.sidecar_overflowed = false;
                        v.sidecar_cursor = v.next_usn;
                        v.sidecar_base = (v.journal_id, v.next_usn);
                        // P2-14:重扫窗口清空日志,在飞 ack 按世代丢弃。
                        v.sidecar_epoch = v.sidecar_epoch.wrapping_add(1);
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
                    // P2-8：默认增量后端是 journal（worker.rs 的 USN 轮询），这条
                    // 路径不经过 watcher；只靠 DirChildren 收集 pinyin_dirty 会让
                    // 中文名新建/改名在默认配置下仍要等最长 30min 的全量保存才
                    // 能拼音命中。这里把本批记录对应的 slot 也收进待刷新表。
                    if self.volumes[i].complete {
                        let slots: Vec<u32> = match &self.volumes[i].store {
                            Some(store) if !store.is_scanning() => recs
                                .iter()
                                .filter_map(|r| store.find_slot(r.frn))
                                .collect(),
                            _ => Vec::new(),
                        };
                        let room =
                            PINYIN_DIRTY_CAP.saturating_sub(self.volumes[i].pinyin_dirty.len());
                        self.volumes[i]
                            .pinyin_dirty
                            .extend(slots.into_iter().take(room));
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
                {
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
                    if let Some(store) = &v.store {
                        let _ = count.max(store.len() as u32);
                    }
                }
                // 首扫收尾的整卷 compact:大卷交给真后台线程,小卷同步跑完
                // (毫秒级)。换血前的查询继续用扫描态 store(功能正确,只是
                // 少了名序倒排),不再让查询线程等整卷重排。
                self.start_compact_for(i);
                // 内存分项诊断:卷就绪时落一次日志,定位最肥的列供瘦身。
                if let Some(store) = &self.volumes[i].store {
                    crate::snapshot::debug_log(&format!(
                        "mem vol {} entries={} {}",
                        self.volumes[i].letter,
                        store.len(),
                        store.memory_breakdown_mb()
                    ));
                }
            }
            EngineMsg::DirSyncBegin { guid } => {
                if let Some(i) = self.volume_idx(guid, true, '?') {
                    if self.volumes[i].store.is_none() {
                        let rules = self.noise_rules();
                        let letter = self.volumes[i].letter;
                        self.volumes[i].store = Some(VolumeStore::new(rules, letter));
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
                    store.remove_unseen()
                };
                // 对账收尾的整卷 compact 实测 3.4-3.6s(C 123 万条):大卷现在
                // 交给真后台线程,引擎线程只做 O(1) 换血后继续服务查询。
                // 小卷(<20k)仍同步跑完,毫秒级,不值得起线程。
                let start = self.start_compact_for(i);
                // 对账删条目后旧 Hit 必须失效(即使后台 compact 还没换血)。
                self.bump_generation(i);
                if start != CompactStart::Background {
                    // 后台换血会带回新拼音;同步/Skipped 路径立即重建。
                    if let Some(store) = &self.volumes[i].store {
                        self.pinyin[i] = PinyinCache::rebuild(store);
                    }
                }
                self.volumes[i].syncing = false;
                self.volumes[i].dirty = true;
                // 对账结果立即落盘:这是本轮启动后最值钱的快照。
                // (后台 compact 未完成时保存由后续 maintain 轮次接手)
                self.volumes[i].save_retry_at = None;
                self.recompute_phase();
                let (count, letter) = self
                    .volumes
                    .get(i)
                    .map(|v| (v.store.as_ref().map(|s| s.len()).unwrap_or(0), v.letter))
                    .unwrap_or((0, '?'));
                snapshot::debug_log(&format!(
                    "dirsync end {guid:032x} letter={letter} removed={removed} count={count}{}",
                    if start == CompactStart::Background { " (compact background)" } else { "" }
                ));
            }
            EngineMsg::DirChildren { guid, parent_frn, recs } => {
                let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
                    return;
                };
                let mut recs = recs;
                // 噪声子树入口短路(P0-4):watch 每收到一个变更就枚举整个父目录并
                // 整批送进来,构建产物/依赖目录的 churn 会把这个帧顶到数万条,把旁车
                // 挤到 32768 条上限后每 60s 触发一次 62MB 全量重写。
                // 硬噪声父链:子项全部不可见,整帧丢弃;软噪声(构建产物):只保留
                // 目录与可执行文件(与 store 的 classify 语义一致),其余不进索引/旁车。
                if parent_frn != ROOT_FRN {
                    if let Some(store) = &self.volumes[i].store {
                        if let Some(parent_slot) = store.find_slot(parent_frn) {
                            let (hard, soft) = store.parent_noise_kind(parent_slot);
                            if hard {
                                return;
                            }
                            if soft {
                                recs.retain(|r| {
                                    r.attrs & 0x10 != 0
                                        || crate::noise::is_exec_name(
                                            r.name.to_ascii_lowercase().as_bytes(),
                                        )
                                });
                            }
                        }
                    }
                }
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
                // P1-4：本帧新建/改名条目立即排进拼音缓存刷新队列，下一轮
                // maintain 局部刷新（此前要等全量保存，最长 30 分钟）。
                if !recs.is_empty() {
                    let slots: Vec<u32> = match &self.volumes[i].store {
                        Some(store) => recs
                            .iter()
                            .filter_map(|rec| store.find_slot(rec.frn))
                            .collect(),
                        None => Vec::new(),
                    };
                    let v = &mut self.volumes[i];
                    let room = PINYIN_DIRTY_CAP.saturating_sub(v.pinyin_dirty.len());
                    v.pinyin_dirty.extend(slots.into_iter().take(room));
                }
                if added > 0 || removed > 0 {
                    self.note_watch_event(i, added, removed);
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
                        v.sidecar_flushed = 0;
                        v.sidecar_pending = false;
                        v.sidecar_overflowed = false;
                        v.sidecar_base = (0, 0);
                        v.sidecar_cursor = 0;
                        v.sidecar_epoch = v.sidecar_epoch.wrapping_add(1);
                        // 状态显示字段一并重置:旧断点/旧游标时间对已清空的
                        // 卷没有意义(pct 进度条、游标 age 会显示陈旧值)。
                        v.resume_frn = 0;
                        v.cursor_at = None;
                    }
                    // 索引已清空,旧 Hit 一律失效(H8)
                    self.bump_generation(i);
                    self.pinyin[i] = PinyinCache::empty();
                    let dir = self.cfg.index_dir.clone();
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&dir, guid));
                    // P2-14:删除也排队,避免与在飞 append 竞态留下旧基线文件。
                    self.enqueue_sidecar_remove(i);
                }
            }
            EngineMsg::VolumeGone { guid } => {
                if let Some(i) = self.volumes.iter().position(|v| v.guid == guid) {
                    // 未消化的分批缓冲随卷丢弃
                    self.pending_applies.retain(|p| p.guid != guid);
                    // P2-14:卷移除前把旁车删除排进写线程队列。
                    self.enqueue_sidecar_remove(i);
                    // swap_remove:O(1) 收尾。代次跨卷唯一,旧 Hit 的 (vol,generation)
                    // 要么越界、要么代次不符,由访问器的校验拒绝(H8)。
                    self.volumes.swap_remove(i);
                    self.pinyin.swap_remove(i);
                    let dir = self.cfg.index_dir.clone();
                    let _ = std::fs::remove_file(snapshot::snapshot_path(&dir, guid));
                }
            }
            EngineMsg::VolumeAppeared { guid, letter } => {
                let _ = self.volume_idx(guid, true, letter);
            }
        }
        self.recompute_phase();
    }

    /// 是否有正在进行的可中断保存(宿主据此不阻塞等待请求,持续给保存推进)。
    pub fn save_in_progress(&self) -> bool {
        self.save_task.is_some()
    }

    /// 是否有保存期间积压、待重放的消息。
    pub fn has_deferred_msgs(&self) -> bool {
        !self.deferred_msgs.is_empty()
    }

    fn save_guid(&self) -> Option<u128> {
        self.save_task.as_ref().map(|t| t.guid())
    }

    /// 中止当前保存(丢弃 tmp)。store 未被修改,无需回滚。
    fn abort_save(&mut self, reason: &str) {
        if let Some(task) = self.save_task.take() {
            crate::snapshot::debug_log(&format!(
                "save vol {} aborted after {}ms: {reason}",
                self.volume_letter(task.guid()),
                task.elapsed().as_millis()
            ));
        }
    }

    fn volume_letter(&self, guid: u128) -> char {
        self.volumes
            .iter()
            .find(|v| v.guid == guid)
            .map(|v| v.letter)
            .unwrap_or('?')
    }

    /// 按原序重放保存/compact 期间积压的消息。保存已结束(成功/失败/中止)
    /// 后调用;重放引入的变更照常进入旁车/dirty,不会丢。
    fn flush_deferred(&mut self) {
        self.flush_deferred_some(usize::MAX);
    }

    /// 有界版:[`Self::flush_deferred`] 的查询友好形式。后台 compact 结束
    /// 时积压的消息可能成千上万,一轮全放会把查询线程重新堵住数百 ms;
    /// 每轮只回放一段,剩余留给宿主在查询间隙继续。
    fn flush_deferred_some(&mut self, max: usize) {
        let mut replayed = 0usize;
        while self.save_task.is_none() && self.bg_compact.is_none() && replayed < max {
            let Some(msg) = self.deferred_msgs.pop_front() else {
                return;
            };
            self.handle(msg);
            replayed += 1;
        }
    }

    /// 注入查询让路钩子(见 [`Engine::query_yield`])。宿主(search worker)
    /// 在线程内检查「是否有等待中的查询请求」并注入;测试/基准可注入任意判定。
    pub fn set_query_yield_hook(&mut self, hook: Option<Box<dyn Fn() -> bool + Send>>) {
        self.query_yield = hook;
    }

    /// 注入元数据回填探针(路径 → (size, mtime);None = 已删/无权限,丢弃不重试)。
    /// 生产不注入(None)时走 `win::find_meta`;测试注入假探针即可离线验证。
    pub fn set_meta_probe(
        &mut self,
        probe: Option<Box<dyn Fn(&str) -> Option<(u64, i64)> + Send>>,
    ) {
        self.meta_probe = probe;
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
                store.insert_meta(
                    rec.frn,
                    rec.parent_frn,
                    rec.attrs,
                    rec.name.as_bytes(),
                    rec.mtime,
                    rec.size,
                );
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
        // 保存期间 store 冻结:未消化的首扫/对账缓冲也必须停手(否则会改
        // store,破坏快照视图)。消息在 handle 层已排队,这里只是防御。
        if self.save_task.is_some() {
            return 0;
        }
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

    /// P2-17：收到卷元数据 / 扫描开始说明 worker 已经恢复（Fallback 只在
    /// WorkerLost 时设置）。把 Fallback 清掉，后续 `recompute_phase` 才能
    /// 重新进入 Loading/Scanning/Ready，否则 UI 会一直显示「索引不可用」。
    fn clear_fallback(&mut self) {
        if self.phase == Phase::Fallback {
            self.phase = Phase::Loading;
            self.phase_since = Instant::now();
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
                    heap_mb: (v.store.as_ref().map(|s| s.heap_bytes()).unwrap_or(0)
                        / (1024 * 1024)) as u32,
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
        // 通知空闲维护「刚有查询」:compact 会等这段安静窗口过去再启动。
        self.last_query_at.set(Instant::now());
        let mut out = QueryOutcome::default();
        // 查询取消:UI 每发新搜索就刷新 CANCEL_GEN;这里取快照,热循环发现
        // 代次变化即中止(旧查询如单字符 "a" 命中 200 万条要 600ms+,
        // 不中止会把后续击键堵在队列里)。
        let cancel_my = crate::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed);
        let path_matcher = crate::query::PathMatcher::for_spec(spec);
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
            // 盘符过滤(c:/d: 限定、-c: 排除):卷不在掩码内直接跳过
            if let Some(mask) = spec.vol_mask {
                if mask & crate::query::volume_bit(v.letter) == 0 {
                    continue;
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
            let total_base = out.total;
            out.total += total;
            out.capped |= capped;
            // 命中一律带上索引 mtime(0=未知),候选排序零 FS。
            let hit = |slot: u32| Hit {
                vol: i as u32,
                slot,
                generation,
                mtime: store.mtime[slot as usize] as i64,
            };
            let mut hits: Vec<Hit> = slots.into_iter().map(|slot| hit(slot)).collect();
            // 拼音候选(仅 ASCII 查询;名字里不含查询词时靠拼音命中)
            if !spec.case_sensitive
                && !spec.whole_word
                && spec.regex.is_none()
                && !spec.tokens.is_empty()
                && spec.tokens.iter().all(|t| t.is_ascii())
            {
                let tokens: Vec<Vec<u8>> = spec
                    .tokens
                    .iter()
                    .map(|t| crate::store::fold_case(t.as_bytes()))
                    .collect();
                // 拼音去重用 HashSet（旧实现 hits.iter().any 是 O(n²)，
                // 2000 候选×2000 拼音命中最坏数百万次比较）。
                let mut hit_slots: std::collections::HashSet<u32> =
                    hits.iter().map(|h| h.slot).collect();
                for slot in self.pinyin[i].search(store, &tokens, self.cfg.candidate_cap) {
                    if hit_slots.insert(slot)
                        && crate::query::passes_filters(store, slot, spec)
                        && path_matcher
                            .as_ref()
                            .is_none_or(|m| m.matches(store, slot))
                    {
                        out.total += 1;
                        hits.push(hit(slot));
                    }
                }
            }
            // 深路径前缀后置过滤:仅 UNC 等 PathMatcher 无法处理的前缀需要
            // （本地盘符前缀已在 emit 内与拼音补集里用 PathMatcher 过滤，
            // 再走一遍 path_of+to_lowercase 是冗余的父链拼接+分配）。
            let prefix_handled = path_matcher.as_ref().is_some_and(|m| m.has_prefix());
            if let (Some(prefix), false) = (&spec.path_prefix, prefix_handled) {
                hits.retain(|h| {
                    self.path_of(h)
                        .is_some_and(|p| crate::query::path_in_prefix(&p.to_lowercase(), prefix))
                });
            }
            // 本卷过滤前 total 未超 cap → hit 已全部收集、未截断,过滤后
            // 用实际保留数修正 total;超 cap 时 hits 被截断,保持近似值。
            let vol_total = out.total - total_base;
            if vol_total <= cap {
                out.total = total_base + hits.len();
            }
            out.hits.append(&mut hits);
        }
        out
    }

    /// 「全部」双路一次查询:单次候选遍历同时产出文件夹前缀、文件前缀与两个
    /// 精确计数。旧实现 folders/files 各跑一次全引擎(两次 n-gram/memmem 扫描
    /// + 两次拼音扫描),这里合并成一次;结果语义(顺序/total/去重)与旧双路
    /// 完全一致,由 `query_split_matches_two_pass` 对拍锁死。
    ///
    /// 调用方须传 `spec.kind = KindFilter::All`(分清 kind 由本函数完成)。
    pub fn query_split(&self, spec: &QuerySpec) -> SplitOutcome {
        self.query_split_with_exact(spec, None)
    }

    /// 双路查询 + 精确同名候选并趟。`exact` = 旧 `search_exact` 用的
    /// `query.text.to_lowercase()`(None = 不收集)。候选顺序与旧
    /// `Engine::query(exact spec)` 逐条一致:
    /// - 有名字 token:offs 二分的同名槽位在前,拼音补集在后;
    /// - 无名字 token(纯 ext:/path: 查询):旧 exact 走「过滤器全量列举」,
    ///   等价于本卷 all 前缀(逐条相同),不是同名匹配。
    pub fn query_split_with_exact(
        &self,
        spec: &QuerySpec,
        exact: Option<&str>,
    ) -> SplitOutcome {
        // 传入非 All 的 kind 会让「文件前缀」失去语义;调用方只说「查全部」。
        debug_assert_eq!(spec.kind, KindFilter::All, "query_split 只接受 All-kind 查询");
        self.last_query_at.set(Instant::now());
        let mut out = SplitOutcome::default();
        let cancel_my = crate::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed);
        let path_matcher = crate::query::PathMatcher::for_spec(spec);
        // 与 Engine::query 相同的收集上限:仅过滤器查询(无 name token)放大 8 倍,
        // 否则 candidate_cap。两条前缀各用这个上限(旧双路各自也是这个上限)。
        let cap = if spec.tokens.is_empty() {
            self.cfg.candidate_cap.saturating_mul(8)
        } else {
            self.cfg.candidate_cap
        };
        for (i, v) in self.volumes.iter().enumerate() {
            if crate::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed) != cancel_my {
                out.aborted = true;
                break;
            }
            // 路径前缀带盘符 → 只查对应卷(与 Engine::query 一致)
            if let Some(prefix) = &spec.path_prefix {
                if prefix.len() >= 2 && prefix.as_bytes()[1] == b':' {
                    let want = prefix.as_bytes()[0].to_ascii_lowercase();
                    if want != v.letter.to_ascii_lowercase() as u8 {
                        continue;
                    }
                }
            }
            // 盘符过滤(与 Engine::query 一致)
            if let Some(mask) = spec.vol_mask {
                if mask & crate::query::volume_bit(v.letter) == 0 {
                    continue;
                }
            }
            let Some(store) = &v.store else { continue };
            let generation = v.generation;
            let (all_slots, folder_slots, total_all, total_folders, aborted) =
                crate::query::query_volume_split_cancel(
                    store,
                    spec,
                    cap,
                    cap,
                    Some(cancel_my),
                );
            if aborted {
                out.aborted = true;
                break;
            }
            let total_all_base = out.total_all;
            let total_folders_base = out.total_folders;
            out.total_all += total_all;
            out.total_folders += total_folders;
            // 命中一律带上索引 mtime(0=未知),候选排序零 FS。
            let hit = |slot: u32| Hit {
                vol: i as u32,
                slot,
                generation,
                mtime: store.mtime[slot as usize] as i64,
            };
            let mut all_hits: Vec<Hit> = all_slots.into_iter().map(|slot| hit(slot)).collect();
            let mut folder_hits: Vec<Hit> =
                folder_slots.into_iter().map(|slot| hit(slot)).collect();
            // 精确同名候选:旧 search_exact 的第二趟,这里并进同一次遍历。
            // 同名槽位由 offs 二分直接拿(成本 ~µs),不需要扫候选。
            let mut exact_hits: Vec<Hit> = Vec::new();
            let mut exact_set: std::collections::HashSet<u32> = std::collections::HashSet::new();
            if let Some(needle) = exact {
                if spec.tokens.is_empty() {
                    // 旧 exact 在无 token 时走过滤器全量列举(= 本卷 all 前缀),
                    // 排序/条数与 all_hits 相同;放在此处分叉以保持逐条一致。
                    exact_hits = all_hits.clone();
                    exact_set = exact_hits.iter().map(|h| h.slot).collect();
                } else {
                    let slots = crate::query::exact_slots(store, spec, needle.as_bytes());
                    exact_hits = slots.into_iter().map(|slot| hit(slot)).collect();
                    exact_set = exact_hits.iter().map(|h| h.slot).collect();
                }
            }
            // 前缀补集(A-3):与精确同名补集并列,offs 二分拿前缀区间,保证
            // 宽查询的前缀命中不被子串候选窗挤出。独立 vec,不并入 all/folder_hits
            // 也不计 total,双路对拍 invariant 不受影响;去重交给 finalize。
            // 全词模式下前缀不一定满足词边界,跳过(主候选 walk 已按 MatchMatcher 过滤)。
            let mut prefix_hits: Vec<Hit> = Vec::new();
            if let Some(needle) = exact {
                if !spec.tokens.is_empty() && spec.regex.is_none() && !spec.whole_word {
                    for slot in crate::query::prefix_slots(
                        store,
                        spec,
                        needle.as_bytes(),
                        PREFIX_SUPPLEMENT_CAP,
                    ) {
                        if path_matcher
                            .as_ref()
                            .is_none_or(|m| m.matches(store, slot))
                        {
                            prefix_hits.push(hit(slot));
                        }
                    }
                }
            }
            // 拼音候选(仅 ASCII 查询;名字里不含查询词时靠拼音命中)。
            // 旧双路各自调一次 pinyin.search(同一序列),这里只扫一次,再按
            // 「是否已在各自 hits 里」独立补进两条前缀并各自计数 —— 与旧实现
            // 的两个 total 口径完全一致。
            if !spec.case_sensitive
                && !spec.whole_word
                && spec.regex.is_none()
                && !spec.tokens.is_empty()
                && spec.tokens.iter().all(|t| t.is_ascii())
            {
                let tokens: Vec<Vec<u8>> = spec
                    .tokens
                    .iter()
                    .map(|t| crate::store::fold_case(t.as_bytes()))
                    .collect();
                let mut all_set: std::collections::HashSet<u32> =
                    all_hits.iter().map(|h| h.slot).collect();
                let mut folder_set: std::collections::HashSet<u32> =
                    folder_hits.iter().map(|h| h.slot).collect();
                for slot in self.pinyin[i].search(store, &tokens, self.cfg.candidate_cap) {
                    if !crate::query::passes_filters(store, slot, spec) {
                        continue;
                    }
                    if path_matcher
                        .as_ref()
                        .is_some_and(|m| !m.matches(store, slot))
                    {
                        continue;
                    }
                    if all_set.insert(slot) {
                        out.total_all += 1;
                        all_hits.push(hit(slot));
                    }
                    if store.is_dir(slot) && folder_set.insert(slot) {
                        out.total_folders += 1;
                        folder_hits.push(hit(slot));
                    }
                    // 旧 exact 的拼音补集与同名槽位去重后按同一序列追加。
                    if exact.is_some() && exact_set.insert(slot) {
                        exact_hits.push(hit(slot));
                    }
                }
            }
            // 拼音扫描/排序段不可中断(缓存 ~0.2% 条目、candidate_cap 封顶);
            // 段后补一次代次检查,保证旧查询不会带完整结果继续拼路径。
            if crate::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed) != cancel_my {
                out.aborted = true;
                break;
            }
            // 深路径前缀后置过滤:目录自身不算命中(见 path_in_prefix)。
            if let Some(prefix) = &spec.path_prefix {
                let in_prefix = |h: &Hit| {
                    self.path_of(h)
                        .is_some_and(|p| crate::query::path_in_prefix(&p.to_lowercase(), prefix))
                };
                all_hits.retain(|h| in_prefix(h));
                folder_hits.retain(|h| in_prefix(h));
                exact_hits.retain(|h| in_prefix(h));
                prefix_hits.retain(|h| in_prefix(h));
            }
            // 精确同名补集绕过了 emit 的路径过滤；相对路径分段（`A\B`）必须
            // 在这里补一次，否则同名的链外条目会混进结果。
            if !spec.path_components.is_empty() {
                if let Some(matcher) = &path_matcher {
                    exact_hits.retain(|h| matcher.matches(store, h.slot));
                }
            }
            // total 精确化(与 Engine::query 同规则):本卷过滤前计数未超 cap
            // 时 hit 已全部收集,用过滤后实际保留数修正;超 cap 保持近似。
            let vol_total_all = out.total_all - total_all_base;
            if vol_total_all <= cap {
                out.total_all = total_all_base + all_hits.len();
            }
            let vol_total_folders = out.total_folders - total_folders_base;
            if vol_total_folders <= cap {
                out.total_folders = total_folders_base + folder_hits.len();
            }
            // 文件前缀 = All 前缀里的非文件夹项(旧 files 路在应用层用 kind_of
            // 丢文件夹;这里等价地按 attrs 目录位提前剔除,少了无用路径拼接)。
            out.file_hits.extend(
                all_hits
                    .into_iter()
                    .filter(|h| !self.is_dir(h)),
            );
            out.folder_hits.append(&mut folder_hits);
            out.exact_hits.append(&mut exact_hits);
            out.prefix_hits.append(&mut prefix_hits);
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
    ///
    /// 只在最后做一次 `String` 分配:父链名字保留为 `&[u8]`(旧实现每层
    /// `from_utf8_lossy().into_owned()` 都会分配一个 String,500 条候选 ×
    /// 路径深度 5 ≈ 2500 次短分配)。
    pub fn path_of(&self, hit: &Hit) -> Option<String> {
        let store = self.store_for_hit(hit)?;
        let v = &self.volumes[hit.vol as usize];
        let mut parts: Vec<&[u8]> = Vec::with_capacity(8);
        let mut cur = hit.slot;
        let mut hops = 0;
        while cur != crate::store::NO_PARENT && hops < 64 {
            // 父已删除(目录被删,子项删除记录还在路上):链不完整,不给路径
            if store.attrs[cur as usize] & crate::store::ATTR_DEAD != 0 {
                return None;
            }
            parts.push(store.name(cur));
            cur = store.parent[cur as usize];
            hops += 1;
        }
        if hops >= 64 {
            return None;
        }
        parts.reverse();
        let mut out = String::with_capacity(2 + parts.iter().map(|p| p.len() + 1).sum::<usize>());
        out.push(v.letter);
        out.push_str(":\\");
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(&String::from_utf8_lossy(part));
        }
        Some(out)
    }

    /// 拼音 (initials, full),无缓存/Hit 失效则 None。
    pub fn pinyin_of(&self, hit: &Hit) -> Option<(&str, &str)> {
        self.store_for_hit(hit)?;
        self.pinyin.get(hit.vol as usize)?.get(hit.slot)
    }

    /// 取一条真实元数据:测试注入的探针优先,生产回落 `win::find_meta`。
    fn probe_meta(&self, path: &str) -> Option<(u64, i64)> {
        match &self.meta_probe {
            Some(probe) => probe(path),
            None => crate::win::find_meta(path).map(|(size, mtime, _)| (size, mtime)),
        }
    }

    /// 是否有真后台 compact 在跑(宿主据此避免忙轮询;查询本身不受影响)。
    pub fn compact_in_progress(&self) -> bool {
        self.bg_compact.is_some()
    }

    fn compact_guid(&self) -> Option<u128> {
        self.bg_compact.as_ref().map(|c| c.guid)
    }

    /// 为第 i 卷启动 compact。小卷同步做完(毫秒级),大卷 fork 一份快照交给
    /// 独立线程;查询线程只保留 channel 轮询与最后的 O(1) 换血。
    ///
    /// 返回 Background 时调用方不要再同步 compact;代次/拼音/脏标记由
    /// [`Self::install_compacted`] 在换血时收尾。
    fn start_compact_for(&mut self, i: usize) -> CompactStart {
        if self.save_task.is_some() || self.bg_compact.is_some() {
            return CompactStart::Skipped;
        }
        let complete = self.volumes[i].complete;
        let (need, len, guid, letter) = {
            let Some(store) = self.volumes[i].store.as_ref() else {
                return CompactStart::Skipped;
            };
            // 未完成的首扫不在这里压:列还会继续追加,由 VolumeReady 收尾。
            // 注意不能只看 needs_compact():扫描中也可能攒够死槽,而 install
            // 会把 scan_mode 提前关掉,后续扫描就跑进增量路径了。
            let need = if store.is_scanning() {
                complete
            } else {
                store.needs_compact()
            };
            (need, store.len(), self.volumes[i].guid, self.volumes[i].letter)
        };
        if !need {
            return CompactStart::Skipped;
        }
        // 分批缓冲未消化完:快照会漏掉这些记录(它们稍后才 apply)。
        // 等宿主把缓冲推进完,下一轮再压。
        if self.has_pending_apply_for(guid) {
            return CompactStart::Skipped;
        }

        if len < COMPACT_BACKGROUND_MIN_LEN {
            return self.compact_inline(i);
        }

        // 大卷:名字区 Arc 共享,其余列浅拷贝(毫秒级);任务在独立线程跑,
        // 期间引擎线程照常服务查询/轮询,修改类消息进 deferred。
        let t_fork = Instant::now();
        let fork = self
            .volumes[i]
            .store
            .as_ref()
            .expect("compact store")
            .fork_for_compact();
        let fork_ms = t_fork.elapsed().as_millis();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        let started = Instant::now();
        let spawn = std::thread::Builder::new()
            .name(format!("lstr-compact-{letter}"))
            .spawn(move || {
                // 与整卷扫描同款负载整形:CPU/IO 都降到 Windows 后台档,
                // 后台重排永远给前台查询让路(代价是大卷重排可能略慢)。
                crate::win::thread_io_background_begin();
                let mut store = fork;
                let Some(mut task) = CompactTask::new(&store) else {
                    crate::win::thread_io_background_end();
                    return;
                };
                loop {
                    if cancel_thread.load(Ordering::Relaxed) {
                        crate::win::thread_io_background_end();
                        return;
                    }
                    if task.advance(&mut store, COMPACT_BACKGROUND_SLICE) {
                        break;
                    }
                }
                if cancel_thread.load(Ordering::Relaxed) {
                    crate::win::thread_io_background_end();
                    return;
                }
                let busy = task.busy();
                let slices = task.slices();
                // 拼音缓存也在后台重建,换血时一次带回;引擎线程零重建。
                let pinyin = PinyinCache::rebuild(&store);
                crate::win::thread_io_background_end();
                let _ = tx.send(CompactOutcome {
                    guid,
                    store,
                    pinyin,
                    wall: started.elapsed(),
                    busy,
                    slices,
                });
            });
        if let Err(err) = spawn {
            snapshot::debug_log(&format!(
                "compact vol {letter} (background) spawn failed: {err}; falling back to inline"
            ));
            // 线程起不来就退回同线程整卷重排:宁可这一次卡住查询线程,也不能
            // 让卷停在扫描态(后续保存还要再同步 compact 一次,更慢)。
            return self.compact_inline(i);
        }
        snapshot::debug_log(&format!(
            "compact vol {letter} (background) begin len={len} fork={fork_ms}ms"
        ));
        self.bg_compact = Some(BackgroundCompact { guid, started, cancel, rx });
        CompactStart::Background
    }

    /// 在引擎线程内同步跑完整卷 compact(小卷主路径;大卷后台线程创建失败
    /// 时的回退)。小卷毫秒级,大卷回退会卡一次查询 —— 前者常态、后者罕见。
    fn compact_inline(&mut self, i: usize) -> CompactStart {
        let letter = self.volumes[i].letter;
        let Some(mut task) = self.volumes[i].store.as_ref().and_then(CompactTask::new) else {
            return CompactStart::Skipped;
        };
        let started = Instant::now();
        let store = self.volumes[i].store.as_mut().expect("compact store");
        while !task.advance(store, Duration::from_secs(60)) {}
        // 小卷同步路径:与旧行为一致,立即失效旧 Hit/拼音。
        if task.installed() {
            self.bump_generation(i);
            if let Some(store) = &self.volumes[i].store {
                self.pinyin[i] = PinyinCache::rebuild(store);
            }
            self.volumes[i].dirty = true;
        }
        let ms = started.elapsed().as_millis();
        if ms > 100 {
            snapshot::debug_log(&format!(
                "compact vol {letter} (inline) done wall={ms}ms busy={}ms slices={}",
                task.busy().as_millis(),
                task.slices()
            ));
        }
        CompactStart::Inline
    }

    /// 轮询后台 compact:完成则换血。返回是否真的安装了一版新列。
    fn poll_background_compact(&mut self) -> bool {
        let Some(job) = &self.bg_compact else { return false };
        match job.rx.try_recv() {
            Ok(outcome) => {
                self.bg_compact = None;
                self.install_compacted(outcome);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                let job = self.bg_compact.take().expect("compact job");
                snapshot::debug_log(&format!(
                    "compact vol {} (background) worker exited without result after {}ms",
                    self.volume_letter(job.guid),
                    job.started.elapsed().as_millis()
                ));
                false
            }
        }
    }

    /// 把后台 compact 的结果换进引擎。旧 store 整体替换,旧 Hit 代次失效;
    /// 拼音缓存随新列一起替换,不在这里重建 1.2M 条。
    fn install_compacted(&mut self, outcome: CompactOutcome) {
        let Some(i) = self.volumes.iter().position(|v| v.guid == outcome.guid) else {
            // 卷已移除:结果直接丢弃(后台线程持有的 fork 随 drop 释放)。
            return;
        };
        // 冻结窗口里卷可能已被清空/重置(噪声规则重置、控制帧先到):此时的
        // fork 基于旧列,换血会把已丢弃的 store 复活成 complete=false 的僵尸
        // 卷,直到下一次 ScanBegin 才纠正。校验不过就丢弃结果。
        if self.volumes[i].store.is_none() || !self.volumes[i].complete {
            snapshot::debug_log(&format!(
                "compact vol {} (background) discarded: store reset during compaction",
                self.volumes[i].letter
            ));
            return;
        }
        let letter = self.volumes[i].letter;
        self.volumes[i].store = Some(outcome.store);
        self.pinyin[i] = outcome.pinyin;
        self.volumes[i].dirty = true;
        self.bump_generation(i);
        snapshot::debug_log(&format!(
            "compact vol {letter} (background) done wall={}ms busy={}ms slices={}",
            outcome.wall.as_millis(),
            outcome.busy.as_millis(),
            outcome.slices
        ));
        // 冻结期间积压的修改按序回放(分轮,避免回放本身把查询堵住)。
        self.flush_deferred_some(DEFERRED_REPLAY_PER_ROUND);
    }

    /// 中止后台 compact(控制帧要重建/清空同一卷)。线程只置取消位并 detach;
    /// 它持有的是独立 fork,不会再碰引擎列,结果经已断开的 channel 丢弃。
    /// 立即回放 deferred 也不会触发 fork 里 names 的 Arc::make_mut 深拷贝:
    /// 除退出超时外,所有中止路径随后都会替换/清空该卷 store,旧 Arc 不再
    /// 与引擎共享;退出路径只落盘,不再改列。
    fn abort_compact(&mut self, reason: &str) {
        let Some(job) = self.bg_compact.take() else { return };
        job.cancel.store(true, Ordering::Relaxed);
        snapshot::debug_log(&format!(
            "compact vol {} (background) aborted after {}ms: {reason}",
            self.volume_letter(job.guid),
            job.started.elapsed().as_millis()
        ));
    }

    /// 退出路径:等待进行中的后台 compact 完成并换血(快照要求列已归并)。
    fn finish_compact_sync(&mut self) {
        while self.bg_compact.is_some() {
            let result = {
                let job = self.bg_compact.as_ref().expect("compact job");
                job.rx.recv_timeout(Duration::from_secs(30))
            };
            match result {
                Ok(outcome) => {
                    self.bg_compact = None;
                    self.install_compacted(outcome);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.abort_compact("shutdown wait timeout");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let job = self.bg_compact.take().expect("compact job");
                    snapshot::debug_log(&format!(
                        "compact vol {} (background) worker exited before shutdown",
                        self.volume_letter(job.guid)
                    ));
                }
            }
        }
    }

    /// 限量回填 USN 增量条目的真实 mtime / size(查询路径不做 FS IO 的配套)。
    ///
    /// 每卷每轮最多 [`MTIME_BACKFILL_PER_ROUND`] 个 slot(两个待回填队列合计);
    /// 已经填过的字段直接丢弃。probe 返回 None(文件已删/无权限)也丢弃 ——
    /// 不再重试,避免对死路径每轮反复 stat。首扫/对账窗口跳过:首扫的元数据
    /// 由目录枚举写入,store 尚未定型。
    ///
    /// 一次 `find_meta` 同时返回 size 与 mtime,所以两个队列合并去重后只 stat
    /// 一次。返回是否真的写入了字段(maintain 的「有事可做」信号)。
    fn backfill_meta(&mut self) -> bool {
        let mut wrote = false;
        for i in 0..self.volumes.len() {
            if self.volumes[i].syncing {
                continue;
            }
            let generation = self.volumes[i].generation;
            let batch = {
                let Some(store) = self.volumes[i].store.as_mut() else { continue };
                if store.is_scanning() {
                    continue; // 首扫元数据来自目录枚举,等 VolumeReady 后再看
                }
                let mut slots = store.take_mtime_pending(MTIME_BACKFILL_PER_ROUND);
                let remaining = MTIME_BACKFILL_PER_ROUND.saturating_sub(slots.len());
                if remaining > 0 {
                    slots.extend(store.take_size_pending(remaining));
                }
                slots
            };
            if batch.is_empty() {
                continue;
            }
            // 同一 slot 可能因多条 USN 记录重复登记:批内先去重,避免同一
            // 路径重复 stat。批间重复由「字段已填」检查兜底。
            let mut slots = batch;
            slots.sort_unstable();
            slots.dedup();
            // (slot, size, mtime, fill_mtime, fill_size)
            let mut found: Vec<(u32, u64, i64, bool, bool)> = Vec::with_capacity(slots.len());
            for slot in slots {
                let Some(store) = self.volumes[i].store.as_ref() else { break };
                if slot as usize >= store.len() || store.name(slot).is_empty() {
                    continue;
                }
                let fill_mtime = store.mtime[slot as usize] == 0;
                let fill_size = store.size[slot as usize] == u64::MAX;
                if !fill_mtime && !fill_size {
                    continue;
                }
                let hit = Hit { vol: i as u32, slot, generation, mtime: 0 };
                // 父链断裂/祖先已删(path_of None):无法定位真实文件,丢弃。
                let Some(path) = self.path_of(&hit) else { continue };
                if let Some((size, mtime)) = self.probe_meta(&path) {
                    if (fill_mtime && mtime > 0) || fill_size {
                        found.push((slot, size, mtime, fill_mtime, fill_size));
                    }
                }
            }
            if found.is_empty() {
                continue;
            }
            let changed = {
                let Some(store) = self.volumes[i].store.as_mut() else { continue };
                let mut changed = false;
                for (slot, size, mtime, fill_mtime, fill_size) in found {
                    if (slot as usize) >= store.len() {
                        continue;
                    }
                    if fill_mtime && store.mtime[slot as usize] == 0 && mtime > 0 {
                        store.mtime[slot as usize] =
                            mtime.clamp(0, u32::MAX as i64) as u32;
                        changed = true;
                    }
                    if fill_size && store.size[slot as usize] == u64::MAX {
                        store.size[slot as usize] = size;
                        changed = true;
                    }
                }
                changed
            };
            if changed {
                self.volumes[i].dirty = true;
                wrote = true;
            }
        }
        wrote
    }

    /// 空闲维护:需要的卷 compact + 落盘(旁车优先,全量降频)。
    /// 返回是否做了事(调用方可据此让路)。
    pub fn maintain(&mut self) -> bool {
        // 先收异步旁车写盘的回执:ack 只改旁车记账(不动 store),放在最前
        // 保证本轮后续的节流判断看到最新「已落盘前缀」。
        self.drain_sidecar_acks();
        // P1-4：把上一轮 watch 收集到的新建/改名 slot 局部刷进拼音缓存。
        // 放在 compact/保存之前:此时 store 未冻结；若随后触发 compact，
        // 完成时会整卷 rebuild，增量刷新只是提前生效。
        self.flush_pinyin_dirty();
        // 0. 后台 compact 完成?先换血再让路:换血后的下一步(分轮回放冻结
        //    消息/保存)放到下一轮,避免同一次 maintain 把查询堵住。
        if self.poll_background_compact() {
            return true;
        }
        // watcher 聚合日志的补发:窗口到点但此后没有新事件时,最后一批计数
        // 否则会一直压在内存里不落日志。
        for i in 0..self.volumes.len() {
            let due = self.volumes[i]
                .watch_log_at
                .is_none_or(|at| at.elapsed() >= WATCH_LOG_INTERVAL);
            if due && self.volumes[i].watch_dirs > 0 {
                Self::flush_watch_log(&mut self.volumes[i]);
            }
        }
        // 后台 compact 结束后的冻结消息:分轮回放,回放期间不启动保存/重排。
        let mut did = false;
        if self.bg_compact.is_none()
            && self.save_task.is_none()
            && !self.deferred_msgs.is_empty()
        {
            self.flush_deferred_some(DEFERRED_REPLAY_PER_ROUND);
            did = true;
            if !self.deferred_msgs.is_empty() {
                return true; // 宿主下一轮继续回放
            }
        }
        // 后台 compact 在跑:本轮不碰 store/磁盘,查询优先。返回 true 表示
        // 「维护仍在进行」(调用方可据此继续推进;宿主不依赖此返回值)。
        if self.bg_compact.is_some() {
            return true;
        }
        // 有分段保存在进行:本轮只推进保存(小步 + 让路),不再做 compact/旁车
        // —— store 冻结,且磁盘争用越小,保存期间的查询延迟越稳。保存收尾后
        // 重放积压的修改消息。
        if self.save_task.is_some() {
            self.advance_save();
            if self.save_task.is_none() {
                self.flush_deferred();
            }
            return true;
        }
        // 先归并(compact 会 remap,旧 Hit 失效),再落盘。
        // 1) 最近 800ms 内有查询、或已有等待中的查询 → 本轮不启动 compact;
        // 2) 每轮至多启动一卷 —— 大卷重排虽然已移到后台线程,仍会抢 CPU/
        //    内存带宽;多卷同时开工只会让查询 P99 变差。
        if self.last_query_at.get().elapsed() >= self.compact_idle_gap {
            for i in 0..self.volumes.len() {
                if self.query_pending() {
                    break; // 打字时查询优先,compact 让路
                }
                if self.volumes[i].syncing {
                    continue; // 对账期间不 compact/落盘:SEEN 位是临时态
                }
                match self.start_compact_for(i) {
                    CompactStart::Skipped => continue,
                    CompactStart::Inline | CompactStart::Background => {
                        // 小卷已同步压完/大卷已交给后台线程:本轮到此为止,
                        // 让宿主回去服务查询,保存留给下一轮。
                        return true;
                    }
                }
            }
        }
        // USN 增量条目的 mtime 回填:compact 已重建 pending,这里拿到的 slot
        // 与当前 store 一致。限量(每卷 128/轮),避免长时间占住引擎线程。
        if self.backfill_meta() {
            did = true;
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
            // 完整卷仍在 scan 模式 = 后台 compact 还没换血:不能落盘 ——
            // save_report 会对它再做一次同步整卷 compact,又把查询堵住。
            // 等 maintain 的 compact 轮次把任务跑完/换血后再保存。
            if self.volumes[i].complete
                && self.volumes[i]
                    .store
                    .as_ref()
                    .is_some_and(|s| s.is_scanning())
            {
                continue;
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
            // 就绪/已归并卷走可中断分段保存:保存期间查询仍按冻结 store 服务,
            // 不再一次占住引擎线程 1-3 秒。仍处于首扫(scan 模式)的卷列布局
            // 未定型(offs 为空、delta=全部),整列视图需要一次性全量排序,
            // 保留原来的同步 compact+写盘(冷路径,20s 断点保存)。
            let view_ready = self.volumes[i]
                .store
                .as_ref()
                .map(|s| !s.is_scanning())
                .unwrap_or(false);
            if view_ready {
                if self.start_save(i) {
                    // 小卷(单测/小盘)一次 maintain 跑完,保持「maintain 返回即
                    // 已落盘」的既有语义;大卷只在 4ms 预算内推进一块,由宿主
                    // 循环继续(查询始终优先)。
                    let small = self.volumes[i]
                        .store
                        .as_ref()
                        .map(|s| s.len() < 50_000)
                        .unwrap_or(false);
                    if small {
                        while self.save_task.is_some() && !self.query_pending() {
                            self.advance_save();
                        }
                        if self.save_task.is_none() {
                            self.flush_deferred();
                        }
                    } else {
                        self.advance_save();
                    }
                    did = true;
                    break;
                }
                continue;
            }
            if self.save_volume(i) {
                did = true;
                break;
            }
        }
        did
    }

    /// 启动分段保存(不写盘,只建状态机)。前置条件与 [`Engine::save_volume`]
    /// 相同:B-1 缓冲必须已清空、非对账窗口。
    fn start_save(&mut self, i: usize) -> bool {
        if self.save_task.is_some() || self.bg_compact.is_some() {
            return false;
        }
        if self.has_pending_apply_for(self.volumes[i].guid) {
            return false;
        }
        let header = {
            let v = &self.volumes[i];
            if v.syncing {
                return false;
            }
            let Some(store) = &v.store else { return false };
            if store.is_scanning() && v.resume_frn == 0 {
                return false;
            }
            SnapshotHeader {
                guid: v.guid,
                letter: v.letter,
                complete: v.complete,
                noise_hash: self.noise_hash(),
                journal_id: v.journal_id,
                next_usn: v.next_usn,
                resume_frn: v.resume_frn,
                est_records: v.est_records,
            }
        };
        self.save_task = Some(snapshot::SaveTask::new(&self.cfg.index_dir, &header));
        true
    }

    /// 推进当前分段保存一步(受 SAVE_STEP_BUDGET 限制)。有等待查询时本轮
    /// 不推进,宿主会先出队服务查询,下轮再续。
    fn advance_save(&mut self) {
        let Some(guid) = self.save_guid() else { return };
        if self.query_pending() {
            return;
        }
        let Some(i) = self.volumes.iter().position(|v| v.guid == guid) else {
            self.abort_save("volume gone");
            return;
        };
        let Some(store) = self.volumes[i].store.as_ref() else {
            self.abort_save("store gone");
            return;
        };
        let step = {
            // 把让路钩子临时借给状态机:块间(每 64K 元素)发现等待中的查询
            // 立即收手,而不是等服务满一个 4ms 预算。take/归还避免同时借用
            // self 与 self.save_task。
            let hook = self.query_yield.take();
            let step = {
                let task = self.save_task.as_mut().expect("save task");
                let mut yield_now = || hook.as_ref().map(|f| f()).unwrap_or(false);
                task.advance(store, snapshot::SAVE_STEP_BUDGET, &mut yield_now)
            };
            self.query_yield = hook;
            step
        };
        match step {
            snapshot::SaveStep::Pending => {}
            snapshot::SaveStep::Failed(err) => {
                let task = self.save_task.take().expect("save task");
                let elapsed = task.elapsed().as_millis();
                drop(task);
                crate::snapshot::debug_log(&format!(
                    "save vol {} failed after {}ms: {err}",
                    self.volumes[i].letter, elapsed
                ));
                self.apply_save_failure(i, err);
            }
            snapshot::SaveStep::Done(report) => {
                let task = self.save_task.take().expect("save task");
                let elapsed = task.elapsed().as_millis();
                drop(task);
                if elapsed > 100 {
                    crate::snapshot::debug_log(&format!(
                        "save vol {} took {}ms (segmented)",
                        self.volumes[i].letter, elapsed
                    ));
                }
                self.apply_save_success(i, report);
            }
        }
    }

    /// 保存成功后的状态收尾:清 dirty/旁车、恢复捕获、推进游标与节流。
    fn apply_save_success(&mut self, i: usize, report: snapshot::SaveReport) {
        if report.compacted {
            // 旧路径(scan 模式)可能在保存中 compact:slot 重排,拼音缓存
            // 与旧 Hit 必须失效。分段视图不改 store,compacted 恒 false。
            self.bump_generation(i);
            if let Some(store) = &self.volumes[i].store {
                self.pinyin[i] = PinyinCache::rebuild(store);
            }
        }
        if !report.compacted {
            // 分段保存不 compact:把上次 compact 以来的新增/改名 slot 局部
            // 刷进拼音缓存(缓存只含非 ASCII,量 <1%),不重建 250 万条。
            let v = &self.volumes[i];
            if let Some(store) = &v.store {
                let mut slots: Vec<u32> =
                    (store.base_len()..store.len() as u32).collect();
                slots.extend(store.renamed_base_slots());
                if !slots.is_empty() {
                    self.pinyin[i].refresh_slots(store, &slots);
                }
            }
        }
        let next_at = Instant::now() + Self::save_interval(&self.volumes[i]);
        {
            let v = &mut self.volumes[i];
            v.dirty = false;
            v.save_backoff = 0;
            v.save_retry_at = Some(next_at);
            // 全量快照已覆盖全部变更:清旁车日志、基线推进到本次游标、
            // 删旁车文件(基线守卫兜底删除失败的情形,双保险)。
            v.sidecar_base = (v.journal_id, v.next_usn);
            v.sidecar_cursor = v.next_usn;
            v.sidecar_ops = Vec::new();
            v.sidecar_flushed = 0;
            v.sidecar_pending = false;
            v.sidecar_retry_at = None;
            v.sidecar_overflowed = false;
            // P2-14:世代 +1,在飞写任务的 ack 归来时按世代丢弃,避免
            // watch/dir 模式(base 恒 (0,0))下把 flushed 抬回旧值。
            v.sidecar_epoch = v.sidecar_epoch.wrapping_add(1);
            v.capture_on = true;
        }
        // P2-14:删除也排进写线程队列,避免与在飞 append 的 open/create 竞态。
        self.enqueue_sidecar_remove(i);
    }

    /// 保存失败:保留 dirty,指数退避(H6)。tmp 由 SaveTask::drop 清掉。
    fn apply_save_failure(&mut self, i: usize, err: std::io::Error) {
        eprintln!("lstr-index: save snapshot failed: {err}");
        crate::snapshot::debug_log(&format!(
            "save {:032x} failed: {err}",
            self.volumes[i].guid
        ));
        let v = &mut self.volumes[i];
        v.save_backoff = v.save_backoff.saturating_add(1);
        // 1s, 2s, 4s … 上限 60s;dirty 保留,下一轮 maintain 再试
        let secs = (1u64 << v.save_backoff.min(6)).min(60);
        v.save_retry_at = Some(Instant::now() + Duration::from_secs(secs));
    }

    /// P2-14:把「删除旁车文件」排进写线程队列,与 append 串行执行。
    ///
    /// 全量保存清空日志时,写线程可能正拿着旧基线文件做 open/create;
    /// 若在引擎线程直接 remove,写线程随后的写会把旧文件重新建出来,
    /// 启动时 base 校验(尤其 watch/dir 模式恒 (0,0))会接受并回放旧 ops。
    /// 只有写线程已退出(Engine Drop 窗口)才回退直接删除。
    fn enqueue_sidecar_remove(&mut self, i: usize) {
        let dir = self.cfg.index_dir.clone();
        let guid = self.volumes[i].guid;
        // 单测里没有真实在飞写与并发,直接删以免测试必须等异步 ack。
        #[cfg(test)]
        {
            sidecar::remove(&dir, guid);
        }
        #[cfg(not(test))]
        {
            let letter = self.volumes[i].letter;
            let epoch = self.volumes[i].sidecar_epoch;
            let seq = self.sidecar_writer.next_seq;
            self.sidecar_writer.next_seq = seq.wrapping_add(1);
            let Some(tx) = self.sidecar_writer.tx.as_ref() else {
                sidecar::remove(&dir, guid);
                return;
            };
            let _ = tx.send(SidecarJob {
                seq,
                dir,
                guid,
                letter,
                base: (0, 0),
                cursor: 0,
                total: 0,
                ops: Vec::new(),
                epoch,
                remove: true,
            });
        }
    }

    /// 把旁车日志新增部分交给写盘线程(异步)。返回是否已受理/已推进:
    /// 单测里会等这笔 ack 回来,保持「maintain 返回即落盘」的旧断言语义;
    /// 生产路径立即返回,`sync_data` 的秒级卡顿不再阻塞搜索线程。
    fn write_sidecar(&mut self, i: usize) -> bool {
        if self.volumes[i].sidecar_inflight.is_some() {
            // 上一笔还没落盘:等它 ack 再追加,避免文件块顺序与 seq 乱序。
            return false;
        }
        let (guid, letter, base, cursor, ops, total, epoch) = {
            let v = &self.volumes[i];
            let from = v.sidecar_flushed.min(v.sidecar_ops.len());
            (
                v.guid,
                v.letter,
                v.sidecar_base,
                v.sidecar_cursor,
                v.sidecar_ops[from..].to_vec(),
                v.sidecar_ops.len(),
                v.sidecar_epoch,
            )
        };
        if ops.is_empty() {
            // 只有游标推进、没有条目级变更:不产生新块,清 pending 即可。
            let v = &mut self.volumes[i];
            v.sidecar_pending = false;
            v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
            return true;
        }
        let Some(tx) = self.sidecar_writer.tx.as_ref() else {
            // 写线程已退出(只可能是 Engine 正在 Drop):回退同步写。
            return self.write_sidecar_sync(i, guid, base, cursor, &ops, total);
        };
        let seq = self.sidecar_writer.next_seq;
        self.sidecar_writer.next_seq = self.sidecar_writer.next_seq.wrapping_add(1);
        let job = SidecarJob {
            seq,
            dir: self.cfg.index_dir.clone(),
            guid,
            letter,
            base,
            cursor,
            total,
            ops,
            epoch,
            remove: false,
        };
        if tx.send(job).is_err() {
            let v = &mut self.volumes[i];
            v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
            return false;
        }
        {
            let v = &mut self.volumes[i];
            v.sidecar_inflight = Some(seq);
            v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
        }
        // 单测里,既有断言假设「maintain/flush_all 返回即旁车文件就位」。
        // 测例量级很小(几十条),同步等这一笔完成不影响测试时长;生产路径
        // 绝不等待 —— 这正是本次改动要消除的搜索线程卡顿。
        #[cfg(test)]
        {
            while self.volumes[i].sidecar_inflight.is_some() {
                if !self.recv_sidecar_ack(Duration::from_secs(30)) {
                    break;
                }
            }
            self.volumes[i].sidecar_flushed >= total
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    /// 写盘线程已退出时的同步回退(仅 Drop 窗口可能走到)。
    fn write_sidecar_sync(
        &mut self,
        i: usize,
        guid: u128,
        base: (u64, i64),
        cursor: i64,
        ops: &[SidecarOp],
        total: usize,
    ) -> bool {
        let started = Instant::now();
        let result = sidecar::append(&self.cfg.index_dir, guid, base, cursor, ops);
        let ms = started.elapsed().as_millis();
        if ms > 100 {
            crate::snapshot::debug_log(&format!(
                "sidecar vol {} took {}ms ({} ops, sync fallback)",
                self.volumes[i].letter,
                ms,
                ops.len()
            ));
        }
        let v = &mut self.volumes[i];
        match result {
            Ok(()) => {
                v.sidecar_flushed = total;
                v.sidecar_pending = false;
                v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
                true
            }
            Err(err) => {
                eprintln!("lstr-index: save sidecar failed: {err}");
                v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
                false
            }
        }
    }

    /// 收掉所有已完成的旁车写回执(不阻塞)。
    fn drain_sidecar_acks(&mut self) {
        let mut acked: Vec<SidecarDone> = Vec::new();
        while let Ok(done) = self.sidecar_writer.done.try_recv() {
            acked.push(done);
        }
        for done in acked {
            self.apply_sidecar_ack(done);
        }
    }

    /// P1-4：把上次全量保存以来新建/改名的 slot 局部刷进拼音缓存。
    ///
    /// `PinyinCache::refresh_slots` 只重算这些 slot（中文名占比 <1%），
    /// 千级 slot 是微秒级；相比整卷 rebuild（C 卷实测 58–62ms）可以放在
    /// maintain 里做。刷新后新文件/改名文件立即可用拼音命中，不再等最长
    /// [`FULL_SAVE_INTERVAL_SECS`] 的保存窗口。
    fn flush_pinyin_dirty(&mut self) {
        for i in 0..self.volumes.len() {
            if self.volumes[i].pinyin_dirty.is_empty() {
                continue;
            }
            let dirty = std::mem::take(&mut self.volumes[i].pinyin_dirty);
            let Some(store) = self.volumes[i].store.as_ref() else {
                continue;
            };
            let started = Instant::now();
            self.pinyin[i].refresh_slots(store, &dirty);
            let ms = started.elapsed().as_millis();
            if ms >= 20 {
                crate::snapshot::debug_log(&format!(
                    "pinyin: refreshed {} slots in {}ms (vol {})",
                    dirty.len(),
                    ms,
                    self.volumes[i].letter
                ));
            }
        }
    }

    /// 处理一笔回执:清在飞标记;基线已变(全量保存清空日志/换基线)的 ack
    /// 视为陈旧直接丢弃;成功推进已落盘前缀,失败保留待写部分下轮重试。
    fn apply_sidecar_ack(&mut self, done: SidecarDone) {
        let Some(i) = self.volumes.iter().position(|v| v.guid == done.guid) else {
            return;
        };
        let v = &mut self.volumes[i];
        if v.sidecar_inflight != Some(done.seq) {
            return;
        }
        v.sidecar_inflight = None;
        if done.epoch != v.sidecar_epoch {
            // P2-14:全量保存已清空日志并推进世代:这笔 ack 对应的内容已被快照
            // 覆盖。watch/dir 模式 base 恒 (0,0),不能只靠基线识别陈旧 ack。
            return;
        }
        if v.sidecar_base != done.base {
            // 全量保存已清空日志并推进基线:这笔 ack 对应的内容已被快照覆盖。
            return;
        }
        match done.result {
            Ok(()) => {
                // 防御:P2-14 起把已落盘前缀夹在当前日志长度内,杜绝
                // 「flushed > ops.len()」导致 from 切空、再也不写盘。
                v.sidecar_flushed = done.total.min(v.sidecar_ops.len());
                v.sidecar_pending = v.sidecar_flushed < v.sidecar_ops.len();
                v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
            }
            Err(err) => {
                eprintln!("lstr-index: save sidecar failed: {err}");
                snapshot::debug_log(&format!("sidecar {:032x} failed: {err}", v.guid));
                v.sidecar_retry_at = Some(Instant::now() + SIDECAR_SAVE_INTERVAL);
            }
        }
    }

    /// 阻塞等一笔回执(仅单测/退出路径用);超时/线程关闭返回 false。
    fn recv_sidecar_ack(&mut self, timeout: Duration) -> bool {
        match self.sidecar_writer.done.recv_timeout(timeout) {
            Ok(done) => {
                self.apply_sidecar_ack(done);
                true
            }
            Err(_) => false,
        }
    }

    /// 退出路径:有界等待在飞旁车写入完成。worker 走 `process::exit` 不跑
    /// Drop,必须显式等,否则最后一笔 async 写可能丢(最多 5s 增量,重启由
    /// journal 回放补齐,能等就等)。
    pub fn wait_sidecar_idle(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.volumes.iter().any(|v| v.sidecar_inflight.is_some()) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let _ = self.recv_sidecar_ack(left.min(Duration::from_millis(100)));
        }
    }

    /// watcher 目录事件计数:窗口内的增删合并成一行日志(见 WATCH_LOG_INTERVAL)。
    fn note_watch_event(&mut self, i: usize, added: usize, removed: usize) {
        let v = &mut self.volumes[i];
        v.watch_dirs = v.watch_dirs.saturating_add(1);
        v.watch_added = v.watch_added.saturating_add(added as u64);
        v.watch_removed = v.watch_removed.saturating_add(removed as u64);
        let due = v
            .watch_log_at
            .is_none_or(|at| at.elapsed() >= WATCH_LOG_INTERVAL);
        if due {
            Self::flush_watch_log(v);
        }
    }

    /// 落一行 watcher 聚合日志并清空计数(无事件时为空操作)。
    fn flush_watch_log(v: &mut VolumeState) {
        if v.watch_dirs == 0 {
            return;
        }
        snapshot::debug_log(&format!(
            "watch {:032x} dirs={} added={} removed={}",
            v.guid, v.watch_dirs, v.watch_added, v.watch_removed
        ));
        v.watch_dirs = 0;
        v.watch_added = 0;
        v.watch_removed = 0;
        v.watch_log_at = Some(Instant::now());
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
                noise_hash: self.noise_hash(),
                journal_id: v.journal_id,
                next_usn: v.next_usn,
                resume_frn: v.resume_frn,
                est_records: v.est_records,
            };
            let view = !store.is_scanning();
            let started = Instant::now();
            // 非 scan 模式:走与分段保存完全相同的列视图/写盘状态机(一次跑完),
            // 不 compact store —— 退出/回落路径与分段路径字节一致。
            let result = if view {
                snapshot::save_view_sync(&self.cfg.index_dir, &header, store)
            } else {
                snapshot::save_report(
                    &self.cfg.index_dir,
                    &header,
                    self.volumes[i].store.as_mut().unwrap(),
                )
            };
            let ms = started.elapsed().as_millis();
            if ms > 100 {
                crate::snapshot::debug_log(&format!(
                    "save vol {} took {}ms{}",
                    self.volumes[i].letter,
                    ms,
                    if view { " (view)" } else { "" }
                ));
            }
            result
        };
        match save_result {
            Ok(report) => {
                self.apply_save_success(i, report);
                true
            }
            Err(err) => {
                self.apply_save_failure(i, err);
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
        // 快照要求列已归并:先把进行中的后台 compact 跑完,并回放冻结消息。
        // 回放如果又启动了一轮 compact(对账/Ready 消息),再等一轮;上限
        // 防御性兜底,正常最多两轮。
        let mut rounds = 0;
        while (self.bg_compact.is_some() || !self.deferred_msgs.is_empty()) && rounds < 8 {
            self.finish_compact_sync();
            self.flush_deferred();
            rounds += 1;
        }
        // 兜底:轮数用尽仍有积压(回放本身可能又启动一轮 compact)时,退出前
        // 再无条件收一轮。否则这些已越过 USN 游标的增量消息会静默丢失,下次
        // 启动只能靠全量重扫找回。
        if !self.deferred_msgs.is_empty() || self.bg_compact.is_some() {
            self.finish_compact_sync();
            self.flush_deferred();
            self.finish_compact_sync();
            if !self.deferred_msgs.is_empty() {
                snapshot::debug_log(&format!(
                    "flush_all: {} deferred messages still pending at exit",
                    self.deferred_msgs.len()
                ));
            }
        }
        // 退出路径没有查询压力:把进行中的分段保存一次跑完(复用同一状态机),
        // 再处理积压消息,避免把半成品 tmp 留在盘上等下次启动清理。
        if self.save_task.is_some() {
            let hook = self.query_yield.take();
            while self.save_task.is_some() {
                self.advance_save();
            }
            self.query_yield = hook;
            self.flush_deferred();
        }
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
        // 旁车写盘已异步化:退出前有界等待在飞那一笔完成(worker 进程随后
        // 走 process::exit,不跑 Drop,不能指望析构兜底)。
        self.wait_sidecar_idle(Duration::from_secs(5));
    }

    pub fn cfg(&self) -> &EngineConfig {
        &self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::KindFilter;

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
            noise: NoiseRules::builtin(),
            candidate_cap: 1000,
            volumes,
        })
    }

    fn rec(frn: u64, parent: u64, attrs: u32, name: &str) -> Rec {
        Rec { frn, parent_frn: parent, attrs, op: Op::Upsert, name: name.to_string(), mtime: 0, size: u64::MAX }
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

    /// 空卷的握手序列:直接进入 ready(scan_mode=false),随后到达的 Records
    /// 走 USN 增量插入路径(mtime=0),正好覆盖待回填场景。
    fn feed_ready_empty(engine: &mut Engine, guid: u128, letter: char) {
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter,
            journal_id: 7,
            next_usn: 100,
            est_records: 0,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 0 });
    }

    /// 等后台 compact 完成并换血(测试专用:生产由宿主循环自然驱动)。
    /// 先跑一轮 maintain 让待归并卷有机会启动后台任务;之后 2ms 节拍轮询,
    /// 避免空转烧 CPU。
    fn settle_compact(engine: &mut Engine) {
        engine.maintain();
        let mut spins = 0;
        while engine.compact_in_progress() {
            engine.maintain();
            std::thread::sleep(Duration::from_millis(2));
            spins += 1;
            assert!(spins < 20_000, "后台 compact 必须在有限时间内完成");
        }
    }

    /// 任务 1 的引擎级对拍:一次 `query_split` ≡ 旧 folders/files 两次 query。
    /// 覆盖两卷、CJK 拼音候选、路径前缀、ext 过滤、单字符位图与 n-gram 路径。
    #[test]
    fn split_query_matches_two_pass_engine() {
        fn feed_volume(engine: &mut Engine, guid: u128, letter: char, recs: Vec<Rec>) {
            let count = recs.len() as u32;
            engine.handle(EngineMsg::VolumeMeta {
                guid,
                letter,
                journal_id: 7,
                next_usn: 100,
                est_records: count as u64,
            });
            engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
            engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
            engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count });
        }
        let dir = temp_index_dir("split-parity");
        let mut engine = engine_with(&dir);
        feed_volume(
            &mut engine,
            0xA11u128,
            'C',
            vec![
                rec(5, 0, 0x10, "docs"),
                rec(10, 5, 0x10, "ai_dir"),
                rec(11, 5, 0, "ai_report.txt"),
                rec(12, 5, 0, "ai_tool.exe"),
                rec(13, 5, 0, "报告_ai.docx"),
                rec(14, 5, 0x10, "报告目录"),
            ],
        );
        feed_volume(
            &mut engine,
            0xB22u128,
            'E',
            vec![
                rec(20, 0, 0x10, "workspace"),
                rec(21, 20, 0x10, "ai_notes"),
                rec(22, 20, 0, "notes_ai.txt"),
                rec(23, 20, 0, "文件_ai.log"),
                rec(24, 20, 0, "alphafile.rs"),
            ],
        );
        assert_eq!(engine.phase, Phase::Ready);

        let check = |spec_all: &QuerySpec| {
            let needle = spec_all.tokens.join(" ").to_lowercase();
            let split = engine.query_split_with_exact(spec_all, Some(&needle));
            let mut spec_folders = spec_all.clone();
            spec_folders.kind = KindFilter::Folders;
            let all = engine.query(spec_all);
            let folders = engine.query(&spec_folders);
            let mut spec_exact = spec_all.clone();
            spec_exact.exact = Some(needle.clone());
            let exact = engine.query(&spec_exact);
            assert!(!split.aborted);
            assert_eq!(split.total_all, all.total, "total_all: {:?}", spec_all.tokens);
            assert_eq!(
                split.total_folders, folders.total,
                "total_folders: {:?}",
                spec_all.tokens
            );
            assert_eq!(
                split.folder_hits, folders.hits,
                "文件夹前缀: {:?}",
                spec_all.tokens
            );
            let expected_files: Vec<Hit> = all
                .hits
                .iter()
                .filter(|h| !engine.is_dir(h))
                .copied()
                .collect();
            assert_eq!(
                split.file_hits, expected_files,
                "文件前缀: {:?}",
                spec_all.tokens
            );
            assert_eq!(
                split.exact_hits, exact.hits,
                "exact 候选: {:?}",
                spec_all.tokens
            );
        };

        for spec in [
            QuerySpec { tokens: vec!["ai".into()], kind: KindFilter::All, ..Default::default() },
            QuerySpec { tokens: vec!["a".into()], kind: KindFilter::All, ..Default::default() },
            QuerySpec { tokens: vec!["bg".into()], kind: KindFilter::All, ..Default::default() },
            QuerySpec {
                tokens: vec!["ai".into()],
                kind: KindFilter::All,
                path_prefix: Some("c:\\docs".into()),
                ..Default::default()
            },
            QuerySpec {
                tokens: vec!["ai".into()],
                kind: KindFilter::All,
                ext: Some("txt".into()),
                ..Default::default()
            },
            QuerySpec { tokens: Vec::new(), kind: KindFilter::All, ext: Some("rs".into()), ..Default::default() },
        ] {
            check(&spec);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 问题 1 验收:USN Records 插入的条目 mtime=0,时间筛选一律不通过;
    /// maintain 用可注入探针回填后(多卷各自),同一条目应立刻可被时间筛选命中。
    #[test]
    fn usn_mtime_backfill_enables_time_filter_on_all_volumes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const DIR: u32 = 0x0010;
        const MTIME: i64 = 1_750_000_000;
        let dir = temp_index_dir("mtime-backfill");
        let (gc, ge) = (0x501u128, 0x502u128);
        let mut engine = engine_with(&dir);
        feed_ready_empty(&mut engine, gc, 'C');
        feed_ready_empty(&mut engine, ge, 'E');
        engine.handle(EngineMsg::Records {
            guid: gc,
            recs: vec![
                rec(10, 0, DIR, "docs"),
                rec(11, 10, 0, "alpha.txt"),
                rec(12, 10, 0, "beta.txt"),
            ],
            next_usn: 200,
        });
        engine.handle(EngineMsg::Records {
            guid: ge,
            recs: vec![rec(20, 0, DIR, "notes"), rec(21, 20, 0, "gamma.txt")],
            next_usn: 200,
        });

        // 回填前:mtime 全为 0 → 时间筛选命中不到(问题现场)。
        let alpha = QuerySpec {
            tokens: vec!["alpha".into()],
            mtime_after: Some(MTIME - 1),
            ..Default::default()
        };
        assert_eq!(engine.query(&alpha).total, 0, "mtime=0 不得通过时间筛选");
        let gamma = QuerySpec {
            tokens: vec!["gamma".into()],
            mtime_after: Some(MTIME - 1),
            ..Default::default()
        };
        assert_eq!(engine.query(&gamma).total, 0);

        // 回填前:size 全未知 → 大小筛选命中不到。
        let alpha_size = QuerySpec {
            tokens: vec!["alpha".into()],
            size_after: Some(1000),
            ..Default::default()
        };
        assert_eq!(engine.query(&alpha_size).total, 0, "size=MAX 不得通过大小筛选");

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        engine.set_meta_probe(Some(Box::new(move |_path| {
            probe_calls.fetch_add(1, Ordering::Relaxed);
            Some((4096, MTIME))
        })));
        assert!(engine.maintain(), "回填写入后 maintain 应报告做了事");
        assert_eq!(calls.load(Ordering::Relaxed), 5, "两条卷的 5 个条目都应探测");

        // 两个卷的 slot 都回填到索引 mtime / size。
        for (vol, frn) in [(0usize, 11u64), (1usize, 21u64)] {
            let store = engine.volumes[vol].store.as_ref().unwrap();
            let slot = store.find_slot(frn).expect("entry");
            assert_eq!(
                store.mtime[slot as usize] as i64, MTIME,
                "vol {vol} frn {frn} 的 mtime 应已回填"
            );
            assert_eq!(store.size[slot as usize], 4096, "size 应同时回填");
            assert!(store.mtime_pending.is_empty(), "回填后 pending 应清空");
            assert!(store.size_pending.is_empty(), "回填后 size pending 应清空");
        }
        assert_eq!(engine.query(&alpha).total, 1, "回填后时间筛选必须命中 C 卷");
        assert_eq!(engine.query(&gamma).total, 1, "回填后时间筛选必须命中 E 卷");
        assert_eq!(
            engine.query(&alpha_size).total,
            1,
            "回填后大小筛选必须命中 C 卷"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 问题 1:probe 返回 None(文件已删/无权限)→ 丢弃该 slot,后续 maintain
    /// 不得再次尝试(探针调用计数可断言)。
    #[test]
    fn missing_mtime_probe_is_dropped_without_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let dir = temp_index_dir("mtime-missing");
        let guid = 0x511u128;
        let mut engine = engine_with(&dir);
        feed_ready_empty(&mut engine, guid, 'C');
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(30, 0, 0, "gone.txt"), rec(31, 0, 0, "kept.txt")],
            next_usn: 200,
        });

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let (probe_calls, probe_seen) = (calls.clone(), seen.clone());
        engine.set_meta_probe(Some(Box::new(move |path| {
            probe_calls.fetch_add(1, Ordering::Relaxed);
            probe_seen.lock().unwrap().push(path.to_string());
            // 只让 gone.txt 探测失败;kept.txt 正常回填。
            if path.ends_with("gone.txt") {
                None
            } else {
                Some((8192, 1_750_000_000))
            }
        })));

        assert!(engine.maintain());
        assert_eq!(calls.load(Ordering::Relaxed), 2, "两条各探测一次");
        {
            let store = engine.volumes[0].store.as_ref().unwrap();
            let gone = store.find_slot(30).unwrap();
            let kept = store.find_slot(31).unwrap();
            assert_eq!(store.mtime[gone as usize], 0, "探测失败保持未知");
            assert_eq!(store.mtime[kept as usize], 1_750_000_000);
            assert!(store.mtime_pending.is_empty(), "失败条目也必须出队");
        }
        // 后续 maintain 不得重试 gone.txt(None 视为终态)。
        engine.maintain();
        engine.maintain();
        assert_eq!(calls.load(Ordering::Relaxed), 2, "失败条目只探测一次");
        let gone_hits = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.ends_with("gone.txt"))
            .count();
        assert_eq!(gone_hits, 1, "gone.txt 只应被探测一次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 问题 1:单轮每卷最多回填 [`MTIME_BACKFILL_PER_ROUND`] 条,避免一次
    /// maintain 对大量 USN 条目做 FS 探测把查询线程堵住。
    #[test]
    fn mtime_backfill_is_rate_limited_per_round() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let dir = temp_index_dir("mtime-limit");
        let guid = 0x521u128;
        let mut engine = engine_with(&dir);
        feed_ready_empty(&mut engine, guid, 'C');
        let recs: Vec<Rec> = (0..300u64)
            .map(|i| rec(1000 + i, crate::store::ROOT_FRN, 0, &format!("f{i:03}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 200 });

        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        engine.set_meta_probe(Some(Box::new(move |_| {
            probe_calls.fetch_add(1, Ordering::Relaxed);
            Some((2048, 1_750_000_000))
        })));

        assert_eq!(MTIME_BACKFILL_PER_ROUND, 128);
        engine.maintain();
        assert_eq!(calls.load(Ordering::Relaxed), 128, "首轮只处理 128 条");
        assert_eq!(
            engine.volumes[0].store.as_ref().unwrap().mtime_pending.len(),
            172,
            "未处理条目保留到下一轮"
        );
        engine.maintain();
        assert_eq!(calls.load(Ordering::Relaxed), 256);
        assert_eq!(
            engine.volumes[0].store.as_ref().unwrap().mtime_pending.len(),
            44
        );
        engine.maintain();
        assert_eq!(calls.load(Ordering::Relaxed), 300);
        assert!(engine.volumes[0].store.as_ref().unwrap().mtime_pending.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MAJOR 回归(端到端):空卷(新 U 盘/全删卷)首扫完成 → VolumeReady
    /// compact 后必须进入 ready 形态;journal 新建文件的 ≥2 字符查询必须命中。
    /// 旧实现空卷 compact 后 is_scanning 恒真:insert 不进 overlay、
    /// maintain/save 对 scanning 卷跳过,查询又被空倒排静默归零。
    #[test]
    fn empty_volume_ready_then_new_file_is_searchable() {
        let dir = temp_index_dir("empty-ready");
        let guid = 0xe0e0u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'E',
            journal_id: 7,
            next_usn: 100,
            est_records: 0,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 0 });
        {
            let store = engine.volumes[0].store.as_ref().unwrap();
            assert!(!store.is_scanning(), "空卷首扫完成(compact)后必须退出 scan 模式");
            assert!(store.is_empty());
        }
        // 卷已就绪:新建文件走 journal 增量(而不是重新扫描)
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(10, 0, "alpha.txt")],
            next_usn: 101,
        });
        let spec = QuerySpec { tokens: vec!["alpha".into()], ..Default::default() };
        let outcome = engine.query(&spec);
        assert_eq!(outcome.total, 1, "空卷 compact 后新建的文件必须可查");
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(engine.entry_name(&outcome.hits[0]), "alpha.txt");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// 旁车追加式写盘:每个节流窗口只把新增记录追加到文件末尾,历史块字节
    /// 原样保留(旧实现每次重写「自快照以来的全部日志」,C 卷日志长到 1.6MB
    /// 时等于每 5s 写 1.6MB)。
    #[test]
    fn sidecar_appends_instead_of_rewriting() {
        let dir = temp_index_dir("sidecar-append");
        let guid = 0x3333u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain(), "首次全量保存");

        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(20, 10, "append0.txt")],
            next_usn: 200,
        });
        assert!(engine.maintain(), "首块追加");
        let first_bytes = std::fs::read(sidecar::path(&dir, guid)).unwrap();

        for k in 1..3u64 {
            engine.handle(EngineMsg::Records {
                guid,
                recs: vec![rec_file(20 + k, 10, &format!("append{k}.txt"))],
                next_usn: 200 + k as i64,
            });
            // 越过 5s 节流窗口(测试里直接清掉到期时间)
            engine.volumes[0].sidecar_retry_at = None;
            assert!(engine.maintain(), "后续块追加");
        }
        let later = std::fs::read(sidecar::path(&dir, guid)).unwrap();
        assert!(
            later.starts_with(&first_bytes),
            "历史块字节必须原样保留,只允许在末尾增长"
        );
        assert!(later.len() > first_bytes.len(), "追加后文件应变长");
        let sc = sidecar::load(&dir, guid).unwrap().unwrap();
        assert_eq!(sc.next_usn, 202);
        assert_eq!(sc.ops.len(), 3);
        assert_eq!(engine.volumes[0].sidecar_flushed, 3, "已落盘前缀应与日志等长");
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

    /// P0-4:watch 刷新整目录时,噪声子树在入口短路——硬噪声整帧丢弃(不 apply、
    /// 不进旁车),软噪声(构建产物)只保留目录与可执行文件。这是 62MB 全量重写
    /// (53GB/天)与搜索卡顿的主因之一:构建期间 target/ 的每个变更都会把整目录
    /// 子项塞进旁车,几秒就能击穿 32768 条上限。
    #[test]
    fn dir_children_under_noise_is_skipped_at_entry() {
        let dir = temp_index_dir("noise-short-circuit");
        let guid = 0xb1u128;
        let mut engine = engine_with(&dir);
        const DIR: u32 = 0x10;
        let recs = vec![
            rec(10, 0, DIR, "proj"),
            rec(20, 10, DIR, "target"),
            rec(21, 20, DIR, "release"),
            rec(22, 10, DIR, "node_modules"),
            rec(23, 10, DIR, "src"),
            rec(24, 23, 0, "main.rs"),
        ];
        let count = recs.len() as u32;
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'E',
            journal_id: 7,
            next_usn: 100,
            est_records: count as u64,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count });
        assert!(engine.maintain());
        // watch 刷新只会在首次 compact 之后才有 FRN→slot 索引(生产上稳态即如此)。
        engine.volumes[0].store.as_mut().unwrap().compact();
        assert!(engine.volumes[0].store.as_ref().unwrap().find_slot(20).is_some());
        assert!(engine.volumes[0].store.as_ref().unwrap().find_slot(22).is_some());

        // 软噪声:target 下只保留目录与可执行文件,中间产物不进旁车。
        let before = engine.volumes[0].sidecar_ops.len();
        engine.handle(EngineMsg::DirChildren {
            guid,
            parent_frn: 20,
            recs: vec![
                rec(30, 20, 0, "app.exe"),
                rec(31, 20, 0, "app.rlib"),
                rec(32, 20, DIR, "deps"),
            ],
        });
        assert_eq!(
            engine.volumes[0].sidecar_ops.len(),
            before + 1,
            "软噪声只捕获过滤后的整帧(一次 Children 操作)"
        );
        let store = engine.volumes[0].store.as_ref().unwrap();
        assert!(store.find_slot(30).is_some(), "构建出来的 exe 应保留可搜");
        assert!(store.find_slot(31).is_none(), "中间产物 rlib 不应落库");
        assert!(store.find_slot(32).is_some(), "目录必须保留(路径靠父链解析)");

        // 硬噪声:node_modules 整帧丢弃,不进 store、不进旁车。
        let before = engine.volumes[0].sidecar_ops.len();
        engine.handle(EngineMsg::DirChildren {
            guid,
            parent_frn: 22,
            recs: vec![rec(40, 22, 0, "dep.txt"), rec(41, 22, DIR, "pkg")],
        });
        assert_eq!(engine.volumes[0].sidecar_ops.len(), before, "硬噪声整帧不进旁车");
        let store = engine.volumes[0].store.as_ref().unwrap();
        assert!(store.find_slot(40).is_none() && store.find_slot(41).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-4:watch 新建/改名的中文条目必须在下一轮 maintain 就进拼音缓存，
    /// 不能等到最长 30min 的全量保存窗口。
    #[test]
    fn p1_4_chinese_watch_entry_hits_pinyin_within_maintain() {
        let dir = temp_index_dir("p14-pinyin-fresh");
        let guid = 0xe1u128;
        let mut engine = engine_with(&dir);
        const DIR: u32 = 0x10;
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 2,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(10, 0, DIR, "docs"), rec(11, 10, 0, "alpha.txt")],
            next_usn: 100,
        });
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count: 2,
        });
        engine.maintain();
        engine.volumes[0].store.as_mut().unwrap().compact();
        assert!(engine.volumes[0].store.as_ref().unwrap().find_slot(11).is_some());

        // watch:新建中文文件 + 把已有文件改名为中文名。
        engine.handle(EngineMsg::DirChildren {
            guid,
            parent_frn: 10,
            recs: vec![rec(12, 10, 0, "报告.docx"), rec(11, 10, 0, "文件.txt")],
        });
        engine.maintain();

        let store = engine.volumes[0].store.as_ref().unwrap();
        let hits: Vec<u32> = engine.pinyin[0]
            .search(store, &[b"bg".to_vec()], 100)
            .collect();
        assert!(
            hits.contains(&store.find_slot(12).unwrap()),
            "新建中文名应立即拼音可搜"
        );
        let hits: Vec<u32> = engine.pinyin[0]
            .search(store, &[b"wenjian".to_vec()], 100)
            .collect();
        assert_eq!(
            hits,
            vec![store.find_slot(11).unwrap()],
            "改名后的中文名应立即生效"
        );
        assert_eq!(
            store.name_lc(store.find_slot(11).unwrap()),
            "文件.txt".as_bytes(),
            "store 内改名已生效"
        );
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

    /// P2-14:全量保存换代后在飞的旁车 ack 不得把 flushed 抬回旧值。
    ///
    /// watch/dir 模式 base 恒 (0,0),旧实现只靠 base 判断陈旧;若 append 在飞
    /// 时全量保存完成,ack 会把 flushed 抬到旧 total(>新 ops.len()),此后
    /// `from = flushed.min(len)` 切空 → 既不写盘也不触发全量保存。
    #[test]
    fn stale_sidecar_ack_is_discarded_by_epoch() {
        let dir = temp_index_dir("sidecar-epoch");
        let guid = 0xa6u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());

        {
            let v = &mut engine.volumes[0];
            v.sidecar_ops = vec![SidecarOp::Journal {
                recs: vec![rec_file(100, 10, "a.txt")],
            }];
            v.sidecar_flushed = 0;
            v.sidecar_base = (0, 0);
            v.sidecar_inflight = Some(7);
            v.sidecar_epoch = 3;
        }
        engine.apply_sidecar_ack(SidecarDone {
            seq: 7,
            guid,
            base: (0, 0),
            total: 1,
            result: Ok(()),
            epoch: 2, // 全量保存已把世代推进到 3
        });
        assert_eq!(
            engine.volumes[0].sidecar_flushed, 0,
            "旧世代 ack 不得把 flushed 抬回"
        );
        assert_eq!(engine.volumes[0].sidecar_inflight, None, "ack 仍应清在飞标记");

        // 防御:同世代但 total 超长时夹到当前日志长度。
        engine.volumes[0].sidecar_inflight = Some(8);
        engine.apply_sidecar_ack(SidecarDone {
            seq: 8,
            guid,
            base: (0, 0),
            total: 99,
            result: Ok(()),
            epoch: 3,
        });
        assert_eq!(engine.volumes[0].sidecar_flushed, 1);
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
        let scheduled = engine.volumes[0]
            .save_retry_at
            .expect("溢出应安排全量保存");
        assert!(
            scheduled <= Instant::now() + SIDECAR_OVERFLOW_SAVE_DELAY,
            "溢出保存应落在延迟窗口内"
        );
        assert!(engine.volumes[0].sidecar_overflowed, "溢出闩应置位");
        // 再溢出不得反复改写截止时间(旧实现每个批次都清成 None → E 卷连发)。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec_file(90_000, 10, "after-overflow.txt")],
            next_usn: 300,
        });
        assert_eq!(
            engine.volumes[0].save_retry_at,
            Some(scheduled),
            "溢出闩只触发一次,后续批次不得再改截止时间"
        );

        // 模拟溢出延迟已到:全量保存执行(大卷走分段保存,按宿主节奏多轮推进)
        engine.volumes[0].save_retry_at = None;
        // 3.2 万条增量会触发整卷 compact;大卷已交给后台线程,先等换血。
        settle_compact(&mut engine);
        let mut rounds = 0;
        while engine.volumes[0].dirty && rounds < 500 {
            assert!(engine.maintain(), "溢出后全量保存应执行");
            rounds += 1;
        }
        assert!(!engine.volumes[0].dirty, "全量保存应在有限轮内完成");
        assert!(!engine.volumes[0].sidecar_overflowed, "保存成功清溢出闩");
        assert!(sidecar::load(&dir, guid).unwrap().is_none(), "全量保存后旁车删除");
        assert_ne!(std::fs::read(&snap_path).unwrap(), before, "快照应已重写");
        assert!(engine.volumes[0].sidecar_ops.is_empty());

        // 再次高变更:新溢出只能按延迟窗口排一次保存,不得立刻连发整卷重写。
        let now = Instant::now();
        for k in 0..(SIDECAR_MAX_RECS / 100 + 2) {
            let base = 200_000u64 + k as u64 * 100;
            let recs: Vec<Rec> = (0..100u64)
                .map(|j| rec_file(base + j, 10, &format!("again{k:03}_{j:03}.txt")))
                .collect();
            engine.handle(EngineMsg::Records { guid, recs, next_usn: 400 + k as i64 });
        }
        let again = engine.volumes[0]
            .save_retry_at
            .expect("溢出应安排全量保存");
        assert!(
            again > now + SIDECAR_OVERFLOW_SAVE_DELAY - Duration::from_secs(5),
            "溢出触发的全量保存必须受延迟窗口约束(避免 E 卷式连发)"
        );
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

    /// 分段保存不 compact store(slot 身份不变),因此拼音缓存不会因保存而
    /// 越界;但快照视图必须仍把 DEAD 条目排除在外(与 compact 后的字节语义
    /// 一致)。旧实现 save 内部 compact,这里改为「保存视图 + 延迟 compact」。
    #[test]
    fn save_view_keeps_slots_and_drops_dead_entries() {
        let dir = temp_index_dir("pinyin-save-view");
        let guid = 0x4444u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        // VolumeReady 已 compact 过,这次落盘不会 compact。
        assert!(engine.maintain());

        // 删掉一条 + 就地改名:保存视图必须排除 DEAD、按新名重建名字区。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![Rec {
                frn: 12,
                parent_frn: 10,
                attrs: 0,
                op: Op::Remove,
                name: String::new(),
                mtime: 0,
                size: u64::MAX,
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
        assert!(engine.maintain(), "分段保存应写入快照");

        let store = engine.volumes[0].store.as_ref().unwrap();
        assert_eq!(store.len(), 3, "保存视图不改 store:DEAD slot 留在内存,等 compact");
        assert!(
            engine.pinyin[0]
                .slots
                .iter()
                .all(|&slot| (slot as usize) < store.len()),
            "保存不 remap slot,拼音缓存天然不越界"
        );
        // 快照本身必须是 compact 后语义:DEAD 条目被排除,改名生效。
        let nr = NoiseRules::builtin().rules();
        let loaded = snapshot::load(&dir, guid, nr.hash(), &nr).expect("snapshot loads");
        assert_eq!(loaded.store.len(), 2, "快照视图必须排除 DEAD 条目");
        assert!(loaded.store.find_slot(12).is_none());
        assert_eq!(
            loaded
                .store
                .find_slot(11)
                .map(|s| loaded.store.name(s).to_vec()),
            Some(b"alpaca.txt".to_vec())
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
        assert_eq!(engine.path_of(&hit).as_deref(), Some("C:\\docs\\alpha.txt"));

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
        let stale = Hit {
            vol: 0,
            slot: 0,
            generation: hit.generation.wrapping_add(1),
            mtime: 0,
        };
        assert!(engine.path_of(&stale).is_none());
        assert!(engine.pinyin_of(&stale).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 对账收尾的整卷 compact 不能同步跑:实测 C 卷 3.4-3.6s,同线程排队中的
    /// 查询要整段等待(用户打字实测 queued=3184ms、状态行「总 3203ms / 引擎
    /// 10ms」)。超过小卷阈值后必须交给真后台线程,查询线程零占用,换血后再落盘。
    #[test]
    fn dirsync_end_backgrounds_whole_volume_compact() {
        let dir = temp_index_dir("dirsync-bg-compact");
        let guid = 0x3436u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');

        // 第一轮对账:3 万条全部「本轮见过」(低于 COMPACT_DELTA,不触发重排)
        engine.handle(EngineMsg::DirSyncBegin { guid });
        let recs: Vec<Rec> = (0..30_000u64)
            .map(|k| rec(10_000 + k, 0, 0, &format!("f{k:05}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 500 });
        engine.handle(EngineMsg::DirSyncEnd { guid, ok: true });
        assert!(
            !engine.compact_in_progress(),
            "未到 compact 阈值不得启动整卷重排"
        );
        let mut rounds = 0;
        while engine.volumes[0].dirty && rounds < 200 {
            engine.maintain();
            rounds += 1;
        }

        // 第二轮对账:只见到 10 条 → 其余 29990 条判死,越过 dead 阈值
        engine.handle(EngineMsg::DirSyncBegin { guid });
        let recs: Vec<Rec> = (0..10u64)
            .map(|k| rec(10_000 + k, 0, 0, &format!("f{k:05}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 600 });
        engine.handle(EngineMsg::DirSyncEnd { guid, ok: true });

        assert!(
            engine.compact_in_progress(),
            "对账收尾应把大卷 compact 交给真后台线程,不得同步跑完"
        );
        // 后台期间查询可用(旧实现这里要等完整卷重排)
        let probe = QuerySpec {
            tokens: vec!["f000".into()],
            ..Default::default()
        };
        assert!(
            engine.query(&probe).total > 0,
            "后台 compact 期间查询必须仍能服务"
        );

        // 后台完成 → 换血;随后自动落盘
        settle_compact(&mut engine);
        let mut rounds = 0;
        while engine.volumes[0].dirty && rounds < 500 {
            engine.maintain();
            rounds += 1;
        }
        assert!(!engine.volumes[0].dirty, "换血后应完成落盘");

        let engine2 = engine_with(&dir);
        let store2 = engine2.volumes[0].store.as_ref().unwrap();
        assert_eq!(store2.len(), 10, "只剩第二轮见到的 10 条");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首扫收尾现在走后台 compact:size 列必须随 slot 重排,否则 `size:`
    /// 过滤会命中错误文件(CompactTask 曾漏掉 size 列;UI 有逐条 find_meta
    /// 兜底,肉眼很难发现,错位列还会被快照持久化)。
    #[test]
    fn first_scan_background_compact_keeps_size_column() {
        const N: u64 = 30_000;
        let dir = temp_index_dir("first-scan-size-col");
        let guid = 0x5151u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: N,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        // 乱序下发(7919 与 30000 互素 → 是 0..N 的置换):compact 后的 slot
        // 顺序按 FRN 排,必然与插入序不同;size 取 FRN 的倍数便于逐槽校验。
        let recs: Vec<Rec> = (0..N)
            .map(|i| {
                let frn = 1_000 + (i * 7_919) % N;
                Rec {
                    frn,
                    parent_frn: 0,
                    attrs: 0,
                    op: Op::Upsert,
                    name: format!("f{frn}.txt"),
                    mtime: 0,
                    size: frn * 10,
                }
            })
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 110,
            count: N as u32,
        });
        assert!(
            engine.compact_in_progress(),
            "3 万条的首扫收尾应交给真后台线程"
        );
        settle_compact(&mut engine);

        let store = engine.volumes[0].store.as_ref().unwrap();
        assert_eq!(store.size.len(), store.len(), "size 列必须与 live 列等长");
        for slot in 0..store.len() as u32 {
            assert_eq!(
                store.size[slot as usize],
                store.frn[slot as usize] * 10,
                "size 列与 slot 错位(slot {slot})"
            );
        }
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

    /// JournalReset 清卷:store/游标清零之外,状态显示字段(resume_frn 进度、
    /// cursor_at 游标 age)一并重置 —— 已清空的卷不应再显示陈旧进度/游标。
    #[test]
    fn journal_reset_clears_status_fields() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("journal-reset");
        let (g_ready, g_scan) = (0x82u128, 0x83u128);
        let mut engine = engine_with(&dir);
        // 就绪卷:有游标 age
        feed_ready(&mut engine, g_ready, 'C');
        assert!(engine.maintain());
        assert!(snapshot::snapshot_path(&dir, g_ready).is_file());
        // 扫描中卷:有断点进度(50/100)
        engine.handle(EngineMsg::VolumeMeta {
            guid: g_scan,
            letter: 'D',
            journal_id: 7,
            next_usn: 100,
            est_records: 100,
        });
        engine.handle(EngineMsg::ScanBegin { guid: g_scan, resume_frn: 0 });
        engine.handle(EngineMsg::Records {
            guid: g_scan,
            recs: vec![rec(10, 0, DIR, "docs")],
            next_usn: 100,
        });
        engine.handle(EngineMsg::ScanProgress { guid: g_scan, frn: 50, est: 100, count: 1 });
        let st = engine.status();
        let (ready, scan) = (&st.volumes[0], &st.volumes[1]);
        assert!(ready.cursor_age_secs.is_some());
        assert_eq!(scan.pct, Some(0.5));

        engine.handle(EngineMsg::JournalReset { guid: g_ready });
        engine.handle(EngineMsg::JournalReset { guid: g_scan });
        for guid in [g_ready, g_scan] {
            let i = engine.volumes.iter().position(|v| v.guid == guid).unwrap();
            let v = &engine.volumes[i];
            assert!(!v.complete && v.store.is_none());
            assert_eq!((v.journal_id, v.next_usn, v.resume_frn), (0, 0, 0));
        }
        let st = engine.status();
        let (ready, scan) = (&st.volumes[0], &st.volumes[1]);
        assert_eq!(ready.count, 0);
        assert!(ready.cursor_age_secs.is_none(), "游标 age 应随重置清空");
        assert_eq!(scan.pct, Some(0.0), "断点进度应随重置归零,不显示陈旧 50%");
        assert!(!snapshot::snapshot_path(&dir, g_ready).is_file(), "快照文件应删除");
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

    /// 盘符掩码：vol_mask 只查选中卷，排除卷不参与；全排除=零结果；
    /// 与 path_prefix 的盘符过滤叠加时取交集。
    #[test]
    fn vol_mask_restricts_query_to_selected_volumes() {
        let dir = temp_index_dir("vol-mask");
        let (gc, gf) = (0x77u128, 0x78u128);
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, gc, 'C');
        feed_ready(&mut engine, gf, 'F');

        let mut spec = QuerySpec { tokens: vec!["alpha".into()], ..Default::default() };
        // 默认：两卷各 1 条（两卷喂了同名夹具）
        assert_eq!(engine.query(&spec).total, 2);
        // 只要 C：1 条且命中都在 C 卷
        spec.vol_mask = Some(crate::query::volume_bit('C'));
        let only_c = engine.query(&spec);
        assert_eq!(only_c.total, 1);
        let c_idx = engine.volumes.iter().position(|v| v.letter == 'C').unwrap() as u32;
        assert!(only_c.hits.iter().all(|h| h.vol == c_idx));
        // 排除 C（=只要 F）：1 条且命中都在 F 卷
        spec.vol_mask = Some(((1u32 << 26) - 1) & !crate::query::volume_bit('C'));
        let only_f = engine.query(&spec);
        assert_eq!(only_f.total, 1);
        let f_idx = engine.volumes.iter().position(|v| v.letter == 'F').unwrap() as u32;
        assert!(only_f.hits.iter().all(|h| h.vol == f_idx));
        // 全排除：0
        spec.vol_mask = Some(0);
        assert_eq!(engine.query(&spec).total, 0);
        // 与 path_prefix 叠加：path 指 F、mask 只要 C → 0
        spec.vol_mask = Some(crate::query::volume_bit('C'));
        spec.path_prefix = Some("f:\\".into());
        assert_eq!(engine.query(&spec).total, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 问题 3 验收:path: 前缀不得把目录自身算进结果,同前缀兄弟目录
    /// (`...-文档2`)也不能被 `path:...-文档` 命中;小结果集 total 必须
    /// 按过滤后实际条数精确给出(query 与 split 双路)。
    #[test]
    fn path_prefix_excludes_self_and_sibling_and_fixes_total() {
        const DIR: u32 = 0x0010;
        let dir = temp_index_dir("path-prefix-self");
        let guid = 0x601u128;
        let mut engine = engine_with(&dir);
        feed_ready_empty(&mut engine, guid, 'C');
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![
                rec(10, 0, DIR, "LstrAcc-文档"),
                rec(11, 10, 0, "a.txt"),
                rec(12, 10, 0, "b.txt"),
                rec(13, 10, 0, "c.txt"),
                rec(14, 10, DIR, "sub"),
                rec(20, 0, DIR, "LstrAcc-文档2"),
                rec(21, 20, 0, "sibling.txt"),
            ],
            next_usn: 200,
        });

        // 前缀小写(生产 build_spec 已统一小写),路径比较时由引擎 lower。
        let prefix = r"c:\lstracc-文档";
        let spec = QuerySpec { path_prefix: Some(prefix.into()), ..Default::default() };
        let out = engine.query(&spec);
        let names: Vec<String> = out.hits.iter().map(|h| engine.entry_name(h)).collect();
        assert_eq!(out.total, 4, "目录自身/兄弟目录不计入,total 必须精确");
        assert_eq!(out.hits.len(), 4);
        for expected in ["a.txt", "b.txt", "c.txt", "sub"] {
            assert!(names.iter().any(|n| n == expected), "缺少 {expected}: {names:?}");
        }
        for unexpected in ["LstrAcc-文档", "LstrAcc-文档2", "sibling.txt"] {
            assert!(!names.iter().any(|n| n == unexpected), "不应命中 {unexpected}: {names:?}");
        }

        // split 双路:total_all / total_folders 同规则精确化。
        let split = engine.query_split(&spec);
        assert_eq!(split.total_all, 4, "split total_all 必须精确");
        assert_eq!(split.total_folders, 1, "只应统计前缀下的 sub 目录");
        assert_eq!(split.file_hits.len(), 3);
        assert_eq!(split.folder_hits.len(), 1);

        // 前缀自带末尾分隔符时同样匹配其全部后代。
        let with_sep = QuerySpec {
            path_prefix: Some(format!(r"{prefix}\")),
            ..Default::default()
        };
        assert_eq!(engine.query(&with_sep).total, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 就绪卷收到带高水位游标的 VolumeMeta(Full 计划的扫描起点高水位):    /// 游标对 (journal_id, next_usn) 不得被覆盖 —— 否则到随后的 ScanBegin
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

        // 阻断旁车写盘:把旁车最终路径占成目录,OpenOptions 打不开必失败
        // (追加式写盘不经过 tmp,不能再拿 tmp 路径做故障注入)。
        let blocked = sidecar::path(&dir, guid);
        std::fs::create_dir_all(&blocked).unwrap();
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
            Rec { frn: 11, parent_frn: 10, attrs: 0, op: Op::Upsert, name: "alpha.txt".into(), mtime: 1_700_000_000, size: u64::MAX },
            rec(13, 10, 0, "gamma.txt"),
            Rec { frn: 12, parent_frn: 10, attrs: 0, op: Op::Remove, name: String::new(), mtime: 0, size: u64::MAX },
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
        Rec { frn, parent_frn: parent, attrs, op: Op::Upsert, name, mtime, size: u64::MAX }
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
                noise: NoiseRules::builtin(),
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
            noise: NoiseRules::builtin(),
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
            // 预置一轮:VolumeReady 的 dirty 已由首次 maintain 消化(计时窗外;
            // 大卷走分段保存,需驱动到完成)
            engine.maintain();
            while engine.save_in_progress() {
                engine.maintain();
            }
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
                    // 旧行为:引擎线程里一次 compact+写完整卷(阻塞查询)。
                    // 直接调用 snapshot::save_report,不再经过新的分段状态机。
                    let header = SnapshotHeader {
                        guid,
                        letter: 'C',
                        complete: true,
                        noise_hash: 0,
                        journal_id: 7,
                        next_usn: usn,
                        resume_frn: 0,
                        est_records: 0,
                    };
                    snapshot::save_report(
                        &dir,
                        &header,
                        engine.volumes[0].store.as_mut().unwrap(),
                    )
                    .unwrap();
                    engine.volumes[0].save_retry_at = None;
                } else {
                    engine.maintain();
                }
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
        // - sidecar 模式 p90 上限 60ms(查询评估本身 ~10ms,余量数倍);
        // - sidecar 模式最大延迟 < full-save 模式 p50(整个分布移出保存窗口)。
        assert!(
            p90_b < p90_a / 2.0,
            "sidecar 模式 p90 {p90_b:.1}ms 应显著低于 full-save 模式 p90 {p90_a:.1}ms"
        );
        assert!(p90_b < 60.0, "sidecar 模式 p90 {p90_b:.1}ms 应落在打字无感区间");
        assert!(
            max_b < max_a / 2.0,
            "sidecar 模式 max {max_b:.1}ms 应显著低于 full-save 模式 max {max_a:.1}ms"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 基准:分段保存 vs 一次性整存(保存期间查询延迟,250 万条) ----------
    //
    // 旧行为 = 引擎线程里直接 snapshot::save_report(内部 compact + 写 50MB),
    // 保存期间查询只能排队;新行为 = maintain 每次推进 4ms 的分段保存,查询
    // 在步间被服务。查询用稀有 token("steady",只命中每轮注入的 4 条),评估
    // 成本 ~20ms,让「保存阻塞」成为延迟分布的主体。
    //
    // 断言取结构性判定:
    // 1. 分段保存 p90 < 50ms(任务目标);
    // 2. 分段保存 p90 明显低于旧整存;
    // 3. 保存墙钟(分段路径含步间服务查询)不劣于旧路径 1.2 倍。

    #[test]
    #[cfg_attr(debug_assertions, ignore = "release-profile 基准(250 万条,真实落盘)")]
    fn bench_segmented_save_query_latency() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        const GUID: u128 = 0x5a7e_u128;
        let dir_a = std::env::temp_dir().join(format!("lstr-seg-bench-a-{}", std::process::id()));
        let dir_b = std::env::temp_dir().join(format!("lstr-seg-bench-b-{}", std::process::id()));
        for dir in [&dir_a, &dir_b] {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir).unwrap();
        }

        let make_engine = |dir: &std::path::Path| {
            let mut e = Engine::new(EngineConfig {
                index_dir: dir.to_path_buf(),
                noise: NoiseRules::builtin(),
                candidate_cap: 100,
                volumes: Vec::new(),
            });
            e.set_apply_chunk_records(0);
            for m in bench_build_msgs(GUID) {
                e.handle(m);
            }
            e
        };
        // 把初始 VolumeReady 的 dirty 落盘(不计时),大卷要驱动多次。
        let settle = |e: &mut Engine| {
            e.maintain();
            while e.save_in_progress() {
                e.maintain();
            }
        };

        let (q_tx, q_rx) = std::sync::mpsc::channel::<(Instant, QuerySpec)>();
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let q_thread = std::thread::Builder::new()
            .name("seg-save-bench-query".to_string())
            .spawn(move || {
                let spec = || QuerySpec {
                    tokens: vec!["steady".to_string()],
                    ..Default::default()
                };
                // 30ms 节奏:比真实打字密,给保存窗口足够样本。
                while !done2.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(30));
                    let _ = q_tx.send((Instant::now(), spec()));
                }
            })
            .unwrap();
        let service = |engine: &mut Engine, lat: &Mutex<Vec<f64>>, q_rx: &std::sync::mpsc::Receiver<(Instant, QuerySpec)>| {
            while let Ok((sent, spec)) = q_rx.try_recv() {
                let _ = engine.query(&spec);
                lat.lock()
                    .unwrap()
                    .push(sent.elapsed().as_secs_f64() * 1000.0);
            }
        };
        // 丢弃计时窗口外(建库/settle)积压的样本,只统计保存窗口内的查询。
        let discard = |q_rx: &std::sync::mpsc::Receiver<(Instant, QuerySpec)>| {
            while q_rx.try_recv().is_ok() {}
        };
        let inject = |e: &mut Engine, round: usize| {
            let usn = 1_000 + round as i64;
            let recs: Vec<Rec> = (0..4)
                .map(|j| {
                    bench_rec(
                        5_000_000 + (round * 4 + j) as u64,
                        10,
                        format!("steady_{round:03}_{j}.txt"),
                        0,
                        0,
                    )
                })
                .collect();
            e.handle(EngineMsg::Records { guid: GUID, recs, next_usn: usn });
        };

        // ---- 旧行为:一次整存(compact + 写),保存期间不服务查询 ----
        let mut engine_a = make_engine(&dir_a);
        settle(&mut engine_a);
        let lat_a = Mutex::new(Vec::new());
        let mut busy_a = Duration::ZERO;
        for round in 0..3usize {
            inject(&mut engine_a, round);
            engine_a.volumes[0].save_retry_at = None;
            discard(&q_rx); // 只统计保存窗口内的查询
            let started = Instant::now();
            let header = SnapshotHeader {
                guid: GUID,
                letter: 'C',
                complete: true,
                noise_hash: 0,
                journal_id: 7,
                next_usn: 1_000 + round as i64,
                resume_frn: 0,
                est_records: 0,
            };
            snapshot::save_report(
                &dir_a,
                &header,
                engine_a.volumes[0].store.as_mut().unwrap(),
            )
            .unwrap();
            busy_a += started.elapsed();
            service(&mut engine_a, &lat_a, &q_rx);
        }

        // ---- 新行为:分段保存,步间服务查询 ----
        let mut engine_b = make_engine(&dir_b);
        settle(&mut engine_b);
        let lat_b = Mutex::new(Vec::new());
        let mut busy_b = Duration::ZERO;
        for round in 0..3usize {
            inject(&mut engine_b, round);
            engine_b.volumes[0].save_retry_at = None;
            discard(&q_rx);
            let started = Instant::now();
            loop {
                engine_b.maintain();
                service(&mut engine_b, &lat_b, &q_rx);
                if !engine_b.save_in_progress() && !engine_b.has_deferred_msgs() {
                    break;
                }
            }
            busy_b += started.elapsed();
            service(&mut engine_b, &lat_b, &q_rx);
        }
        done.store(true, Ordering::Relaxed);
        q_thread.join().unwrap();

        let mut lat_a = lat_a.into_inner().unwrap();
        let mut lat_b = lat_b.into_inner().unwrap();
        lat_a.sort_by(|x, y| x.partial_cmp(y).unwrap());
        lat_b.sort_by(|x, y| x.partial_cmp(y).unwrap());
        assert!(
            lat_a.len() >= 15 && lat_b.len() >= 15,
            "样本不足: old={} new={}",
            lat_a.len(),
            lat_b.len()
        );
        let (p50_a, p90_a, max_a) = (
            percentile(&lat_a, 0.5),
            percentile(&lat_a, 0.9),
            *lat_a.last().unwrap(),
        );
        let (p50_b, p90_b, max_b) = (
            percentile(&lat_b, 0.5),
            percentile(&lat_b, 0.9),
            *lat_b.last().unwrap(),
        );
        let save_a = busy_a.as_secs_f64() * 1000.0 / 3.0;
        let save_b = busy_b.as_secs_f64() * 1000.0 / 3.0;
        println!(
            "[bench] save-oneshot (旧,250万条): {} queries p50={p50_a:.1}ms p90={p90_a:.1}ms max={max_a:.1}ms | save={save_a:.0}ms/次",
            lat_a.len()
        );
        println!(
            "[bench] save-segmented(新,250万条): {} queries p50={p50_b:.1}ms p90={p90_b:.1}ms max={max_b:.1}ms | save={save_b:.0}ms/次",
            lat_b.len()
        );
        assert!(p90_b < 50.0, "分段保存期间 p90 应 <50ms: {p90_b:.1}ms");
        assert!(
            p90_b < p90_a / 2.0,
            "分段保存 p90 {p90_b:.1}ms 应显著低于整存 p90 {p90_a:.1}ms"
        );
        assert!(
            save_b <= save_a * 1.2,
            "分段保存墙钟 {save_b:.0}ms 不应比整存 {save_a:.0}ms 慢 20% 以上"
        );
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    // ---------- 基准:宽查询分项剖析("ai" 类,250 万条) ----------
    //
    // 真实机器(C 卷 117 万条)上 "ai" 引擎执行 ~56ms、命中 17630。这里用
    // 250 万条合成记录拆分项,决定 bigram/trigram 优化砍哪几块:
    //   ① memmem 纯扫描(find_iter 只计数,不解析 slot)
    //   ② 命中→slot 解析(slot_at_offset 二分,无过滤)
    //   ③ 完整 query_volume_cancel(= ①+②+过滤+收集,引擎单卷主体)
    //   ④ 拼音匹配(PinyinCache::search,含引擎侧去重量级)
    //   ⑤ 附:3 字符中频 / 窄 token / 空 token 的完整路径(验收对照基线)
    // 名字分布对齐真实卷:"ai" ≈1.5%、"con" ≈4%(3 字符中频)、
    // 窄 token ~50 条、CJK ≈0.2%(拼音候选,部分全拼含 "ai")。
    // 不设绝对时间断言(避免抖动 flaky);数字打印 --nocapture 可见,
    // 提交信息与报告引用。唯一断言:各查询形态命中数自洽(设置正确性)。

    const PROFILE_DIRS: u64 = 2000;
    const PROFILE_FILES: u64 = 2_498_000;

    /// 与真实卷对齐的名字分布。判据顺序固定,保证 "ai"/"con"/窄/CJK 计数确定。
    fn profile_name(f: u64) -> String {
        if f % 500 == 3 {
            // CJK ~0.2%:全拼 shibaibaogao… 含 "ai"(拼音路径真实形态)
            return format!("失败报告{f:06}.log");
        }
        if f % 50000 == 7 {
            return format!("steadyq{f:08}.bin"); // 窄 token ~50 条
        }
        if f % 67 == 0 {
            let stem = ["main", "email", "detail", "train", "domain"][f as usize % 5];
            return format!("{stem}_{f:08}.txt"); // "ai" ≈1.49%
        }
        if f % 25 == 0 {
            let stem = ["config", "console", "connect", "control"][f as usize % 4];
            return format!("{stem}{f:08}.dll"); // "con" ≈3.96%
        }
        let stem = ["data", "readme", "notes", "setup", "cache", "driver", "system", "user", "project", "backup"]
            [f as usize % 10];
        format!("{stem}_{f:08}.bin")
    }

    fn profile_store() -> (VolumeStore, PinyinCache) {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for d in 0..PROFILE_DIRS {
            s.insert(10 + d, crate::store::ROOT_FRN, 0x0010, format!("dir{d:05}").as_bytes());
        }
        for f in 0..PROFILE_FILES {
            let frn = 10 + PROFILE_DIRS + f;
            let parent = if f % 10 == 9 { 10 + (f / 10 % PROFILE_DIRS) } else { crate::store::ROOT_FRN };
            s.insert_meta(
                frn,
                parent,
                0,
                profile_name(f).as_bytes(),
                1_600_000_000 + (f % 1_000_000) as i64,
                u64::MAX,
            );
        }
        s.compact();
        let pinyin = PinyinCache::rebuild(&s);
        (s, pinyin)
    }

    fn profile_ms(mut f: impl FnMut() -> usize, warmup: usize, iters: usize) -> (Vec<f64>, usize) {
        let mut last = 0usize;
        for _ in 0..warmup {
            last = f();
        }
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            last = f();
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (samples, last)
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "release-profile 基准(250 万条合成记录)")]
    fn bench_wide_query_phase_profile() {
        let t0 = Instant::now();
        let (store, pinyin) = profile_store();
        let build_s = t0.elapsed().as_secs_f64();
        let mb = |n: usize| n as f64 / (1024.0 * 1024.0);
        println!(
            "[profile] build: {build_s:.1}s entries={} names={:.1}MB names_lc={:.1}MB 列合计={:.1}MB",
            store.len(),
            mb(store.names.len()),
            mb(store.names_lc.len()),
            mb(store.heap_bytes()),
        );

        let ai = QuerySpec { tokens: vec!["ai".into()], ..Default::default() };
        let con = QuerySpec { tokens: vec!["con".into()], ..Default::default() };
        let narrow = QuerySpec { tokens: vec!["steadyq".into()], ..Default::default() };
        let empty = QuerySpec::default();
        let ai_tokens = vec![b"ai".to_vec()];

        // ① memmem 纯扫描
        let (s1, hits_raw) = profile_ms(
            || memchr::memmem::Finder::new("ai").find_iter(&store.names_lc).count(),
            2,
            15,
        );
        // ② 命中→slot 解析(含陈项过滤与同条目去重,无 token AND/无过滤/无收集)
        // 去重口径与 query 的 total 一致(同一名字内多次出现只算一条),
        // 否则 "hits_resolved" 按原始出现计数,和 total 的语义错位。
        let (s2, hits_resolved) = profile_ms(
            || {
                let finder = memchr::memmem::Finder::new("ai");
                let mut n = 0usize;
                let mut last: Option<u32> = None;
                for off in finder.find_iter(&store.names_lc) {
                    if let Some(slot) = store.slot_at_offset(off) {
                        if last != Some(slot) {
                            last = Some(slot);
                            n += 1;
                        }
                    }
                }
                n
            },
            2,
            15,
        );
        // ③ 完整引擎单卷路径("ai")
        let (s3, ai_total) = profile_ms(|| crate::query::query_volume(&store, &ai, 100).1, 2, 25);
        // ④ 拼音匹配
        let (s4, py_hits) = profile_ms(|| pinyin.search(&store, &ai_tokens, 100).count(), 2, 25);
        // ⑤ 其他查询形态完整路径(验收对照的「改前」基线)
        let (s_con, con_total) = profile_ms(|| crate::query::query_volume(&store, &con, 100).1, 2, 15);
        let (s_narrow, narrow_total) = profile_ms(|| crate::query::query_volume(&store, &narrow, 100).1, 2, 15);
        let (s_empty, empty_total) = profile_ms(|| crate::query::query_volume(&store, &empty, 800).1, 2, 10);
        // 任务 1 合成对照:production cap(2000)下,旧双路(All + Folders 各跑
        // query_volume)vs 一次 split 遍历。只算引擎候选评估(不含拼音/应用层
        // 元数据),用于证明「三趟→一趟」省的是同一批候选的第二遍扫描。
        let mut folders_spec = ai.clone();
        folders_spec.kind = KindFilter::Folders;
        let (s_all_2k, all_2k) = profile_ms(|| crate::query::query_volume(&store, &ai, 2000).1, 2, 15);
        let (s_fold_2k, fold_2k) = profile_ms(|| crate::query::query_volume(&store, &folders_spec, 2000).1, 2, 15);
        let (s_split_2k, split_totals) = profile_ms(
            || {
                let (a, f, ta, tf, _) =
                    crate::query::query_volume_split_cancel(&store, &ai, 2000, 2000, None);
                assert!(a.len() >= f.len());
                ta + tf
            },
            2,
            15,
        );
        // 拼音候选:旧双路各扫一次同一序列,合并后只扫一次(引擎层 query_split
        // 已把这一份省掉)。cap=2000 的实测用于折算完整旧/新引擎耗时。
        let (s_py_2k, py_hits_2k) =
            profile_ms(|| pinyin.search(&store, &ai_tokens, 2000).count(), 2, 15);
        // 拼音缓存重建耗时(快照「加载时重建」方案的成本依据)
        let (s_rebuild, py_entries) = profile_ms(|| PinyinCache::rebuild(&store).slots.len(), 1, 3);
        // n-gram 倒排:内存占用 + 重建耗时(compact/加载时重建的成本依据)。
        // 放在全部计时测量之后,不影响上面的采样;此后不再查询。
        let ng_mb = mb(store.ngram_heap_bytes());
        let mut store = store;
        let t = Instant::now();
        store.rebuild_ngram();
        println!(
            "[profile] ngram heap={ng_mb:.1}MB rebuild(2.5M)={:.0}ms",
            t.elapsed().as_millis()
        );

        let p = |samples: &[f64]| (percentile(samples, 0.5), percentile(samples, 0.9));
        let (ai_p50, ai_p90) = p(&s3);
        println!("[profile] ai   hits_raw={hits_raw} resolved={hits_resolved} total={ai_total} (expect≈{} files+{} dirs)", 37_300, 0);
        println!(
            "[profile] ai   ①memmem扫描 p50={:.2}ms p90={:.2}ms | ②+slot解析 p50={:.2}ms | ③完整查询 p50={ai_p50:.2}ms p90={ai_p90:.2}ms",
            p(&s1).0, p(&s1).1, p(&s2).0,
        );
        println!(
            "[profile] pinyin entries={} rebuild p50={:.1}ms | 'ai' pinyin p50={:.2}ms hits={py_hits}",
            py_entries, p(&s_rebuild).0, p(&s4).0,
        );
        let two_pass = p(&s_all_2k).0 + p(&s_fold_2k).0 + 2.0 * p(&s_py_2k).0;
        let one_pass = p(&s_split_2k).0 + p(&s_py_2k).0;
        println!(
            "[bench-merge] 'ai' cap=2000: 旧双路 All p50={:.2}ms + Folders p50={:.2}ms + 2×拼音 p50={:.2}ms = {two_pass:.2}ms → split 一遍{:.2}ms + 1×拼音 = {one_pass:.2}ms (saved {:.2}ms, totals={}/{}, pinyin_hits={py_hits_2k})",
            p(&s_all_2k).0,
            p(&s_fold_2k).0,
            p(&s_py_2k).0,
            p(&s_split_2k).0,
            two_pass - one_pass,
            all_2k,
            fold_2k,
        );
        assert_eq!(split_totals, all_2k + fold_2k, "split 的两个 total 必须等于双路 total 之和");
        println!(
            "[profile] con  total={con_total} p50={:.2}ms p90={:.2}ms",
            p(&s_con).0, p(&s_con).1,
        );
        println!(
            "[profile] narrow total={narrow_total} p50={:.2}ms p90={:.2}ms",
            p(&s_narrow).0, p(&s_narrow).1,
        );
        println!(
            "[profile] empty total={empty_total} p50={:.2}ms p90={:.2}ms",
            p(&s_empty).0, p(&s_empty).1,
        );
        // 自洽:解析数应等于完整 total 与拼音之和的量级(拼音命中与名字命中部分重叠)
        assert_eq!(ai_total, hits_resolved, "memmem→slot 解析数必须等于完整 total(单 token 无过滤)");
        assert!(ai_total > 30_000 && ai_total < 50_000, "ai 命中量级应≈1.5%(真实 17630/117万), got {ai_total}");
        assert!(con_total > 80_000, "con 中频量级, got {con_total}");
        assert!(narrow_total >= 40 && narrow_total <= 60, "窄 token ~50, got {narrow_total}");
        assert!(py_entries > 4_000, "CJK 拼音候选 ~0.2%, got {py_entries}");

        // ---------- MINOR-2 基准:改名风暴下 reindex overlay 满态 ----------
        // compact 后先做 5 万条就地改名(只进 overlay、不进 delta,旧实现
        // needs_compact 看不到它);再注入 32768 条追加(delta 同为阈值量级),
        // 复现 overlay 8.3 万的最坏查询态。断言 needs_compact 捕获该状态,
        // 并测 "ai"/"con" 满态延迟与 compact 恢复后的延迟。
        let renames = 50_000usize;
        for slot in 0..renames {
            let frn = store.frn[slot];
            let attrs = store.attrs[slot] as u32;
            store.insert(
                frn,
                crate::store::ROOT_FRN,
                attrs,
                format!("ai_con_{slot:06}.txt").as_bytes(),
            );
        }
        assert!(store.delta_is_empty(), "就地改名不进 delta");
        assert!(
            store.needs_compact(),
            "改名风暴的 overlay 必须纳入 needs_compact(旧实现此处为 false)"
        );
        for i in 0..crate::store::COMPACT_DELTA as u64 {
            let frn = 10 + PROFILE_DIRS + PROFILE_FILES + i;
            store.insert(
                frn,
                crate::store::ROOT_FRN,
                0,
                profile_name(PROFILE_FILES + i).as_bytes(),
            );
        }
        let overlay = store.reindex_slots().len();
        assert_eq!(overlay, renames + crate::store::COMPACT_DELTA);
        assert!(store.needs_compact());

        // 满态正确性:快路径 total 必须与强制 memmem 路径一致
        let mem_ai = crate::query::query_volume_memmem_scan(&store, &ai, &ai_tokens, 0, b"ai", 100, None).1;
        let (s_ai_ov, ai_ov_total) = profile_ms(|| crate::query::query_volume(&store, &ai, 100).1, 2, 15);
        assert_eq!(ai_ov_total, mem_ai, "overlay 满态下 ai 快路径 total 必须与 memmem 一致");
        let con_tokens = vec![b"con".to_vec()];
        let mem_con =
            crate::query::query_volume_memmem_scan(&store, &con, &con_tokens, 0, b"con", 100, None).1;
        let (s_con_ov, con_ov_total) = profile_ms(|| crate::query::query_volume(&store, &con, 100).1, 2, 15);
        assert_eq!(con_ov_total, mem_con, "overlay 满态下 con 快路径 total 必须与 memmem 一致");

        // compact 恢复(重建倒排、清空 overlay)后,同一批查询应回到常数成本
        let t_compact = Instant::now();
        store.compact();
        let compact_ms = t_compact.elapsed().as_millis();
        let (s_ai_post, ai_post_total) = profile_ms(|| crate::query::query_volume(&store, &ai, 100).1, 2, 15);
        let (s_con_post, con_post_total) = profile_ms(|| crate::query::query_volume(&store, &con, 100).1, 2, 15);
        assert_eq!(ai_post_total, ai_ov_total, "compact 不改变 ai 查询结果");
        assert_eq!(con_post_total, con_ov_total, "compact 不改变 con 查询结果");
        println!(
            "[bench-overlay] renames={renames} appends={} overlay={overlay} needs_compact=caught | ai total={ai_ov_total} 满态 p50={:.2}ms p90={:.2}ms → compact后 p50={:.2}ms p90={:.2}ms",
            crate::store::COMPACT_DELTA,
            p(&s_ai_ov).0,
            p(&s_ai_ov).1,
            p(&s_ai_post).0,
            p(&s_ai_post).1,
        );
        println!(
            "[bench-overlay] con total={con_ov_total} 满态 p50={:.2}ms p90={:.2}ms → compact后 p50={:.2}ms p90={:.2}ms | compact(2.5M+改名)={compact_ms}ms",
            p(&s_con_ov).0,
            p(&s_con_ov).1,
            p(&s_con_post).0,
            p(&s_con_post).1,
        );
    }

    /// 分段保存期间:store 冻结(修改类消息排队),查询按冻结视图服务;
    /// 保存完成后积压消息按序应用,快照只包含保存开始时的状态,新增量
    /// 由旁车承载(崩溃恢复语义不变)。
    #[test]
    fn segmented_save_freezes_store_and_buffers_mutations() {
        let dir = temp_index_dir("seg-engine-freeze");
        let guid = 0x5e6u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());

        // 保存前变更(应进快照)
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(13, 10, 0, "pre.txt")],
            next_usn: 150,
        });
        engine.volumes[0].save_retry_at = None;
        assert!(engine.start_save(0));
        assert!(engine.save_in_progress());

        // 压住让路钩子:保存暂停在起始阶段
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hook = force.clone();
        engine.set_query_yield_hook(Some(Box::new(move || {
            hook.load(std::sync::atomic::Ordering::Relaxed)
        })));
        assert!(engine.maintain());
        assert!(engine.save_in_progress(), "有查询时保存必须让路而不是整存");

        // 保存期间到达的修改:排队,不得改 store
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(14, 10, 0, "post.txt")],
            next_usn: 160,
        });
        assert!(engine.has_deferred_msgs());
        assert!(
            engine.volumes[0]
                .store
                .as_ref()
                .unwrap()
                .find_slot(14)
                .is_none(),
            "保存期间 store 必须冻结"
        );
        // 冻结视图下查询照常:pre 可见,post 尚不可见。
        let find = |e: &Engine, token: &str| {
            e.query(&QuerySpec {
                tokens: vec![token.to_string()],
                ..Default::default()
            })
            .total
        };
        assert_eq!(find(&engine, "pre"), 1);
        assert_eq!(find(&engine, "post"), 0);

        // 放行:保存收尾 → 积压消息重放
        force.store(false, std::sync::atomic::Ordering::Relaxed);
        while engine.save_in_progress() {
            engine.maintain();
        }
        assert!(!engine.has_deferred_msgs(), "积压消息必须重放完");
        assert_eq!(find(&engine, "post"), 1, "保存后新增量必须应用");

        // 快照只含保存开始时的状态;post 由旁车承载。
        let nr = NoiseRules::builtin().rules();
        let loaded = snapshot::load(&dir, guid, nr.hash(), &nr).expect("snapshot loads");
        assert!(loaded.store.find_slot(13).is_some(), "pre 必须进快照");
        assert!(loaded.store.find_slot(14).is_none(), "post 不得进本次快照");
        engine.maintain(); // 旁车节流写盘(几 KB)
        let sc = sidecar::load(&dir, guid).unwrap().expect("sidecar persisted");
        assert!(!sc.ops.is_empty(), "保存后新增量必须进旁车");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// flush_all(退出路径)必须把进行中的分段保存一次跑完:快照落盘、
    /// tmp 不残留、保存态清空。
    #[test]
    fn flush_all_finishes_active_segmented_save() {
        let dir = temp_index_dir("seg-engine-flush");
        let guid = 0x5e9u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(13, 10, 0, "flush.txt")],
            next_usn: 150,
        });
        engine.volumes[0].save_retry_at = None;
        assert!(engine.start_save(0));

        // 压住让路钩子,让保存停在半途(模拟退出时保存正被查询打断)。
        let force = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hook = force.clone();
        engine.set_query_yield_hook(Some(Box::new(move || {
            hook.load(std::sync::atomic::Ordering::Relaxed)
        })));
        assert!(engine.maintain());
        assert!(engine.save_in_progress());

        engine.flush_all();
        assert!(!engine.save_in_progress(), "退出收尾必须完成分段保存");
        assert!(!engine.has_deferred_msgs());
        assert!(snapshot::snapshot_path(&dir, guid).is_file());
        let nr = NoiseRules::builtin().rules();
        let loaded = snapshot::load(&dir, guid, nr.hash(), &nr).expect("snapshot loads");
        assert!(loaded.store.find_slot(13).is_some());
        let tmp = dir.join(format!("vol_{:032x}.tmp", std::process::id()));
        assert!(!tmp.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 控制帧(JournalReset)会取代冻结 store:中止保存、清理 tmp,
    /// 先重放已排队消息再处理控制帧(顺序语义不丢)。
    #[test]
    fn segmented_save_aborts_on_journal_reset() {
        let dir = temp_index_dir("seg-engine-abort");
        let guid = 0x5e7u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        assert!(engine.start_save(0));
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(13, 10, 0, "queued.txt")],
            next_usn: 150,
        });
        assert!(engine.has_deferred_msgs());

        engine.handle(EngineMsg::JournalReset { guid });
        assert!(!engine.save_in_progress(), "控制帧必须中止保存");
        assert!(!engine.has_deferred_msgs());
        assert!(
            engine.volumes[0].store.is_none(),
            "JournalReset 按原序生效(先重放,后清空)"
        );
        assert!(!snapshot::snapshot_path(&dir, guid).is_file());
        let tmp = dir.join(format!("vol_{:032x}.tmp", std::process::id()));
        assert!(!tmp.exists(), "中止后不得残留 tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启动清理:上次保存中途被杀留下的 vol_*.tmp 必须被删除。
    #[test]
    fn orphan_save_tmp_is_removed_on_startup() {
        let dir = temp_index_dir("seg-tmp-orphan");
        let guid = 0x5e8u128;
        let mut engine = engine_with(&dir);
        feed_ready(&mut engine, guid, 'C');
        assert!(engine.maintain());
        let tmp = dir.join(format!("vol_{:032x}.tmp", std::process::id()));
        std::fs::write(&tmp, b"partial snapshot").unwrap();
        assert!(tmp.exists());
        let _ = engine_with(&dir); // 新会话:load_snapshots 清孤儿 tmp
        assert!(!tmp.exists(), "启动必须清理孤儿 tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归(用户实测 13s):整卷 compact 不可中断且单卷 3s 级。最近有查询、
    /// 或有等待中的查询时不得启动 compact;多卷同时到期也只允许一轮压一卷,
    /// 否则一次击键要等所有卷重排完。
    #[test]
    fn compact_waits_for_idle_and_query() {
        let dir = temp_index_dir("compact-gate");
        let guid = 0x77u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 4_200,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let recs: Vec<Rec> = (0..4_200)
            .map(|i| rec(1_000 + i, 0, 0, &format!("f{i}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady { guid, journal_id: 7, next_usn: 100, count: 4_200 });
        // 删掉 4100 条 → dead_count 越过 COMPACT_DEAD,needs_compact() 为真。
        let removes: Vec<Rec> = (0..4_100)
            .map(|i| Rec {
                frn: 1_000 + i,
                parent_frn: 0,
                attrs: 0,
                op: Op::Remove,
                name: String::new(),
                mtime: 0,
                size: u64::MAX,
            })
            .collect();
        engine.handle(EngineMsg::Records { guid, recs: removes, next_usn: 120 });
        let needs = |engine: &Engine| {
            engine.volumes[0]
                .store
                .as_ref()
                .map(|s| s.needs_compact())
                .unwrap_or(false)
        };
        assert!(needs(&engine), "构造出待 compact 状态");

        // 1) 刚查过:本轮不得 compact(800ms 安静窗口没到)。
        let spec = QuerySpec {
            tokens: vec!["f".to_string()],
            ..Default::default()
        };
        let _ = engine.query(&spec);
        engine.maintain();
        assert!(needs(&engine), "最近有查询时不得启动 compact");

        // 2) 有等待中的查询:即便空闲窗口已过也让路。
        engine.set_compact_idle_gap(Duration::ZERO);
        engine.set_query_yield_hook(Some(Box::new(|| true)));
        engine.maintain();
        assert!(needs(&engine), "有等待查询时不得启动 compact");

        // 3) 真正空闲:启动分片 compact,按片推进直到完成。
        engine.set_query_yield_hook(Some(Box::new(|| false)));
        engine.maintain();
        let mut slices = 0;
        while engine.compact_in_progress() {
            engine.maintain();
            slices += 1;
            assert!(slices < 10_000, "分片 compact 必须能收敛");
        }
        assert!(!needs(&engine), "空闲时应完成 compact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真后台 compact 期间:修改类消息排队(store 冻结)、查询照常;完成后按序重放。
    #[test]
    fn background_compact_defers_mutations_and_replays() {
        let dir = temp_index_dir("bg-compact-defer");
        let guid = 0x78u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 125_000,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let recs: Vec<Rec> = (0..125_000)
            .map(|i| rec(1_000 + i, 0, 0, &format!("f{i}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count: 125_000,
        });
        let removes: Vec<Rec> = (0..5_000)
            .map(|i| Rec {
                frn: 1_000 + i,
                parent_frn: 0,
                attrs: 0,
                op: Op::Remove,
                name: String::new(),
                mtime: 0,
                size: u64::MAX,
            })
            .collect();
        engine.handle(EngineMsg::Records { guid, recs: removes, next_usn: 120 });
        engine.set_compact_idle_gap(Duration::ZERO);

        engine.maintain(); // 推进/轮询真后台 compact
        assert!(
            engine.compact_in_progress(),
            "12 万条在后台线程内不可能瞬间压完"
        );
        // 冻结期间的查询照常走旧列。
        let old_spec = QuerySpec {
            tokens: vec!["f120000".to_string()],
            ..Default::default()
        };
        assert!(engine.query(&old_spec).total >= 1, "压缩期间查询不得失效");

        // 压缩期间到达的新文件：排队，查询暂时看不到。
        engine.handle(EngineMsg::Records {
            guid,
            recs: vec![rec(9_000_000, 0, 0, "during_compact.txt")],
            next_usn: 200,
        });
        assert!(engine.has_deferred_msgs(), "冻结期间修改必须排队");
        let new_spec = QuerySpec {
            tokens: vec!["during_compact".to_string()],
            ..Default::default()
        };
        assert_eq!(engine.query(&new_spec).total, 0, "冻结期间不得看到新消息");

        // 查询线程只轮询 channel:100 轮 maintain 加起来应是微秒~毫秒级,
        // 而不是旧分片实现的 100×4ms 预算(后者会明显拖慢同线程查询)。
        let t_poll = Instant::now();
        for _ in 0..100 {
            engine.maintain();
        }
        assert!(
            t_poll.elapsed() < Duration::from_millis(250),
            "引擎线程不得在 maintain 里执行 compact 切片: {:?}",
            t_poll.elapsed()
        );

        let mut slices = 0usize;
        settle_compact(&mut engine);
        while engine.has_deferred_msgs() && slices < 200 {
            engine.maintain();
            slices += 1;
        }
        assert!(!engine.has_deferred_msgs(), "完成后必须重放积压消息");
        assert_eq!(engine.query(&new_spec).total, 1, "重放后新文件必须可搜");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 控制帧(JournalReset)要清空同卷 store:必须中止后台任务,不能继续往
    /// 已清空的列上换血。
    #[test]
    fn background_compact_aborts_on_journal_reset() {
        let dir = temp_index_dir("bg-compact-abort");
        let guid = 0x79u128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 125_000,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let recs: Vec<Rec> = (0..125_000)
            .map(|i| rec(1_000 + i, 0, 0, &format!("g{i}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count: 125_000,
        });
        let removes: Vec<Rec> = (0..5_000)
            .map(|i| Rec {
                frn: 1_000 + i,
                parent_frn: 0,
                attrs: 0,
                op: Op::Remove,
                name: String::new(),
                mtime: 0,
                size: u64::MAX,
            })
            .collect();
        engine.handle(EngineMsg::Records { guid, recs: removes, next_usn: 120 });
        engine.set_compact_idle_gap(Duration::ZERO);
        engine.maintain();
        assert!(engine.compact_in_progress());
        engine.handle(EngineMsg::JournalReset { guid });
        assert!(!engine.compact_in_progress(), "控制帧必须中止后台 compact");
        assert!(engine.volumes[0].store.is_none(), "JournalReset 已清空 store");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 噪声规则重置会清空 store:同样必须中止后台 compact,否则任务完成时
    /// 会把旧规则下 fork 的整卷 store 又装回去(install 守卫也会拒绝,但先
    /// 中止能立刻释放线程与内存,语义也更直白)。
    #[test]
    fn reset_volumes_for_noise_aborts_background_compact() {
        let dir = temp_index_dir("bg-compact-noise-reset");
        let guid = 0x7bu128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 125_000,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let recs: Vec<Rec> = (0..125_000)
            .map(|i| rec(1_000 + i, 0, 0, &format!("n{i}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count: 125_000,
        });
        assert!(engine.compact_in_progress(), "大卷首扫收尾应交给后台线程");

        let affected = engine.reset_volumes_for_noise(Some(&['C']));
        assert_eq!(affected, vec!['C']);
        assert!(!engine.compact_in_progress(), "重置必须中止后台 compact");
        assert!(engine.volumes[0].store.is_none(), "重置已清空 store");
        assert!(!engine.volumes[0].complete, "重置后卷回到未就绪");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 退出收尾必须等后台 compact 换血,否则快照会写到扫描态列(未归并)。
    #[test]
    fn flush_all_waits_for_background_compact() {
        let dir = temp_index_dir("bg-compact-flush");
        let guid = 0x7au128;
        let mut engine = engine_with(&dir);
        engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: 'C',
            journal_id: 7,
            next_usn: 100,
            est_records: 125_000,
        });
        engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let recs: Vec<Rec> = (0..125_000)
            .map(|i| rec(1_000 + i, 0, 0, &format!("h{i}.txt")))
            .collect();
        engine.handle(EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count: 125_000,
        });
        assert!(engine.compact_in_progress(), "大卷首扫收尾应交给后台线程");

        engine.flush_all();
        assert!(!engine.compact_in_progress(), "退出前必须收掉后台任务");
        assert!(
            !engine.volumes[0].store.as_ref().unwrap().is_scanning(),
            "换血后应退出 scan 模式"
        );

        let engine2 = engine_with(&dir);
        assert_eq!(
            engine2.volumes[0].store.as_ref().unwrap().len(),
            125_000,
            "退出快照必须包含完整重排后的列"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
