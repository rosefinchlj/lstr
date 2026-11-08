pub mod apps;
pub mod browse;
pub mod everything;
pub mod filters;
pub mod fuzzy;
pub mod pinyin;
pub mod query;
pub mod rank;
pub mod uwp;

use crate::config::{Config, KeywordConfig, RankConfig};
use crate::history::History;
use crate::model::{Entry, Kind, Query, Scope};
use crate::search::filters::SearchFilters;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use eframe::egui;
use std::time::{Duration, Instant};

/// 应用索引的刷新周期：新装/卸载应用后最多等这么久就能搜到/消失。
/// 重建成本是枚举开始菜单 + App Paths（本机 200+ 应用，几十 ms），放 worker 线程无感。
const APPS_RELOAD_INTERVAL: Duration = Duration::from_secs(90);

struct Request {
    generation: u64,
    query: Query,
    limit: usize,
    keywords: Vec<KeywordConfig>,
    /// 对话框模式（Quick Switch）：空查询时把 Explorer 已打开的文件夹置顶。
    dialog_quick: bool,
    /// 筛选条选择：位置 / 时间 / 类型。由 UI 按钮生成，用户不输入语法。
    filters: SearchFilters,
    /// 排序权重（设置面板可调，实时生效）。
    rank: RankConfig,
    /// 空查询是否显示「最近使用」历史（输入框右侧的历史开关）。
    show_history: bool,
    /// P0-1：收藏列表（Quick Switch 数据源）。
    favorites: Vec<crate::assets::Favorite>,
    /// P0-2：「Explorer 内键入即搜」锁定的当前目录（None = 全局搜索）。
    base_folder: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub generation: u64,
    pub entries: Vec<Entry>,
    pub total: usize,
    pub elapsed_ms: f32,
    pub error: Option<String>,
}

pub struct SearchService {
    tx: Sender<Request>,
    rx: Receiver<Response>,
    generation: u64,
    history: Arc<Mutex<History>>,
    limit: usize,
    keywords: Vec<KeywordConfig>,
    /// P0-1：收藏列表，新增收藏后由 UI 同步进来。
    favorites: Vec<crate::assets::Favorite>,
    /// 「重新加载」命令置位，worker 下一请求立即重建应用索引。
    apps_dirty: Arc<AtomicBool>,
}

impl SearchService {
    pub fn new(config: Config, history: Arc<Mutex<History>>, ctx: egui::Context) -> Self {
        let (tx, request_rx) = mpsc::channel::<Request>();
        let (response_tx, rx) = mpsc::channel::<Response>();
        let worker_history = Arc::clone(&history);
        let worker_config = config.clone();
        let apps_dirty = Arc::new(AtomicBool::new(false));
        let worker_apps_dirty = Arc::clone(&apps_dirty);
        std::thread::Builder::new()
            .name("lstr-search".to_string())
            .spawn(move || {
                run_worker(
                    request_rx,
                    response_tx,
                    worker_history,
                    worker_config,
                    ctx,
                    worker_apps_dirty,
                )
            })
            .expect("spawn search worker");
        Self {
            tx,
            rx,
            generation: 0,
            history,
            limit: config.max_results,
            keywords: config.keywords,
            favorites: config.favorites,
            apps_dirty,
        }
    }

    pub fn search(
        &mut self,
        raw: &str,
        scope: Scope,
        dialog_quick: bool,
        filters: &SearchFilters,
        rank: &RankConfig,
        show_history: bool,
    ) -> u64 {
        self.search_in(raw, scope, dialog_quick, filters, rank, show_history, None)
    }

    /// P0-2：带目录上下文的搜索。base_folder 由「Explorer 内键入即搜」提供，
    /// worker 把它作为隐含的 path: 过滤；用户显式输入路径模式时自动忽略。
    pub fn search_in(
        &mut self,
        raw: &str,
        scope: Scope,
        dialog_quick: bool,
        filters: &SearchFilters,
        rank: &RankConfig,
        show_history: bool,
        base_folder: Option<PathBuf>,
    ) -> u64 {
        self.generation += 1;
        let query = query::parse(raw, scope);
        let _ = self.tx.send(Request {
            generation: self.generation,
            query,
            limit: self.limit,
            keywords: self.keywords.clone(),
            dialog_quick,
            filters: filters.clone(),
            rank: rank.clone(),
            show_history,
            favorites: self.favorites.clone(),
            base_folder,
        });
        self.generation
    }

