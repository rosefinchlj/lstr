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
        self.run(query, filters, limit, |_| true)
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
        self.run(query, filters, limit, move |kind| {
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
            let Some(path) = self.engine.path_of(hit.vol, hit.slot) else { continue };
            let name = self.engine.entry_name(hit.vol, hit.slot);
            let is_dir = self.engine.is_dir(hit.vol, hit.slot);
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
        keep: impl Fn(Kind) -> bool,
    ) -> Result<(Vec<Entry>, usize), String> {
        let spec = build_spec(query, filters);
        let outcome = self.engine.query(&spec);
        let want_pinyin = query.text.is_ascii();

        let mut entries = Vec::with_capacity(outcome.hits.len());
        for hit in &outcome.hits {
            let Some(path) = self.engine.path_of(hit.vol, hit.slot) else {
                continue;
            };
            let name = self.engine.entry_name(hit.vol, hit.slot);
            if name.is_empty() {
                continue;
            }
            let is_dir = self.engine.is_dir(hit.vol, hit.slot);
            let kind = kind_of(&name, is_dir);
            if !keep(kind) {
                continue;
            }
            // 懒填元数据:FindFirstFileW(约 100 条 × ~20µs);不存在=USN 延迟,丢弃
            let Some((size, modified, fs_dir)) = idx_win::find_meta(&path) else {
                continue;
            };
            if fs_dir != is_dir {
                // 类型漂移(罕见):以文件系统为准
                let kind = kind_of(&name, fs_dir);
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
        ext: query.ext.clone(),
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
/// `connect_timeout` 覆盖 UAC 等待;断线后按 `retry` 节奏重连(引擎状态保留,
/// 重连握手从快照位点续传)。返回是否已发起 worker 拉起。
pub fn spawn_client_thread(
    handshake_slot: std::sync::Arc<std::sync::Mutex<Vec<(u128, u64, i64)>>>,
    tx: Sender<EngineMsg>,
    shutdown: std::sync::Arc<AtomicBool>,
    launched_worker: bool,
) {
    std::thread::Builder::new()
        .name("lstr-idx-client".to_string())
        .spawn(move || {
            let mut launched = launched_worker;
            let mut first_connect = !launched_worker;
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // 先拉起 worker 再连接(UAC 一次/会话);已提权常驻时秒连
                if !launched && !pipe_exists() {
                    launch_worker();
                }
                launched = true;
                // 首连给足 UAC 时间;重连短超时。握手位点每次现取:
                // 引擎进度推进后,重连即从新位点续传(方案 §4.1)。
                let snapshot = handshake_slot.lock().unwrap().clone();
                let timeout = if first_connect { Duration::from_secs(60) } else { Duration::from_secs(10) };
                first_connect = false;
                match idx_client::connect_and_handshake(snapshot, timeout) {
                    Ok(client) => {
                        launched = true;
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
                // 重连退避;worker 进程可能已死,再拉一次
                if !launched || !pipe_exists() {
                    launch_worker();
                    launched = true;
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

fn frame_loop(client: windows::Win32::Foundation::HANDLE, tx: &Sender<EngineMsg>, shutdown: &AtomicBool) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match idx_client::read_frame(client, Duration::from_secs(2)) {
            Some(frame) => {
                if let Some(msg) = to_engine_msg(frame) {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
            }
            None => {
                // 超时 → 探活;管道断开则退出本循环
                if !pipe_alive(client) {
                    return;
                }
            }
        }
    }
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
        F::Batch { guid, recs, .. } => EngineMsg::Records { guid, recs },
        F::VolumeReady { guid, journal_id, next_usn, count } => {
            EngineMsg::VolumeReady { guid, journal_id, next_usn, count }
        }
        F::JournalReset { guid } => EngineMsg::JournalReset { guid },
        F::VolumeGone { guid } => EngineMsg::VolumeGone { guid },
        F::VolumeAppeared { guid, letter } => EngineMsg::VolumeAppeared { guid, letter },
        F::WorkerExit { .. } | F::Shutdown => return None,
        F::Hello { .. } | F::Welcome { .. } | F::Rescan { .. } => return None,
    })
}

fn pipe_exists() -> bool {
    idx_client::connect_and_handshake(Vec::new(), Duration::from_millis(800)).is_ok()
}

fn pipe_alive(client: windows::Win32::Foundation::HANDLE) -> bool {
    idx_client::pipe_alive(client)
}

/// ShellExecute runas 拉起提权 worker(UAC 一次/会话)。
pub fn launch_worker() -> bool {
    let Some(exe) = std::env::current_exe().ok() else { return false };
    let exe_str = exe.display().to_string();
    // 复用 lstr 的 shell_execute_with 语义:runas + 参数
    crate::win::shell::shell_execute_runas_with_args(&exe_str, "--index-worker").is_ok()
}

/// 引擎构造(主进程内,加载快照)。
pub fn make_engine(cfg: &crate::config::IndexConfig) -> Engine {
    Engine::new(EngineConfig {
        index_dir: crate::config::attach_dir().join("index"),
        exclude_noise: cfg.exclude_noise,
        candidate_cap: cfg.candidate_cap,
    })
}
