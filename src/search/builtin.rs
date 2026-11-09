//! 自研索引后端(方案 §4/§6):BuiltinProvider 对齐 EverythingProvider 的
//! 查询面(search / search_split / search_exact),产出 Entry 走既有
//! finalize/rank 管线;client 线程负责 worker 连接与帧→EngineMsg 转换。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use lstr_index::{
    client as idx_client, win as idx_win, Engine, EngineConfig, EngineMsg, KindFilter, QuerySpec,
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
        let outcome = self.engine.query(&spec);
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
        let outcome = self.engine.query(&spec);
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
    };
    if let Some(location) = &filters.location {
        let p = location.path().display().to_string().to_lowercase();
        if spec.path_prefix.is_none() {
            spec.path_prefix = Some(p);
        }
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
) {
    let extra_args = worker_launch_args(&index_cfg);
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
                if !launch_declined
                    && !pipe_exists()
                    && last_launch.elapsed() > Duration::from_secs(1)
                {
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
                                frame_loop(client, &tx, &shutdown);
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

fn frame_loop(client: windows::Win32::Foundation::HANDLE, tx: &std::sync::mpsc::SyncSender<EngineMsg>, shutdown: &AtomicBool) {
    'frames: loop {
        if shutdown.load(Ordering::Relaxed) {
            // M4:主进程要退出时显式发 Shutdown,worker 收到后不再重建管道
            // 并自行退出;否则提权的常驻 worker 会在 lstr 关闭后一直留着。
            let _ = idx_client::write_frame(client, &lstr_index::proto::Frame::Shutdown);
            break 'frames;
        }
        match idx_client::read_frame(client, Duration::from_secs(2)) {
            Some(frame) => {
                if let Some(msg) = to_engine_msg(frame) {
                    if tx.send(msg).is_err() {
                        // 引擎线程已经收工(服务被 drop):顺手通知 worker 退出。
                        let _ = idx_client::write_frame(client, &lstr_index::proto::Frame::Shutdown);
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
        F::VolumeGone { guid } => EngineMsg::VolumeGone { guid },
        F::VolumeAppeared { guid, letter } => EngineMsg::VolumeAppeared { guid, letter },
        F::WorkerExit { .. } | F::Shutdown => return None,
        F::Hello { .. } | F::Welcome { .. } | F::Rescan { .. } => return None,
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
pub fn worker_launch_args(index: &crate::config::IndexConfig) -> String {
    let mut extra = String::new();
    // 阶段 1 默认就是 dir(worker 端默认值),只有显式 usn 时才追加开关
    if index.scan_mode.eq_ignore_ascii_case("usn") {
        extra.push_str(" --index-scan-mode usn");
        if index.journal_enabled {
            extra.push_str(" --index-journal");
        }
    }
    let volumes: Vec<String> = index
        .volumes
        .iter()
        .filter_map(|v| v.trim().chars().next())
        .map(|c| c.to_ascii_uppercase().to_string())
        .collect();
    if !volumes.is_empty() {
        extra.push_str(&format!(" --index-volumes {}", volumes.join(",")));
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
}