    /// 设置里的「最大结果数」改动后立即生效，无需重启。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.clamp(1, 50);
    }

    /// 「重新加载」命令后同步最新关键字，不必重启进程。
    pub fn set_keywords(&mut self, keywords: Vec<KeywordConfig>) {
        self.keywords = keywords;
    }

    /// P0-1：收藏变更后同步给 worker，Quick Switch 立即生效，无需重载配置。
    pub fn set_favorites(&mut self, favorites: Vec<crate::assets::Favorite>) {
        self.favorites = favorites;
    }

    /// 应用索引立刻重建：新装/卸载应用后不想等 TTL 就执行「重新加载」命令。
    pub fn invalidate_apps(&self) {
        self.apps_dirty.store(true, Ordering::Relaxed);
    }

    pub fn poll(&self) -> Option<Response> {
        let mut latest = None;
        while let Ok(response) = self.rx.try_recv() {
            latest = Some(response);
        }
        latest
    }

    pub fn record_open(&self, entry: &Entry) {
        if let Ok(mut history) = self.history.lock() {
            // SQLite UPSERT 立即提交，不再需要全量写盘。
            history.record(entry);
        }
    }
}

/// 收尾管线：排序 → 去重 → 截断 → 置顶（关键字 / Quick Switch）。
/// 抽成函数是为了「两阶段返回」：先发不含精确补齐的一版，再补发最终版。
fn finalize_entries(
    mut entries: Vec<Entry>,
    request: &Request,
    history: &Arc<Mutex<History>>,
    config: &Config,
    keyword_rows: Vec<Entry>,
) -> Vec<Entry> {
    if let Ok(history) = history.lock() {
        rank::rank_with(
            &mut entries,
            &request.query.text,
            &history,
            config.app_boost,
            &request.rank,
        );
    }

    // 两遍去重：借用不跨 retain，省去每条 id/name 的 clone 分配。
    let keep_ids: Vec<bool> = {
        let mut seen_ids = HashSet::new();
        entries
            .iter()
            .map(|entry| seen_ids.insert(entry.id.as_str()))
            .collect()
    };
    let mut keep_iter = keep_ids.into_iter();
    entries.retain(|_| keep_iter.next().unwrap_or(false));
    // 同名条目（不同路径下的 clip 文件夹、debug/release 同名 exe）只保留排名最高的一个。
    // 例外：精确同名（用户就是打的名字，如 use → E:\use）保留前 6 个，
    // 多个同名目录是正常需求，全砍掉会让 E:\use 被系统深处同名目录挤掉。
    let needle_lower = request.query.text.to_lowercase();
    let keep_names: Vec<bool> = {
        let mut seen_names = HashSet::new();
        let mut exact_counts: std::collections::HashMap<(Kind, String), usize> =
            std::collections::HashMap::new();
        entries
            .iter()
            .map(|entry| {
                let key = (entry.kind, entry.name.to_lowercase());
                if !needle_lower.is_empty() && key.1 == needle_lower {
                    let count = exact_counts.entry(key).or_insert(0);
                    *count += 1;
                    *count <= 6
                } else {
                    seen_names.insert(key)
                }
            })
            .collect()
    };
    let mut keep_iter = keep_names.into_iter();
    entries.retain(|_| keep_iter.next().unwrap_or(false));
    // 列表可滚动：多送一些候选（最多 100 条），UI 只按窗口高度展示前几条，
    // 滚轮可以继续往下看——不再一屏截断。
    let deliver = request.limit.max(100);
    entries.truncate(deliver);

    // 关键字行钉在顶部：它们的名字（「用 Google 搜索：rust」）不含查询词，
    // 走打分会沉底，这里显式置顶，与 Listary 的 web 搜索行为一致。
    if !keyword_rows.is_empty() {
        entries.truncate(deliver.saturating_sub(keyword_rows.len()));
        let mut pinned = keyword_rows;
        pinned.append(&mut entries);
        entries = pinned;
    }

    // 对话框模式空查询：已打开的 Explorer 文件夹置顶（Quick Switch）。
    // 同样不参与打分——历史 frecency 会把它们压下去。
    if request.query.is_empty() && request.dialog_quick && request.query.scope == Scope::Folder {
        // P0-3：活动目录 → 收藏 → 历史文件夹 → 已打开 Explorer 目录。
        // 由 assets 统一生成并去重，取代原来「只列已打开窗口」的单一来源。
        let mut quick: Vec<Entry> = if let Ok(history) = history.lock() {
            crate::assets::quick_switch_entries(
                &request.favorites,
                &history,
                crate::win::explorer::open_explorer_folders(),
                crate::win::explorer::active_explorer_folder(),
            )
        } else {
            Vec::new()
        };
        // 和历史结果、quick 内部都按路径去重，避免同一目录出现两行。
        quick.retain(|q| {
            !entries
                .iter()
                .any(|e| e.full_path().eq_ignore_ascii_case(&q.full_path()))
        });
        quick.dedup_by(|a, b| a.full_path().eq_ignore_ascii_case(&b.full_path()));
        if !quick.is_empty() {
            entries.truncate(deliver.saturating_sub(quick.len()));
            let mut pinned = quick;
            pinned.append(&mut entries);
            entries = pinned;
        }
    }
    entries
}

