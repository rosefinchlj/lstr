//! 自研索引后端(方案 §4/§6):BuiltinProvider 提供 search / search_split /
//! search_exact 查询面,产出 Entry 走既有 finalize/rank 管线;client 线程负责
//! worker 连接与帧→EngineMsg 转换。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use lstr_index::{
    client as idx_client, win as idx_win, ClientCommand, Engine, EngineConfig, EngineMsg,
    KindFilter, QuerySpec,
};

use crate::model::{Entry, Kind, Query, Scope};
use crate::search::drive_weight::EffectiveWeights;
use crate::search::entrymeta::{initial_of, kept_in_scope, kind_of, tone_for};
use crate::search::filters::SearchFilters;
use crate::search::pinyin;

/// P0-2：UI 里的「重新授权」入口。用户拒绝过 UAC 后 client 线程会记住
/// `launch_declined` 并且不再弹窗（防骚扰）；点这个标志后线程在下一次
/// 循环里重置拒绝状态、给足 UAC 等待时间并重新 runas 拉起 worker。
pub(crate) static RELAUNCH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 请求重新拉起提权 worker（用户点击「重新授权」按钮时调用）。
pub fn request_worker_relaunch() {
    RELAUNCH_REQUESTED.store(true, Ordering::SeqCst);
}

/// P0-3：GUI 正常退出时保留提权 worker 常驻（免下次 UAC、免重建索引）。
/// 由设置页 `index.keep_worker_alive` 初始化，运行期改设置即时生效。
pub(crate) static KEEP_WORKER_ALIVE: AtomicBool = AtomicBool::new(true);

/// 托盘「退出并停止索引服务」：本次退出强制发 `Shutdown`，不等 keep-alive。
pub(crate) static STOP_WORKER_ON_EXIT: AtomicBool = AtomicBool::new(false);

/// P2-9：托盘「退出（保留索引服务）」是显式承诺，必须无视设置里的
/// `keep_worker_alive=false`；仅在本次退出生效。
pub(crate) static FORCE_KEEP_ON_EXIT: AtomicBool = AtomicBool::new(false);

/// 更新退出时是否保留索引服务（设置页开关调用）。
pub fn set_keep_worker_alive(enabled: bool) {
    KEEP_WORKER_ALIVE.store(enabled, Ordering::SeqCst);
}

/// 本次退出是否要连带停掉索引服务。
pub fn set_stop_worker_on_exit(enabled: bool) {
    STOP_WORKER_ON_EXIT.store(enabled, Ordering::SeqCst);
}

/// P2-9：托盘「退出（保留索引服务）」调用；优先于设置开关。
pub(crate) fn set_force_keep_on_exit(enabled: bool) {
    FORCE_KEEP_ON_EXIT.store(enabled, Ordering::SeqCst);
}

/// 退出收尾：常驻模式发 `Detach`（worker 不退出，保持索引热态），
/// 否则发 `Shutdown` 并等 worker 收尾（有超时）。
fn farewell(client: windows::Win32::Foundation::HANDLE) {
    let keep = FORCE_KEEP_ON_EXIT.load(Ordering::Relaxed)
        || (KEEP_WORKER_ALIVE.load(Ordering::Relaxed)
            && !STOP_WORKER_ON_EXIT.load(Ordering::Relaxed));
    if keep {
        let _ = idx_client::detach(client);
        lstr_index::snapshot::debug_log(
            "client: detached (worker kept alive for next launch)",
        );
    } else {
        let _ = idx_client::shutdown_and_wait(
            client,
            lstr_index::PIPE_NAME,
            WORKER_EXIT_WAIT,
        );
    }
}

