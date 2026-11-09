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
use crate::search::everything::{initial_of, kind_of, tone_for};
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
        self.run(query, filters, limit, KindFilter::All, |_| true)
    }

    /// 「全部」双路:kind 过滤在引擎侧完成,语义与 Everything 的
    /// folder:/!folder: 一致。
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
        let want_pinyin = query.text.is_ascii();

        let mut entries = Vec::with_capacity(outcome.hits.len().min(limit));
        for hit in &outcome.hits {
            // H3:limit 契约 —— 调用方预算(40-120/60-500)必须生效,
            // 不能对最多 2000×卷 的全部候选逐条 find_meta 后才返回。
            if entries.len() >= limit {
                break;
            }
            let Some(path) = self.engine.path_of(hit) else {
                continue;
            };
            let name = self.engine.entry_name(hit);
            if name.is_empty() {
                continue;
            }
            let is_dir = self.engine.is_dir(hit);
            let mut kind = kind_of(&name, is_dir);
            if !keep(kind) {
                continue;
            }
            // 懒填元数据:FindFirstFileW(约 100 条 × ~20µs);不存在=USN 延迟,丢弃
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
        Ok((entries, outcome.total))
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
) {
    std::thread::Builder::new()
        .name("lstr-idx-client".to_string())
        .spawn(move || {
            let mut first_connect = !launched_worker;
            let mut launch_declined = false;
            let mut last_launch = std::time::Instant::now() - Duration::from_secs(60);
            loop {
                if shutdown.load(Ordering::Relaxed) {
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
            // M4:主进程要退出时显式发 Shutdown,worker 收到后不再重建管道
            // 并自行退出;否则提权的常驻 worker 会在 lstr 关闭后一直留着。
            let _ = idx_client::write_frame(client, &lstr_index::proto::Frame::Shutdown);
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
                        // 引擎线程已经收工(服务被 drop):顺手通知 worker 退出。
                        let _ = idx_client::write_frame(
                            client,
                            &lstr_index::proto::Frame::Shutdown,
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