fn run_worker(
    rx: Receiver<Request>,
    tx: Sender<Response>,
    history: Arc<Mutex<History>>,
    config: Config,
    ctx: egui::Context,
    apps_dirty: Arc<AtomicBool>,
) {
    let mut everything = match everything::EverythingProvider::new() {
        Ok(provider) => Some(provider),
        Err(err) => {
            eprintln!("lstr: Everything 不可用: {err}");
            None
        }
    };
    let mut apps = apps::AppIndex::load();
    let mut apps_loaded = Instant::now();
    let mut dir_cache = browse::DirCache::new();
    let (uwp_tx, uwp_rx) = mpsc::channel::<Vec<Entry>>();
    // UWP 枚举要走 PowerShell（Get-StartApps，冷启动 ~2s）。
    // 懒加载：只在第一次真正需要应用结果时才启动 PowerShell；
    // 只用文件/文件夹搜索的用户，PowerShell 永远不会被拉起。
    let mut uwp_started = false;

    while let Ok(mut request) = rx.recv() {
        // 用户可能刚点了提示条启动 Everything；每次查询前尝试重连，
        // 而不是启动时连不上就永久报错。
        if everything.is_none() {
            everything = everything::EverythingProvider::new().ok();
        }
        while let Ok(uwp_entries) = uwp_rx.try_recv() {
            apps.append_uwp(uwp_entries);
        }
        // 应用索引时效：TTL 到点或「重新加载」信号到了就重建。
        // 重载后 UWP 懒加载标志复位，下次需要应用结果时重新枚举。
        if apps_dirty.swap(false, Ordering::Relaxed)
            || apps_loaded.elapsed() > APPS_RELOAD_INTERVAL
        {
            apps = apps::AppIndex::load();
            uwp_started = false;
            apps_loaded = Instant::now();
        }
        while let Ok(next) = rx.try_recv() {
            request = next;
        }

        // P0-2：目录上下文。用户显式输入路径模式时退出，手输 path: 时以用户为准，
        // 避免把「在当前目录里搜」悄悄套到用户指定的另一个目录上。
        let in_folder_context =
            request.base_folder.is_some() && browse::parse(&request.query.raw).is_none();
        if in_folder_context && request.query.path_filter.is_none() {
            if let Some(folder) = &request.base_folder {
                let dir = folder.to_string_lossy().into_owned();
                if !dir.is_empty() {
                    request.query.path_filter = Some(dir);
                }
            }
        }

        // 路径模式（D:\foo\bar、\\server\share）：直接列目录，
        // 不碰 Everything，也不必为它懒启动 PowerShell 枚举 UWP。
        if let Some(path_query) = browse::parse(&request.query.raw) {
            let started = Instant::now();
            let filter = path_query.filter.to_lowercase();
            let mut entries: Vec<Entry> = dir_cache
                .children(&path_query.dir)
                .iter()
                .filter(|entry| {
                    filter.is_empty()
                        || entry.name.to_lowercase().contains(&filter)
                        || entry
                            .pinyin
                            .as_deref()
                            .map(|pinyin| pinyin.to_lowercase().contains(&filter))
                            .unwrap_or(false)
                })
                .cloned()
                .collect();
            let total = entries.len();
            if let Ok(history) = history.lock() {
                rank::rank_with(
                    &mut entries,
                    &path_query.filter,
                    &history,
                    config.app_boost,
                    &request.rank,
                );
            }
            // 浏览目录不是「搜索前 N 条」：放宽上限，列表靠 UI 滚动查看。
            entries.truncate(request.limit.max(200));
            let _ = tx.send(Response {
                generation: request.generation,
                entries,
                total,
                elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                error: None,
            });
            ctx.request_repaint();
            continue;
        }

        // P0-2：目录上下文下清空输入时直接列当前目录的子项（就地浏览），
        // 而不是「最近使用」。查询时仍有隐含 path:，范围始终是这个目录。
        if in_folder_context && request.query.is_empty() {
            let started = Instant::now();
            let dir = request
                .base_folder
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut entries: Vec<Entry> = dir_cache.children(&dir).to_vec();
            let total = entries.len();
            entries.truncate(request.limit.max(200));
            let _ = tx.send(Response {
                generation: request.generation,
                entries,
                total,
                elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                error: None,
            });
            ctx.request_repaint();
            continue;
        }

        let needs_uwp = match request.query.scope {
            Scope::App => true,
            Scope::All => !request.query.is_empty() && !in_folder_context,
            _ => false,
        };
        if needs_uwp && !uwp_started {
            uwp_started = true;
            let uwp_tx = uwp_tx.clone();
            std::thread::Builder::new()
                .name("lstr-uwp".to_string())
                .spawn(move || {
                    let _ = uwp_tx.send(uwp::load());
                })
                .ok();
        }

        let started = Instant::now();
        let mut entries = Vec::new();
        let mut total = 0usize;
        let mut error = None;
        // 自定义关键字行（g rust → 用 Google 搜索）：之后钉在结果顶部，不参与打分。
        let mut keyword_rows: Vec<Entry> = Vec::new();

        if request.query.is_empty() {
            if request.show_history {
                if let Ok(history) = history.lock() {
                    entries = history.recent_entries(request.limit.max(8));
                }
            }
            entries.retain(|entry| scope_matches(request.query.scope, entry));
            // 历史是旧快照：系统入口的启动目标可能已经改过，用当前索引里的同名/同 id 条目刷新。
            for entry in &mut entries {
                if matches!(entry.kind, Kind::App | Kind::Program) {
                    if let Some(fresh) = apps.all().iter().find(|app| app.id == entry.id) {
                        *entry = fresh.clone();
                    }
                }
            }
            // 再按当前世界过滤：文件可能已删除/移动/改名，应用可能已卸载，
            // 否则「最近使用」里点开就是「打开失败」。
            entries.retain(|entry| !stale_history_entry(entry, apps.all()));
            // 空查询 = 纯「最近使用」：清空历史后就是空列表，不再补常用应用
            // （补位会让用户以为历史没清掉）；想看已安装应用去「应用」标签。
            if request.query.scope == Scope::App {
                entries = apps.top(request.limit);
            } else if request.query.scope == Scope::Command {
                entries = commands("", &config.commands);
            }
            total = entries.len();
        } else {
            match request.query.scope {
                Scope::App => {
                    entries = apps.search(&request.query.text, request.limit);
                    total = entries.len();
                }
                Scope::Command => {
                    entries = commands(&request.query.text, &config.commands);
                    total = entries.len();
                }
                Scope::All => {
                    // 有筛选时只搜文件：位置/时间/类型对应用和命令没有意义，
                    // 硬塞进来只会让人疑惑「选了 D 盘怎么还出现微信」。
                    if request.filters.is_empty() && !in_folder_context {
                        keyword_rows = keyword_entries(&request.keywords, &request.query.text);
                    }
                    let mut needs_exact = false;
                    if let Some(provider) = &everything {
                        let fetch = fetch_count(&request.query, request.limit);
                        // 分两路取候选：文件夹一路、非文件夹一路。Everything 会在
                        // 返回前按排序截断，单次查询可能整类缺席（实测 "rust" 最近
                        // 400 条里 0 个文件夹，精确命中的「Rust」目录进不了排序），
                        // 于是「全部」标签只剩下文件。文件夹查询很便宜（~50ms）。
                        let folder_fetch = (fetch / 5).clamp(40, 120);
                        let file_fetch = fetch.saturating_sub(folder_fetch).max(60);
                        let mut truncation = 0usize;
                        match provider.search_split(
                            &request.query,
                            &request.filters,
                            folder_fetch,
                            true,
                        ) {
                            Ok((mut folders, count)) => {
                                total += count;
                                truncation += count.saturating_sub(folders.len());
                                entries.append(&mut folders);
                            }
                            Err(err) => error = Some(err),
                        }
                        match provider.search_split(
                            &request.query,
                            &request.filters,
                            file_fetch,
                            false,
                        ) {
                            Ok((mut files, count)) => {
                                total += count;
                                truncation += count.saturating_sub(files.len());
                                entries.append(&mut files);
                            }
                            Err(err) => error = error.or(Some(err)),
                        }
                        // 截断才需要 wfn 精确兜底；延后到第一版结果发出后再查。
                        needs_exact = truncation > 0;
                    } else {
                        error = Some("Everything 未运行".to_string());
                    }
                    if request.filters.is_empty() && !in_folder_context {
                        let mut app_hits = apps.search(&request.query.text, request.limit);
                        total += app_hits.len();
                        entries.append(&mut app_hits);
                        // 内置命令也参与“全部”搜索（打开设置等）。
                        let mut command_hits = commands(&request.query.text, &config.commands);
                        total += command_hits.len();
                        entries.append(&mut command_hits);
                    }
                    // 两阶段返回：先发不含精确补齐的结果（约 200ms 可见），
                    // 再补 wfn 精确项；最终结果与一次性返回等价。
                    if needs_exact {
                        let first = finalize_entries(
                            entries.clone(),
                            &request,
                            &history,
                            &config,
                            keyword_rows.clone(),
                        );
                        let _ = tx.send(Response {
                            generation: request.generation,
                            entries: first,
                            total,
                            elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                            error: error.clone(),
                        });
                        ctx.request_repaint();
                        if let Some(provider) = &everything {
                            if let Ok((mut exact, _)) =
                                provider.search_exact(&request.query, &request.filters, 50)
                            {
                                entries.append(&mut exact);
                            }
                        }
                    }
                }
                _ => {
                    let mut needs_exact = false;
                    if let Some(provider) = &everything {
                        let fetch = fetch_count(&request.query, request.limit);
                        match provider.search(&request.query, &request.filters, fetch) {
                            Ok((mut files, count)) => {
                                total = count;
                                needs_exact = count > files.len();
                                entries.append(&mut files);
                            }
                            Err(err) => error = Some(err),
                        }
                    } else {
                        error = Some("Everything 未运行".to_string());
                    }
                    if needs_exact {
                        let first = finalize_entries(
                            entries.clone(),
                            &request,
                            &history,
                            &config,
                            Vec::new(),
                        );
                        let _ = tx.send(Response {
                            generation: request.generation,
                            entries: first,
                            total,
                            elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                            error: error.clone(),
                        });
                        ctx.request_repaint();
                        if let Some(provider) = &everything {
                            if let Ok((mut exact, _)) =
                                provider.search_exact(&request.query, &request.filters, 50)
                            {
                                entries.append(&mut exact);
                            }
                        }
                    }
                }
            }
        }

        let entries = finalize_entries(entries, &request, &history, &config, keyword_rows);

let _ = tx.send(Response {
            generation: request.generation,
            entries,
            total,
            elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
            error,
        });
        ctx.request_repaint();
    }
}