/// UI 聚合状态:索引构建进度。
#[derive(Clone, Debug)]
pub struct IndexUiStatus {
    pub phase: lstr_index::Phase,
    pub volumes: Vec<lstr_index::VolumeProgress>,
    /// 当前阶段开始时间：P1-9 用它估算 ETA / 显示已等待秒数。
    pub since: std::time::Instant,
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
    /// 前缀补集(A-3):宽查询下保证名字以查询词开头的命中进入排序。
    pub prefix: Vec<Entry>,
    pub exact_ms: f32,
    /// P2-7：本次 total 是否为「计数预算内下界」。引擎对空 token 且非 regex 的
    /// 纯过滤查询只数到 FILTER_ONLY_TOTAL_CAP 就停扫（B5）；这里用同一个谓词
    /// 判定，UI 才能对 `ext:rs` / `*.rs` / `C:` 这类同样走纯过滤路径的查询
    /// 显示「N+」，而不是把下界当精确值。
    pub total_rough: bool,
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
            since: st.since,
        }
    }

    pub fn search(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
        weights: &EffectiveWeights,
    ) -> Result<(Vec<Entry>, usize), String> {
        // 单分类作用域(文件夹/程序/文件)必须在这里按 kind 过滤:引擎侧过滤
        // 负责候选,keep 再兜底类型漂移。8e2e208 把 kind 抽成 run 的参数后,
        // 这里一度误传 KindFilter::All + 全放行,导致三个分类 tab 都返回
        // 全部类型(2026-11-09 用户报「选了程序还出文件夹」)。
        let scope = query.scope;
        self.run(query, filters, limit, scope_kind_filter(scope), move |kind| {
            kept_in_scope(scope, kind)
        }, weights)
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
        weights: &EffectiveWeights,
    ) -> Result<(Vec<Entry>, usize), String> {
        let kind_filter = if folders { KindFilter::Folders } else { KindFilter::All };
        self.run(query, filters, limit, kind_filter, move |kind| {
            if folders {
                kind == Kind::Folder
            } else {
                kind != Kind::Folder
            }
        }, weights)
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
        weights: &EffectiveWeights,
    ) -> (Vec<Entry>, usize) {
        let mut spec = build_spec(query, filters, weights);
        spec.kind = if folders { KindFilter::Folders } else { KindFilter::All };
        let outcome = self.query_spec(&spec);
        let ordered = self.ordered_hits(&outcome.hits, weights);
        let mut entries = Vec::with_capacity(outcome.hits.len().min(limit));
        self.fill_entries_per_entry(
            &ordered,
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
        weights: &EffectiveWeights,
    ) -> Result<SplitOnceResult, String> {
        let mut spec = build_spec(query, filters, weights);
        spec.kind = KindFilter::All;
        // 精确同名补集与时间/日期条件同查：候选仍要过 mtime 过滤，不会把
        // 区间外的同名项带回来。
        let exact_needle = (!filters.matches.regex && !query.text.is_empty()).then(|| {
            if filters.matches.case_sensitive {
                query.text.clone()
            } else {
                query.text.to_lowercase()
            }
        });
        // 分项剖析:引擎评估与候选 Entry 构造(路径拼接/过滤)分开计时。
        let t_engine = std::time::Instant::now();
        let outcome = self.query_split_exact_spec(&spec, exact_needle.as_deref());
        let engine_ms = t_engine.elapsed().as_secs_f32() * 1000.0;
        // 多卷命中按权重公平交织后再收尾：否则字母序靠前的盘会吃光
        // folder/file 预算（D/E 的候选根本进不了排序池）。
        let folder_ordered = self.ordered_hits(&outcome.folder_hits, weights);
        let file_ordered = self.ordered_hits(&outcome.file_hits, weights);
        let mut folders = Vec::with_capacity(folder_ordered.len().min(folder_limit));
        let mut files = Vec::with_capacity(file_ordered.len().min(file_limit));
        let t_folders = std::time::Instant::now();
        self.collect_entries(
            &folder_ordered,
            query,
            folder_limit,
            |kind| kind == Kind::Folder,
            &mut folders,
        );
        let folder_collect_ms = t_folders.elapsed().as_secs_f32() * 1000.0;
        let t_files = std::time::Instant::now();
        self.collect_entries(
            &file_ordered,
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
            let exact_ordered = self.ordered_hits(&outcome.exact_hits, weights);
            self.collect_exact_entries(&exact_ordered, query, 50, &mut exact);
            exact_ms = t_exact.elapsed().as_secs_f32() * 1000.0;
        }
        // 前缀补集(A-3):与精确同名补集同条件(有截断才补),复用同一收尾。
        let mut prefix = Vec::new();
        if truncation > 0 && exact_needle.is_some() {
            let prefix_ordered = self.ordered_hits(&outcome.prefix_hits, weights);
            self.collect_exact_entries(&prefix_ordered, query, 50, &mut prefix);
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
            // P2-7：与引擎停扫谓词完全一致（空 token && 非 regex && 撞预算）。
            total_rough: spec.tokens.is_empty()
                && spec.regex.is_none()
                && outcome.total_all >= lstr_index::query::FILTER_ONLY_TOTAL_CAP,
            exact,
            prefix,
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
        weights: &EffectiveWeights,
    ) -> Result<(Vec<Entry>, usize), String> {
        let mut spec = build_spec(query, filters, weights);
        spec.exact = Some(if filters.matches.case_sensitive {
            query.text.clone()
        } else {
            query.text.to_lowercase()
        });
        let outcome = self.query_spec(&spec);
        let ordered = self.ordered_hits(&outcome.hits, weights);
        let mut entries = Vec::with_capacity(ordered.len().min(limit));
        self.collect_exact_entries(&ordered, query, limit, &mut entries);
        Ok((entries, outcome.total))
    }

    fn run(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
        kind_filter: KindFilter,
        keep: impl Fn(Kind) -> bool,
        weights: &EffectiveWeights,
    ) -> Result<(Vec<Entry>, usize), String> {
        let mut spec = build_spec(query, filters, weights);
        spec.kind = kind_filter;
        let outcome = self.query_spec(&spec);
        let ordered = self.ordered_hits(&outcome.hits, weights);
        let mut entries = Vec::with_capacity(ordered.len().min(limit));
        self.collect_entries(&ordered, query, limit, keep, &mut entries);
        Ok((entries, outcome.total))
    }

    /// 多卷命中按盘符权重公平交织（返回按「该先看谁」排好的序列）。
    fn ordered_hits(
        &self,
        hits: &[lstr_index::Hit],
        weights: &EffectiveWeights,
    ) -> Vec<lstr_index::Hit> {
        order_hits(hits, |vol| {
            self.engine
                .volumes
                .get(vol as usize)
                .map(|volume| weights.weight(volume.letter))
                .unwrap_or(100)
        })
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
///
/// `weights` 只贡献两件事：权重 0 的盘并入卷掩码（引擎整卷跳过），以及
/// 调用方用同一份权重给候选做公平交织/排序。显式条件锁定的盘已在
/// `EffectiveWeights` 里还原成 100，所以 `path:C:\…` 在 C 权重 0 时仍可搜。
pub fn build_spec(query: &Query, filters: &SearchFilters, weights: &EffectiveWeights) -> QuerySpec {
    // 相对路径分段（`DeepSeek\xxx`）：不是正则时最后一段仍是名字锚点
    // （走 n-gram/memmem 候选），段链由引擎在候选封顶前逐级校验；
    // 尾部带分隔符（`DeepSeek\`）没有名字锚点，交给过滤器全量列举。
    let path_components: Vec<String> = query
        .path_components
        .as_ref()
        .map(|path| path.components.clone())
        .unwrap_or_default();
    let path_trailing = query
        .path_components
        .as_ref()
        .is_some_and(|path| path.trailing);
    let mut spec = QuerySpec {
        tokens: if filters.matches.regex {
            Vec::new()
        } else if !path_components.is_empty() {
            if path_trailing {
                Vec::new()
            } else {
                // 最后一段（名字锚点）参与 n-gram/memmem 候选生成。
                vec![query.path_anchor().unwrap_or_default().to_string()]
            }
        } else {
            query.text.split_whitespace().map(String::from).collect()
        },
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
        path_components,
        path_trailing,
        group_exts: Vec::new(),
        exact: None,
        mtime_after: None,
        mtime_before: None,
        size_after: None,
        size_before: None,
        // 盘符过滤（c:/d: 限定、-c: 排除）：交给引擎在卷循环直接跳过，
        // 省掉整卷扫描；0 掩码（全排除）→ 查询结果为空。
        // 输入框盘符 chip 与查询文本里的盘符 token 求交；权重 0 的盘再并进来。
        vol_mask: effective_vol_mask(query, filters, weights),
        case_sensitive: filters.matches.case_sensitive,
        whole_word: filters.matches.whole_word,
        regex: (filters.matches.regex && !query.text.is_empty()).then(|| query.text.clone()),
        prefix: query.prefix.clone(),
        suffix: query.suffix.clone(),
    };
    // 路径约束的优先级（三者是同一个语义，只取一个）：
    //   1. `query.path_filter`：浏览根 / Explorer 目录上下文 / 兼容文本 `path:`
    //   2. `filters.path`：输入框路径 chip
    //   3. `filters.location`：筛选条「位置」
    // chip 与位置在 UI 侧是「后写覆盖」，这里只需按序回退。
    if spec.path_prefix.is_none() {
        if let Some(path) = &filters.path {
            spec.path_prefix = Some(path.display().to_string().to_lowercase());
        }
    }
    if let Some(location) = &filters.location {
        let p = location.path().display().to_string().to_lowercase();
        if spec.path_prefix.is_none() {
            spec.path_prefix = Some(p);
        }
    }
    // 时间筛选:内置索引的 mtime 列(目录扫描写入)。
    // 预设「今天/7天/…」与输入框日期条件是交集：下界取更晚的，上界取更早的。
    if let Some(t) = filters.time {
        spec.mtime_after = Some(t.since_unix());
    }
    if let Some(range) = &filters.modified {
        if let Some(after) = range.after {
            spec.mtime_after = Some(spec.mtime_after.map_or(after, |old| old.max(after)));
        }
        if let Some(before) = range.before {
            spec.mtime_before = Some(spec.mtime_before.map_or(before, |old| old.min(before)));
        }
    }
    if let Some(range) = &filters.size {
        spec.size_after = range.after;
        spec.size_before = range.before;
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

/// 按卷权重公平交织命中序列。
///
/// 引擎把各卷命中按盘符顺序拼成一个数组，旧的 `collect_entries` 直接从头
/// 截断：只要字母序靠前的盘（C）命中数超过候选预算，其它盘的候选根本
/// 进不了排序池——这是「C 盘霸屏」的主因，与打分权重无关。
///
/// 每次取「已取条数 / 权重」最小的卷（整数交叉相乘比较，避免浮点误差），
/// 权重全 100 时退化为等额轮转；某卷耗尽后份额自动让给其它卷。
/// `Hit` 在引擎输出里同卷连续，按段切 lane 即可，无需重排。
fn order_hits(
    hits: &[lstr_index::Hit],
    weight_of: impl Fn(u32) -> u8,
) -> Vec<lstr_index::Hit> {
    struct Lane {
        start: usize,
        end: usize,
        weight: u64,
    }
    if hits.is_empty() {
        return Vec::new();
    }
    let mut lanes: Vec<Lane> = Vec::new();
    for (index, hit) in hits.iter().enumerate() {
        match lanes.last_mut() {
            Some(lane) if lane.end == index && hits[index - 1].vol == hit.vol => {
                lane.end = index + 1;
            }
            _ => {
                lanes.push(Lane {
                    start: index,
                    end: index + 1,
                    weight: weight_of(hit.vol) as u64,
                });
            }
        }
    }
    // 权重 0 的卷引擎已按掩码跳过；万一混进来也不占份额。
    lanes.retain(|lane| lane.weight > 0);
    let mut taken = vec![0usize; lanes.len()];
    let mut ordered = Vec::with_capacity(hits.len());
    loop {
        let mut best: Option<usize> = None;
        for (index, lane) in lanes.iter().enumerate() {
            if lane.start + taken[index] >= lane.end {
                continue;
            }
            match best {
                None => best = Some(index),
                Some(current) => {
                    if (taken[index] as u64) * lanes[current].weight
                        < (taken[current] as u64) * lane.weight
                    {
                        best = Some(index);
                    }
                }
            }
        }
        let Some(index) = best else { break };
        // Hit 是 Copy（16 字节）；顺序本身才是产出，拷贝成本可忽略。
        ordered.push(hits[lanes[index].start + taken[index]]);
        taken[index] += 1;
    }
    ordered
}

/// 卷掩码 = 显式盘符条件（`C:` / `-C:` / chip）∩ 权重非 0 的盘。
/// 显式条件把搜索限定到某盘时，权重 0 的盘本来就不在掩码里，两者互不干扰。
fn effective_vol_mask(
    query: &Query,
    filters: &SearchFilters,
    weights: &EffectiveWeights,
) -> Option<u32> {
    let base =
        crate::search::filters::intersect_drive_masks(query.drive_mask(), filters.drive_mask());
    let zero = weights.zero_mask();
    if zero == 0 {
        return base;
    }
    const ALL: u32 = (1u32 << 26) - 1;
    Some(base.unwrap_or(ALL) & !zero)
}

/// worker 收到 Shutdown 后的退出等待上限:worker 要 join 各卷线程(对账批次
/// 间检查 stop)、写 clean-exit 标记后才释放管道;超时即放弃 —— 协议容忍
/// 半开,残留 worker 由自身的 idle 超时与 clean-exit 标记/P1-5 逐卷窗口校验兜底。
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
            // P0-3：以启动时的配置初始化「退出保留索引服务」；设置页改动
            // 通过 set_keep_worker_alive 即时更新，无需重启。
            KEEP_WORKER_ALIVE.store(index_cfg.keep_worker_alive, Ordering::SeqCst);
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
                            // 常驻模式下只 Detach：worker 保持索引热态，下次
                            // 启动免 UAC、免重建；托盘「退出并停止」才真退出。
                            farewell(client);
                            idx_win::close(client);
                        }
                    }
                    return;
                }
                // P0-2：用户在 Fallback 横幅里点了「重新授权」——重置拒绝
                // 状态并给足 UAC 时间（首连 60s），下一次迭代重新 runas 拉起。
                if RELAUNCH_REQUESTED.swap(false, Ordering::SeqCst) {
                    launch_declined = false;
                    first_connect = true;
                    last_launch = std::time::Instant::now() - Duration::from_secs(60);
                }
                // 先拉起 worker 再连接(UAC 一次/会话);已提权常驻时秒连。
                // 用户拒绝过 UAC 就不再弹(防骚扰),保持 Fallback 等下次启动。
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
                    // P0-2：用户点了「重新授权」时别等满 20s 退避，立刻回到
                    // 循环重拉（否则按钮点下去最多要等 20s 才弹 UAC）。
                    if RELAUNCH_REQUESTED.load(Ordering::Relaxed) {
                        break;
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
            // M4 + 退出收 worker:主进程要退出时显式发 Shutdown/Detach 并
            // 视模式等待 worker 退出(有超时)。旧实现只写一帧就断开,进程随即
            // 退出 —— 帧能不能到达全看线程调度,实测多次留下提权 worker 占住
            // 唯一管道实例,下次启动撞上旧 worker 出现怪状态。
            farewell(client);
            break 'frames;
        }
        // 设置页指令(切卷/重建卷):非阻塞写帧,不打断下面的读循环。
        loop {
            match index_cmd_rx.try_recv() {
                Ok(cmd) => {
                    let frame = idx_client::command_frame(&cmd);
                    lstr_index::snapshot::debug_log(&format!(
                        "client: sending command {cmd:?}"
                    ));
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
                        // 引擎线程已经收工(服务被 drop):顺手通知 worker 收尾
                        // (常驻模式 Detach 保留热态,与退出路径同款语义)。
                        farewell(client);
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
        F::WorkerExit { .. } | F::Shutdown | F::Detach => return None,
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
    // 实时层选择：incremental="usn"（默认）走 dir 首扫 + journal 轮询；
    // "watch" 只目录监视、零 USN FSCTL。卷打不开时 worker 会按卷退回
    // dir + watcher，所以 USN 不是单点故障。
    if index.incremental == "usn" {
        // 固定 dir 首扫（与设置页文案一致），这是唯一会下发 --index-journal
        // 的自动路径。
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
    if index.keep_worker_alive {
        extra.push_str(" --index-keep-alive");
    }
    extra
}

/// ShellExecute runas 拉起提权 worker(UAC 一次/会话)。
/// 令牌经命令行传给 worker:固定管道名 + 只比 exe 名的校验挡不住同用户
/// 其它进程,worker 会用 Hello 里的令牌二次校验(L5)。
/// P1-1:令牌按用户持久化(`worker_token_hex`),否则常驻 worker 退出后
/// 新进程的随机令牌会让重开握手失败(白等 60s + 再次 UAC)。
pub fn launch_worker(extra_args: &str) -> bool {
    let Some(exe) = std::env::current_exe().ok() else { return false };
    let exe_str = exe.display().to_string();
    let params = format!(
        "--index-worker --index-token {}{}",
        idx_client::worker_token_hex(),
        extra_args
    );
    // 复用 lstr 的 shell_execute_with 语义:runas + 参数
    crate::win::shell::shell_execute_runas_with_args(&exe_str, &params).is_ok()
}

/// 引擎构造(主进程内,加载快照)。`noise` = 排除规则共享槽(重建时换表)。
/// 测试进程(cfg(test) 或 LSTR_TEST_INDEX_DIR)用隔离目录:测试无 worker,
/// 引擎的周期性保存会把「空 store + hash0」覆写到真实快照上(真实踩坑)。
pub fn make_engine(
    cfg: &crate::config::IndexConfig,
    noise: lstr_index::engine::NoiseRules,
) -> Engine {
    let index_dir = if cfg!(test) {
        std::env::temp_dir().join(format!("lstr-test-index-{}", std::process::id()))
    } else if let Some(dir) = std::env::var_os("LSTR_TEST_INDEX_DIR") {
        std::path::PathBuf::from(dir)
    } else {
        crate::config::attach_dir().join("index")
    };
    Engine::new(EngineConfig {
        index_dir,
        noise,
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
    use std::path::PathBuf;
    use std::time::Instant;

    /// 测试基线：全盘中性（权重不参与），与旧行为逐条等价。
    fn neutral_weights() -> EffectiveWeights {
        EffectiveWeights::neutral()
    }

    fn fake_hits(vol: u32, count: usize) -> Vec<lstr_index::Hit> {
        (0..count as u32)
            .map(|slot| lstr_index::Hit {
                vol,
                slot,
                generation: 1,
                mtime: 0,
            })
            .collect()
    }

    #[test]
    fn weighted_interleave_respects_drive_weights() {
        let mut hits = fake_hits(0, 200);
        hits.extend(fake_hits(1, 200));
        hits.extend(fake_hits(2, 200));
        // 卷 0（C）= 50，其余 100。
        let ordered = order_hits(&hits, |vol| if vol == 0 { 50 } else { 100 });
        let window: Vec<u32> = ordered.iter().take(120).map(|hit| hit.vol).collect();
        let count = |vol: u32| window.iter().filter(|v| **v == vol).count();
        let (c, d, e) = (count(0), count(1), count(2));
        assert!(c < d && c < e, "低权重盘份额应更小: C={c} D={d} E={e}");
        assert!((d as i32 - e as i32).abs() <= 1, "同权重盘应等额: {d}/{e}");
        // 50:100 ≈ 1:2（交错误差 ±1）。
        let expect_c = (d as i32) / 2;
        assert!(
            (c as i32 - expect_c).abs() <= 2,
            "C 份额应约为 D 的一半: C={c} D={d}"
        );
    }

    #[test]
    fn equal_weights_round_robin_evenly() {
        // 全 100 时退化为等额轮转：旧实现是「C 从头上吃光预算」。
        let mut hits = fake_hits(0, 40);
        hits.extend(fake_hits(1, 40));
        let ordered = order_hits(&hits, |_| 100);
        let first: Vec<u32> = ordered.iter().take(4).map(|hit| hit.vol).collect();
        assert_eq!(first, vec![0, 1, 0, 1]);
        let c = ordered.iter().take(40).filter(|hit| hit.vol == 0).count();
        assert_eq!(c, 20, "前 40 个候选应两盘各半");
    }

    #[test]
    fn exhausted_drive_spills_share_to_others() {
        let mut hits = fake_hits(0, 5);
        hits.extend(fake_hits(1, 50));
        let ordered = order_hits(&hits, |vol| if vol == 0 { 50 } else { 100 });
        assert_eq!(ordered.len(), 55, "所有命中都必须保留");
        assert_eq!(ordered.iter().filter(|hit| hit.vol == 1).count(), 50);
    }

    #[test]
    fn zero_weight_drive_is_skipped_by_interleave() {
        let mut hits = fake_hits(0, 10);
        hits.extend(fake_hits(1, 10));
        let ordered = order_hits(&hits, |vol| if vol == 0 { 0 } else { 100 });
        assert!(ordered.iter().all(|hit| hit.vol == 1));
    }

    /// 真实 Engine 双卷夹具：假 C 卷（100 条候选，不落盘）+ 真实夹具卷。
    /// 验证权重 0 是引擎级硬排除，而 `path:C:\fake` 能把它恢复回来。
    #[test]
    fn zero_drive_weight_excludes_volume_and_path_condition_restores_it() {
        let Some(mut fx) = real_index(20, 2, "drive-weight") else { return };
        let fx_letter = fx
            .test_dir
            .to_string_lossy()
            .chars()
            .next()
            .expect("夹具盘符");
        let fake_letter = if fx_letter.eq_ignore_ascii_case(&'C') { 'D' } else { 'C' };
        let guid = 0x7777u128;
        fx.provider.engine.handle(EngineMsg::VolumeMeta {
            guid,
            letter: fake_letter,
            journal_id: 1,
            next_usn: 100,
            est_records: 0,
        });
        fx.provider.engine.handle(EngineMsg::ScanBegin { guid, resume_frn: 0 });
        let mut recs = vec![Rec {
            frn: 1,
            parent_frn: 0,
            attrs: 0x10,
            op: Op::Upsert,
            name: "fake".to_string(),
            mtime: 0,
            size: 0,
        }];
        for index in 0..100u64 {
            recs.push(Rec {
                frn: 1000 + index,
                parent_frn: 1,
                attrs: 0,
                op: Op::Upsert,
                name: format!("ai_fake_{index:03}.txt"),
                mtime: 0,
                size: 0,
            });
        }
        fx.provider.engine.handle(EngineMsg::Records {
            guid,
            recs,
            next_usn: 100,
        });
        fx.provider.engine.handle(EngineMsg::VolumeReady {
            guid,
            journal_id: 1,
            next_usn: 100,
            count: 101,
        });

        let query = parse("ai", Scope::All);
        let mut cfg = crate::config::RankConfig::default();
        cfg.drive_weights.clear();
        cfg.set_drive_weight(fake_letter, 0);
        let filters = SearchFilters::default();
        let weights = EffectiveWeights::new(&cfg, &query, &filters, None, None);
        let result = fx
            .provider
            .search_split_once(&query, &filters, 40, 60, &weights)
            .expect("split");
        let candidates = |result: &SplitOnceResult| -> Vec<String> {
            result
                .files
                .iter()
                .chain(result.folders.iter())
                .map(|entry| entry.full_path())
                .collect()
        };
        let paths_found = candidates(&result);
        assert!(
            paths_found
                .iter()
                .all(|path| !path.starts_with(&format!("{fake_letter}:"))),
            "权重 0 的盘不应产生候选: {paths_found:?}"
        );
        assert!(result.all_total > 0, "真实夹具卷仍应命中");
        assert!(
            paths_found
                .iter()
                .all(|path| path.starts_with(&format!("{fx_letter}:"))),
            "剩余候选都应来自真实夹具卷"
        );

        // path:C:\fake 显式锁定该盘：权重 0 被覆盖，候选取自假卷。
        let mut filters = SearchFilters::default();
        filters.path = Some(PathBuf::from(format!("{fake_letter}:\\fake")));
        let weights = EffectiveWeights::new(&cfg, &query, &filters, None, None);
        assert_eq!(weights.weight(fake_letter), 100, "path: 条件锁定该盘");
        let result = fx
            .provider
            .search_split_once(&query, &filters, 40, 60, &weights)
            .expect("split");
        assert!(
            result.all_total >= 100,
            "假卷 100 条候选都应计入总数: {}",
            result.all_total
        );
        let paths_found = candidates(&result);
        assert!(
            paths_found
                .iter()
                .all(|path| path.starts_with(&format!("{fake_letter}:\\fake"))),
            "path: 锁定后候选应来自假卷: {paths_found:?}"
        );
    }

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

    fn disk_size(path: &std::path::Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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
        for name in &dir_names {
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
        noise: Default::default(),
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
                size: 0,
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
            size: 0,
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
                size: if *is_dir {
                    0
                } else {
                    disk_size(&disk_parent.join(name))
                },
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
            size: 0,
        });
        recs.push(Rec {
            frn: 6001,
            parent_frn: 6000,
            attrs: 0,
            op: Op::Upsert,
            name: "ai_denied_file.txt".to_string(),
            mtime: 0,
            size: u64::MAX,
        });
        recs.push(Rec {
            frn: 5000,
            parent_frn: test_dir_frn,
            attrs: 0,
            op: Op::Upsert,
            name: missing.to_string(),
            mtime: 0,
            size: u64::MAX,
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
            let legacy_folders = fx.provider.search_split(&query, &filters, folder_limit, true, &neutral_weights()).unwrap();
            let legacy_files = fx.provider.search_split(&query, &filters, file_limit, false, &neutral_weights()).unwrap();
            let result = fx
                .provider
                .search_split_once(&query, &filters, folder_limit, file_limit, &neutral_weights())
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
        let mut spec = build_spec(&query, &filters, &neutral_weights());
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
        let mut spec = build_spec(&query, &filters, &neutral_weights());
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
            let mut spec = build_spec(&query, &filters, &neutral_weights());
            spec.kind = KindFilter::All;
            // 与 search_split_once 的 exact_needle 同口径：空文本（纯过滤器 /
            // 尾分隔符路径）不再走「全量列举」的 exact 补集。
            let needle =
                (filters.time.is_none() && !query.text.is_empty()).then(|| query.text.to_lowercase());
            let outcome = fx
                .provider
                .engine
                .query_split_with_exact(&spec, needle.as_deref());
            let result = fx
                .provider
                .search_split_once(&query, &filters, folder_limit, file_limit, &neutral_weights())
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

    /// 真实索引上的相对路径分段查询：`前缀\目录` 命中该目录，`前缀\目录\`
    /// 列出它下面的所有后代；名字同名但父目录不同的条目不能被带进来。
    #[test]
    fn path_components_query_on_real_index() {
        let Some(fx) = real_index(60, 4, "path-parts") else { return };
        let tag = format!("lstr-prov-path-parts-{}", std::process::id());
        let filters = SearchFilters::default();

        // `tag\ai_dir_0001`：父目录是 tag、名字是 ai_dir_0001 的文件夹。
        let query = parse(&format!(r"{tag}\ai_dir_0001"), Scope::All);
        assert!(query.path_components.is_some());
        let result = fx
            .provider
            .search_split_once(&query, &filters, 30, 60, &neutral_weights())
            .unwrap();
        let names: Vec<&str> = result.folders.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"ai_dir_0001"), "{names:?}");
        assert!(
            result
                .folders
                .iter()
                .all(|e| e.full_path().to_lowercase().contains(&tag.to_lowercase())),
            "父目录必须匹配"
        );

        // 大小写不敏感（ASCII）。
        let upper = parse(&format!(r"{tag}\AI_DIR_0001"), Scope::All);
        let result = fx
            .provider
            .search_split_once(&upper, &filters, 30, 60, &neutral_weights())
            .unwrap();
        assert!(result.folders.iter().any(|e| e.name == "ai_dir_0001"));

        // `tag\ai_dir_0001\`：该目录下所有后代（文件），目录自身不算。
        let query = parse(&format!(r"{tag}\ai_dir_0001\"), Scope::All);
        assert!(query.path_components.as_ref().is_some_and(|p| p.trailing));
        let result = fx
            .provider
            .search_split_once(&query, &filters, 30, 60, &neutral_weights())
            .unwrap();
        assert!(!result.files.is_empty(), "应列出目录内文件");
        assert!(
            result
                .files
                .iter()
                .all(|e| e.full_path().to_lowercase().contains("ai_dir_0001")),
            "只应包含该目录下的条目"
        );
        assert!(
            !result.folders.iter().any(|e| e.name == "ai_dir_0001"),
            "尾分隔符查询不把目录自身列出来"
        );

        // 不存在的父目录链：0 结果（不能退化成名字搜索把别的 ai_dir 带出来）。
        let missing = parse(&format!(r"{tag}\no_such_dir\ai_dir_0001"), Scope::All);
        let result = fx
            .provider
            .search_split_once(&missing, &filters, 30, 60, &neutral_weights())
            .unwrap();
        assert!(result.folders.is_empty() && result.files.is_empty());
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
        let mut spec = build_spec(&query, &filters, &neutral_weights());
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
                let noise = lstr_index::engine::NoiseRules::from_static(Vec::new());
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            deliver: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            hidden: Vec::new(),
            show_hidden: false,
            base_folder: None,
            browse_root: None,
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
            &noise,
            Some(&pending),
            &neutral_weights(),
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
            .search_split_per_entry(&query, &filters, folder_fetch, true, &neutral_weights());
        let (mut old_files, all_total) = fx
            .provider
            .search_split_per_entry(&query, &filters, file_fetch, false, &neutral_weights());
        let file_total = all_total.saturating_sub(folder_total);
        let truncation = folder_total.saturating_sub(old_folders.len())
            + file_total.saturating_sub(old_files.len());
        old_folders.append(&mut old_files);
        let mut old_entries = old_folders;
        if truncation > 0 {
            let mut spec = build_spec(&query, &filters, &neutral_weights());
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
            .search_split_once(&query, &filters, folder_fetch, file_fetch, &neutral_weights())
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
                let noise = lstr_index::engine::NoiseRules::from_static(Vec::new());
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            deliver: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            hidden: Vec::new(),
            show_hidden: false,
            base_folder: None,
            browse_root: None,
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
            &noise,
            Some(&pending),
            &neutral_weights(),
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
            .search_split_per_entry(&query, &filters, folder_fetch, true, &neutral_weights());
        let (mut old_files, all_total) = fx
            .provider
            .search_split_per_entry(&query, &filters, file_fetch, false, &neutral_weights());
        let file_total = all_total.saturating_sub(folder_total);
        let truncation = folder_total.saturating_sub(old_folders.len())
            + file_total.saturating_sub(old_files.len());
        old_folders.append(&mut old_files);
        let mut old_entries = old_folders;
        if truncation > 0 {
            let mut spec = build_spec(&query, &filters, &neutral_weights());
            spec.kind = KindFilter::All;
            let outcome = fx.provider.engine.query_split_with_exact(&spec, Some("ai"));
            let mut exact = Vec::new();
            fx.provider
                .fill_exact_entries_per_entry(&outcome.exact_hits, &query, 50, &mut exact);
            old_entries.append(&mut exact);
        }

        let result = fx
            .provider
            .search_split_once(&query, &filters, folder_fetch, file_fetch, &neutral_weights())
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
                let noise = lstr_index::engine::NoiseRules::from_static(Vec::new());
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            deliver: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            hidden: Vec::new(),
            show_hidden: false,
            base_folder: None,
            browse_root: None,
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
            &noise,
            Some(&pending),
            &neutral_weights(),
        );
        let deleted_str = deleted.to_string_lossy().to_lowercase();
        assert!(
            new_final
                .iter()
                .all(|e| e.full_path().to_lowercase() != deleted_str),
            "已删条目不得出现在结果里"
        );
        // 同名不再设上限后,两条路径的差异只剩「已删候选占用一个已收集名额」:
        // 旧路径候选阶段逐条填元数据、丢弃已删项后还能再补一条;新路径在
        // finalize 才发现已删,列表长度可能少一条。候选池有冗余(deliver 前
        // fetch 了更多),这一条不应传导成用户可见的缺行。
        assert_eq!(
            old_final.len(),
            new_final.len(),
            "已删候选不得让结果列表少一条"
        );
        // mtime 修复后不能再断言 full Entry 相等:本测试删除文件会更新
        // 父目录的 FS mtime,旧路径在候选阶段取元数据、新路径在 finalize
        // 取元数据,两次 FS 快照可能相差 1 秒(索引 mtime 也滞后于删除)。
        // 顺序语义由 rank 单测覆盖。
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
                let noise = lstr_index::engine::NoiseRules::from_static(Vec::new());
        let query = parse(&name, Scope::All);
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            deliver: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            hidden: Vec::new(),
            show_hidden: false,
            base_folder: None,
            browse_root: None,
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
            &noise,
            Some(&pending),
            &neutral_weights(),
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
        let mut spec = build_spec(&query, &filters, &neutral_weights());
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
        let mut spec = build_spec(&query, &filters, &neutral_weights());
        spec.kind = KindFilter::All;
        let outcome = fx
            .provider
            .engine
            .query_split_with_exact(&spec, Some("ai"));

        let history = std::sync::Arc::new(std::sync::Mutex::new(History::in_memory()));
        let config = crate::config::Config::default();
                let noise = lstr_index::engine::NoiseRules::from_static(Vec::new());
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: query.clone(),
            limit: 100,
            deliver: 100,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: crate::config::RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            hidden: Vec::new(),
            show_hidden: false,
            base_folder: None,
            browse_root: None,
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
                fx.provider.search_split_once(&query, &filters, 120, 480, &neutral_weights()).unwrap();
            }),
            1,
            7,
        );
        // 新路径完整链路:候选零 FS → finalize 内只对最终候选 find_meta。
        let one = fx
            .provider
            .search_split_once(&query, &filters, 120, 480, &neutral_weights())
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
                    &noise,
                    Some(&pending),
                    &neutral_weights(),
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
            &noise,
            Some(&pending),
            &neutral_weights(),
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
        let spec = build_spec(&parse("ext:RS", Scope::All), &filters, &neutral_weights());
        assert_eq!(spec.ext.as_deref(), Some("rs"));
        let spec = build_spec(&parse("ext:.arj", Scope::All), &filters, &neutral_weights());
        assert_eq!(spec.ext.as_deref(), Some("arj"));
    }

    #[test]
    fn path_chip_feeds_path_prefix_with_documented_precedence() {
        // 1) 路径 chip → QuerySpec.path_prefix（小写，与引擎 PathMatcher 对齐）。
        let mut filters = SearchFilters {
            path: Some(PathBuf::from(r"D:\Work")),
            ..Default::default()
        };
        let spec = build_spec(&parse("", Scope::All), &filters, &neutral_weights());
        assert_eq!(spec.path_prefix.as_deref(), Some(r"d:\work"));

        // 2) 手输/浏览根的 query.path_filter 优先于 chip。
        let spec = build_spec(&parse(r"path:E:\foo", Scope::All), &filters, &neutral_weights());
        assert_eq!(spec.path_prefix.as_deref(), Some(r"e:\foo"));

        // 3) chip 优先于筛选条「位置」。
        filters.location = Some(crate::search::filters::LocationFilter::Drive('C'));
        let spec = build_spec(&parse("", Scope::All), &filters, &neutral_weights());
        assert_eq!(spec.path_prefix.as_deref(), Some(r"d:\work"));

        // 4) 没有 chip 时回退到位置。
        filters.path = None;
        let spec = build_spec(&parse("", Scope::All), &filters, &neutral_weights());
        assert_eq!(spec.path_prefix.as_deref(), Some(r"c:\"));
    }

    #[test]
    fn regex_spec_keeps_pattern_and_drops_tokens() {
        let mut filters = SearchFilters::default();
        filters.matches.regex = true;
        let spec = build_spec(
            &crate::search::query::parse_regex(r"ext:rs ^api-.*\.dll$", Scope::All),
            &filters,
            &neutral_weights(),
        );
        assert_eq!(spec.regex.as_deref(), Some(r"ext:rs ^api-.*\.dll$"));
        assert!(spec.tokens.is_empty(), "正则模式不能再按空白拆 token");
        assert_eq!(spec.ext, None, "正则里的 ext: 不能被当成筛选语法");
        assert!(spec.exact.is_none());
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
    fn relaunch_request_is_consumed_once() {
        // P0-2：「重新授权」只触发一次拉起：client 线程 swap 消费后标志归零，
        // 不会因为下一轮循环又弹一次 UAC。
        RELAUNCH_REQUESTED.store(false, Ordering::SeqCst);
        assert!(!RELAUNCH_REQUESTED.swap(false, Ordering::SeqCst));
        request_worker_relaunch();
        assert!(RELAUNCH_REQUESTED.swap(false, Ordering::SeqCst));
        assert!(
            !RELAUNCH_REQUESTED.swap(false, Ordering::SeqCst),
            "消费后必须归零"
        );
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

    /// incremental 决定内置索引的实时层:usn(默认)→ dir 首扫 + journal +
    /// poll-ms;watch → 零 USN 开关。
    #[test]
    fn incremental_layer_selects_journal_or_watch() {
        // 默认配置:USN 增量(目录首扫 + journal 轮询)
        let cfg = crate::config::IndexConfig::default();
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-scan-mode dir --index-journal"), "args={args}");
        assert!(args.contains(" --index-poll-ms 500"), "args={args}");
        assert!(!args.contains("--index-throttle-ms"), "args={args}");

        // watch:纯目录监视,零 USN 开关
        let cfg = crate::config::IndexConfig {
            incremental: "watch".into(),
            ..Default::default()
        };
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(!args.contains("--index-journal"), "args={args}");
        assert!(!args.contains("--index-poll-ms"), "args={args}");
        assert!(!args.contains("--index-scan-mode usn"), "args={args}");

        // poll-ms 下限在参数侧同样兜底
        let cfg = crate::config::IndexConfig {
            journal_poll_ms: 10,
            ..Default::default()
        };
        let args = worker_launch_args(&cfg, lstr_index::volmask::MASK_ALL);
        assert!(args.contains(" --index-poll-ms 100"), "args={args}");

        // 旧 scan_mode=usn 首扫模式(手动配置):保持既有行为
        let mut cfg = crate::config::IndexConfig {
            incremental: "watch".into(),
            scan_mode: "usn".into(),
            ..Default::default()
        };
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