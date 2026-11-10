//! 自研索引后端(方案 §4/§6):BuiltinProvider 对齐 EverythingProvider 的
//! 查询面(search / search_split / search_exact),产出 Entry 走既有
//! finalize/rank 管线;client 线程负责 worker 连接与帧→EngineMsg 转换。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use lstr_index::{
    client as idx_client, win as idx_win, ClientCommand, Engine, EngineConfig, EngineMsg,
    KindFilter, QuerySpec,
};

use crate::model::{Entry, Kind, Query, Scope};
use crate::search::everything::{initial_of, kept_in_scope, kind_of, tone_for};
use crate::search::filters::SearchFilters;
use crate::search::pinyin;

/// 运行期后端形态(供 UI 文案)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// 自研索引(worker 管道在线)。
    Builtin,
    /// Everything IPC。
    Everything,
}

/// UI 聚合状态:索引构建进度 + 当前实际后端。
#[derive(Clone, Debug)]
pub struct IndexUiStatus {
    pub phase: lstr_index::Phase,
    pub volumes: Vec<lstr_index::VolumeProgress>,
    pub phase_since: std::time::Instant,
    pub backend: BackendKind,
    /// Everything IPC 是否可用(时间筛选的降级依据)。
    pub everything_ok: bool,
}

pub struct BuiltinProvider {
    pub engine: Engine,
}

impl BuiltinProvider {
    /// 包一层 panic 隔离:查询路径上的任何 bug 都只能让这一次查询返回空,
    /// 不能把 run_worker 线程打死(否则 UI 会永远停在「搜索中…」)。
    /// 根因修复见 engine::save_volume 的 pinyin 重建;这里是兜底。
    fn query_spec(&self, spec: &QuerySpec) -> lstr_index::QueryOutcome {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.engine.query(spec))) {
            Ok(outcome) => outcome,
            Err(_) => {
                lstr_index::snapshot::debug_log("search: engine.query panicked, returning empty");
                lstr_index::QueryOutcome::default()
            }
        }
    }

    /// 单次 All-kind 查询的双路结果,panic 隔离与 query_spec 相同。
    fn query_split_spec(&self, spec: &QuerySpec) -> lstr_index::SplitOutcome {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.engine.query_split(spec))) {
            Ok(outcome) => outcome,
            Err(_) => {
                lstr_index::snapshot::debug_log("search: engine.query_split panicked, returning empty");
                lstr_index::SplitOutcome::default()
            }
        }
    }
}

impl BuiltinProvider {
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    pub fn status(&self) -> IndexUiStatus {
        let st = self.engine.status();
        IndexUiStatus {
            phase: st.phase,
            volumes: st.volumes,
            phase_since: st.since,
            backend: BackendKind::Builtin,
            everything_ok: false,
        }
    }