fn fetch_count(query: &Query, limit: usize) -> usize {
    let short = query.text.chars().count() < 2 && query.ext.is_none() && query.path_filter.is_none();
    if short {
        80
    } else {
        // 实测 Everything IPC 的瓶颈在查询评估本身（~120ms，与条数基本无关），
        // 300→500 只多花约 8ms，却能给排序多 66% 的候选，值得保留。
        (limit * 50).min(500)
    }
}

fn scope_matches(scope: Scope, entry: &Entry) -> bool {
    match scope {
        Scope::All => entry.kind != Kind::Command,
        Scope::Folder => entry.kind == Kind::Folder,
        Scope::Program => entry.kind == Kind::Program,
        Scope::File => entry.kind == Kind::File,
        Scope::App => entry.kind == Kind::App,
        Scope::Command => entry.kind == Kind::Command,
    }
}

/// 历史条目是否已失效（空查询展示前过滤）：
/// - 文件/文件夹/程序：目标不存在（删除/移动/改名）即失效；UNC 路径不做
///   存在性检查——死掉的 SMB 路径 `exists()` 会阻塞数秒，宁可留着旧条目。
/// - 应用（`app:`/`sys:`）：当前索引里找不到说明 .lnk/App Paths 项已消失
///   （卸载）；`uwp:` 走懒加载，枚举回来之前索引里没有，不能误杀。
pub fn stale_history_entry(entry: &Entry, apps: &[Entry]) -> bool {
    match entry.kind {
        Kind::File | Kind::Folder | Kind::Program => {
            let Some(path) = entry.path.as_deref() else {
                return false;
            };
            if path.as_os_str().to_string_lossy().starts_with("\\\\") {
                return false;
            }
            !path.exists()
        }
        Kind::App => {
            !entry.id.starts_with("uwp:") && !apps.iter().any(|app| app.id == entry.id)
        }
        Kind::Command => false,
    }
}

