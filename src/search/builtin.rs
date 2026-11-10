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

/// 「全部」双路一次查询的结果:两条前缀 + 两个精确计数 + 精确同名补齐。
/// `exact` 只在「有截断且未按时间筛选」时填充,与旧 `search_exact` 的第二趟
/// 结果逐条一致;`exact_ms` 供 stage 日志把 exact 与 split 分项计时。
#[derive(Clone, Debug, Default)]
pub struct SplitOnceResult {
    pub folders: Vec<Entry>,
    pub files: Vec<Entry>,
    pub folder_total: usize,
    pub all_total: usize,
    pub exact: Vec<Entry>,
    pub exact_ms: f32,
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

    /// 双路 + 精确同名候选(与旧 search_exact 的候选逐条一致),同一趟产出。
    fn query_split_exact_spec(
        &self,
        spec: &QuerySpec,
        exact: Option<&str>,
    ) -> lstr_index::SplitOutcome {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.engine.query_split_with_exact(spec, exact)
        })) {
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

    /// 旧元数据路径(逐条 find_meta)的双路 oracle:只用于 release 基准对照
    /// 批量填充的收益,生产/对拍均已走 [`Self::search_split_once`]。
    #[cfg(test)]
    fn search_split_per_entry(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
        folders: bool,
    ) -> (Vec<Entry>, usize) {
        let mut spec = build_spec(query, filters);
        spec.kind = if folders { KindFilter::Folders } else { KindFilter::All };
        let outcome = self.query_spec(&spec);
        let mut entries = Vec::with_capacity(outcome.hits.len().min(limit));
        self.fill_entries_per_entry(
            &outcome.hits,
            query,
            limit,
            |kind| {
                if folders {
                    kind == Kind::Folder
                } else {
                    kind != Kind::Folder
                }
            },
            &mut entries,
        );
        (entries, outcome.total)
    }

    /// 「全部」双路合并版:一次 All-kind 引擎查询拿到文件夹前缀、文件前缀、
    /// 两个精确计数,并顺带收集精确同名候选(旧 `search_exact` 第二趟)。
    /// 旧 files 路总数 = `all_total - folder_total`。
    ///
    /// 候选阶段只做索引内可完成的工作(名字/kind/路径/过滤/限流),不碰
    /// 文件系统:size 留空;modified 取索引 mtime(`Hit.mtime`,0=未知),
    /// 供 rank 的 recency 加权使用,零 FS 调用。真实 size/mtime 与类型漂移
    /// 校正在搜索线程 finalize 时只对最终展示条目统一填充(见
    /// `search::finalize_entries_with_pending`)。
    pub fn search_split_once(
        &self,
        query: &Query,
        filters: &SearchFilters,
        folder_limit: usize,
        file_limit: usize,
    ) -> Result<SplitOnceResult, String> {
        let mut spec = build_spec(query, filters);
        spec.kind = KindFilter::All;
        // 时间筛选时 mod.rs 不会补 exact(Everything 分支才支持),这里不做无用功。
        let exact_needle = filters.time.is_none().then(|| query.text.to_lowercase());
        // 分项剖析:引擎评估与候选 Entry 构造(路径拼接/过滤)分开计时。
        let t_engine = std::time::Instant::now();
        let outcome = self.query_split_exact_spec(&spec, exact_needle.as_deref());
        let engine_ms = t_engine.elapsed().as_secs_f32() * 1000.0;
        let mut folders = Vec::with_capacity(outcome.folder_hits.len().min(folder_limit));
        let mut files = Vec::with_capacity(outcome.file_hits.len().min(file_limit));
        let t_folders = std::time::Instant::now();
        self.collect_entries(
            &outcome.folder_hits,
            query,
            folder_limit,
            |kind| kind == Kind::Folder,
            &mut folders,
        );
        let folder_collect_ms = t_folders.elapsed().as_secs_f32() * 1000.0;
        let t_files = std::time::Instant::now();
        self.collect_entries(
            &outcome.file_hits,
            query,
            file_limit,
            |kind| kind != Kind::Folder,
            &mut files,
        );
        let file_collect_ms = t_files.elapsed().as_secs_f32() * 1000.0;
        // 与 mod.rs 的 truncation 口径一致:总数减去已取回条数。
        let file_total = outcome.total_all.saturating_sub(outcome.total_folders);
        let truncation = outcome
            .total_folders
            .saturating_sub(folders.len())
            + file_total.saturating_sub(files.len());
        let mut exact = Vec::new();
        let mut exact_ms = 0.0f32;
        if truncation > 0 && exact_needle.is_some() {
            let t_exact = std::time::Instant::now();
            self.collect_exact_entries(&outcome.exact_hits, query, 50, &mut exact);
            exact_ms = t_exact.elapsed().as_secs_f32() * 1000.0;
        }
        // 测试构建不落盘日志。
        #[cfg(test)]
        let _ = (engine_ms, folder_collect_ms, file_collect_ms);
        // 测试里不写 APPDATA 日志(只用 release 基准/真实部署观测)。
        #[cfg(not(test))]
        if engine_ms + folder_collect_ms + file_collect_ms + exact_ms > 15.0
            || super::profile_queries()
        {
            lstr_index::snapshot::debug_log(&format!(
                "search: split engine={engine_ms:.1}ms folder_collect={folder_collect_ms:.1}ms(entries={}) file_collect={file_collect_ms:.1}ms(entries={}) exact_collect={exact_ms:.1}ms(entries={}) hits={}+{} text={:?}",
                folders.len(),
                files.len(),
                exact.len(),
                outcome.folder_hits.len(),
                outcome.file_hits.len(),
                query.text
            ));
        }
        Ok(SplitOnceResult {
            folders,
            files,
            folder_total: outcome.total_folders,
            all_total: outcome.total_all,
            exact,
            exact_ms,
        })
    }

    /// 完全同名:索引内 offs 二分,毫秒级(替代 wfn: 二次 IPC)。
    /// 候选阶段零 FS,元数据同样在 finalize 内统一填。
    pub fn search_exact(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<(Vec<Entry>, usize), String> {
        let mut spec = build_spec(query, filters);
        spec.exact = Some(query.text.to_lowercase());
        let outcome = self.query_spec(&spec);
        let mut entries = Vec::with_capacity(outcome.hits.len().min(limit));
        self.collect_exact_entries(&outcome.hits, query, limit, &mut entries);
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
        self.collect_entries(&outcome.hits, query, limit, keep, &mut entries);
        Ok((entries, outcome.total))
    }

    /// 候选阶段收尾(零 FS):limit 预算、空名/kind 过滤、路径拼接。
    /// `modified` 来自索引 mtime(0 → None),`size` 留空;展示值由 finalize
    /// 对最终条目统一从 FS 刷新。
    fn collect_entries(
        &self,
        hits: &[lstr_index::Hit],
        query: &Query,
        limit: usize,
        keep: impl Fn(Kind) -> bool,
        entries: &mut Vec<Entry>,
    ) {
        let want_pinyin = query.text.is_ascii();
        for hit in hits {
            if entries.len() >= limit {
                break;
            }
            let name = self.engine.entry_name(hit);
            // 索引里的空名(异常记录)与旧 fill_entries 一样直接丢弃;
            // exact 路径保留旧 search_exact 行为(不按空名过滤)。
            if name.is_empty() {
                continue;
            }
            let is_dir = self.engine.is_dir(hit);
            let kind = kind_of(&name, is_dir);
            if !keep(kind) {
                continue;
            }
            let Some(path) = self.engine.path_of(hit) else {
                continue;
            };
            entries.push(pending_entry(path, name, kind, want_pinyin, hit.mtime));
        }
    }

    /// 精确同名候选收尾(语义与旧 `search_exact` 循环一致:不做类型漂移
    /// 校正、不按空名过滤;元数据延迟到展示层)。
    fn collect_exact_entries(
        &self,
        hits: &[lstr_index::Hit],
        query: &Query,
        limit: usize,
        entries: &mut Vec<Entry>,
    ) {
        let want_pinyin = query.text.is_ascii();
        for hit in hits {
            let Some(path) = self.engine.path_of(hit) else { continue };
            let name = self.engine.entry_name(hit);
            let kind = kind_of(&name, self.engine.is_dir(hit));
            entries.push(pending_entry(path, name, kind, want_pinyin, hit.mtime));
            if entries.len() >= limit {
                break;
            }
        }
    }

    /// 旧逐条 `find_meta` 的 Entry 收尾(无损对拍 oracle,仅测试用)。
    #[cfg(test)]
    fn fill_entries_per_entry(
        &self,
        hits: &[lstr_index::Hit],
        query: &Query,
        limit: usize,
        keep: impl Fn(Kind) -> bool,
        entries: &mut Vec<Entry>,
    ) {
        let want_pinyin = query.text.is_ascii();
        for hit in hits {
            if entries.len() >= limit {
                break;
            }
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
            let Some((size, modified, fs_dir)) = idx_win::find_meta(&path) else {
                continue;
            };
            if fs_dir != is_dir {
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
    }

    /// 旧 `search_exact` 的 Entry 收尾(无损对拍 oracle,仅测试用)。
    #[cfg(test)]
    fn fill_exact_entries_per_entry(
        &self,
        hits: &[lstr_index::Hit],
        query: &Query,
        limit: usize,
        entries: &mut Vec<Entry>,
    ) {
        let want_pinyin = query.text.is_ascii();
        for hit in hits {
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
    }

}

/// 候选 Entry 构造(零 FS):`size` 留空,`modified` 取索引 mtime
/// (0 = 未知 → None)。rank 的 recency 加权因此完全用索引数据,不产生
/// 任何 FS 调用;真实 size 与展示用 mtime 由
/// `search::finalize_entries_with_pending` 在展示前对最终条目刷新。
fn pending_entry(
    path: String,
    name: String,
    kind: Kind,
    want_pinyin: bool,
    mtime: i64,
) -> Entry {
    let pinyin = (want_pinyin && pinyin::has_cjk(&name)).then(|| pinyin::keys(&name));
    Entry {
        tone: tone_for(&path),
        initial: initial_of(&name),
        id: path.clone(),
        kind,
        name,
        path: Some(std::path::PathBuf::from(path)),
        size: None,
        modified: (mtime > 0).then_some(mtime),
        pinyin,
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
        // 盘符过滤（c:/d: 限定、-c: 排除）：交给引擎在卷循环直接跳过，
        // 省掉整卷扫描；0 掩码（全排除）→ 查询结果为空。
        vol_mask: query.drive_mask(),
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
    use super::super::{PendingMeta, Request};
    use crate::history::History;
    use crate::search::query::parse;
    use lstr_index::{Op, Phase, Rec};
    use std::time::Instant;

    struct RealIndex {
        provider: BuiltinProvider,
        test_dir: std::path::PathBuf,
        snap_dir: std::path::PathBuf,
        /// 被 ACL 拒绝列出的目录:Drop 前必须先恢复权限,否则删不掉。
        denied_dir: Option<std::path::PathBuf>,
    }

    impl RealIndex {
        /// 恢复 ACL 后删除测试目录(模拟「磁盘上全部消失」)。
        fn delete_files(&self) {
            if let Some(denied) = &self.denied_dir {
                let _ = std::process::Command::new("icacls")
                    .arg(denied)
                    .arg("/remove:d")
                    .arg("*S-1-1-0")
                    .output();
            }
            let _ = std::fs::remove_dir_all(&self.test_dir);
        }
    }

    /// 文件/目录的真实 mtime(unix 秒)。
    /// 索引记录写同一个值,保证时间筛选口径与磁盘一致。
    fn disk_mtime(path: &std::path::Path) -> i64 {
        std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
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
        let mut dir_names: Vec<String> = Vec::new();
        for i in 0..dirs {
            let name = format!("ai_dir_{i:04}");
            std::fs::create_dir_all(test_dir.join(&name)).unwrap();
            dir_names.push(name);
        }
        // 文件分散到各 ai_dir 子目录(每条父目录 2-4 个候选),模拟真实查询
        // 候选来自几十个父目录的形态;第三个元素 = 父目录在 dir_names 的序号。
        let mut names: Vec<(String, bool, Option<usize>)> = Vec::new();
        for (i, name) in dir_names.iter().enumerate() {
            names.push((name.clone(), true, None));
        }
        for i in 0..files {
            let name = format!("ai_file_{i:04}.txt");
            let parent_idx = (dirs > 0).then_some(i % dirs);
            let parent = parent_idx
                .map(|p| test_dir.join(&dir_names[p]))
                .unwrap_or_else(|| test_dir.clone());
            std::fs::write(parent.join(&name), b"x").unwrap();
            names.push((name, false, parent_idx));
        }
        // 每个子目录再放一个无扩展名文件 "ai":精确同名候选 > 50,专门
        // 验证 exact 补齐的 50 条预算与 offs 顺序,而不只是测到 4 条。
        for (i, dir_name) in dir_names.iter().enumerate() {
            std::fs::write(test_dir.join(dir_name).join("ai"), b"x").unwrap();
            names.push(("ai".to_string(), false, Some(i)));
        }
        // 文件夹就叫 "ai":正文里排在全部 ai_dir_* 之后,通常落在 fetch 窗口
        // 之外,专门用来验证 exact 补齐会把它捞回来并置顶。
        std::fs::create_dir_all(test_dir.join("ai")).unwrap();
        names.push(("ai".to_string(), true, None));
        for name in ["ai_tool.exe", "报告_ai.txt", "AI_CAPS.TXT"] {
            std::fs::write(test_dir.join(name), b"x").unwrap();
            names.push((name.to_string(), false, None));
        }
        // 索引名与磁盘实际大小写不同(Windows 路径大小写不敏感):逐条
        // FindFirstFileW 能命中,批量枚举必须按 ASCII 折叠名同样命中。
        for (disk, index_name) in [
            ("AI_CASE_MISMATCH.TXT", "ai_case_mismatch.txt"),
            ("报告_AI_CASE.TXT", "报告_ai_case.txt"),
        ] {
            std::fs::write(test_dir.join(disk), b"x").unwrap();
            names.push((index_name.to_string(), false, None));
        }
        let missing = "ai_missing.txt"; // 索引有、磁盘无:两条路径都必须丢弃

        // 索引内外类型漂移(两条路径都必须丢弃):
        // - 索引 attrs 说目录、磁盘是文件;
        // - 索引 attrs 说文件(.exe → Program)、磁盘是目录。
        std::fs::write(test_dir.join("ai_drift_dir_index.txt"), b"x").unwrap();
        names.push(("ai_drift_dir_index.txt".to_string(), true, None));
        std::fs::create_dir_all(test_dir.join("ai_drift_file_index.exe")).unwrap();
        names.push(("ai_drift_file_index.exe".to_string(), false, None));

        // 已删但仍在索引窗口内的候选(不是排在最后,确保会进入候选/排名区):
        // 磁盘上没有这个文件,find_meta 失败,新旧路径都必须丢弃并向后补位。
        names.insert(
            (dirs + 50).min(names.len()),
            ("ai_deleted_in_window.txt".to_string(), false, None),
        );

        // 无权限:目录 ACL 拒绝 Everyone 列出/读取,目录内的候选 find_meta
        // 返回 ACCESS_DENIED;新旧路径同样丢弃。目录/文件记录单独挂在测试
        // 目录下(frn 6000/6001),不参与 names 的 frn 映射。
        let denied_dir = "ai_denied_dir";
        let denied_path = test_dir.join(denied_dir);
        std::fs::create_dir_all(&denied_path).unwrap();
        std::fs::write(denied_path.join("ai_denied_file.txt"), b"x").unwrap();
        let denied = std::process::Command::new("icacls")
            .arg(&denied_path)
            .arg("/deny")
            .arg("*S-1-1-0:(OI)(CI)(RD)")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|_| denied_path.clone());

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
        // 父链目录的索引 mtime 也取磁盘真实值,保证时间筛选与磁盘一致。
        let mut chain_disk = std::path::PathBuf::from(format!("{letter}:\\"));
        for (j, name) in chain.iter().enumerate() {
            if j < rel.len() {
                chain_disk.push(name);
            } else {
                chain_disk = base.join("target-opt");
            }
            let frn = 100 + j as u64;
            recs.push(Rec {
                frn,
                parent_frn: parent,
                attrs: 0x10,
                op: Op::Upsert,
                name: name.clone(),
                mtime: disk_mtime(&chain_disk),
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
            mtime: disk_mtime(&test_dir),
        });
        for (i, (name, is_dir, parent_idx)) in names.iter().enumerate() {
            let parent_frn = parent_idx
                .map(|p| 1000 + p as u64)
                .unwrap_or(test_dir_frn);
            // 索引 mtime 与磁盘真实 mtime 对齐,保证时间筛选口径一致。
            let disk_parent = parent_idx
                .map(|p| test_dir.join(&dir_names[p]))
                .unwrap_or_else(|| test_dir.clone());
            recs.push(Rec {
                frn: 1000 + i as u64,
                parent_frn,
                attrs: if *is_dir { 0x10 } else { 0 },
                op: Op::Upsert,
                name: name.clone(),
                mtime: disk_mtime(&disk_parent.join(name)),
            });
        }
        // 无权限目录 + 其中的文件(挂在测试目录下,父链真实存在;目录 ACL
        // 拒绝列出后 find_meta 对文件失败)。
        recs.push(Rec {
            frn: 6000,
            parent_frn: test_dir_frn,
            attrs: 0x10,
            op: Op::Upsert,
            name: denied_dir.to_string(),
            mtime: 0,
        });
        recs.push(Rec {
            frn: 6001,
            parent_frn: 6000,
            attrs: 0,
            op: Op::Upsert,
            name: "ai_denied_file.txt".to_string(),
            mtime: 0,
        });
        recs.push(Rec {
            frn: 5000,
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
            denied_dir: denied,
        })
    }

    impl Drop for RealIndex {
        fn drop(&mut self) {
            self.delete_files();
            let _ = std::fs::remove_dir_all(&self.snap_dir);
        }
    }

    /// 合并 split_once 与旧双路 `query_spec` 的候选顺序/total 对拍(候选阶段
    /// 两条路径都不碰 FS,只验证合并查询的候选与总数口径一致)。
    #[test]
    fn split_once_candidates_match_two_pass_with_real_files() {
        let Some(fx) = real_index(120, 60, "parity") else { return };
        for (text, folder_limit, file_limit) in [
            ("ai", 80usize, 320usize),
            ("a", 80, 320),
            ("ai ext:txt", 80, 320),
            ("AI_CASE_MISMATCH.TXT", 40, 60),
        ] {
            let query = parse(text, Scope::All);
            let filters = SearchFilters::default();
            let legacy_folders = fx.provider.search_split(&query, &filters, folder_limit, true).unwrap();
            let legacy_files = fx.provider.search_split(&query, &filters, file_limit, false).unwrap();
            let result = fx
                .provider
                .search_split_once(&query, &filters, folder_limit, file_limit)
                .unwrap();
            assert_eq!(result.folders, legacy_folders.0, "folders 候选不一致: {text}");
            assert_eq!(result.files, legacy_files.0, "files 候选不一致: {text}");
            assert_eq!(result.folder_total, legacy_folders.1, "folder_total 不一致: {text}");
            assert_eq!(result.all_total, legacy_files.1, "all_total 不一致: {text}");
        }
    }

    /// 候选 Entry 的 modified 直接来自索引 mtime(Hit.mtime),零 FS:
    /// 手工改写 Hit.mtime 后 collect_entries 必须原样带出;0 = 未知 → None。
    #[test]
    fn candidate_entries_take_mtime_from_hit() {
        let Some(fx) = real_index(20, 3, "hit-mtime") else { return };
        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let mut spec = build_spec(&query, &filters);
        spec.kind = KindFilter::All;
        let outcome = fx.provider.engine.query(&spec);
        let mut hit = *outcome.hits.first().expect("fixture 应至少有一个候选");

        hit.mtime = 1_700_000_123;
        let mut entries = Vec::new();
        fx.provider.collect_entries(&[hit], &query, 10, |_| true, &mut entries);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].modified, Some(1_700_000_123));
        assert_eq!(entries[0].size, None, "候选阶段不查 FS size");

        hit.mtime = 0;
        let mut entries = Vec::new();
        fx.provider.collect_entries(&[hit], &query, 10, |_| true, &mut entries);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].modified, None, "索引 mtime=0 回落 None");
    }

    /// 展示阶段用 FS mtime 覆盖候选的索引 mtime:rank 之后预览面板要看到
    /// 磁盘上的最新时间,而不是可能过期的索引值。
    #[test]
    fn display_mtime_is_refreshed_from_fs_after_rank() {
        let Some(fx) = real_index(10, 2, "display-mtime") else { return };
        let path = fx.test_dir.join("ai_tool.exe");
        let path_str = path.display().to_string();
        let entry = Entry {
            id: path_str.clone(),
            kind: Kind::File,
            name: "ai_tool.exe".to_string(),
            path: Some(path.clone()),
            size: None,
            modified: Some(1), // 故意用错误的索引值,验证会被 FS 覆盖
            pinyin: None,
            initial: "A".to_string(),
            tone: 0,
        };
        let filled = super::super::fill_one_meta(entry, true).expect("文件存在,必须填充成功");
        let expected = disk_mtime(&path);
        assert!(expected > 0, "测试文件应有真实 mtime");
        assert_eq!(filled.modified, Some(expected), "展示 mtime 必须来自 FS");
    }

    /// 惰性填充对拍:候选阶段零 FS 的 Entry 交给 `fill_one_meta` 逐条补
    /// size/mtime/漂移校正,必须与旧候选阶段逐条 `find_meta` 的结果逐字段相等。
    /// 真实临时 FS 覆盖:子目录、大小写不同名、已删、无权限、目录/文件类型
    /// 漂移(索引 attrs 与磁盘相反)。
    #[test]
    fn lazy_meta_fill_matches_per_entry_on_real_files() {
        let Some(fx) = real_index(600, 120, "meta-parity") else { return };
        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let mut spec = build_spec(&query, &filters);
        spec.kind = KindFilter::All;
        let outcome = fx
            .provider
            .engine
            .query_split_with_exact(&spec, Some("ai"));
        assert!(!outcome.aborted);
        assert!(fx.denied_dir.is_some(), "无权限夹具必须生效(icacls 可用)");

        for (hits, keep_folder) in [(&outcome.folder_hits, true), (&outcome.file_hits, false)] {
            let keep = |kind: Kind| {
                if keep_folder {
                    kind == Kind::Folder
                } else {
                    kind != Kind::Folder
                }
            };
            // limit = MAX:候选全量取出后再填,等价于旧路径逐条扫描 hits。
            let mut lazy = Vec::new();
            fx.provider
                .collect_entries(hits, &query, usize::MAX, keep, &mut lazy);
            let lazy: Vec<Entry> = lazy
                .into_iter()
                .filter_map(|entry| super::super::fill_one_meta(entry, true))
                .collect();
            let mut per_entry = Vec::new();
            fx.provider
                .fill_entries_per_entry(hits, &query, usize::MAX, keep, &mut per_entry);
            assert_eq!(
                lazy, per_entry,
                "惰性填充与逐条填充不一致(folder={keep_folder}, hits={})",
                hits.len()
            );
        }

        // 精确同名补齐:旧路径不做类型漂移校正,新路径 correct=false。
        let mut lazy = Vec::new();
        fx.provider
            .collect_exact_entries(&outcome.exact_hits, &query, 50, &mut lazy);
        let lazy: Vec<Entry> = lazy
            .into_iter()
            .filter_map(|entry| super::super::fill_one_meta(entry, false))
            .collect();
        let mut per_entry = Vec::new();
        fx.provider
            .fill_exact_entries_per_entry(&outcome.exact_hits, &query, 50, &mut per_entry);
        assert_eq!(lazy, per_entry, "exact 惰性填充与逐条填充不一致");
    }

    /// split 并趟带回的 exact 候选:惰性填充后与旧 `search_exact` 逐条一致
    /// (字段/顺序/50 条预算)。
    #[test]
    fn split_once_exact_matches_search_exact_with_real_files() {
        let Some(fx) = real_index(600, 120, "exact-parity") else { return };
        let filters = SearchFilters::default();
        for (text, folder_limit, file_limit) in [
            ("ai", 30usize, 60usize),
            ("a", 20, 40),
            ("ai ext:txt", 20, 40),
            // 纯过滤器查询(text 为空、tokens 为空):旧 exact 走全量列举,
            // 并趟必须产出同一批候选。
            ("ext:txt", 20, 40),
        ] {
            let query = parse(text, Scope::All);
            let mut spec = build_spec(&query, &filters);
            spec.kind = KindFilter::All;
            let needle = filters.time.is_none().then(|| query.text.to_lowercase());
            let outcome = fx
                .provider
                .engine
                .query_split_with_exact(&spec, needle.as_deref());
            let result = fx
                .provider
                .search_split_once(&query, &filters, folder_limit, file_limit)
                .unwrap();
            let lazy: Vec<Entry> = result
                .exact
                .clone()
                .into_iter()
                .filter_map(|entry| super::super::fill_one_meta(entry, false))
                .collect();
            let mut per_entry = Vec::new();
            fx.provider
                .fill_exact_entries_per_entry(&outcome.exact_hits, &query, 50, &mut per_entry);
            assert_eq!(lazy, per_entry, "exact 补齐不一致: {text}");
        }
    }

    /// 硬性验收(端到端):同一份真实临时索引(>800 候选)上,旧路径
    /// (候选阶段逐条 find_meta → finalize)与新路径(候选零 FS → finalize
    /// 内只对最终候选 find_meta,已删/漂移在去重前丢弃并向后补位)
    /// 最终返回的 entries 逐字段相等(size/modified/kind/顺序),total 口径不变。
    #[test]
    fn lazy_meta_end_to_end_matches_eager_pipeline() {
        let Some(fx) = real_index(600, 120, "e2e-parity") else { return };
        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let mut spec = build_spec(&query, &filters);
        spec.kind = KindFilter::All;
        let outcome = fx
            .provider
            .engine
            .query_split_with_exact(&spec, Some("ai"));
        assert!(!outcome.aborted);
        assert!(outcome.file_hits.len() > 500, "候选规模必须覆盖真实分布");

        // 旧路径:候选阶段逐条 find_meta(folders → files → exact)。
        let mut old_entries = Vec::new();
        fx.provider.fill_entries_per_entry(
            &outcome.folder_hits,
            &query,
            usize::MAX,
            |kind| kind == Kind::Folder,
            &mut old_entries,
        );
        fx.provider.fill_entries_per_entry(
            &outcome.file_hits,
            &query,
            usize::MAX,
            |kind| kind != Kind::Folder,
            &mut old_entries,
        );
        let mut old_exact = Vec::new();
        fx.provider
            .fill_exact_entries_per_entry(&outcome.exact_hits, &query, 50, &mut old_exact);
        old_entries.append(&mut old_exact);
        let old_total = outcome.total_all;

        // 新路径:候选阶段零 FS;exact 标记为「不校正漂移」(旧路径也不校正)。
        let mut new_entries = Vec::new();
        fx.provider.collect_entries(
            &outcome.folder_hits,
            &query,
            usize::MAX,
            |kind| kind == Kind::Folder,
            &mut new_entries,
        );
        let folder_len = new_entries.len();
        fx.provider.collect_entries(
            &outcome.file_hits,
            &query,
            usize::MAX,
            |kind| kind != Kind::Folder,
            &mut new_entries,
        );
        let mut new_exact = Vec::new();
        fx.provider
            .collect_exact_entries(&outcome.exact_hits, &query, 50, &mut new_exact);
        let mut pending = PendingMeta::new();
        pending.mark(&new_entries[..folder_len], true);
        pending.mark(&new_entries[folder_len..], true);
        pending.mark(&new_exact, false);
        new_entries.append(&mut new_exact);

        let history = std::sync::Arc::new(std::sync::Mutex::new(History::in_memory()));
        let config = crate::config::Config::default();
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
        };
        let old_final = super::super::finalize_entries(
            old_entries,
            &request,
            &history,
            &config,
            Vec::new(),
        );
        let (new_final, _meta_ms) = super::super::finalize_entries_with_pending(
            new_entries,
            &request,
            &history,
            &config,
            Vec::new(),
            Some(&pending),
        );
        assert_eq!(old_final, new_final, "最终 Entry 逐字段/顺序不一致");
        assert!(!new_final.is_empty(), "最终条目不能为空");
        assert_eq!(outcome.total_folders + (old_total - outcome.total_folders), old_total);
    }

    /// 生产限流(100 folders / 400 files)下的端到端对拍:旧路径先逐条
    /// find_meta 再截断候选,新路径先按同一预算截断候选、finalize 后只填
    /// 最终展示条目。已删/漂移候选不能挤掉后面的有效候选。
    #[test]
    fn lazy_meta_matches_eager_with_production_limits() {
        let Some(fx) = real_index(600, 120, "limit-parity") else { return };
        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let folder_fetch = 100usize;
        let file_fetch = 400usize;

        // 旧路径:folders/files 两路各自逐条 find_meta,达到预算才停。
        let (mut old_folders, folder_total) = fx
            .provider
            .search_split_per_entry(&query, &filters, folder_fetch, true);
        let (mut old_files, all_total) = fx
            .provider
            .search_split_per_entry(&query, &filters, file_fetch, false);
        let file_total = all_total.saturating_sub(folder_total);
        let truncation = folder_total.saturating_sub(old_folders.len())
            + file_total.saturating_sub(old_files.len());
        old_folders.append(&mut old_files);
        let mut old_entries = old_folders;
        if truncation > 0 {
            let mut spec = build_spec(&query, &filters);
            spec.kind = KindFilter::All;
            let outcome = fx.provider.engine.query_split_with_exact(&spec, Some("ai"));
            let mut exact = Vec::new();
            fx.provider
                .fill_exact_entries_per_entry(&outcome.exact_hits, &query, 50, &mut exact);
            old_entries.append(&mut exact);
        }

        // 新路径:同一预算下候选零 FS,元数据留给 finalize 之后。
        let result = fx
            .provider
            .search_split_once(&query, &filters, folder_fetch, file_fetch)
            .unwrap();
        let mut pending = PendingMeta::new();
        pending.mark(&result.folders, true);
        pending.mark(&result.files, true);
        pending.mark(&result.exact, false);
        let mut new_entries = result.folders.clone();
        new_entries.extend(result.files.iter().cloned());
        new_entries.extend(result.exact.iter().cloned());

        let history = std::sync::Arc::new(std::sync::Mutex::new(History::in_memory()));
        let config = crate::config::Config::default();
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
        };
        let old_final = super::super::finalize_entries(
            old_entries,
            &request,
            &history,
            &config,
            Vec::new(),
        );
        let (new_final, _meta_ms) = super::super::finalize_entries_with_pending(
            new_entries,
            &request,
            &history,
            &config,
            Vec::new(),
            Some(&pending),
        );
        assert_eq!(
            old_final, new_final,
            "生产限流下最终 Entry 逐字段/顺序不一致"
        );
        assert_eq!(folder_total, result.folder_total, "folder_total 不一致");
        assert_eq!(all_total, result.all_total, "all_total 不一致");
    }

    /// 已删候选与 valid 候选同名时的对拍:旧路径在候选阶段就把已删条目
    /// 丢掉,同名去重仍能补满 6 条精确同名;新路径若先去重再填元数据,
    /// 已删条目会占掉一个同名名额。用 120 个同名 "ai" 文件覆盖该边界。
    #[test]
    fn lazy_meta_deleted_duplicate_does_not_steal_exact_name_slot() {
        let Some(fx) = real_index(600, 120, "dup-exact-parity") else { return };
        // 删掉第一个子目录里的精确同名文件:索引仍在,磁盘已无。
        let deleted = fx.test_dir.join("ai_dir_0000").join("ai");
        std::fs::remove_file(&deleted).unwrap();

        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let (folder_fetch, file_fetch) = (100usize, 400usize);

        let (mut old_folders, folder_total) = fx
            .provider
            .search_split_per_entry(&query, &filters, folder_fetch, true);
        let (mut old_files, all_total) = fx
            .provider
            .search_split_per_entry(&query, &filters, file_fetch, false);
        let file_total = all_total.saturating_sub(folder_total);
        let truncation = folder_total.saturating_sub(old_folders.len())
            + file_total.saturating_sub(old_files.len());
        old_folders.append(&mut old_files);
        let mut old_entries = old_folders;
        if truncation > 0 {
            let mut spec = build_spec(&query, &filters);
            spec.kind = KindFilter::All;
            let outcome = fx.provider.engine.query_split_with_exact(&spec, Some("ai"));
            let mut exact = Vec::new();
            fx.provider
                .fill_exact_entries_per_entry(&outcome.exact_hits, &query, 50, &mut exact);
            old_entries.append(&mut exact);
        }

        let result = fx
            .provider
            .search_split_once(&query, &filters, folder_fetch, file_fetch)
            .unwrap();
        let mut pending = PendingMeta::new();
        pending.mark(&result.folders, true);
        pending.mark(&result.files, true);
        pending.mark(&result.exact, false);
        let mut new_entries = result.folders.clone();
        new_entries.extend(result.files.iter().cloned());
        new_entries.extend(result.exact.iter().cloned());

        let history = std::sync::Arc::new(std::sync::Mutex::new(History::in_memory()));
        let config = crate::config::Config::default();
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
        };
        let old_final = super::super::finalize_entries(
            old_entries,
            &request,
            &history,
            &config,
            Vec::new(),
        );
        let (new_final, _meta_ms) = super::super::finalize_entries_with_pending(
            new_entries,
            &request,
            &history,
            &config,
            Vec::new(),
            Some(&pending),
        );
        assert_eq!(
            old_final.iter().filter(|e| e.name == "ai").count(),
            new_final.iter().filter(|e| e.name == "ai").count(),
            "已删同名条目不能占用精确同名名额"
        );
        // mtime 修复后不能再断言 full Entry 相等:本测试删除文件会更新
        // 父目录的 FS mtime,旧路径在候选阶段取元数据、新路径在 finalize
        // 取元数据,两次 FS 快照可能相差 1 秒(索引 mtime 也滞后于删除)。
        // 本测试只关心同名名额与顺序;recency 的排序语义由 rank 单测覆盖。
        let without_mtime = |entries: Vec<Entry>| -> Vec<Entry> {
            entries
                .into_iter()
                .map(|mut entry| {
                    entry.modified = None;
                    entry
                })
                .collect()
        };
        assert_eq!(
            without_mtime(old_final),
            without_mtime(new_final),
            "已删同名场景最终 Entry 不一致(modified 除外)"
        );
    }

    /// 类型漂移的精确同名条目:旧路径的 split 候选在候选阶段被丢弃,但
    /// `search_exact` 的同 id 副本(不做漂移校正)仍会展示。新路径必须在
    /// 去重前丢掉漂移的 split 条目,不能把 exact 副本一起吃掉。
    #[test]
    fn lazy_meta_drift_split_does_not_eat_exact_duplicate() {
        let path = std::env::temp_dir().join(format!(
            "lstr-drift-exact-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"x").unwrap();
        let path_str = path.display().to_string();
        let name = "ai_drift_dir_index.txt".to_string();
        let entry = || Entry {
            id: path_str.clone(),
            // 索引 attrs 说目录,磁盘实际是文件。
            kind: Kind::Folder,
            name: name.clone(),
            path: Some(path.clone()),
            size: None,
            modified: None,
            pinyin: None,
            initial: "A".to_string(),
            tone: 0,
        };
        let mut pending = PendingMeta::new();
        pending.mark(&[entry()], true); // split 候选:校正类型
        pending.mark(&[entry()], false); // exact 副本:不校正
        // 旧路径:split 候选在候选阶段漂移被丢,只留下已填好元数据的 exact 副本。
        let old_entry = super::super::fill_one_meta(entry(), false).unwrap();
        let history = std::sync::Arc::new(std::sync::Mutex::new(History::in_memory()));
        let config = crate::config::Config::default();
        let query = parse(&name, Scope::All);
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
        };
        let old_final = super::super::finalize_entries(
            vec![old_entry],
            &request,
            &history,
            &config,
            Vec::new(),
        );
        let (new_final, _) = super::super::finalize_entries_with_pending(
            vec![entry(), entry()],
            &request,
            &history,
            &config,
            Vec::new(),
            Some(&pending),
        );
        assert_eq!(old_final, new_final);
        assert_eq!(new_final.len(), 1, "exact 副本必须保留");
        assert_eq!(new_final[0].kind, Kind::Folder);
        assert_eq!(new_final[0].size, Some(1));
        let _ = std::fs::remove_file(&path);
    }

    /// 整棵测试目录被删:候选阶段不再崩溃(零 FS),最终填充统一丢弃全部
    /// 条目,与旧路径行为一致(不返回陈旧元数据)。
    #[test]
    fn lazy_meta_falls_back_when_directory_disappears() {
        let Some(fx) = real_index(160, 40, "meta-fallback") else { return };
        let query = parse("ai", Scope::All);
        let filters = SearchFilters::default();
        let mut spec = build_spec(&query, &filters);
        spec.kind = KindFilter::All;
        let outcome = fx.provider.engine.query_split(&spec);

        // 删掉测试目录(先恢复 ACL):所有候选路径都不存在。
        fx.delete_files();
        let mut lazy = Vec::new();
        fx.provider.collect_entries(
            &outcome.file_hits,
            &query,
            usize::MAX,
            |kind| kind != Kind::Folder,
            &mut lazy,
        );
        assert!(!lazy.is_empty(), "候选阶段零 FS:磁盘不存在也要先产出候选");
        let lazy: Vec<Entry> = lazy
            .into_iter()
            .filter_map(|entry| super::super::fill_one_meta(entry, true))
            .collect();
        let mut per_entry = Vec::new();
        fx.provider.fill_entries_per_entry(
            &outcome.file_hits,
            &query,
            usize::MAX,
            |kind| kind != Kind::Folder,
            &mut per_entry,
        );
        assert_eq!(lazy, per_entry, "目录消失后的结果不一致");
        assert!(lazy.is_empty(), "磁盘不存在的条目必须丢弃");
    }

    /// 基准(手动运行):旧路径(候选阶段逐条 find_meta)vs 新路径(候选零 FS
    /// + finalize 内只对最终候选 find_meta)在真实文件系统上的分项耗时。
    /// `cargo test -p lstr --release bench_lazy_meta -- --nocapture`
    #[test]
    #[cfg_attr(debug_assertions, ignore = "release 基准:真实文件系统元数据")]
    fn bench_lazy_meta() {
        let Some(fx) = real_index(600, 120, "bench") else { return };
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
        let mut spec = build_spec(&query, &filters);
        spec.kind = KindFilter::All;
        let outcome = fx
            .provider
            .engine
            .query_split_with_exact(&spec, Some("ai"));

        let history = std::sync::Arc::new(std::sync::Mutex::new(History::in_memory()));
        let config = crate::config::Config::default();
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
        };
        // 旧完整路径:候选阶段对全部候选逐条 find_meta(folder 120 / file 480)
        // → finalize。
        let (old_p50, _) = samples(
            Box::new(|| {
                let mut entries = Vec::new();
                fx.provider.fill_entries_per_entry(
                    &outcome.folder_hits,
                    &query,
                    120,
                    |kind: Kind| kind == Kind::Folder,
                    &mut entries,
                );
                fx.provider.fill_entries_per_entry(
                    &outcome.file_hits,
                    &query,
                    480,
                    |kind: Kind| kind != Kind::Folder,
                    &mut entries,
                );
                let mut exact = Vec::new();
                fx.provider.fill_exact_entries_per_entry(
                    &outcome.exact_hits,
                    &query,
                    50,
                    &mut exact,
                );
                entries.append(&mut exact);
                let out =
                    super::super::finalize_entries(entries, &request, &history, &config, Vec::new());
                assert!(!out.is_empty());
            }),
            1,
            7,
        );
        // 新路径第 1 步:候选构造(零 FS)。
        let (candidate_p50, _) = samples(
            Box::new(|| {
                fx.provider.search_split_once(&query, &filters, 120, 480).unwrap();
            }),
            1,
            7,
        );
        // 新路径完整链路:候选零 FS → finalize 内只对最终候选 find_meta。
        let one = fx
            .provider
            .search_split_once(&query, &filters, 120, 480)
            .unwrap();
        let mut pending = PendingMeta::new();
        pending.mark(&one.folders, true);
        pending.mark(&one.files, true);
        pending.mark(&one.exact, false);
        let mut new_entries = one.folders.clone();
        new_entries.extend(one.files.iter().cloned());
        new_entries.extend(one.exact.iter().cloned());
        let new_len = new_entries.len();
        let (new_p50, _) = samples(
            Box::new(|| {
                let (out, _) = super::super::finalize_entries_with_pending(
                    new_entries.clone(),
                    &request,
                    &history,
                    &config,
                    Vec::new(),
                    Some(&pending),
                );
                assert!(!out.is_empty());
            }),
            1,
            7,
        );
        let (_, meta_ms) = super::super::finalize_entries_with_pending(
            new_entries.clone(),
            &request,
            &history,
            &config,
            Vec::new(),
            Some(&pending),
        );
        let (engine_p50, _) = samples(
            Box::new(|| {
                fx.provider.engine.query_split_with_exact(&spec, Some("ai"));
            }),
            2,
            7,
        );
        println!(
            "[bench-lazy] real 600 files/120 dirs: 旧候选逐条 find_meta+finalize p50={old_p50:.2}ms | 新候选(零 FS,{new_len} 条) p50={candidate_p50:.2}ms | 新完整链路 p50={new_p50:.2}ms(其中 final 元数据 {meta_ms:.2}ms) | 引擎 query_split+exact={engine_p50:.2}ms"
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