    pub fn search(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<(Vec<Entry>, usize), String> {
        // 单分类作用域(文件夹/程序/文件)必须在这里按 kind 过滤:引擎侧过滤
        // 负责候选,keep 再兜底类型漂移。8e2e208 把 kind 抽成 run 的参数后,
        // 这里一度误传 KindFilter::All + 全放行,导致三个分类 tab 都返回
        // 全部类型(2026-11-09 用户报「选了程序还出文件夹」)。
        let scope = query.scope;
        self.run(query, filters, limit, scope_kind_filter(scope), move |kind| {
            kept_in_scope(scope, kind)
        })
    }

    /// 旧「全部」双路实现:folders/files 各跑一次全引擎。生产路径已改为
    /// [`Self::search_split_once`];这里保留为对拍 oracle(见 tests 里的
    /// `split_once_matches_two_pass_with_real_files`)。
    #[cfg(test)]
    pub fn search_split(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
        folders: bool,
    ) -> Result<(Vec<Entry>, usize), String> {
        let kind_filter = if folders { KindFilter::Folders } else { KindFilter::All };
        self.run(query, filters, limit, kind_filter, move |kind| {
            if folders {
                kind == Kind::Folder
            } else {
                kind != Kind::Folder
            }
        })
    }

    /// 「全部」双路合并版:一次 All-kind 引擎查询拿到文件夹前缀、文件前缀与
    /// 两个精确计数,再按各自预算产出 Entry。返回
    /// `(folders, files, folder_total, all_total)`;旧 files 路总数 =
    /// `all_total - folder_total`。结果与旧双路逐条一致(对拍测试锁死)。
    pub fn search_split_once(
        &self,
        query: &Query,
        filters: &SearchFilters,
        folder_limit: usize,
        file_limit: usize,
    ) -> Result<(Vec<Entry>, Vec<Entry>, usize, usize), String> {
        let mut spec = build_spec(query, filters);
        spec.kind = KindFilter::All;
        // 一次性分项剖析:引擎候选(eval)与收尾(path_of/find_meta/Entry)分开计时。
        // 旧双路里 files 路 15.8ms vs folders 8.4ms 的根因就在这里 —— 引擎两部分
        // 几乎相等,差的是 files 路要为 4 倍的候选逐条 FindFirstFileW。
        let t_engine = std::time::Instant::now();
        let outcome = self.query_split_spec(&spec);
        let engine_ms = t_engine.elapsed().as_secs_f32() * 1000.0;
        let mut folders = Vec::with_capacity(outcome.folder_hits.len().min(folder_limit));
        let mut files = Vec::with_capacity(outcome.file_hits.len().min(file_limit));
        let t_folders = std::time::Instant::now();
        let folder_meta = self.fill_entries(
            &outcome.folder_hits,
            query,
            folder_limit,
            |kind| kind == Kind::Folder,
            &mut folders,
        );
        let folder_fill_ms = t_folders.elapsed().as_secs_f32() * 1000.0;
        let t_files = std::time::Instant::now();
        let file_meta = self.fill_entries(
            &outcome.file_hits,
            query,
            file_limit,
            |kind| kind != Kind::Folder,
            &mut files,
        );
        let file_fill_ms = t_files.elapsed().as_secs_f32() * 1000.0;
        // 测试里不写 APPDATA 日志(只用 release 基准/真实部署观测)。
        #[cfg(not(test))]
        if engine_ms + folder_fill_ms + file_fill_ms > 15.0 {
            lstr_index::snapshot::debug_log(&format!(
                "search: split engine={engine_ms:.1}ms folder_fill={folder_fill_ms:.1}ms(folder_meta={folder_meta}) file_fill={file_fill_ms:.1}ms(file_meta={file_meta}) hits={}+{} text={:?}",
                outcome.folder_hits.len(),
                outcome.file_hits.len(),
                query.text
            ));
        }
        Ok((
            folders,
            files,
            outcome.total_folders,
            outcome.total_all,
        ))
    }

    /// 完全同名:索引内 offs 二分,毫秒级(替代 wfn: 二次 IPC)。
    pub fn search_exact(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<(Vec<Entry>, usize), String> {
        let mut spec = build_spec(query, filters);
        spec.exact = Some(query.text.to_lowercase());
        let outcome = self.query_spec(&spec);
        let want_pinyin = query.text.is_ascii();
        let mut entries = Vec::with_capacity(outcome.hits.len());
        for hit in &outcome.hits {
            let Some(path) = self.engine.path_of(hit) else { continue };
            let name = self.engine.entry_name(hit);
            let is_dir = self.engine.is_dir(hit);
            let kind = kind_of(&name, is_dir);
            let Some((size, modified, fs_dir)) = idx_win::find_meta(&path) else { continue };
            let pinyin = (want_pinyin && pinyin::has_cjk(&name)).then(|| pinyin::keys(&name));
            entries.push(Entry {
                tone: tone_for(&path),
                initial: initial_of(&name),
                id: path.clone(),
                kind,
                name,
                path: Some(std::path::PathBuf::from(path)),
                size: (!fs_dir && size > 0).then_some(size),
                modified: (modified > 0).then_some(modified),
                pinyin,
            });
            if entries.len() >= limit {
                break;
            }
        }
        Ok((entries, outcome.total))
    }

    fn run(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
        kind_filter: KindFilter,
        keep: impl Fn(Kind) -> bool,
    ) -> Result<(Vec<Entry>, usize), String> {
        let mut spec = build_spec(query, filters);
        spec.kind = kind_filter;
        let outcome = self.query_spec(&spec);
        let mut entries = Vec::with_capacity(outcome.hits.len().min(limit));
        let _ = self.fill_entries(&outcome.hits, query, limit, keep, &mut entries);
        Ok((entries, outcome.total))
    }

    /// Hit → Entry 的统一收尾:limit 预算、名字/kind 过滤、路径拼接、懒填元数据、
    /// 类型漂移校正。旧 `run` 与新的合并双路共用这一份逻辑,保证两条路径的
    /// Entry 字段与顺序逐条一致。
    /// 返回实际发起的 `FindFirstFileW` 次数(剖析用:files 路慢在元数据条数)。
    fn fill_entries(
        &self,
        hits: &[lstr_index::Hit],
        query: &Query,
        limit: usize,
        keep: impl Fn(Kind) -> bool,
        entries: &mut Vec<Entry>,
    ) -> usize {
        let want_pinyin = query.text.is_ascii();
        let mut meta_calls = 0usize;
        for hit in hits {
            // H3:limit 契约 —— 调用方预算(40-120/60-500)必须生效,
            // 不能对最多 2000×卷 的全部候选逐条 find_meta 后才返回。
            if entries.len() >= limit {
                break;
            }
            // 先做零成本的名字/kind 过滤,再拼路径:旧实现先 path_of(沿父链
            // 逐级分配 String),被 kind 丢弃的文件夹候选也白付了整条路径成本。
            let name = self.engine.entry_name(hit);
            if name.is_empty() {
                continue;
            }
            let is_dir = self.engine.is_dir(hit);
            let mut kind = kind_of(&name, is_dir);
            if !keep(kind) {
                continue;
            }
            let Some(path) = self.engine.path_of(hit) else {
                continue;
            };
            // 懒填元数据:FindFirstFileW(约 100 条 × ~20µs);不存在=USN 延迟,丢弃
            meta_calls += 1;
            let Some((size, modified, fs_dir)) = idx_win::find_meta(&path) else {
                continue;
            };
            if fs_dir != is_dir {
                // 类型漂移(罕见):以文件系统为准
                kind = kind_of(&name, fs_dir);
                if !keep(kind) {
                    continue;
                }
            }
            let pinyin = (want_pinyin && pinyin::has_cjk(&name)).then(|| pinyin::keys(&name));
            entries.push(Entry {
                tone: tone_for(&path),
                initial: initial_of(&name),
                id: path.clone(),
                kind,
                name,
                path: Some(std::path::PathBuf::from(path)),
                size: (!fs_dir && size > 0).then_some(size),
                modified: (modified > 0).then_some(modified),
                pinyin,
            });
        }
        meta_calls
    }
}

/// 单分类作用域 → 引擎侧 KindFilter(索引只收集对应 kind 的候选)。
fn scope_kind_filter(scope: Scope) -> KindFilter {
    match scope {
        Scope::Folder => KindFilter::Folders,
        Scope::Program => KindFilter::Programs,
        Scope::File => KindFilter::Files,
        _ => KindFilter::All,
    }
}

/// Query + SearchFilters → 引擎查询规格。
pub fn build_spec(query: &Query, filters: &SearchFilters) -> QuerySpec {
    let mut spec = QuerySpec {
        tokens: query.text.split_whitespace().map(String::from).collect(),
        kind: match query.scope {
            Scope::Folder => KindFilter::Folders,
            Scope::Program => KindFilter::Programs,
            Scope::File => KindFilter::Files,
            _ => KindFilter::All,
        },
        // M1:ext 规范化 —— ext:RS / ext:.rs 必须与索引里的小写名匹配。
        ext: query
            .ext
            .as_ref()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase()),
        path_prefix: query.path_filter.as_deref().map(str::to_lowercase),
        group_exts: Vec::new(),
        exact: None,
        mtime_after: None,
        mtime_before: None,
    };
    if let Some(location) = &filters.location {
        let p = location.path().display().to_string().to_lowercase();
        if spec.path_prefix.is_none() {
            spec.path_prefix = Some(p);
        }
    }
    // 时间筛选:B 步起目录扫描把 mtime 写进索引,内置模式也能按时间过滤。
    if let Some(t) = filters.time {
        spec.mtime_after = Some(t.since_unix());
    }
    if spec.ext.is_none() {
        if let Some(group) = &filters.group {
            spec.group_exts = group
                .extensions()
                .iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect();
        }
    }
    spec
}

/// worker 收到 Shutdown 后的退出等待上限:worker 要 join 各卷线程(对账批次
/// 间检查 stop)、写 clean-exit 标记后才释放管道;超时即放弃 —— 协议容忍
/// 半开,残留 worker 由自身的 idle 超时与 clean-exit 降级机制兜底。
const WORKER_EXIT_WAIT: Duration = Duration::from_secs(3);

/// client 线程任何退出路径都置位 `client_done`:run_worker 据此确认
/// 「Shutdown 已发出/worker 已收尾」,GUI 的 on_exit 等待不会悬空。
struct ClientDoneGuard(std::sync::Arc<AtomicBool>);

impl Drop for ClientDoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// 启动客户端线程:连接 worker(runas 拉起含在内)→ 帧循环 → EngineMsg。
/// UAC 等待由首次连接的 60s 超时覆盖;断线后按固定节奏重连(引擎状态保留,
/// 重连握手从快照位点续传)。线程自管拉起与重连,便于调用方 fire-and-forget。
pub fn spawn_client_thread(
    handshake_slot: std::sync::Arc<std::sync::Mutex<Vec<(u128, u64, i64, u64, bool)>>>,
    tx: std::sync::mpsc::SyncSender<EngineMsg>,
    shutdown: std::sync::Arc<AtomicBool>,
    launched_worker: bool,
    index_cfg: crate::config::IndexConfig,
    index_cmd_rx: Receiver<ClientCommand>,
    volume_mask: Arc<AtomicU32>,
    client_done: std::sync::Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("lstr-idx-client".to_string())
        .spawn(move || {
            let _done_guard = ClientDoneGuard(client_done);
            let mut first_connect = !launched_worker;
            let mut launch_declined = false;
            // 测试进程里不拉起提权 worker:UAC 弹窗会卡住无人值守的测试。
            // 生产构建(cfg!(test) == false)行为不变。
            if cfg!(test) {
                launch_declined = true;
            }
            let mut last_launch = std::time::Instant::now() - Duration::from_secs(60);
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    // 退出收尾:worker 可能活着但当前无会话(重连间隙/重连退避)。
                    // 必须带**真实握手状态**连一次 —— 空握手会让 worker 对每卷
                    // 从头全量扫(pipe_exists 的历史踩坑)。连上后 frame_loop 同款
                    // 语义:发 Shutdown 并等 worker 退出(有超时)。
                    if idx_client::pipe_available() {
                        let snapshot = handshake_slot.lock().unwrap().clone();
                        if let Ok(client) =
                            idx_client::connect_and_handshake(snapshot, Duration::from_secs(3))
                        {
                            let _ = idx_client::shutdown_and_wait(
                                client,
                                lstr_index::PIPE_NAME,
                                WORKER_EXIT_WAIT,
                            );
                            idx_win::close(client);
                        }
                    }
                    return;
                }
                // 先拉起 worker 再连接(UAC 一次/会话);已提权常驻时秒连。
                // 用户拒绝过 UAC 就不再弹(防骚扰),保持 Fallback 走 Everything。
                // 守卫还要求距上次拉起 >1s:worker 启动后要一点时间建管道,
                // 否则 HandShake 失败与「管道不存在」会在循环里空转重拉。
                // 参数按「当前」掩码现算:设置页可能改过卷选择,worker 重启后
                // 必须跟着新卷集合走,不能沿用启动时的旧参数。
                if !launch_declined
                    && !pipe_exists()
                    && last_launch.elapsed() > Duration::from_secs(1)
                {
                    let extra_args =
                        worker_launch_args(&index_cfg, volume_mask.load(Ordering::Relaxed));
                    last_launch = std::time::Instant::now();
                    if !launch_worker(&extra_args) {
                        launch_declined = true;
                    }
                }
                // 首连给足 UAC 时间;重连短超时。握手位点每次现取:
                // 引擎进度推进后,重连即从新位点续传(方案 §4.1)。
                let snapshot = handshake_slot.lock().unwrap().clone();
                let timeout = if first_connect { Duration::from_secs(60) } else { Duration::from_secs(10) };
                first_connect = false;
                match idx_client::connect_and_handshake(snapshot, timeout) {
                    Ok(client) => {
                        frame_loop(client, &tx, &shutdown, &index_cmd_rx, &volume_mask);
                        let _ = tx.send(EngineMsg::WorkerLost);
                    }
                    Err(_err) => {
                        let _ = tx.send(EngineMsg::WorkerLost);
                    }
                }
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // 重连退避;worker 进程可能已死,再拉一次。
                // 但用户拒过 UAC 就不再弹(与上面的守卫一致),否则每 20s 骚扰一次。
                if !launch_declined
                    && !pipe_exists()
                    && last_launch.elapsed() > Duration::from_secs(1)
                {
                    let extra_args =
                        worker_launch_args(&index_cfg, volume_mask.load(Ordering::Relaxed));
                    last_launch = std::time::Instant::now();
                    if !launch_worker(&extra_args) {
                        launch_declined = true;
                    }
                }
                for _ in 0..40 {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        })
        .expect("spawn idx client");
}

fn frame_loop(
    client: windows::Win32::Foundation::HANDLE,
    tx: &std::sync::mpsc::SyncSender<EngineMsg>,
    shutdown: &AtomicBool,
    index_cmd_rx: &Receiver<ClientCommand>,
    volume_mask: &AtomicU32,
) {
    // 连上先把当前卷选择同步给 worker:worker 是常驻提权进程,可能带着上一次
    // 会话/启动参数进来;这一帧保证会话内的卷掩码与主进程一致。
    let _ = idx_client::write_frame(
        client,
        &lstr_index::proto::Frame::SetVolumes { mask: volume_mask.load(Ordering::Relaxed) },
    );
    'frames: loop {
        if shutdown.load(Ordering::Relaxed) {
            // M4 + 退出收 worker:主进程要退出时显式发 Shutdown 并**等待
            // worker 退出**(有超时)。旧实现只写一帧就断开,进程随即退出 ——
            // 帧能不能到达全看线程调度,实测多次留下提权 worker 占住唯一
            // 管道实例,下次启动撞上旧 worker 出现怪状态。
            let _ = idx_client::shutdown_and_wait(client, lstr_index::PIPE_NAME, WORKER_EXIT_WAIT);
            break 'frames;
        }
        // 设置页指令(切卷/重建卷):非阻塞写帧,不打断下面的读循环。
        loop {
            match index_cmd_rx.try_recv() {
                Ok(cmd) => {
                    let frame = idx_client::command_frame(&cmd);
                    if !idx_client::write_frame(client, &frame) {
                        break 'frames;
                    }
                    if matches!(cmd, ClientCommand::Shutdown) {
                        break 'frames;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // 服务被 drop:继续读,主进程退出时由 shutdown 收尾。
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        // 读超时取 300ms(而不是 2s):设置页切卷的指令要靠这个间隙尽快写出去。
        match idx_client::read_frame(client, Duration::from_millis(300)) {
            Some(frame) => {
                if let Some(msg) = to_engine_msg(frame) {
                    if tx.send(msg).is_err() {
                        // 引擎线程已经收工(服务被 drop):顺手通知 worker 退出
                        // 并等其收尾(与退出路径同款语义)。
                        let _ = idx_client::shutdown_and_wait(
                            client,
                            lstr_index::PIPE_NAME,
                            WORKER_EXIT_WAIT,
                        );
                        break 'frames;
                    }
                }
            }
            None => {
                // 超时 → 探活;管道断开则退出本循环
                if !pipe_alive(client) {
                    break 'frames;
                }
            }
        }
    }
    // 句柄所有权在本函数:任何退出路径都关闭。旧实现直接 return,
    // 每个会话/重连泄漏一个 HANDLE,还让 worker 的实例一直被占。
    idx_win::close(client);
}

fn to_engine_msg(frame: lstr_index::proto::Frame) -> Option<EngineMsg> {
    use lstr_index::proto::Frame as F;
    Some(match frame {
        F::VolumeMeta { guid, letter, journal_id, next_usn, est_records } => {
            EngineMsg::VolumeMeta { guid, letter, journal_id, next_usn, est_records }
        }
        F::ScanBegin { guid, resume_frn } => EngineMsg::ScanBegin { guid, resume_frn },
        F::ScanProgress { guid, frn, est, count } => {
            EngineMsg::ScanProgress { guid, frn, est, count }
        }
        F::Batch { guid, recs, next_usn } => EngineMsg::Records { guid, recs, next_usn },
        F::VolumeReady { guid, journal_id, next_usn, count } => {
            EngineMsg::VolumeReady { guid, journal_id, next_usn, count }
        }
        F::JournalReset { guid } => EngineMsg::JournalReset { guid },
        F::DirSyncBegin { guid } => EngineMsg::DirSyncBegin { guid },
        F::DirSyncEnd { guid, ok } => EngineMsg::DirSyncEnd { guid, ok },
        F::DirChildren { guid, parent_frn, recs } => {
            EngineMsg::DirChildren { guid, parent_frn, recs }
        }
        F::VolumeGone { guid } => EngineMsg::VolumeGone { guid },
        F::VolumeAppeared { guid, letter } => EngineMsg::VolumeAppeared { guid, letter },
        F::UsnWatch { guid, state } => EngineMsg::UsnWatch { guid, state },
        F::WorkerExit { .. } | F::Shutdown => return None,
        F::Hello { .. } | F::Welcome { .. } | F::Rescan { .. } => return None,
        // C→W 控制帧(设置页切卷):worker 不会回给主进程,遇到即忽略。
        F::SetVolumes { .. } => return None,
    })
}

fn pipe_exists() -> bool {
    // 只探活,不建会话:旧实现 connect_and_handshake(Vec::new()) 会真的连上
    // worker(空握手 → 每卷从 FRN 0 全量扫),且不关句柄 → 永久占住唯一实例
    // (worker 卡在会话里,主进程从此连不上)。
    idx_client::pipe_available()
}

fn pipe_alive(client: windows::Win32::Foundation::HANDLE) -> bool {
    idx_client::pipe_alive(client)
}

/// 组装 worker 启动参数:`--index-worker --index-token <hex>` 之后追加首扫模式、
/// journal 开关与卷过滤(阶段 1 默认 `dir` = 零 USN FSCTL)。
///
/// 卷过滤以 `volume_mask` 为准(设置页运行期可改,worker 每次重启都按当前
/// 掩码重新生成参数)。掩码=全部时不加 `--index-volumes`:让 worker 自己枚举
/// 本机固定 NTFS 卷,新插盘也能自动纳入。
pub fn worker_launch_args(index: &crate::config::IndexConfig, volume_mask: u32) -> String {
    let mut extra = String::new();
    if index.backend == "usn" {
        // USN 实时增量层(实验):dirscan 首扫 → journal 轮询(worker 侧带熔断/
        // 游标校验/clean-exit 护栏)。固定 dir 首扫,与设置页文案一致;
        // 这是唯一会下发 --index-journal 的自动路径。
        extra.push_str(" --index-scan-mode dir --index-journal");
        extra.push_str(&format!(" --index-poll-ms {}", index.journal_poll_ms.max(100)));
    } else if index.scan_mode.eq_ignore_ascii_case("usn") {
        // 旧 USN 首扫模式(蓝屏前遗留,仅手动配置可达):行为不变。
        extra.push_str(" --index-scan-mode usn");
        if index.journal_enabled {
            extra.push_str(" --index-journal");
        }
    }
    // 负载整形:dirscan 批次间限流(>0 才下发,0 = 不限,与旧行为一致)。
    if index.scan_throttle_ms > 0 {
        extra.push_str(&format!(" --index-throttle-ms {}", index.scan_throttle_ms));
    }
    if !lstr_index::volmask::is_all(volume_mask) {
        let volumes: Vec<String> = lstr_index::volmask::letters(volume_mask)
            .iter()
            .map(|c| c.to_string())
            .collect();
        if !volumes.is_empty() {
            extra.push_str(&format!(" --index-volumes {}", volumes.join(",")));
        }
    }
    extra
}

/// ShellExecute runas 拉起提权 worker(UAC 一次/会话)。
/// 能力令牌经命令行传给 worker:固定管道名 + 只比 exe 名的校验挡不住同用户
/// 其它进程,worker 会用 Hello 里的令牌二次校验(L5)。
/// 令牌通道取舍:主进程不提升,能传给提权 worker 的任何通道(用户目录文件/
/// 环境变量/共享内存)对同用户非提权进程都更易读;命令行反而是最小暴露面。
/// 属能力校验而非密码学防护,残余风险见 worker.rs 模块文档的威胁模型。
pub fn launch_worker(extra_args: &str) -> bool {
    let Some(exe) = std::env::current_exe().ok() else { return false };
    let exe_str = exe.display().to_string();
    let params = format!(
        "--index-worker --index-token {}{}",
        idx_client::session_token_hex(),
        extra_args
    );
    // 复用 lstr 的 shell_execute_with 语义:runas + 参数
    crate::win::shell::shell_execute_runas_with_args(&exe_str, &params).is_ok()
}

/// 引擎构造(主进程内,加载快照)。
pub fn make_engine(cfg: &crate::config::IndexConfig) -> Engine {
    Engine::new(EngineConfig {
        index_dir: crate::config::attach_dir().join("index"),
        exclude_noise: cfg.exclude_noise,
        candidate_cap: cfg.candidate_cap,
        // 只加载选中盘的快照;未选中的快照留在磁盘,重新勾选即可秒开。
        volumes: cfg.volume_letters(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::parse;
    use lstr_index::{Op, Phase, Rec};

    struct RealIndex {
        provider: BuiltinProvider,
        test_dir: std::path::PathBuf,
        snap_dir: std::path::PathBuf,
    }

    /// 真实临时索引 fixture:从盘符到 cwd 的目录链 + 测试目录,全部作为引擎
    /// 记录的父链,子项落到真实文件系统(files 个文件 / dirs 个目录 + 特殊项)。
    /// 让 find_meta、kind_of、拼音、类型漂移都走生产路径。
    fn real_index(files: usize, dirs: usize, tag: &str) -> Option<RealIndex> {
        use std::path::Component;

        let base = std::env::current_dir().expect("cwd");
        let Some(Component::Prefix(prefix)) = base.components().next() else {
            return None; // 非盘符路径(UNC)不做文件系统对拍
        };
        let prefix = prefix.as_os_str().to_string_lossy().to_string();
        let letter = prefix.chars().next().filter(|c| c.is_ascii_alphabetic())?;
        let rel: Vec<String> = base
            .components()
            .skip(1)
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let tag = format!("lstr-prov-{tag}-{}", std::process::id());
        let test_dir = base.join("target-opt").join(&tag);
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        let mut names = Vec::new();
        for i in 0..files {
            let name = format!("ai_file_{i:04}.txt");
            std::fs::write(test_dir.join(&name), b"x").unwrap();
            names.push((name, false));
        }
        for i in 0..dirs {
            let name = format!("ai_dir_{i:04}");
            std::fs::create_dir_all(test_dir.join(&name)).unwrap();
            names.push((name, true));
        }
        for name in ["ai_tool.exe", "报告_ai.txt", "AI_CAPS.TXT"] {
            std::fs::write(test_dir.join(name), b"x").unwrap();
            names.push((name.to_string(), false));
        }
        let missing = "ai_missing.txt"; // 索引有、磁盘无:两条路径都必须丢弃

        let snap_dir =
            std::env::temp_dir().join(format!("lstr-prov-snap-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&snap_dir);
        let mut engine = Engine::new(EngineConfig {
            index_dir: snap_dir.clone(),
            exclude_noise: false,
            candidate_cap: 2000,
            volumes: Vec::new(),
        });
        let guid = 0x5151u128;
        engine.handle(lstr_index::EngineMsg::VolumeMeta {
            guid,
            letter,
            journal_id: 7,
            next_usn: 100,
            est_records: 0,
        });
        engine.handle(lstr_index::EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let mut recs = Vec::new();
        let mut parent = 0u64;
        let mut chain: Vec<String> = rel.clone();
        chain.push("target-opt".to_string());
        for (j, name) in chain.iter().enumerate() {
            let frn = 100 + j as u64;
            recs.push(Rec {
                frn,
                parent_frn: parent,
                attrs: 0x10,
                op: Op::Upsert,
                name: name.clone(),
                mtime: 0,
            });
            parent = frn;
        }
        let test_dir_frn = 100 + rel.len() as u64 + 1;
        recs.push(Rec {
            frn: test_dir_frn,
            parent_frn: parent,
            attrs: 0x10,
            op: Op::Upsert,
            name: tag.clone(),
            mtime: 0,
        });
        for (i, (name, is_dir)) in names.iter().enumerate() {
            recs.push(Rec {
                frn: 1000 + i as u64,
                parent_frn: test_dir_frn,
                attrs: if *is_dir { 0x10 } else { 0 },
                op: Op::Upsert,
                name: name.clone(),
                mtime: 0,
            });
        }
        recs.push(Rec {
            frn: 2000,
            parent_frn: test_dir_frn,
            attrs: 0,
            op: Op::Upsert,
            name: missing.to_string(),
            mtime: 0,
        });
        let count = recs.len() as u32;
        engine.handle(lstr_index::EngineMsg::Records { guid, recs, next_usn: 100 });
        engine.handle(lstr_index::EngineMsg::VolumeReady {
            guid,
            journal_id: 7,
            next_usn: 100,
            count,
        });
        assert_eq!(engine.status().phase, Phase::Ready);
        Some(RealIndex {
            provider: BuiltinProvider::new(engine),
            test_dir,
            snap_dir,
        })
    }

    impl Drop for RealIndex {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.test_dir);
            let _ = std::fs::remove_dir_all(&self.snap_dir);
        }
    }

    /// 任务 1 的端到端对拍:同一份真实临时索引上,合并后的 `search_split_once`
    /// 与旧双路 `search_split ×2` 的 Entry 顺序/字段/total 完全一致。
    #[test]
    fn split_once_matches_two_pass_with_real_files() {
        let Some(fx) = real_index(120, 60, "parity") else { return };
        for (text, folder_limit, file_limit) in [
            ("ai", 80usize, 320usize),
            ("a", 80, 320),
            ("ai ext:txt", 80, 320),
        ] {
            let query = parse(text, Scope::All);
            let filters = SearchFilters::default();
            let legacy_folders = fx.provider.search_split(&query, &filters, folder_limit, true).unwrap();
            let legacy_files = fx.provider.search_split(&query, &filters, file_limit, false).unwrap();
            let (folders, files, folder_total, all_total) = fx
                .provider
                .search_split_once(&query, &filters, folder_limit, file_limit)
                .unwrap();
            assert_eq!(folders, legacy_folders.0, "folders 顺序/字段不一致: {text}");
            assert_eq!(files, legacy_files.0, "files 顺序/字段不一致: {text}");
            assert_eq!(folder_total, legacy_folders.1, "folder_total 不一致: {text}");
            assert_eq!(all_total, legacy_files.1, "all_total 不一致: {text}");
        }
    }

    /// 任务 2 基准(手动运行):真实文件系统上旧双路 vs 合并单次的耗时,
    /// 以及引擎评估/Entry 收尾(路径拼接 + FindFirstFileW)的分项。
    /// `cargo test -p lstr --release bench_split_phase_profile -- --nocapture`
    #[test]
    #[cfg_attr(debug_assertions, ignore = "release 基准:真实文件系统元数据")]
    fn bench_split_phase_profile() {
        let Some(fx) = real_index(400, 100, "bench") else { return };
        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let samples = |mut f: Box<dyn FnMut() + '_>, warm: usize, iters: usize| {
            let mut out = Vec::new();
            for i in 0..(warm + iters) {
                let t = std::time::Instant::now();
                f();
                if i >= warm {
                    out.push(t.elapsed().as_secs_f64() * 1000.0);
                }
            }
            out.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (out[iters / 2], out[iters - 1])
        };
        let (old_fold_p50, _) = samples(
            Box::new(|| {
                fx.provider.search_split(&query, &filters, 80, true).unwrap();
            }),
            1,
            5,
        );
        let (old_file_p50, _) = samples(
            Box::new(|| {
                fx.provider.search_split(&query, &filters, 320, false).unwrap();
            }),
            1,
            5,
        );
        let (new_p50, new_max) = samples(
            Box::new(|| {
                fx.provider.search_split_once(&query, &filters, 80, 320).unwrap();
            }),
            1,
            7,
        );
        let mut spec = build_spec(&query, &filters);
        spec.kind = KindFilter::All;
        let (engine_p50, _) = samples(Box::new(|| { fx.provider.engine.query_split(&spec); }), 2, 7);
        println!(
            "[bench-split] real files(500): 旧 folders={old_fold_p50:.2}ms + files={old_file_p50:.2}ms = {:.2}ms | 新 once p50={new_p50:.2}ms max={new_max:.2}ms | 其中引擎 query_split={engine_p50:.2}ms",
            old_fold_p50 + old_file_p50
        );
    }

    #[test]
    fn ext_filter_is_normalized_for_index() {
        // M1:ext:RS / ext:.rs 必须规范化成索引里的小写无点形式。
        let filters = SearchFilters::default();
        let spec = build_spec(&parse("ext:RS", Scope::All), &filters);
        assert_eq!(spec.ext.as_deref(), Some("rs"));
        let spec = build_spec(&parse("ext:.arj", Scope::All), &filters);
        assert_eq!(spec.ext.as_deref(), Some("arj"));
    }

    #[test]
    fn scope_kind_filter_follows_single_scope_tabs() {
        // 8e2e208 曾把这里退化成 KindFilter::All + 全放行,分类 tab 才混进
        // 其它类型;映射与 keep 兜底都要守住。
        assert_eq!(scope_kind_filter(Scope::Folder), KindFilter::Folders);
        assert_eq!(scope_kind_filter(Scope::Program), KindFilter::Programs);
        assert_eq!(scope_kind_filter(Scope::File), KindFilter::Files);
        assert_eq!(scope_kind_filter(Scope::All), KindFilter::All);
        assert!(kept_in_scope(Scope::Folder, Kind::Folder));
        assert!(!kept_in_scope(Scope::Folder, Kind::Program));
        assert!(kept_in_scope(Scope::Program, Kind::Program));
        assert!(!kept_in_scope(Scope::Program, Kind::Folder));
        assert!(!kept_in_scope(Scope::Program, Kind::File));
        assert!(kept_in_scope(Scope::File, Kind::File));
        assert!(!kept_in_scope(Scope::File, Kind::Program));
    }

    #[test]
    fn launch_args_follow_volume_mask() {
        let cfg = crate::config::IndexConfig::default();
        // 全部选中:不加 --index-volumes,交给 worker 自己枚举固定 NTFS 卷
        // (新插盘也能自动纳入)。
        let all = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(!all.contains("--index-volumes"), "全部盘时不应限制卷: {all}");
        // 部分选中:只传掩码里的盘,且按盘符升序。
        let mask = lstr_index::volmask::mask_from_letters(['f', 'c']);
        let args = worker_launch_args(&cfg, mask);
        assert!(args.contains(" --index-volumes C,F"), "args={args}");
    }

    /// backend="usn" 是唯一启用 USN 增量层的取值:固定 dir 首扫 + journal 轮询
    /// + 轮询间隔;auto/builtin 默认不加任何 USN/节流开关(行为与旧版一致)。
    #[test]
    fn usn_backend_enables_journal_layer_only_for_usn() {
        // 默认配置(auto):零 USN 开关
        let cfg = crate::config::IndexConfig::default();
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(!args.contains("--index-journal"), "args={args}");
        assert!(!args.contains("--index-poll-ms"), "args={args}");
        assert!(!args.contains("--index-throttle-ms"), "args={args}");

        // backend=usn:dir 首扫 + journal + poll-ms;即便 scan_mode 被旧配置
        // 遗留成 usn,也强制 dir(与设置页「目录扫描+USN 实时增量」文案一致)。
        let mut cfg = crate::config::IndexConfig { backend: "usn".into(), ..Default::default() };
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-scan-mode dir --index-journal"), "args={args}");
        assert!(args.contains(" --index-poll-ms 500"), "args={args}");
        assert!(!args.contains("--index-scan-mode usn"), "args={args}");
        cfg.scan_mode = "usn".into();
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains("--index-scan-mode dir"), "usn 后端固定 dir 首扫: {args}");
        // poll-ms 下限在参数侧同样兜底
        cfg.journal_poll_ms = 10;
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-poll-ms 100"), "args={args}");

        // 旧 usn 首扫模式(手动配置):保持既有行为
        let mut cfg = crate::config::IndexConfig::default();
        cfg.scan_mode = "usn".into();
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-scan-mode usn"), "args={args}");
        assert!(!args.contains("--index-journal"), "journal_enabled 默认关: {args}");
        cfg.journal_enabled = true;
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-scan-mode usn --index-journal"), "args={args}");
        assert!(!args.contains("--index-poll-ms"), "旧模式不接 poll-ms: {args}");
    }

    /// scan_throttle_ms > 0 时下发给 worker(任意后端);0 时不出现。
    #[test]
    fn scan_throttle_forwarded_when_positive() {
        let mut cfg = crate::config::IndexConfig { scan_throttle_ms: 30, ..Default::default() };
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-throttle-ms 30"), "args={args}");
        cfg.scan_throttle_ms = 0;
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(!args.contains("--index-throttle-ms"), "args={args}");
    }
}