/// 内置命令的别名：让「设置」这种常用说法精确命中「打开设置」，
/// 而不是被同名但更少用的应用/文件抢走。
pub fn command_aliases(id: &str) -> &'static [&'static str] {
    match id {
        "cmd:settings" => &["设置", "settings", "偏好设置"],
        "cmd:reload" => &["重新加载", "reload"],
        "cmd:quit" => &["退出", "quit"],
        _ => &[],
    }
}

fn commands(q: &str, custom: &[crate::assets::CommandDef]) -> Vec<Entry> {
    let all = [
        ("cmd:settings", "打开设置"),
        ("cmd:reload", "重新加载"),
        ("cmd:everything", "启动 Everything"),
        ("cmd:quit", "退出 lstr"),
    ];
    let needle = q.to_lowercase();
    let mut out: Vec<Entry> = all
        .into_iter()
        .filter(|(id, name)| {
            if needle.is_empty() {
                return true;
            }
            if name.to_lowercase().contains(&needle) {
                return true;
            }
            // 别名（settings/设置）之前只参与打分、不参与过滤，导致
            // 输入 "settings" 时「打开设置」根本进不了结果列表。
            if command_aliases(id)
                .iter()
                .any(|alias| alias.to_lowercase().contains(&needle))
            {
                return true;
            }
            let pinyin = pinyin::keys(name);
            if pinyin.to_lowercase().contains(&needle) {
                return true;
            }
            fuzzy::score(name, &needle).is_some()
        })
        .map(|(id, name)| Entry {
            id: id.to_string(),
            kind: Kind::Command,
            name: name.to_string(),
            path: None,
            size: None,
            modified: None,
            pinyin: Some(pinyin::keys(name)),
            initial: ">".to_string(),
            tone: 0xE0A040,
        })
        .collect();
    // P0-1：自定义命令（{query} / {current_folder}）与内置命令同榜。
    for def in custom {
        if !needle.is_empty()
            && !def.trigger.to_lowercase().contains(&needle)
            && !def.title.to_lowercase().contains(&needle)
        {
            continue;
        }
        out.push(Entry {
            id: format!("usercmd:{}", def.trigger),
            kind: Kind::Command,
            name: def.title.clone(),
            path: None,
            size: None,
            modified: None,
            pinyin: Some(pinyin::keys(&def.title)),
            initial: ">".to_string(),
            tone: 0xE0A040,
        });
    }
    out
}

/// 自定义关键字：查询形如 "trigger 剩余词"（或刚好是 trigger）时，
/// 生成网络搜索/命令行结果行。Enter 打开模板替换 {query} 后的目标。
fn keyword_entries(keywords: &[KeywordConfig], q: &str) -> Vec<Entry> {
    let trimmed = q.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (trimmed, ""),
    };
    keywords
        .iter()
        .filter(|kw| kw.trigger.eq_ignore_ascii_case(head))
        .filter_map(|kw| {
            if let Some(url) = &kw.url {
                let target = url.replace("{query}", &url_encode(rest));
                let name = if rest.is_empty() {
                    format!("用 {} 搜索…", kw.name)
                } else {
                    format!("用 {} 搜索：{}", kw.name, rest)
                };
                Some(keyword_entry(format!("kwurl:{}", kw.trigger), name, target))
            } else if let Some(exec) = &kw.exec {
                let target = exec.replace("{query}", rest);
                let name = if rest.is_empty() {
                    format!("{}（输入参数）", kw.name)
                } else {
                    format!("{}：{}", kw.name, rest)
                };
                Some(keyword_entry(format!("kwexec:{}", kw.trigger), name, target))
            } else {
                None
            }
        })
        .collect()
}

fn keyword_entry(id: String, name: String, target: String) -> Entry {
    Entry {
        id,
        kind: Kind::Command,
        name,
        path: Some(PathBuf::from(target)),
        size: None,
        modified: None,
        pinyin: None,
        initial: ">".to_string(),
        tone: 0xE0A040,
    }
}

/// Quick Switch 行：Explorer 已打开的文件夹。
fn quick_switch_entry(path: PathBuf) -> Entry {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.display().to_string());
    Entry {
        id: format!("qs:{}", path.display()),
        kind: Kind::Folder,
        name,
        path: Some(path),
        size: None,
        modified: None,
        pinyin: None,
        initial: String::new(),
        tone: 0x6C9BFF,
    }
}

/// RFC 3986 unreserved 之外的字节逐个百分号编码（UTF-8 字节流，中文安全）。
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn google() -> Vec<KeywordConfig> {
        vec![KeywordConfig {
            trigger: "g".to_string(),
            name: "Google".to_string(),
            url: Some("https://www.google.com/search?q={query}".to_string()),
            exec: None,
        }]
    }

    #[test]
    fn keyword_with_query_builds_url_row() {
        let rows = keyword_entries(&google(), "g rust 语言");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "kwurl:g");
        assert_eq!(rows[0].name, "用 Google 搜索：rust 语言");
        assert_eq!(
            rows[0].path.as_ref().unwrap().to_string_lossy(),
            "https://www.google.com/search?q=rust%20%E8%AF%AD%E8%A8%80"
        );
    }

    #[test]
    fn bare_trigger_shows_hint_row_opening_homepage() {
        let rows = keyword_entries(&google(), "g");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "用 Google 搜索…");
        assert_eq!(
            rows[0].path.as_ref().unwrap().to_string_lossy(),
            "https://www.google.com/search?q="
        );
    }

    #[test]
    fn unknown_or_partial_trigger_yields_nothing() {
        assert!(keyword_entries(&google(), "go rust").is_empty());
        assert!(keyword_entries(&google(), "x rust").is_empty());
        assert!(keyword_entries(&google(), "").is_empty());
    }

    #[test]
    fn exec_keyword_substitutes_query() {
        let keywords = vec![KeywordConfig {
            trigger: "np".to_string(),
            name: "记事本打开".to_string(),
            url: None,
            exec: Some("notepad {query}".to_string()),
        }];
        let rows = keyword_entries(&keywords, "np a.txt");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "kwexec:np");
        assert_eq!(rows[0].path.as_ref().unwrap().to_string_lossy(), "notepad a.txt");
    }

    #[test]
    fn url_encode_leaves_unreserved_and_encodes_rest() {
        assert_eq!(url_encode("a-b_c.d~e f"), "a-b_c.d~e%20f");
        assert_eq!(url_encode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    #[ignore = "requires a running Everything instance"]
    fn exact_folder_survives_candidate_truncation() {
        // 回归：E:\use 这种老目录在常规候选里会被「最近修改的 User*」挤出，
        // wfn: 兜底后必须能搜到。机器上没有 E:\use 时跳过。
        let target = std::path::Path::new(r"E:\use");
        if !target.exists() {
            return;
        }
        let config = crate::config::Config::default();
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);
        let generation = service.search("use", crate::model::Scope::All, false, &Default::default(), &Default::default(), true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(response) = service.poll() {
                if response.generation == generation {
                    assert!(
                        response
                            .entries
                            .iter()
                            .any(|e| e.full_path().eq_ignore_ascii_case(r"E:\use")),
                        "「use」的结果里应有 E:\\use，实际前几条: {:?}",
                        response
                            .entries
                            .iter()
                            .take(8)
                            .map(|e| e.full_path())
                            .collect::<Vec<_>>()
                    );
                    return;
                }
            }
            assert!(std::time::Instant::now() < deadline, "等待搜索结果超时");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn stale_history_entries_are_filtered() {
        use std::path::PathBuf;

        // 已删除/移动的文件：路径不存在即失效（Z: 盘不存在，天然是死路径）。
        let deleted = Entry {
            id: r"Z:\no\such\file.txt".to_string(),
            kind: Kind::File,
            name: "file.txt".to_string(),
            path: Some(PathBuf::from(r"Z:\no\such\file.txt")),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        };
        assert!(stale_history_entry(&deleted, &[]));

        // 存在的文件：保留（用 crate 根的 Cargo.toml，测试工作目录就在这）。
        let cwd = std::env::current_dir().unwrap();
        let alive = Entry {
            id: cwd.join("Cargo.toml").display().to_string(),
            kind: Kind::File,
            name: "Cargo.toml".to_string(),
            path: Some(cwd.join("Cargo.toml")),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        };
        assert!(!stale_history_entry(&alive, &[]));

        // UNC 路径不做存在性检查（死 SMB 路径会阻塞数秒），一律保留。
        let unc = Entry {
            id: r"\\deadhost\share\a.txt".to_string(),
            kind: Kind::File,
            name: "a.txt".to_string(),
            path: Some(PathBuf::from(r"\\deadhost\share\a.txt")),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        };
        assert!(!stale_history_entry(&unc, &[]));

        // 卸载的应用：索引里找不到 app: 条目即失效。
        let mut uninstalled = deleted.clone();
        uninstalled.kind = Kind::App;
        uninstalled.id = "app:C:\\gone\\app.lnk".to_string();
        assert!(stale_history_entry(&uninstalled, &[]));

        // 还在索引里的应用：保留。
        let mut installed = uninstalled.clone();
        installed.id = "app:C:\\here\\app.lnk".to_string();
        assert!(!stale_history_entry(&installed, &[installed.clone()]));

        // UWP 懒加载没回来前索引里没有 uwp: 条目，不能误杀。
        let mut uwp = uninstalled;
        uwp.id = "uwp:Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_string();
        assert!(!stale_history_entry(&uwp, &[]));
    }

    #[test]
    fn browse_mode_flows_through_service() {
        // 路径模式只依赖 read_dir，无需 Everything，可做端到端回归。
        let config = crate::config::Config::default();
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);
        let cwd = std::env::current_dir().expect("cwd");
        // 过滤词「carg」应命中 crate 根的 Cargo.toml。
        let query = format!("{}\\carg", cwd.display());
        let generation = service.search(&query, crate::model::Scope::All, false, &Default::default(), &Default::default(), true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(response) = service.poll() {
                if response.generation == generation {
                    assert!(
                        response.entries.iter().any(|e| e.name == "Cargo.toml"),
                        "路径模式应列出 Cargo.toml，实际: {:?}",
                        response.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
                    );
                    return;
                }
            }
            assert!(std::time::Instant::now() < deadline, "等待路径模式响应超时");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "requires a running Everything instance"]
    fn pipeline_perf() {
        let config = crate::config::Config::default();
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);
        for q in ["c", "rs", "clip", "设置", "weixin"] {
            let generation = service.search(q, crate::model::Scope::All, false, &Default::default(), &Default::default(), true);
            let started = std::time::Instant::now();
            loop {
                if let Some(response) = service.poll() {
                    if response.generation == generation {
                        println!(
                            "{q}: {} entries (total {}) worker {:.1}ms, pipeline {:.1}ms",
                            response.entries.len(),
                            response.total,
                            response.elapsed_ms,
                            started.elapsed().as_secs_f32() * 1000.0
                        );
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}
