pub mod apps;
pub mod bookmarks;
pub mod browse;
pub mod calc;
#[cfg(test)]
mod acceptance_tests;
pub mod builtin;
pub mod conditions;
pub mod entrymeta;
pub mod filters;
pub mod fuzzy;
pub mod pinyin;
pub mod query;
pub mod rank;
pub mod uwp;

use crate::config::{Config, KeywordConfig, RankConfig, MAX_LOAD_RESULTS, MIN_LOAD_RESULTS};
use crate::history::History;
use lstr_index::{ClientCommand, EngineMsg};
use crate::model::{Entry, Kind, Query, Scope};
use crate::search::filters::SearchFilters;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use eframe::egui;
use std::time::{Duration, Instant};

/// 应用索引的刷新周期：新装/卸载应用后最多等这么久就能搜到/消失。
/// 重建成本是枚举开始菜单 + App Paths（本机 200+ 应用，几十 ms），放 worker 线程无感。
const APPS_RELOAD_INTERVAL: Duration = Duration::from_secs(90);

/// 浏览器书签的刷新周期。书签文件由浏览器原子写入，重载只是解析 JSON /
/// 复制 places.sqlite（本机 500 条量级 <10ms），比应用索引更宽松即可。
const BOOKMARKS_RELOAD_INTERVAL: Duration = Duration::from_secs(120);

/// 首屏交付的候选上限。窗口只按「一屏结果数」长高，其余靠滚轮查看。
pub(crate) const MIN_DELIVER: usize = 100;
/// 非精确同名条目每种**排在前面的**保留几条（A-4），其余追加到列表尾部
/// 而不是丢弃——总数与可滚动到的条数必须一致。精确同名（name == 查询词）
/// 不设上限：用户打的就是完整文件名，每个不同路径都是有效结果。
const SAME_NAME_KEEP: usize = 3;
/// 「加载更多」时单次向索引要的候选数上限。
/// 3000 条交付上限按 2 倍留去重余量 → 6000。
const MAX_FETCH: usize = 6000;

/// 下一档交付上限；已到设置里的上限（`max`）返回 None（UI 不再发请求）。
fn next_deliver(current: usize, max: usize) -> Option<usize> {
    (current < max).then(|| (current * 2).min(max))
}

/// 等待中的查询请求数:UI 线程发送前 +1,搜索线程出队后 -1。
/// 引擎分批 apply 的块间让路钩子读它:>0 = 有打字请求在排队,对账/首扫的
/// 记录 apply 立即收手——0294602「打字时查询优先于索引维护」的 apply 中段
/// 扩展(此前只在 idle 轮边界检查,一轮 400 条大消息仍会把查询堵住秒级)。
/// 计数而非布尔的理由:UI +1 与 send 之间、worker 出队与下一请求之间存在
/// 窗口,布尔会出现「队列里有请求但标志已清」的假阴性;计数不会。
static QUERY_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

/// 分项计时强制落日志开关(部署后验收用):设置 `LSTR_PROFILE_QUERY=1`
/// 后,即使单次查询低于慢查询阈值也把 stage/split/finalize 分项写进
/// engine.log;默认仍只在 >15ms(查询)/>3ms(finalize)时记录。
fn profile_queries() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LSTR_PROFILE_QUERY").is_some())
}

/// 出队了一个请求:等待计数 -1(saturating,防 debug 断言下溢)。
fn query_inflight_dec() {
    let prev = QUERY_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    if prev == 0 {
        // 只有诊断用途;计数不该为负,恢复 0。
        QUERY_INFLIGHT.fetch_add(1, Ordering::Relaxed);
    }
}

struct Request {
    generation: u64,
    /// 诊断用：UI 发出时刻。worker 出队时用它区分「排队慢」还是「执行慢」。
    sent_at: Instant,
    query: Query,
    /// 一屏结果行数：决定窗口高度，也参与候选抓取/应用条数的预算。
    limit: usize,
    /// 本次最多交付多少条候选：首屏 [`MIN_DELIVER`]，滚到底按设置上限翻倍。
    deliver: usize,
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
    /// 深路径浏览模式的当前目录：输入框只放过滤词，worker 直接列这个目录。
    browse_root: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub generation: u64,
    pub entries: Vec<Entry>,
    pub total: usize,
    pub elapsed_ms: f32,
    pub error: Option<String>,
}

/// 诊断：响应发出时距请求发出的总耗时（含排队）。仅慢查询写 engine.log。
/// 与 UI 侧 `search: ui total=` 对照可区分「worker 慢」与「UI 帧等待慢」。
fn send_response(tx: &Sender<Response>, request: &Request, response: Response) {
    // 最新查询已经换人:被取消/已过期的结果直接丢弃,避免 UI 先闪一版
    // 部分结果(total 也偏小),也避免无谓的序列化。
    if lstr_index::query::CANCEL_GEN.load(std::sync::atomic::Ordering::Relaxed) != request.generation
    {
        return;
    }
    let total_ms = request.sent_at.elapsed().as_millis();
    if total_ms > 200 {
        lstr_index::snapshot::debug_log(&format!(
            "search: worker send gen={} total={}ms execute={:.1}ms entries={} total={}",
            response.generation, total_ms, response.elapsed_ms, response.entries.len(), response.total
        ));
    }
    let _ = tx.send(response);
}

/// 上一次查询的完整参数：滚到底加载更多时原样重发，只放大交付上限。
#[derive(Clone)]
struct LastQuery {
    raw: String,
    scope: Scope,
    dialog_quick: bool,
    filters: SearchFilters,
    rank: RankConfig,
    show_history: bool,
    base_folder: Option<PathBuf>,
    browse_root: Option<PathBuf>,
}

pub struct SearchService {
    tx: Sender<Request>,
    rx: Receiver<Response>,
    generation: u64,
    history: Arc<Mutex<History>>,
    limit: usize,
    /// 当前「加载更多」的交付上限（100 → 200 → … → `max_deliver`）。
    deliver: usize,
    /// 设置页「加载更多上限」；来自 config，运行期可改。
    max_deliver: usize,
    /// 上一次查询参数；`more()` 用它重发同一个查询。
    last: Option<LastQuery>,
    keywords: Vec<KeywordConfig>,
    /// P0-1：收藏列表，新增收藏后由 UI 同步进来。
    favorites: Vec<crate::assets::Favorite>,
    /// 「重新加载」命令置位，worker 下一请求立即重建应用索引。
    apps_dirty: Arc<AtomicBool>,
    /// 浏览器书签索引开关（设置页即时生效）：worker 每轮维护读取，
    /// 关闭时清空索引并且不再把书签并入结果。
    bookmarks_enabled: Arc<AtomicBool>,
    /// 书签 id → 收藏夹层级（「收藏夹栏 › 工作 › 需求」）。worker 每次
    /// 重载书签索引时整体刷新，UI 用它渲染结果行副标题。
    bookmark_folders: Arc<Mutex<HashMap<String, String>>>,
    /// 自研索引状态(设置页/状态行用);worker 线程随时刷新。
    pub index_status: Arc<Mutex<Option<builtin::IndexUiStatus>>>,
    /// 「参与索引的卷」掩码(lstr_index::volmask 位掩码,bit i='A'+i),
    /// 设置页勾选/取消盘时更新;worker 线程在每次拉起与每帧指令里读取。
    volume_mask: Arc<AtomicU32>,
    /// 发给索引 worker 的控制指令(运行期切卷/重建),由 client 线程非阻塞写帧。
    index_cmd: mpsc::Sender<ClientCommand>,
    /// 排除规则表共享槽(原始模式):设置页「立即重建」时更新,引擎在建新
    /// store/落快照指纹时取新表;finalize 硬过滤也实时取。
    noise_slot: Arc<Mutex<Vec<String>>>,
    /// 优雅退出标志:on_exit 置位后 client 线程给 worker 发 Shutdown 并等其
    /// 退出;run_worker 的请求循环也据此提前收尾(flush_all 不再依赖通道断开)。
    shutdown_client: Arc<AtomicBool>,
    /// 优雅退出收尾完成标志:run_worker 在 flush_all + 等 client 线程收尾后
    /// 置位;`graceful_shutdown` 轮询它给 eframe::App::on_exit 一个有界等待。
    worker_collected: Arc<AtomicBool>,
}

impl SearchService {
    pub fn new(config: Config, history: Arc<Mutex<History>>, ctx: egui::Context) -> Self {
        let (tx, request_rx) = mpsc::channel::<Request>();
        let (response_tx, rx) = mpsc::channel::<Response>();
        let worker_history = Arc::clone(&history);
        let worker_config = config.clone();
        let apps_dirty = Arc::new(AtomicBool::new(false));
        let worker_apps_dirty = Arc::clone(&apps_dirty);
        let bookmarks_enabled = Arc::new(AtomicBool::new(config.index.bookmarks));
        let worker_bookmarks_enabled = Arc::clone(&bookmarks_enabled);
        let bookmark_folders: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let worker_bookmark_folders = Arc::clone(&bookmark_folders);
        let status_slot: Arc<Mutex<Option<builtin::IndexUiStatus>>> = Arc::new(Mutex::new(None));
        let worker_status = Arc::clone(&status_slot);
        // 卷选择:启动时从配置取快照(空列表 = 全部盘),之后设置页改掩码即可
        // 在运行期切卷,不需要重启 lstr。
        let volume_mask = Arc::new(AtomicU32::new(config.index.volume_mask()));
        let worker_volume_mask = Arc::clone(&volume_mask);
        // 排除规则表共享槽:初始 = 配置里的排除行(内置段已迁移进来)。
        let noise_slot = Arc::new(Mutex::new(exclude_patterns(&config)));
        let worker_noise_slot = Arc::clone(&noise_slot);
        let (index_cmd_tx, index_cmd_rx) = mpsc::channel::<ClientCommand>();
        // 优雅退出三件套(缺陷修复:GUI 退出必须收提权 worker):
        // shutdown_client → client 线程/请求循环的收尾信号;
        // client_done    → client 线程确认 Shutdown 已发、worker 已退出;
        // worker_collected → run_worker flush+收尾完成,on_exit 可返回。
        let shutdown_client = Arc::new(AtomicBool::new(false));
        let worker_collected = Arc::new(AtomicBool::new(false));
        let client_done = Arc::new(AtomicBool::new(false));
        let (done_s, done_w, done_c) = (
            Arc::clone(&shutdown_client),
            Arc::clone(&worker_collected),
            Arc::clone(&client_done),
        );
        let worker_index_cmd_tx = index_cmd_tx.clone();
        std::thread::Builder::new()
            .name("lstr-search".to_string())
            .spawn(move || {
                run_worker(WorkerArgs {
                    rx: request_rx,
                    tx: response_tx,
                    history: worker_history,
                    config: worker_config,
                    ctx,
                    apps_dirty: worker_apps_dirty,
                    bookmarks_enabled: worker_bookmarks_enabled,
                    bookmark_folders: worker_bookmark_folders,
                    status_slot: worker_status,
                    index_cmd_rx,
                    index_cmd_tx: worker_index_cmd_tx,
                    volume_mask: worker_volume_mask,
                    noise_slot: worker_noise_slot,
                    shutdown_client: done_s,
                    worker_collected: done_w,
                    client_done: done_c,
                })
            })
            .expect("spawn search worker");
        Self {
            tx,
            rx,
            generation: 0,
            history,
            limit: config.max_results,
            deliver: MIN_DELIVER,
            max_deliver: config
                .max_load_results
                .clamp(MIN_LOAD_RESULTS, MAX_LOAD_RESULTS),
            last: None,
            keywords: config.keywords,
            favorites: config.favorites,
            apps_dirty,
            bookmarks_enabled,
            bookmark_folders,
            index_status: status_slot,
            volume_mask,
            index_cmd: index_cmd_tx,
            noise_slot,
            shutdown_client,
            worker_collected,
        }
    }

    /// 优雅退出(eframe::App::on_exit 调用):通知 client 线程给 worker 发
    /// Shutdown 并等待其退出 + 引擎 flush 收尾。有界超时 —— worker 被强杀/
    /// 卡死时放弃等待,协议容忍半开(残留 worker 由自身 idle 超时与
    /// clean-exit 降级机制自愈),GUI 不因退出卡死。
    pub fn graceful_shutdown(&self, timeout: Duration) {
        self.shutdown_client.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        while !self.worker_collected.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 设置页勾选/取消盘后调用:更新掩码并通知 worker 线程切卷。
    /// worker 没在跑(UAC 被拒/首次连接前)时指令会静默失败,
    /// 下次拉起 worker 会按最新掩码生成启动参数,所以不会丢。
    pub fn set_index_volumes(&self, mask: u32) {
        self.volume_mask.store(mask, Ordering::Relaxed);
        if self.index_cmd.send(ClientCommand::SetVolumes(mask)).is_err() {
            return;
        }
        lstr_index::snapshot::debug_log(&format!(
            "main: set volumes mask=0x{mask:08x} ({})",
            lstr_index::volmask::describe(mask)
        ));
    }

    /// 「立即重建」:排除规则变更后调用。更新共享槽 + 删旧快照。
    /// run_worker 空闲轮询发现槽变化后:引擎重置受影响卷(丢弃旧 NOISE 位)
    /// → 按差集算受影响盘发 Rescan 掩码 → worker 逐卷重扫。待重扫卷 3s
    /// 未收到 ScanBegin 自动重发(自愈);重启后快照指纹不符也会重扫。
    pub fn rebuild_noise_index(&self, patterns: Vec<String>) {
        if let Ok(mut slot) = self.noise_slot.lock() {
            *slot = patterns;
        }
        // 删旧快照:重建完成前的崩溃窗口里,旧快照指纹与新规则不符会被拒,
        // 这里删掉更直接(避免旧 store 的周期性保存把旧快照写回盘)。
        let dir = crate::config::attach_dir().join("index");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let looks_like_snapshot = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("vol_") && n.ends_with(".bin"))
                    .unwrap_or(false);
                if looks_like_snapshot {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        lstr_index::snapshot::debug_log("main: noise rules changed (slot updated)");
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
        self.search_with_context(
            raw,
            scope,
            dialog_quick,
            filters,
            rank,
            show_history,
            base_folder,
            None,
            false,
        )
    }

    /// 深路径浏览：输入框只放过滤词，worker 直接列 `browse_root` 的子项。
    pub fn search_browse(
        &mut self,
        raw: &str,
        scope: Scope,
        dialog_quick: bool,
        filters: &SearchFilters,
        rank: &RankConfig,
        show_history: bool,
        browse_root: PathBuf,
    ) -> u64 {
        self.search_with_context(
            raw,
            scope,
            dialog_quick,
            filters,
            rank,
            show_history,
            None,
            Some(browse_root),
            false,
        )
    }

    fn search_with_context(
        &mut self,
        raw: &str,
        scope: Scope,
        dialog_quick: bool,
        filters: &SearchFilters,
        rank: &RankConfig,
        show_history: bool,
        base_folder: Option<PathBuf>,
        browse_root: Option<PathBuf>,
        keep_deliver: bool,
    ) -> u64 {
        // 新查询/新上下文回到首屏 100 条；只有 more() 续拉时保留上限。
        if !keep_deliver {
            self.deliver = MIN_DELIVER;
        }
        self.last = Some(LastQuery {
            raw: raw.to_string(),
            scope,
            dialog_quick,
            filters: filters.clone(),
            rank: rank.clone(),
            show_history,
            base_folder: base_folder.clone(),
            browse_root: browse_root.clone(),
        });
        self.generation += 1;
        // 通知查询线程:更旧的查询可以立即放弃(打字时 "a"→"ai" 不再排队 600ms)。
        lstr_index::query::CANCEL_GEN
            .store(self.generation, std::sync::atomic::Ordering::Relaxed);
        let query = if filters.matches.regex {
            query::parse_regex(raw, scope)
        } else {
            query::parse(raw, scope)
        };
        // 先计数再发送:搜索线程的分批 apply 让路钩子据此提前收手,
        // 查询不会再等一整轮消息消化(实测对账期一次最多等数百 ms)。
        QUERY_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        if self
            .tx
            .send(Request {
                generation: self.generation,
                sent_at: Instant::now(),
                query,
                limit: self.limit,
                deliver: self.deliver,
                keywords: self.keywords.clone(),
                dialog_quick,
                filters: filters.clone(),
                rank: rank.clone(),
                show_history,
                favorites: self.favorites.clone(),
                base_folder,
                browse_root,
            })
            .is_err()
        {
            // 搜索线程已退出:计数回退,避免遗留计数让后续会话误判
            query_inflight_dec();
        }
        self.generation
    }

    /// 设置里的「一屏结果数」改动后立即生效，无需重启。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.clamp(1, 50);
    }

    /// 设置页「加载更多上限」改动后立即生效（下一次续拉/新查询按新值）。
    pub fn set_max_load(&mut self, limit: usize) {
        self.max_deliver = limit.clamp(MIN_LOAD_RESULTS, MAX_LOAD_RESULTS);
    }

    /// 滚到列表底部：把交付上限翻倍并原样重发上一次查询。
    /// 返回新 generation；已到设置上限或还没发过查询时返回 None。
    pub fn more(&mut self) -> Option<u64> {
        let last = self.last.clone()?;
        self.deliver = next_deliver(self.deliver, self.max_deliver)?;
        let generation = self.search_with_context(
            &last.raw,
            last.scope,
            last.dialog_quick,
            &last.filters,
            &last.rank,
            last.show_history,
            last.base_folder,
            last.browse_root,
            true,
        );
        lstr_index::snapshot::debug_log(&format!(
            "search: load more deliver={} gen={generation}",
            self.deliver
        ));
        Some(generation)
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

    /// 设置页「浏览器书签」开关：worker 下一轮维护立即清空/重建（不等 TTL）。
    pub fn set_bookmarks_enabled(&self, on: bool) {
        self.bookmarks_enabled.store(on, Ordering::Relaxed);
    }

    /// 书签 id → 收藏夹层级快照；UI 在每次拿到新响应时同步一次。
    pub fn bookmark_folders(&self) -> HashMap<String, String> {
        self.bookmark_folders
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default()
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
/// 名称与查询词完全一致的结果钉在最前（不依赖任何权重调参）。
/// 打分是多信号加和：历史、query 学习、位置启发式都可能把精确同名挤下去。
/// 实测搜 `ai` 时，cwd 下名称蹭到 ai 的 Rust 构建产物靠 +500 环境加权压过了
/// 名叫 ai 的目录。精确同名是「用户就是打的名字」，属于产品语义而非启发式，
/// 因此在排序 + 去重之后做一次稳定置顶，keyword 行仍在它上面。
///
/// 命令的别名也算精确命中：`cmd:settings` 的名字是「打开设置」、别名才是「设置」，
/// 用户打「设置」时期望的是设置命令，而不是同名的 app/文件。只按 name 置顶会让
/// 同名 app 靠历史分把命令顶下去（见 rank 的
/// `settings_alias_outranks_frequently_used_app_named_settings`）。
fn pin_exact_name_matches(entries: &mut Vec<Entry>, needle: &str, case_sensitive: bool) {
    if needle.is_empty() {
        return;
    }
    // 默认沿用 Unicode lowercase（与去重/rank 归一化一致）；区分大小写时
    // 必须按原始字节判等，否则 `API` 会把 `api` 也置顶。
    let is_exact = |entry: &Entry| {
        let name_eq = if case_sensitive {
            entry.name == needle
        } else {
            entry.name.to_lowercase() == needle
        };
        if name_eq {
            return true;
        }
        entry.kind == Kind::Command
            && command_aliases(&entry.id).iter().any(|alias| {
                if case_sensitive {
                    *alias == needle
                } else {
                    alias.to_lowercase() == needle
                }
            })
    };
    // 一遍分桶：命中项进 exact（其余进 rest），没有命中时 exact 为空，
    // 顺序与旧实现完全相同。旧实现先 any 扫一遍再 drain 扫一遍，
    // 每个候选的名字被 to_lowercase 两次；命中的查询尤其常见（短查询）。
    let mut exact = Vec::new();
    let mut rest = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if is_exact(&entry) {
            exact.push(entry);
        } else {
            rest.push(entry);
        }
    }
    if exact.is_empty() {
        *entries = rest;
        return;
    }
    // 组内保持原有分数顺序（exact.append 不重排）。
    exact.append(&mut rest);
    *entries = exact;
}

/// 收藏置顶（Pin）：把收藏过的条目稳定移到结果前列，组内保持原有分数顺序。
/// 按全路径小写匹配（Windows 大小写无关）；空收藏表直接返回。放在精确同名
/// 置顶之前调用，使「精确同名」仍排在收藏之上。
fn pin_favorites(entries: &mut Vec<Entry>, favorites: &[crate::assets::Favorite]) {
    if favorites.is_empty() {
        return;
    }
    let fav_paths: HashSet<String> = favorites.iter().map(|f| f.path.to_lowercase()).collect();
    let mut pinned = Vec::new();
    let mut rest = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if fav_paths.contains(&entry.full_path().to_lowercase()) {
            pinned.push(entry);
        } else {
            rest.push(entry);
        }
    }
    if pinned.is_empty() {
        *entries = rest;
        return;
    }
    pinned.append(&mut rest);
    *entries = pinned;
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    fn item(id: &str, name: &str) -> Entry {
        Entry {
            id: id.to_string(),
            kind: Kind::Folder,
            name: name.to_string(),
            path: None,
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        }
    }

    #[test]
    fn exact_name_matches_are_pinned_in_stable_order() {
        let mut entries = vec![
            item("sub", "num-traits"),
            item("exact1", "ai"),
            item("other", "trailer"),
            item("exact2", "AI"),
        ];
        pin_exact_name_matches(&mut entries, "ai", false);
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["exact1", "exact2", "sub", "other"]
        );
    }

    #[test]
    fn empty_needle_keeps_the_original_order() {
        let mut entries = vec![item("a", "a"), item("b", "b")];
        pin_exact_name_matches(&mut entries, "", false);
        assert_eq!(entries[0].id, "a");
    }

    #[test]
    fn pin_uses_unicode_lowercase_not_ascii_only() {
        // L2：去重/rank 都按 to_lowercase 归一化，置顶也必须一致。
        let mut entries = vec![item("sub", "x"), item("exact", "ÄI")];
        pin_exact_name_matches(&mut entries, "äi", false);
        assert_eq!(entries[0].id, "exact");
    }

    #[test]
    fn pin_favorites_moves_favorited_to_front() {
        let mut entries = vec![
            entry_at("a", "report", Kind::File, r"D:\a\report.txt"),
            entry_at("b", "report", Kind::File, r"D:\b\report.txt"),
            entry_at("c", "report", Kind::File, r"D:\c\report.txt"),
        ];
        let favs = vec![crate::assets::Favorite {
            name: "report".to_string(),
            path: r"D:\c\report.txt".to_string(),
        }];
        pin_favorites(&mut entries, &favs);
        assert_eq!(entries[0].id, "c", "收藏条目应置顶");
        // 路径大小写无关匹配
        let favs2 = vec![crate::assets::Favorite {
            name: "x".to_string(),
            path: r"d:\A\REPORT.TXT".to_string(),
        }];
        pin_favorites(&mut entries, &favs2);
        assert_eq!(entries[0].id, "a", "路径大小写无关匹配");
        // 空收藏表保持原序
        let before: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        pin_favorites(&mut entries, &[]);
        assert_eq!(
            entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            before
        );
    }

    fn command(id: &str, name: &str) -> Entry {
        let mut entry = item(id, name);
        entry.kind = Kind::Command;
        entry
    }

    fn entry_at(id: &str, name: &str, kind: Kind, path: &str) -> Entry {
        let mut entry = item(id, name);
        entry.kind = kind;
        entry.path = Some(PathBuf::from(path));
        entry
    }

    fn request(text: &str, limit: usize) -> Request {
        Request {
            generation: 1,
            sent_at: Instant::now(),
            query: Query {
                raw: text.to_string(),
                scope: Scope::All,
                text: text.to_string(),
                ext: None,
                path_filter: None,
                drives_include: 0,
                drives_exclude: 0,
                prefix: None,
                suffix: None,
                path_components: None,
            },
            limit,
            deliver: MIN_DELIVER,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: RankConfig::default(),
            show_history: true,
            favorites: Vec::new(),
            base_folder: None,
            browse_root: None,
        }
    }

    #[test]
    fn finalize_pins_command_alias_above_same_name_app() {
        // H1 回归：命令别名「设置」精确命中，必须压过同名的 app/文件。
        // 旧置顶只看 name，会把同名的 app（即使分数更低）移到命令前面。
        let history = Arc::new(Mutex::new(History::in_memory()));
        let app = entry_at(
            "app:settings",
            "设置",
            Kind::App,
            r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\设置.lnk",
        );
        let file = entry_at("file:settings", "设置", Kind::File, r"D:\docs\设置");
        {
            let mut hist = history.lock().unwrap();
            hist.seed(&app, 20, 0);
        }
        let entries = vec![app, file, command("cmd:settings", "打开设置")];
        let out = finalize_entries(
            entries,
            &request("设置", 8),
            &history,
            &Config::default(),
            Vec::new(),
        );
        assert_eq!(out[0].id, "cmd:settings", "命令别名精确命中应置顶");
        assert_eq!(out[1].id, "app:settings");
    }

    #[test]
    fn keyword_row_stays_above_pinned_command_alias() {
        let history = Arc::new(Mutex::new(History::in_memory()));
        let mut keyword = item("kwurl:g", "用 Google 搜索：设置");
        keyword.kind = Kind::Command;
        let out = finalize_entries(
            vec![command("cmd:settings", "打开设置")],
            &request("设置", 8),
            &history,
            &Config::default(),
            vec![keyword],
        );
        assert_eq!(out[0].id, "kwurl:g");
        assert_eq!(out[1].id, "cmd:settings");
    }

    #[test]
    fn finalize_keeps_all_exact_names_and_pins_them() {
        let history = Arc::new(Mutex::new(History::in_memory()));
        let mut entries: Vec<Entry> = (0..8)
            .map(|i| {
                entry_at(
                    &format!("exact{i}"),
                    "ai",
                    Kind::File,
                    &format!(r"D:\work\{i}\ai"),
                )
            })
            .collect();
        // 前缀命中先排在前面，确保置顶不是「本来就在最前」。
        entries.push(entry_at("prefix", "ai-notes", Kind::File, r"D:\work\ai-notes"));
        let out = finalize_entries(
            entries,
            &request("ai", 8),
            &history,
            &Config::default(),
            Vec::new(),
        );
        let exact_count = out.iter().filter(|entry| entry.name == "ai").count();
        assert_eq!(exact_count, 8, "精确同名不得按名字截断");
        assert!(
            out.iter().take(8).all(|entry| entry.name == "ai"),
            "全部精确同名都应被置顶"
        );
    }

    #[test]
    fn finalize_keeps_non_exact_same_names_without_dropping() {
        // A-4:搜 config（非精确同名）时，跨项目的多个 config.json 都要能看到；
        // 前 N 个保持原位，其余排到尾部（旧实现直接丢弃，且 load more 也补不回）。
        let history = Arc::new(Mutex::new(History::in_memory()));
        let entries: Vec<Entry> = (0..5)
            .map(|i| {
                entry_at(
                    &format!("cfg{i}"),
                    "config.json",
                    Kind::File,
                    &format!(r"D:\proj{i}\config.json"),
                )
            })
            .collect();
        let out = finalize_entries(
            entries,
            &request("config", 8),
            &history,
            &Config::default(),
            Vec::new(),
        );
        let count = out.iter().filter(|e| e.name == "config.json").count();
        assert_eq!(count, 5, "同名候选不得被丢弃（总数与列表条数一致）");
    }

    /// 回归：用户搜 `ucrtbase.dll`，索引里 147 条同名文件（C 60 / D 80 / E 6），
    /// 旧实现按「精确同名每种最多 6 条」砍到只剩 6 条 C 盘（再加 2 条子串命中），
    /// 状态栏却显示 147 —— D 盘命中一条都看不到。精确同名必须不设上限。
    #[test]
    fn finalize_exact_name_matches_are_not_capped() {
        let history = Arc::new(Mutex::new(History::in_memory()));
        let entries: Vec<Entry> = (0..12)
            .map(|i| {
                let drive = if i % 2 == 0 { 'C' } else { 'D' };
                entry_at(
                    &format!("dll{i}"),
                    "ucrtbase.dll",
                    Kind::File,
                    &format!(r"{drive}:\app{i}\ucrtbase.dll"),
                )
            })
            .collect();
        let out = finalize_entries(
            entries,
            &request("ucrtbase.dll", 8),
            &history,
            &Config::default(),
            Vec::new(),
        );
        let count = out.iter().filter(|e| e.name == "ucrtbase.dll").count();
        assert_eq!(count, 12, "精确同名不得按名字截断（D 盘命中必须保留）");
        assert!(
            out.iter().any(|e| e.full_path().starts_with(r"D:\")),
            "D 盘命中必须在结果里"
        );
    }

    #[test]
    fn bookmark_urls_are_not_dropped_by_path_exclude_rules() {
        // 排除规则按文件系统路径分段匹配；书签的 path 是 URL，
        // `https://example.com/cache/x` 不应因为段名 cache 被当文件排除。
        let history = Arc::new(Mutex::new(History::in_memory()));
        let mut bookmark = entry_at(
            "bm:https://example.com/cache/x",
            "Cache 使用说明",
            Kind::Bookmark,
            "https://example.com/cache/x",
        );
        bookmark.pinyin = Some("cache shiyongshuoming".to_string());
        let file = entry_at("file:cache", "cache.txt", Kind::File, r"D:\work\cache\cache.txt");
        let mut config = Config::default();
        config.rank.noise_rules.push(crate::config::NoiseRule {
            pattern: "cache".to_string(),
            penalty: 100,
            exclude: true,
        });
        let out = finalize_entries(
            vec![bookmark, file],
            &request("cache", 8),
            &history,
            &config,
            Vec::new(),
        );
        assert_eq!(out.len(), 1, "文件应被排除、书签必须保留: {out:?}");
        assert_eq!(out[0].kind, Kind::Bookmark);
    }

    /// 基准:finalize 约 480 个候选的分项成本。release 下运行:
    /// `cargo test -p lstr --release bench_finalize_candidates -- --nocapture`
    #[test]
    #[cfg_attr(debug_assertions, ignore = "release 基准")]
    fn bench_finalize_candidates() {
        let history = Arc::new(Mutex::new(History::in_memory()));
        let mut entries: Vec<Entry> = Vec::new();
        {
            let mut hist = history.lock().unwrap();
            for i in 0..320 {
                let name = if i % 40 == 0 { "ai".to_string() } else { format!("ai_report_{i:03}.txt") };
                let e = entry_at(
                    &format!("file{i}"),
                    &name,
                    Kind::File,
                    &format!(r"D:\work\project{i:03}\{name}"),
                );
                if i % 4 == 0 { hist.seed(&e, 3 + i % 20, 1_700_000_000 + i as u64); }
                entries.push(e);
            }
            for i in 0..120 {
                let e = entry_at(
                    &format!("dir{i}"),
                    &format!("ai_dir_{i:03}"),
                    Kind::Folder,
                    &format!(r"E:\use\proj{i:03}\ai_dir_{i:03}"),
                );
                if i % 6 == 0 { hist.seed(&e, 2, 1_700_000_000); }
                entries.push(e);
            }
            for i in 0..40 {
                let mut e = entry_at(
                    &format!("app{i}"),
                    &format!("A.I. Studio {i}"),
                    Kind::App,
                    &format!(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\App{i}.lnk"),
                );
                e.pinyin = Some("ai studio".to_string());
                hist.seed(&e, 5, 1_700_000_000);
                entries.push(e);
            }
        }
        entries.push(command("cmd:settings", "打开设置"));
        let req = request("ai", 8);
        let cfg = Config::default();
        // 预热
        for _ in 0..3 {
            let _ = finalize_entries(entries.clone(), &req, &history, &cfg, Vec::new());
        }
        let iters = 30;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let out = finalize_entries(entries.clone(), &req, &history, &cfg, Vec::new());
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(out.len(), 100);
            samples.push(ms);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "[bench-finalize] candidates={} p50={:.2}ms p90={:.2}ms min={:.2}ms",
            entries.len(),
            samples[iters / 2],
            samples[iters * 9 / 10],
            samples[0]
        );
    }

    /// 基准:文件型 SQLite 上「旧逐候选 2 次 SQL」vs「新批量快照」的成本对照。
    /// `cargo test -p lstr --release bench_finalize_file_db -- --nocapture`
    #[test]
    #[cfg_attr(debug_assertions, ignore = "release 基准:文件型 SQLite")]
    fn bench_finalize_file_db() {
        let db = std::env::temp_dir().join(format!("lstr-hist-bench-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        let history = Arc::new(Mutex::new(History::open_test_db(&db)));
        let mut entries: Vec<Entry> = Vec::new();
        {
            let mut hist = history.lock().unwrap();
            // 1000 条历史(与生产上限一致),其中前 120 条与候选同 id。
            for i in 0..1000 {
                let e = entry_at(
                    &format!("hist{i}"),
                    &format!("hist_{i}.txt"),
                    Kind::File,
                    &format!(r"D:\h\{i}\hist_{i}.txt"),
                );
                hist.seed(&e, 1 + i as u32 % 30, 1_700_000_000 + i as u64);
            }
            for i in 0..320 {
                let e = entry_at(
                    &format!("file{i}"),
                    &format!("ai_report_{i:03}.txt"),
                    Kind::File,
                    &format!(r"D:\work\project{i:03}\ai_report_{i:03}.txt"),
                );
                entries.push(e);
            }
            for i in 0..120 {
                let e = entry_at(
                    &format!("dir{i}"),
                    &format!("ai_dir_{i:03}"),
                    Kind::Folder,
                    &format!(r"E:\use\proj{i:03}\ai_dir_{i:03}"),
                );
                entries.push(e);
            }
            for i in 0..40 {
                let mut e = entry_at(
                    &format!("app{i}"),
                    &format!("A.I. Studio {i}"),
                    Kind::App,
                    &format!(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\App{i}.lnk"),
                );
                e.pinyin = Some("ai studio".to_string());
                entries.push(e);
            }
        }
        let req = request("ai", 8);
        let cfg = Config::default();
        let hist = history.lock().unwrap();
        // 旧 rank 的逐候选主成本:每候选 2 次 SQL。
        let mut sql_samples = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            for e in &entries {
                let _ = hist.score(&e.id);
                let _ = hist.query_boost("ai", &e.id);
            }
            sql_samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        drop(hist);
        let mut samples = Vec::new();
        for _ in 0..15 {
            let t = Instant::now();
            let out = finalize_entries(entries.clone(), &req, &history, &cfg, Vec::new());
            assert_eq!(out.len(), 100);
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        sql_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "[bench-finalize-db] candidates={} 旧逐候选 2×SQL p50={:.2}ms | 新 finalize(批量快照) p50={:.2}ms p90={:.2}ms",
            entries.len(),
            sql_samples[2],
            samples[7],
            samples[13],
        );
        drop(history);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }

    #[test]
    fn exact_name_pin_survives_candidate_truncation() {
        // 105 条前缀命中靠历史分压过 3 条噪声路径里的精确同名：
        // 若不先置顶再截断，精确同名会被 deliver=100 截掉。
        let history = Arc::new(Mutex::new(History::in_memory()));
        let mut entries = Vec::new();
        {
            let mut hist = history.lock().unwrap();
            for i in 0..105 {
                let name = format!("ai-extra-{i:03}");
                let entry = entry_at(&format!("p{i}"), &name, Kind::File, &format!(r"D:\work\{i}\{name}"));
                hist.seed(&entry, 20, 0);
                entries.push(entry);
            }
        }
        for i in 0..3 {
            entries.push(entry_at(
                &format!("exact{i}"),
                "ai",
                Kind::Folder,
                &format!(r"D:\work\node_modules\{i}\ai"),
            ));
        }
        let out = finalize_entries(
            entries,
            &request("ai", 8),
            &history,
            &Config::default(),
            Vec::new(),
        );
        assert_eq!(out.len(), 100, "候选上限仍是 100");
        assert!(
            out.iter().take(3).all(|entry| entry.name == "ai"),
            "截断前必须先把精确同名移到最前"
        );
    }

    #[test]
    fn deliver_cap_grows_for_load_more() {
        // 滚到底加载更多：同一批候选按 200 条交付（首屏上限被放大）。
        let history = Arc::new(Mutex::new(History::in_memory()));
        let make_entries = || {
            (0..250)
                .map(|i| {
                    entry_at(
                        &format!("g{i}"),
                        &format!("ai-{i:03}"),
                        Kind::File,
                        &format!(r"D:\work\{i}\ai-{i:03}"),
                    )
                })
                .collect::<Vec<Entry>>()
        };
        let mut req = request("ai", 8);
        req.deliver = 200;
        let out = finalize_entries(make_entries(), &req, &history, &Config::default(), Vec::new());
        assert_eq!(out.len(), 200, "deliver=200 时应交付 200 条");

        // 首屏默认行为不变：仍是 MIN_DELIVER 条。
        let out = finalize_entries(
            make_entries(),
            &request("ai", 8),
            &history,
            &Config::default(),
            Vec::new(),
        );
        assert_eq!(out.len(), MIN_DELIVER);
    }

    #[test]
    fn load_more_deliver_doubles_and_stops_at_cap() {
        // 默认上限 1000：100→200→400→800→1000，然后停。
        assert_eq!(next_deliver(MIN_DELIVER, 1000), Some(200));
        assert_eq!(next_deliver(200, 1000), Some(400));
        assert_eq!(next_deliver(400, 1000), Some(800));
        assert_eq!(next_deliver(800, 1000), Some(1000));
        assert_eq!(next_deliver(1000, 1000), None);
        // 设置调到 3000 时继续翻倍，最后一档夹到 3000。
        assert_eq!(next_deliver(1000, MAX_LOAD_RESULTS), Some(2000));
        assert_eq!(next_deliver(2000, MAX_LOAD_RESULTS), Some(3000));
        assert_eq!(next_deliver(3000, MAX_LOAD_RESULTS), None);
        // 上限设为 100（等于首屏）时不再加载更多。
        assert_eq!(next_deliver(MIN_DELIVER, MIN_DELIVER), None);
    }

    #[test]
    fn fetch_count_grows_after_first_screen() {
        let q = request("ai", 8).query;
        // 首屏与原来一致：max_results(8) × 50 = 400。
        assert_eq!(fetch_count(&q, 8, MIN_DELIVER), 400);
        // 200 条时原预算（400）仍够用，不额外放大。
        assert_eq!(fetch_count(&q, 8, 200), 400);
        // 800 条时按交付量的 2 倍抓候选（同名去重会吃掉一部分）→ 1600。
        assert_eq!(fetch_count(&q, 8, 800), 1600);
        // 到设置上限 3000 时抓 6000（MAX_FETCH），不再继续放大。
        assert_eq!(fetch_count(&q, 8, MAX_LOAD_RESULTS), MAX_FETCH);
    }
}
/// 「候选阶段零 FS」的标记:内置索引产出的 Entry 只带索引内信息
/// (size 留空,modified 取索引 mtime),真实 size/mtime 与类型漂移校正延迟到
/// finalize 的展示阶段,只对最终候选逐条 `find_meta`。
///
/// key = 条目 id 的小写形式(Windows 路径大小写不敏感);
/// value = 是否需要类型漂移校正(split/单 scope 候选为 true,
/// 精确同名补齐为 false —— 旧 `search_exact` 也不校正)。
#[derive(Default)]
pub(crate) struct PendingMeta {
    /// 同一 id 可能既是 split 候选(true)又是 exact 副本(false):按标记顺序
    /// 保存每次出现的语义,finalize 按 rank 后的条目出现顺序消费。
    map: HashMap<String, Vec<bool>>,
}

impl PendingMeta {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 标记一批来自内置索引的候选。顺序必须与条目进入 finalize 的顺序一致:
    /// split/单 scope 的候选先标记(true),exact 补齐后标记(false)。
    pub(crate) fn mark(&mut self, entries: &[Entry], correct_kind: bool) {
        for entry in entries {
            self.map
                .entry(entry.id.to_lowercase())
                .or_default()
                .push(correct_kind);
        }
    }

    fn modes(&self, id_lower: &str) -> Option<&[bool]> {
        self.map.get(id_lower).map(Vec::as_slice)
    }
}

/// 单条最终候选的元数据填充 + 漂移校正;None = 旧路径同样会丢弃。
/// `correct_kind=false` 用于精确同名补齐(旧 `search_exact` 不校正类型),
/// 只填 size/mtime。
///
/// modified 的取舍:rank 在进入本函数之前已经用候选的**索引 mtime**
/// (`Hit.mtime` → `Entry.modified`)算过 recency,零 FS;这里再用 FS 的
/// `find_meta` 覆盖为**最新值**供预览面板展示。两者可能不同(文件在最近
/// 一次索引扫描后又被修改),这是刻意取舍:排序要避免每个候选一次 FS
/// 调用,展示则要求时间新鲜。索引 mtime 为 0(未知)时,本函数从 FS 拿到
/// 的值仍可补上,从而不影响展示。
#[cfg(test)]
fn fill_one_meta(entry: Entry, correct_kind: bool) -> Option<Entry> {
    // 单条入口(测试 oracle 与无批量上下文用):fresh filler 首个候选必逐条,
    // 行为与旧实现逐字节一致。生产路径走 fill_one_meta_with(共享 filler)。
    fill_one_meta_with(entry, correct_kind, &mut MetaFiller::default())
}

/// 惰性元数据填充的批量优化:同一父目录第 2 个候选起改用一次
/// `list_dir_meta` 目录枚举批量取 size/mtime,取代逐条 `GetFileAttributesExW`
/// (load more 到数千条、结果集中在少数目录时收益明显)。首个候选仍逐条,
/// 避免为单个条目枚举巨型目录;枚举失败/名字歧义一律回退逐条,语义不变。
#[derive(Default)]
struct MetaFiller {
    dirs: HashMap<String, DirMetaState>,
}

enum DirMetaState {
    /// 只见过一个候选:尚未枚举(逐条查)。
    SeenOnce,
    /// 已枚举(可能失败=None);后续候选按名取,取不到回退逐条。
    Batched(Option<lstr_index::win::DirMeta>),
}

impl MetaFiller {
    fn find(&mut self, path: &str) -> Option<(u64, i64, bool)> {
        let p = std::path::Path::new(path);
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            return lstr_index::win::find_meta(path);
        };
        let parent = parent.to_string_lossy().into_owned();
        let name = name.to_string_lossy().into_owned();
        use std::collections::hash_map::Entry;
        match self.dirs.entry(parent) {
            Entry::Vacant(v) => {
                v.insert(DirMetaState::SeenOnce);
                lstr_index::win::find_meta(path)
            }
            Entry::Occupied(mut o) => {
                if matches!(o.get(), DirMetaState::SeenOnce) {
                    let dm = lstr_index::win::list_dir_meta(o.key());
                    o.insert(DirMetaState::Batched(dm));
                }
                let dm = match o.get() {
                    DirMetaState::Batched(d) => d.as_ref(),
                    DirMetaState::SeenOnce => None,
                };
                match dm.and_then(|d| d.get(&name)) {
                    Some(m) => Some((m.size, m.mtime, m.is_dir)),
                    None => lstr_index::win::find_meta(path),
                }
            }
        }
    }
}

fn fill_one_meta_with(entry: Entry, correct_kind: bool, filler: &mut MetaFiller) -> Option<Entry> {
    let Some(path) = entry.path.as_ref() else {
        return Some(entry);
    };
    let path_str = path.to_string_lossy();
    let Some((size, modified, fs_dir)) = filler.find(&path_str) else {
        return None;
    };
    // 候选阶段按索引 attrs 定的 kind:Folder ⇔ 索引说 is_dir。
    let index_is_dir = entry.kind == Kind::Folder;
    if correct_kind && fs_dir != index_is_dir {
        // 旧路径在候选阶段发现漂移后会按 FS kind 重新过 keep:
        // folder↔file 的漂移必然跨出当前 scope/双路分组,最终被丢弃。
        return None;
    }
    let mut entry = entry;
    entry.size = (!fs_dir && size > 0).then_some(size);
    entry.modified = (modified > 0).then_some(modified);
    Some(entry)
}

/// 配置里的排除行模式表(共享槽初始值/测试用)。
fn exclude_patterns(cfg: &Config) -> Vec<String> {
    cfg.rank
        .noise_rules
        .iter()
        .filter(|r| r.exclude)
        .map(|r| r.pattern.clone())
        .collect()
}

#[cfg(test)]
fn finalize_entries(
    entries: Vec<Entry>,
    request: &Request,
    history: &Arc<Mutex<History>>,
    config: &Config,
    keyword_rows: Vec<Entry>,
) -> Vec<Entry> {
    let noise = lstr_index::engine::NoiseRules::from_static(exclude_patterns(config));
    finalize_entries_with_pending(entries, request, history, config, keyword_rows, &noise, None).0
}

/// finalize 的惰性元数据版:rank 之后、去重/截断之前,只对「会进入最终列表」
/// 的候选逐条 `find_meta`(普通条目直到攒够 deliver 个有效候选,精确同名每种
/// kind 最多 6 条),并立即丢弃磁盘已删/无权限/类型漂移的条目。
///
/// 必须在去重之前丢弃:否则已删条目会占掉 id/同名去重名额,把后面仍然有效
/// 的同名候选挤掉(旧路径在候选阶段就丢,不会出现这个问题)。
/// 返回 `(entries, meta_ms)`,meta_ms 供 worker 的 stage 日志分项。
fn finalize_entries_with_pending(
    mut entries: Vec<Entry>,
    request: &Request,
    history: &Arc<Mutex<History>>,
    config: &Config,
    keyword_rows: Vec<Entry>,
    noise: &lstr_index::engine::NoiseRules,
    pending: Option<&PendingMeta>,
) -> (Vec<Entry>, f64) {
    // 排除行硬过滤:索引层已不收录排除目录,但历史/收藏等来源仍可能
    // 带进已排除路径;这里统一剔除,保证「排除=搜不到」对所有来源成立。
    // 目录自身可见(与索引层语义一致),只剔严格位于排除目录之下的条目。
    let exclude = noise.rules();
    if !exclude.is_empty() {
        entries.retain(|e| match &e.path {
            // 书签/命令的 path 是 URL/URI，不是文件系统路径：`/cache/`、
            // `-target/` 这类噪声片段会误伤 `https://…/cache/…`。
            Some(_) if matches!(e.kind, Kind::Bookmark | Kind::Command) => true,
            Some(p) => !exclude
                .path_excluded(&p.to_string_lossy().to_lowercase().replace('\\', "/")),
            None => true,
        });
    }
    // 分项计时:唯一慢查询日志的补充,>3ms 时落 engine.log 定位 rank/去重/
    // 置顶哪一段退化(优化前真实机器 7.3ms 几乎全在 rank 的逐候选 SQL/规则
    // 归一化上)。计时本身 5 次 Instant::now ≈ 0.1µs。
    let t_start = Instant::now();
    let mut t_mark = t_start;
    if let Ok(history) = history.lock() {
        rank::rank_with(
            &mut entries,
            &request.query.text,
            &history,
            config.app_boost,
            &request.rank,
        );
    }
    let t_rank = t_mark.elapsed().as_secs_f64() * 1000.0;
    t_mark = Instant::now();
    let case_sensitive = request.filters.matches.case_sensitive;
    let needle_lower = if request.filters.matches.regex {
        // 正则模式没有“精确同名”语义：pattern 本身不是文件名。
        String::new()
    } else if case_sensitive {
        request.query.text.clone()
    } else {
        request.query.text.to_lowercase()
    };
    let deliver = request.limit.max(request.deliver);
    let mut meta_ms = 0.0;
    let (t_ids, t_names);
    if let Some(pending) = pending {
        // 惰性路径:rank 已排好序,边填充边按旧规则去重。无效条目在
        // seen_* 记账之前就被丢弃,不占名额;后续同名/同 id 候选可正常补位。
        let t_meta = Instant::now();
        let mut seen_ids = HashSet::with_capacity(entries.len());
        let mut name_counts: HashMap<(Kind, String), usize> = HashMap::with_capacity(entries.len());
        let mut kept = Vec::with_capacity(deliver.saturating_add(8));
        // 超过同名配额的非精确条目不再丢弃,而是排到列表尾部:
        // 首屏仍保持同名多样性,滚动/加载更多又能看到全部命中。
        let mut extras: Vec<Entry> = Vec::new();
        let mut non_exact = 0usize;
        // 同一 id 的 split/exact 副本按出现顺序消费 PendingMeta 里的语义。
        let mut pending_uses: HashMap<String, usize> = HashMap::new();
        // 跨候选共享的元数据填充器:同父目录批量枚举(见 MetaFiller)。
        let mut filler = MetaFiller::default();
        for entry in entries {
            let name_lower = entry.name.to_lowercase();
            let is_exact = !needle_lower.is_empty()
                && if case_sensitive {
                    entry.name == needle_lower
                } else {
                    name_lower == needle_lower
                };
            // 普通条目按交付预算收够为止。
            if !is_exact && non_exact >= deliver {
                continue;
            }
            let name_key = (entry.kind, name_lower);
            // 精确同名不设上限:用户打的就是完整文件名(如 ucrtbase.dll),
            // 每个不同路径都是有效结果;旧实现只留 6 条,导致「总数 147、
            // 列表只有 8 条、D 盘命中全被砍掉」。非精确同名仍优先保多样性。
            let over_cap =
                !is_exact && name_counts.get(&name_key).copied().unwrap_or(0) >= SAME_NAME_KEEP;
            let id_lower = entry.id.to_lowercase();
            // 绝大多数 id 只有一个标记;只有 split+exact 同 id 才需要游标。
            let mode = match pending.modes(&id_lower) {
                Some([mode]) => Some(*mode),
                Some(modes) => {
                    let used = pending_uses.entry(id_lower.clone()).or_insert(0);
                    let mode = modes.get(*used).copied();
                    *used += 1;
                    mode
                }
                None => None,
            };
            if seen_ids.contains(&id_lower) {
                continue;
            }
            let filled = match mode {
                Some(correct_kind) => fill_one_meta_with(entry, correct_kind, &mut filler),
                None => Some(entry),
            };
            let Some(entry) = filled else { continue };
            seen_ids.insert(id_lower);
            if !is_exact {
                *name_counts.entry(name_key).or_insert(0) += 1;
                non_exact += 1;
            }
            if over_cap {
                extras.push(entry);
            } else {
                kept.push(entry);
            }
        }
        kept.append(&mut extras);
        entries = kept;
        meta_ms = t_meta.elapsed().as_secs_f64() * 1000.0;
        t_ids = 0.0;
        t_names = 0.0;
    } else {
        // 无内置候选:保持旧行为,对全部候选去重(不再有 FS 调用,也没有
        // 需要丢弃的无效条目)。
        // 去重：retain 闭包持有 seen 集合，一遍完成（旧实现先算 Vec<bool> 掩码
        // 再 retain，两遍且多一次分配；结果完全相同：保留首次出现的条目）。
        // Windows 路径/ID 大小写不敏感，索引与历史对同一个路径给的
        // 大小写可能不同，所以 id 先归一化成小写再比较，否则同一条目出现两行。
        let mut seen_ids: HashSet<String> = HashSet::with_capacity(entries.len());
        entries.retain(|entry| seen_ids.insert(entry.id.to_lowercase()));
        t_ids = t_mark.elapsed().as_secs_f64() * 1000.0;
        t_mark = Instant::now();
        // 非精确同名条目每种前 SAME_NAME_KEEP 个保持原位，其余追加到列表尾部：
        // 不丢结果（总数与可滚动条数一致），首屏也不会被同名变体灌满。
        // 精确同名（用户打的就是完整文件名）完全不设上限。
        let mut name_counts: HashMap<(Kind, String), usize> = HashMap::with_capacity(entries.len());
        let mut kept: Vec<Entry> = Vec::with_capacity(entries.len());
        let mut extras: Vec<Entry> = Vec::new();
        for entry in entries.drain(..) {
            let name_lower = entry.name.to_lowercase();
            let is_exact = !needle_lower.is_empty()
                && if case_sensitive {
                    entry.name == needle_lower
                } else {
                    name_lower == needle_lower
                };
            let key = (entry.kind, name_lower);
            if is_exact {
                kept.push(entry);
                continue;
            }
            let count = name_counts.entry(key).or_insert(0);
            *count += 1;
            if *count <= SAME_NAME_KEEP {
                kept.push(entry);
            } else {
                extras.push(entry);
            }
        }
        kept.append(&mut extras);
        entries = kept;
        t_names = t_mark.elapsed().as_secs_f64() * 1000.0;
    }
    t_mark = Instant::now();
    // 收藏置顶(Pin):收藏过的条目钉到结果前列;精确同名随后置顶,仍最优先。
    pin_favorites(&mut entries, &request.favorites);
    // 名称完全一致的结果硬置顶：档位权重可能被用户调整，或被历史/位置加权翻过。
    pin_exact_name_matches(&mut entries, &needle_lower, case_sensitive);
    let t_pin = t_mark.elapsed().as_secs_f64() * 1000.0;
    t_mark = Instant::now();
    // 列表可滚动：多送一些候选（最多 100 条），UI 只按窗口高度展示前几条，
    // 滚轮可以继续往下看——不再一屏截断。
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
    let tail_ms = t_mark.elapsed().as_secs_f64() * 1000.0;
    let total_ms = t_start.elapsed().as_secs_f64() * 1000.0;
    // 测试不写 APPDATA 日志;生产上 finalize >3ms 时落一条分项,便于现场定位。
    #[cfg(not(test))]
    if total_ms > 3.0 || profile_queries() {
        lstr_index::snapshot::debug_log(&format!(
            "search: finalize {total_ms:.1}ms rank={t_rank:.1}ms meta={meta_ms:.1}ms ids={t_ids:.1}ms names={t_names:.1}ms pin={t_pin:.1}ms tail={tail_ms:.1}ms entries={}",
            entries.len()
        ));
    }
    let _ = (t_rank, t_ids, t_names, t_pin, tail_ms, total_ms);
    (entries, meta_ms)
}

/// 内置索引能否自带时间筛选:目录枚举(dir 首扫)会把 mtime 写进快照;
/// incremental="usn"(默认)固定走 dir 首扫(见 config 注释),同样有 mtime。
/// 纯 USN 首扫(旧 scan_mode="usn")没有 mtime,日期筛选无法工作。
fn builtin_index_has_mtime(index: &crate::config::IndexConfig) -> bool {
    index.scan_mode == "dir" || index.incremental == "usn"
}

/// 搜索 worker 的启动参数：字段多且都是「一次注入、全程只读」的句柄，
/// 打包成结构体避免长参数列表（也便于调用点按名字对照）。
struct WorkerArgs {
    rx: Receiver<Request>,
    tx: Sender<Response>,
    history: Arc<Mutex<History>>,
    config: Config,
    ctx: egui::Context,
    apps_dirty: Arc<AtomicBool>,
    bookmarks_enabled: Arc<AtomicBool>,
    bookmark_folders: Arc<Mutex<HashMap<String, String>>>,
    status_slot: Arc<Mutex<Option<builtin::IndexUiStatus>>>,
    index_cmd_rx: Receiver<ClientCommand>,
    /// 控制指令发送端(run_worker 的排除规则变更检测用它发 Rescan 掩码)。
    index_cmd_tx: Sender<ClientCommand>,
    volume_mask: Arc<AtomicU32>,
    /// 排除规则共享槽(与 SearchService 共享,重建时换表)。
    noise_slot: Arc<Mutex<Vec<String>>>,
    shutdown_client: Arc<AtomicBool>,
    worker_collected: Arc<AtomicBool>,
    client_done: Arc<AtomicBool>,
}

/// 列目录响应：路径模式与深路径浏览共用（不碰内置引擎）。
fn browse_response(
    dir: &str,
    filter: &str,
    request: &Request,
    dir_cache: &mut browse::DirCache,
    history: &Arc<Mutex<History>>,
    config: &Config,
) -> Response {
    let started = Instant::now();
    let needle = filter.to_lowercase();
    let scope = request.query.scope;
    // 只在 Browse（browse_root）下应用本地筛选；PathTyping 的筛选条按旧语义不生效。
    let local_filters = request.browse_root.is_some();
    let time_after = local_filters
        .then(|| request.filters.time.map(|time| time.since_unix()))
        .flatten();
    let group = local_filters.then_some(request.filters.group).flatten();
    let modified = local_filters
        .then_some(request.filters.modified.as_ref())
        .flatten();
    let size_range = local_filters
        .then_some(request.filters.size.as_ref())
        .flatten();
    // 盘符 chip 在浏览模式下同样生效：目录内条目都属于同一卷，
    // 掩码不匹配时整目录为空。
    let drive_mask = local_filters
        .then(|| request.filters.drive_mask())
        .flatten();
    let mut entries: Vec<Entry> = dir_cache
        .children(dir)
        .iter()
        .filter(|entry| match scope {
            Scope::All => true,
            Scope::Folder => entry.kind == Kind::Folder,
            Scope::Program => entry.kind == Kind::Program,
            Scope::File => entry.kind == Kind::File,
            Scope::App | Scope::Bookmark | Scope::Command => false,
        })
        .filter(|entry| {
            drive_mask.is_none_or(|mask| {
                entry
                    .path
                    .as_ref()
                    .is_none_or(|path| filters::path_in_drive_mask(path, mask))
            })
        })
        .filter(|entry| {
            time_after.is_none_or(|after| entry.modified.is_some_and(|mtime| mtime >= after))
        })
        .filter(|entry| {
            modified.is_none_or(|range| {
                entry.modified.is_some_and(|mtime| {
                    range.after.is_none_or(|after| mtime >= after)
                        && range.before.is_none_or(|before| mtime <= before)
                })
            })
        })
        .filter(|entry| {
            // 大小只对文件有意义；文件夹的 size 为 None，条件下一律排除。
            size_range.is_none_or(|range| {
                entry.size.is_some_and(|size| {
                    range.after.is_none_or(|after| size >= after)
                        && range.before.is_none_or(|before| size <= before)
                })
            })
        })
        .filter(|entry| {
            // 类型筛选只过滤文件，文件夹保留用于继续导航。
            if entry.kind == Kind::Folder {
                return true;
            }
            group.is_none_or(|group| {
                entry
                    .name
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| {
                        group
                            .extensions()
                            .iter()
                            .any(|candidate| ext.eq_ignore_ascii_case(candidate))
                    })
            })
        })
        .filter(|entry| {
            needle.is_empty()
                || entry.name.to_lowercase().contains(&needle)
                || entry
                    .pinyin
                    .as_deref()
                    .map(|pinyin| pinyin.to_lowercase().contains(&needle))
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    let total = entries.len();
    if let Ok(history) = history.lock() {
        rank::rank_with(
            &mut entries,
            filter,
            &history,
            config.app_boost,
            &request.rank,
        );
    }
    // 浏览目录不是「搜索前 N 条」：放宽上限，列表靠 UI 滚动查看。
    // 加载更多时跟随 deliver 一起放大。
    entries.truncate(request.limit.max(request.deliver).max(200));
    Response {
        generation: request.generation,
        entries,
        total,
        elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
        error: None,
    }
}

/// 书签索引重建后把「id → 收藏夹层级」整体推给 UI（行副标题用）。
fn publish_bookmark_folders(
    slot: &Arc<Mutex<HashMap<String, String>>>,
    bookmarks: &bookmarks::BookmarkIndex,
) {
    if let Ok(mut map) = slot.lock() {
        *map = bookmarks.folder_map();
    }
}

fn run_worker(args: WorkerArgs) {
    let WorkerArgs {
        rx,
        tx,
        history,
        config,
        ctx,
        apps_dirty,
        bookmarks_enabled,
        bookmark_folders,
        status_slot,
        index_cmd_rx,
        index_cmd_tx,
        volume_mask,
        noise_slot,
        shutdown_client,
        worker_collected,
        client_done,
    } = args;
    // finalize 硬过滤 + 引擎共用同一张排除表(实时取,重建后即新规则)。
    let noise = lstr_index::engine::NoiseRules::from_slot(Arc::clone(&noise_slot));
    lstr_index::snapshot::debug_log(&format!(
        "run_worker: noise slot patterns={}",
        noise_slot.lock().map(|g| g.len()).unwrap_or(0)
    ));
    // 「立即重建」状态:槽内容与已应用表比对;待重扫卷 3s 未收到 ScanBegin 则重发。
    let mut noise_applied_raw = noise_slot.lock().unwrap().clone();
    let mut pending_rescan: Vec<(char, Instant)> = Vec::new();
    let mut last_resend = Instant::now();
    // 目录扫描(dir)会把 mtime 写进快照。纯 USN 首扫(旧 scan_mode="usn")
    // 没有 mtime，日期筛选只能明确报错，不能静默返回空。
    let builtin_time_ok = builtin_index_has_mtime(&config.index);
    // —— 自研索引（唯一后端）:engine 由本线程独占;client 线程推 EngineMsg ——
    // 有界通道(背压):client 线程跑得快时会阻塞在 send,防止整个卷的记录
    // 积压在通道内存里(实测无界时 UI 进程冲到 877MB)。
    let (engine_tx, engine_rx) = mpsc::sync_channel::<EngineMsg>(16);
    let handshake_slot: Arc<Mutex<Vec<(u128, u64, i64, u64, bool)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let mut engine = builtin::make_engine(
        &config.index,
        lstr_index::engine::NoiseRules::from_slot(Arc::clone(&noise_slot)),
    );
    // 分批 apply 的块间让路检查:对账/首扫的记录按块消化,块间发现
    // 等待中的查询(QUERY_INFLIGHT>0)立即收手,让主循环出队服务。
    engine.set_query_yield_hook(Some(Box::new(|| {
        QUERY_INFLIGHT.load(Ordering::Relaxed) > 0
    })));
    *handshake_slot.lock().unwrap() = engine.handshake_state();
    builtin::spawn_client_thread(
        Arc::clone(&handshake_slot),
        engine_tx.clone(),
        Arc::clone(&shutdown_client),
        false,
        config.index.clone(),
        index_cmd_rx,
        volume_mask,
        Arc::clone(&client_done),
    );
    let mut builtin = Some(builtin::BuiltinProvider::new(engine));
    let publish_status = |builtin: Option<&builtin::BuiltinProvider>,
                          slot: &Arc<Mutex<Option<builtin::IndexUiStatus>>>| {
        if let Some(b) = builtin {
            *slot.lock().unwrap() = Some(b.status());
        }
    };
    publish_status(builtin.as_ref(), &status_slot);
    let mut apps = apps::AppIndex::load();
    let mut apps_loaded = Instant::now();
    // 浏览器书签：启动即加载（本机 500 条量级，JSON 解析毫秒级）。
    // 关闭时保持空索引，搜索路径也不会再查询它。
    let mut bookmarks = if bookmarks_enabled.load(Ordering::Relaxed) {
        bookmarks::BookmarkIndex::load()
    } else {
        bookmarks::BookmarkIndex::empty()
    };
    let mut bookmarks_loaded = Instant::now();
    let mut bookmarks_stamp = bookmarks::source_stamp();
    publish_bookmark_folders(&bookmark_folders, &bookmarks);
    if bookmarks_enabled.load(Ordering::Relaxed) {
        lstr_index::snapshot::debug_log(&format!(
            "bookmarks: loaded {} entries",
            bookmarks.count()
        ));
    }
    let mut dir_cache = browse::DirCache::new();
    let (uwp_tx, uwp_rx) = mpsc::channel::<Vec<Entry>>();
    // UWP 枚举要走 PowerShell（Get-StartApps，冷启动 ~2s）。
    // 懒加载：只在第一次真正需要应用结果时才启动 PowerShell；
    // 只用文件/文件夹搜索的用户，PowerShell 永远不会被拉起。
    let mut uwp_started = false;

    // 主循环改为超时驱动:空闲时也要消费引擎消息、发布状态,
    // 否则首装扫描期用户不击键 → 引擎停摆(实测抓出)。
    let mut last_phase: Option<lstr_index::Phase> = None;
    loop {
        // 优雅退出:shutdown 置位后立即结束请求循环 → flush_all → 等 client
        // 线程确认 worker 已收尾。不再依赖「SearchService 被 drop → 通道断开」
        // —— 进程退出时后台线程会被硬杀,断开路径常常来不及跑完。
        if shutdown_client.load(Ordering::Relaxed) {
            break;
        }
        // 0. 先看有没有待处理请求:用户打字时优先查询。维护(整卷 compact 实测
        //    250 万条要 1-2s、落盘 120MB)让路到空闲再跑,否则「输入 bm 后列表
        //    卡几秒才刷」就是被这里堵住的。
        let pending = match rx.try_recv() {
            Ok(r) => {
                query_inflight_dec();
                Some(r)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => break,
        };
        let mut maintenance_busy = false;
        if pending.is_none() {
            let idle_start = Instant::now();
            // 1. 应用自研索引的增量 + 维护(仅空闲路径)
            if let Some(b) = &mut builtin {
                // 分段保存进行中:store 冻结,不再向外拉消息(client 线程在
                // 有界通道上背压即可),只推进保存一小步;查询仍在主循环优先
                // 出队。修改类消息由 Engine::handle 排队兜底。
                if b.engine.save_in_progress() {
                    b.engine.maintain();
                    *handshake_slot.lock().unwrap() = b.engine.handshake_state();
                    maintenance_busy = true;
                } else {
                    // 对账/首扫的分批记录:先消化引擎缓冲。advance 在块间查让路
                    // 钩子,有等待查询时返回 0 → 收手,让下面优先出队服务。
                    while b.engine.has_pending_apply() {
                        if b.engine.advance_pending_apply() == 0 {
                            break;
                        }
                    }
                    let mut applied = 0usize;
                    while let Ok(msg) = engine_rx.try_recv() {
                        b.engine.handle(msg);
                        applied += 1;
                        if applied >= 400 {
                            break;
                        }
                        // 打字优先(0294602 的 apply 中段扩展):消息间隙发现等待中
                        // 的查询、或分批记录还没消化完 → 立即收手回到主循环出队;
                        // 空闲时仍整轮消化,对账吞吐不受损。
                        if QUERY_INFLIGHT.load(Ordering::Relaxed) > 0
                            || b.engine.has_pending_apply()
                        {
                            break;
                        }
                    }
                    b.engine.maintain();
                    *handshake_slot.lock().unwrap() = b.engine.handshake_state();
                    // 分片 compact 也要持续推进:maintenance_busy 让主循环立刻
                    // 回来再给下一片,否则会阻塞在 recv_timeout(150ms) 上。
                    maintenance_busy = b.engine.save_in_progress()
                        || b.engine.has_deferred_msgs()
                        || b.engine.compact_in_progress();
                }
            }
            // —— 排除规则表变化检测(「立即重建」)——
            // 槽内容与已应用表不符 → 引擎重置受影响卷 + Rescan 掩码重扫
            // (盘符锚定规则只重扫对应盘,段名规则重扫全部盘)。
            // 待重扫卷 3s 未收到 ScanBegin 则重发(自愈)。
            {
                let cur_raw = noise_slot.lock().unwrap().clone();
                let norm = |list: &[String]| -> std::collections::BTreeSet<String> {
                    list.iter()
                        .filter_map(|p| lstr_index::noise::normalize_pattern(p))
                        .collect()
                };
                if norm(&cur_raw) != norm(&noise_applied_raw) {
                    let cur = norm(&cur_raw);
                    let old = norm(&noise_applied_raw);
                    let diff: Vec<String> = cur
                        .difference(&old)
                        .chain(old.difference(&cur))
                        .cloned()
                        .collect();
                    let mut letters: Vec<char> = Vec::new();
                    let mut all = false;
                    for p in &diff {
                        match lstr_index::noise::affected_drives_for_pattern(p) {
                            None => all = true,
                            Some(ds) => letters.extend(ds),
                        }
                    }
                    let scope: Option<Vec<char>> = if all {
                        None
                    } else {
                        letters.sort_unstable();
                        letters.dedup();
                        (!letters.is_empty()).then_some(letters)
                    };
                    let (affected, mask) = if let Some(b) = &mut builtin {
                        let affected = b.engine.reset_volumes_for_noise(scope.as_deref());
                        *handshake_slot.lock().unwrap() = b.engine.handshake_state();
                        let mask = match &scope {
                            None => 0u32,
                            Some(ls) => ls.iter().fold(0u32, |m, l| {
                                m | (1 << (l.to_ascii_uppercase() as u8 - b'A'))
                            }),
                        };
                        (affected, mask)
                    } else {
                        (scope.clone().unwrap_or_default(), 0)
                    };
                    let _ = index_cmd_tx.send(ClientCommand::RescanVolumes(mask));
                    pending_rescan = affected.iter().map(|l| (*l, Instant::now())).collect();
                    last_resend = Instant::now();
                    lstr_index::snapshot::debug_log(&format!(
                        "main: noise rules changed, reset {:?}, rescan mask=0x{mask:08x}",
                        affected
                    ));
                    noise_applied_raw = cur_raw;
                }
                // 待重扫自愈:重发后 3s 仍未收到 ScanBegin 的卷 → 重发 Rescan
                if !pending_rescan.is_empty()
                    && last_resend.elapsed() >= Duration::from_secs(3)
                {
                    if let Some(b) = &builtin {
                        let with_store = b.engine.letters_with_store();
                        pending_rescan.retain(|(l, _)| !with_store.contains(l));
                        if !pending_rescan.is_empty() {
                            let mask = pending_rescan.iter().fold(0u32, |m, (l, _)| {
                                m | (1 << (l.to_ascii_uppercase() as u8 - b'A'))
                            });
                            let _ =
                                index_cmd_tx.send(ClientCommand::RescanVolumes(mask));
                            lstr_index::snapshot::debug_log(&format!(
                                "main: rescan retry, mask=0x{mask:08x} (volumes={pending_rescan:?})"
                            ));
                        }
                        last_resend = Instant::now();
                    }
                }
            }
            let idle_ms = idle_start.elapsed().as_millis();
            if idle_ms > 100 {
                lstr_index::snapshot::debug_log(&format!(
                    "search: idle maintenance took {idle_ms}ms"
                ));
            }
        }
        publish_status(builtin.as_ref(), &status_slot);
        // 状态变化时唤醒 UI(横幅进度条不击键也要动)
        {
            let st = status_slot.lock().unwrap();
            let phase = st.as_ref().map(|s| s.phase);
            if phase != last_phase {
                last_phase = phase;
                drop(st);
                ctx.request_repaint();
            }
        }
        // 2. 取请求(有 pending 直接用;否则 150ms 超时 → 回到维护)
        let mut request = match pending {
            Some(r) => r,
            None => {
                // 保存/积压消息待推进:不阻塞等请求,立即回主循环继续维护
                // (顶部 try_recv 仍会最优先服务刚到的查询)。
                if maintenance_busy {
                    continue;
                }
                match rx.recv_timeout(Duration::from_millis(150)) {
                    Ok(r) => {
                        query_inflight_dec();
                        r
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        };
        while let Ok(uwp_entries) = uwp_rx.try_recv() {
            apps.append_uwp(uwp_entries);
        }
        // 应用索引时效：TTL 到点或「重新加载」信号到了就重建。
        // 重载后 UWP 懒加载标志复位，下次需要应用结果时重新枚举。
        let forced = apps_dirty.swap(false, Ordering::Relaxed);
        if forced || apps_loaded.elapsed() > APPS_RELOAD_INTERVAL {
            apps = apps::AppIndex::load();
            uwp_started = false;
            apps_loaded = Instant::now();
        }
        // 书签索引：「重新加载」立即重建；平时按 TTL + 源文件 mtime 判断，
        // 浏览器没改书签就不重复解析。
        if forced || bookmarks_loaded.elapsed() > BOOKMARKS_RELOAD_INTERVAL {
            let enabled = bookmarks_enabled.load(Ordering::Relaxed);
            let stamp = bookmarks::source_stamp();
            if !enabled {
                // 开关刚被关掉（或配置里就是关的）：清空，结果里不再出现书签。
                bookmarks = bookmarks::BookmarkIndex::empty();
                bookmarks_stamp.clear();
                bookmarks_loaded = Instant::now();
                publish_bookmark_folders(&bookmark_folders, &bookmarks);
            } else if forced || stamp != bookmarks_stamp {
                bookmarks = bookmarks::BookmarkIndex::load();
                bookmarks_stamp = stamp;
                bookmarks_loaded = Instant::now();
                publish_bookmark_folders(&bookmark_folders, &bookmarks);
                lstr_index::snapshot::debug_log(&format!(
                    "bookmarks: reloaded {} entries",
                    bookmarks.count()
                ));
            } else {
                bookmarks_loaded = Instant::now();
            }
        }
        while let Ok(next) = rx.try_recv() {
            // 只保留最新请求(取消语义),每个出队的都要回退等待计数
            request = next;
            query_inflight_dec();
        }
        // 诊断:区分「排队慢(worker 忙于引擎维护/落盘)」与「处理慢(IPC 探测等)」。
        {
            let queued_ms = request.sent_at.elapsed().as_millis();
            if queued_ms > 200 {
                lstr_index::snapshot::debug_log(&format!(
                    "search: worker begin gen={} queued={}ms",
                    request.generation, queued_ms
                ));
            }
        }
        // 诊断:记录本次请求的解析结果与索引阶段。
        // 仅剖析模式(LSTR_PROFILE_QUERY)逐查询落盘;默认不写——旧实现每查询
        // 无条件 open/append/close 一次日志,是热路径上的无谓 IO(慢查询另有分项日志)。
        if profile_queries() {
            let phase = builtin
                .as_ref()
                .map(|b| b.status().phase)
                .unwrap_or(lstr_index::Phase::Fallback);
            lstr_index::snapshot::debug_log(&format!(
                "search: req gen={} text={:?} scope={:?} phase={:?} builtin={}",
                request.generation,
                request.query.text,
                request.query.scope,
                phase,
                builtin.is_some(),
            ));
        }

        // P0-2：深路径浏览模式。空过滤词=列直接子项（导航）；有过滤词=在
        // 当前目录下递归搜索（走正常引擎 + path_prefix=root），因此子文件夹
        // 里的内容也能命中，时间/类型/匹配选项同样生效。
        if let Some(root) = request.browse_root.clone() {
            let raw = request.query.raw.clone();
            if raw.trim().is_empty() {
                let mut dir = root.to_string_lossy().into_owned();
                if !dir.ends_with(['\\', '/']) {
                    dir.push('\\');
                }
                let response = browse_response(
                    &dir,
                    "",
                    &request,
                    &mut dir_cache,
                    &history,
                    &config,
                );
                send_response(&tx, &request, response);
                ctx.request_repaint();
                continue;
            }
            let scope = request.query.scope;
            request.query = if request.filters.matches.regex {
                query::parse_regex(&raw, scope)
            } else {
                query::parse_plain(&raw, scope)
            };
            request.query.path_filter = Some(root.to_string_lossy().into_owned());
        }

        // P0-2：目录上下文。用户显式输入路径模式时退出，手输 path: 时以用户为准，
        // 避免把「在当前目录里搜」悄悄套到用户指定的另一个目录上。
        let in_folder_context = request.base_folder.is_some()
            && request.browse_root.is_none()
            && !request.filters.matches.regex
            && browse::parse(&request.query.raw).is_none();
        // 输入框路径 chip 同样以用户为准：它和目录上下文是同一语义，
        // 不能让 Explorer 的当前目录把用户显式选的目录盖掉。
        if in_folder_context
            && request.query.path_filter.is_none()
            && request.filters.path.is_none()
        {
            if let Some(folder) = &request.base_folder {
                let dir = folder.to_string_lossy().into_owned();
                if !dir.is_empty() {
                    request.query.path_filter = Some(dir);
                }
            }
        }

        // 路径模式（D:\foo\bar、\\server\share）：直接列目录，
        // 纯内置索引，也不必为 Everything 懒启动 PowerShell 枚举 UWP。
        if let Some(path_query) = if request.filters.matches.regex
            || request.browse_root.is_some()
        {
            None
        } else {
            browse::parse(&request.query.raw)
        } {
            let path_query = browse::resolve_exact(path_query);
            let response = browse_response(
                &path_query.dir,
                &path_query.filter,
                &request,
                &mut dir_cache,
                &history,
                &config,
            );
            send_response(&tx, &request, response);
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
            entries.truncate(request.limit.max(request.deliver).max(200));
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

        // 目录/路径范围或纯文件过滤（ext:/path:/时间/类型/盘符）只搜文件：
        // 应用、UWP、命令、关键字都不应该混进来。
        let path_scoped = request.browse_root.is_some()
            || request.base_folder.is_some()
            || request.query.path_filter.is_some()
            || request.query.path_components.is_some()
            || browse::parse(&request.query.raw).is_some();
        let file_filter_active = path_scoped
            || request.query.ext.is_some()
            || request.query.prefix.is_some()
            || request.query.suffix.is_some()
            || request.query.path_filter.is_some()
            || request.query.path_components.is_some()
            || request.query.drive_mask().is_some()
            || !request.filters.is_empty();
        let needs_uwp = !file_filter_active
            && !request.filters.matches.regex
            && match request.query.scope {
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
        // 旧 scan_mode="usn" 的纯 USN 首扫没有 mtime：日期/时间条件无法成立。
        // 明确提示，避免用户把「0 结果」误当成「真的没有匹配文件」。
        if (request.filters.time.is_some() || request.filters.modified.is_some())
            && !builtin_time_ok
        {
            error = Some(
                "当前索引模式不记录修改时间，无法按日期筛选（请改用 incremental=usn）"
                    .to_string(),
            );
        }
        // 分项耗时（诊断「引擎 35ms 花在哪」）：split=一次 All-kind 双路查询
        // （旧实现 folders/files 各一次）、exact 补齐、apps 应用索引，
        // finalize 合并打分排序。>15ms 的慢查询落 engine.log。
        let mut diag_split = 0f32;
        let mut diag_apps = 0f32;
        let mut diag_bookmarks = 0f32;
        let mut diag_exact = 0f32;
        // 自定义关键字行（g rust → 用 Google 搜索）：之后钉在结果顶部，不参与打分。
        let mut keyword_rows: Vec<Entry> = Vec::new();
        // 内置索引候选的惰性元数据标记:modified 已由候选阶段用索引 mtime
        // 填好(供 rank 零 FS 打分),size/展示用 mtime 由 finalize 只对最终
        // 展示条目逐条 find_meta(见 finalize_entries_with_pending)。
        let mut pending_meta = PendingMeta::new();

        // 输入框为空但筛选/日期条件仍生效时，不能走「最近使用」分支：
        // 条件单独查询要交给引擎做全表过滤（空 token 的过滤器路径）。
        if request.query.is_empty() && request.filters.is_empty() {
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
                // 收藏的应用置顶：文件夹 Quick Switch 不再收应用，这里
                // 是它们唯一的入口（空输入即见，Enter 启动）。
                let mut pinned: Vec<Entry> = request
                    .favorites
                    .iter()
                    .map(crate::assets::favorite_entry)
                    .filter(|entry| entry.kind == Kind::App)
                    .collect();
                pinned.retain(|entry| !entries.iter().any(|e| e.id == entry.id));
                if !pinned.is_empty() {
                    let room = request.limit.saturating_sub(pinned.len());
                    entries.truncate(room);
                    pinned.append(&mut entries);
                    entries = pinned;
                }
            } else if request.query.scope == Scope::Command {
                entries = commands("", &config.commands);
            } else if request.query.scope == Scope::Bookmark {
                // 书签页空输入 = 按名称列出全部书签（与应用页「空输入列应用」一致）；
                // 取 deliver 而不是 limit：几百条书签也能滚到底「加载更多」。
                entries = if bookmarks_enabled.load(Ordering::Relaxed) {
                    bookmarks.top(request.deliver.max(request.limit))
                } else {
                    Vec::new()
                };
            }
            total = entries.len();
            // 书签页空输入的 total 用真实总数：UI 只在 entries.len() < total
            // 时才发「加载更多」，否则 482 条书签只能看到首屏 100 条。
            if request.query.scope == Scope::Bookmark
                && bookmarks_enabled.load(Ordering::Relaxed)
            {
                total = bookmarks.count();
            }
        } else {
            match request.query.scope {
                Scope::App => {
                    entries = apps.search(&request.query.text, request.limit);
                    entries.retain(|entry| {
                        app_matches_options(&entry.name, &request.query.text, &request.filters)
                    });
                    total = entries.len();
                }
                Scope::Bookmark => {
                    let take = request.deliver.max(request.limit);
                    entries = if bookmarks_enabled.load(Ordering::Relaxed) {
                        if request.query.text.is_empty() {
                            bookmarks.top(take)
                        } else {
                            let mut hits = bookmarks.search(&request.query.text, take);
                            hits.retain(|entry| {
                                app_matches_options(
                                    &entry.name,
                                    &request.query.text,
                                    &request.filters,
                                )
                            });
                            hits
                        }
                    } else {
                        Vec::new()
                    };
                    total = entries.len();
                }
                Scope::Command => {
                    entries = commands(&request.query.text, &config.commands);
                    total = entries.len();
                }
                Scope::All => {
                    // 有筛选时只搜文件：位置/时间/类型对应用和命令没有意义，
                    // 硬塞进来只会让人疑惑「选了 D 盘怎么还出现微信」。
                    if request.filters.is_empty()
                        && !in_folder_context
                        && !file_filter_active
                        && !request.filters.matches.regex
                    {
                        keyword_rows = keyword_entries(&request.keywords, &request.query.text);
                    }
                    let mut needs_exact = false;
                    let fetch = fetch_count(&request.query, request.limit, request.deliver);
                    // 加载更多时同比例放宽文件夹候选预算；首屏仍以 120 封顶（行为不变）。
                    let folder_cap = 120 + request.deliver.saturating_sub(MIN_DELIVER) / 4;
                    let folder_fetch = (fetch / 5).clamp(40, folder_cap);
                    let file_fetch = fetch.saturating_sub(folder_fetch).max(60);
                    let mut truncation = 0usize;
                    // split 并趟产出的精确同名补齐(旧 search_exact 第二趟)。
                    let mut split_exact: Option<Vec<Entry>> = None;
                    let mut split_exact_ms = 0f32;
                    if let Some(b) = &builtin {
                        // 内置索引:本地引擎,双路语义在 kind 过滤层保持
                            // H4:内置双路都用同一 spec 时,旧实现把同一个 All-kind
                            // 总数加了两次(total=2N),且 truncation 恒 >0 → 每次
                            // All 查询都走“先发一版 + exact 补”两阶段。
                            // 合并前:folders 路按 KindFilter::Folders 统计,files 路
                            // 按 All 统计后扣掉文件夹数。现在一次 split 查询同时返回
                            // total_folders 与 total_all,口径与旧实现逐字节一致;
                            // 候选遍历从两遍降为一遍。
                            let t_split = Instant::now();
                            let r_split = b.search_split_once(
                                &request.query,
                                &request.filters,
                                folder_fetch,
                                file_fetch,
                            );
                            match r_split {
                                Ok(mut result) => {
                                    split_exact_ms = result.exact_ms;
                                    diag_split = (t_split.elapsed().as_secs_f32() * 1000.0
                                        - split_exact_ms)
                                        .max(0.0);
                                    truncation += result
                                        .folder_total
                                        .saturating_sub(result.folders.len());
                                    let file_total = result
                                        .all_total
                                        .saturating_sub(result.folder_total);
                                    truncation += file_total.saturating_sub(result.files.len());
                                    total += result.folder_total + file_total;
                                    // 旧实现先 folders 后 files,保持同一 Entry 顺序。
                                    // 候选只带索引信息,由 finalize 内统一填 FS 元数据。
                                    pending_meta.mark(&result.folders, true);
                                    pending_meta.mark(&result.files, true);
                                    entries.append(&mut result.folders);
                                    entries.append(&mut result.files);
                                    // 前缀补集(A-3):宽查询下保证名字以查询词开头的命中
                                    // 进入排序,与精确同名补集一样不做类型漂移校正。
                                    if !result.prefix.is_empty() {
                                        pending_meta.mark(&result.prefix, false);
                                        entries.append(&mut result.prefix);
                                    }
                                    if !result.exact.is_empty() {
                                        // 精确同名补齐不做类型漂移校正(旧路径也不做)。
                                        pending_meta.mark(&result.exact, false);
                                    }
                                    split_exact = Some(result.exact);
                                    needs_exact = truncation > 0;
                                }
                                Err(err) => {
                                    diag_split = t_split.elapsed().as_secs_f32() * 1000.0;
                                    error = Some(err)
                                }
                            }
                    }
                    if request.filters.is_empty()
                        && !in_folder_context
                        && !file_filter_active
                        && !request.filters.matches.regex
                    {
                        let t_apps = Instant::now();
                        let mut app_hits = apps.search(&request.query.text, request.limit);
                        app_hits.retain(|entry| {
                            app_matches_options(
                                &entry.name,
                                &request.query.text,
                                &request.filters,
                            )
                        });
                        diag_apps = t_apps.elapsed().as_secs_f32() * 1000.0;
                        total += app_hits.len();
                        entries.append(&mut app_hits);
                        // 内置命令也参与“全部”搜索（打开设置等）。
                        let mut command_hits = commands(&request.query.text, &config.commands);
                        total += command_hits.len();
                        entries.append(&mut command_hits);
                        // 浏览器书签：与「应用」同语义（筛选/路径上下文时不混入）。
                        if bookmarks_enabled.load(Ordering::Relaxed) {
                            let t_bookmarks = Instant::now();
                            let mut bookmark_hits =
                                bookmarks.search(&request.query.text, request.limit);
                            // 严格匹配（大小写/全词）同样约束书签：拼音/模糊/
                            // 纯 URL 命中都要按名字复核，否则「区分大小写」失效。
                            bookmark_hits.retain(|entry| {
                                app_matches_options(
                                    &entry.name,
                                    &request.query.text,
                                    &request.filters,
                                )
                            });
                            diag_bookmarks = t_bookmarks.elapsed().as_secs_f32() * 1000.0;
                            total += bookmark_hits.len();
                            entries.append(&mut bookmark_hits);
                        }
                    }
                    // M7:应用/命令命中也要计入总数,
                    // 否则会出现「显示 0 个结果但列表里有内容」。
                    total = total.max(entries.len());
                    // 精确同名补齐必须在唯一一次返回之前完成。旧实现把补齐放在
                    // 「先发一版」之后:先发的那版里精确同名可能根本不在候选窗口里
                    // (如搜 "ai" 时文件夹 ai 排在第 246 个候选,而文件夹预算只有
                    // 100),UI 会停在这一版上,用户就看不到 ai。现在只发一次,
                    // 每次返回都经过 pin 置顶,不存在“中间错列表”的窗口。
                    if needs_exact {
                        if let Some(b) = &builtin {
                            // split 并趟已带回精确同名候选(与旧 search_exact
                            // 逐条一致),直接 append;仅在拿不到(如 split 出错)
                            // 时才回退第二趟查询。
                            if let Some(mut exact) = split_exact.take() {
                                entries.append(&mut exact);
                                diag_exact = split_exact_ms;
                            } else {
                                let t_exact = Instant::now();
                                let r_exact =
                                    b.search_exact(&request.query, &request.filters, 50);
                                diag_exact = t_exact.elapsed().as_secs_f32() * 1000.0;
                                if let Ok((mut exact, _)) = r_exact {
                                    pending_meta.mark(&exact, false);
                                    entries.append(&mut exact);
                                }
                            }
                        }
                        total = total.max(entries.len());
                    }
                }
                _ => {
                    let mut needs_exact = false;
                    if let Some(b) = &builtin {
                        let fetch = fetch_count(&request.query, request.limit, request.deliver);
                        match b.search(&request.query, &request.filters, fetch) {
                            Ok((mut files, count)) => {
                                total = count;
                                needs_exact = count > files.len();
                                pending_meta.mark(&files, true);
                                entries.append(&mut files);
                            }
                            Err(err) => error = Some(err),
                        }
                    } else {
                        error = Some("内置索引尚未就绪(需管理员授权或等待首次构建)".to_string());
                    }
                    total = total.max(entries.len());
                    // 同 All 分支:精确补齐在唯一一次返回之前完成,避免 UI 停在
                    // 未置顶的中间版本上。
                    if needs_exact {
                        if let Some(b) = &builtin {
                            if let Ok((mut exact, _)) =
                                b.search_exact(&request.query, &request.filters, 50)
                            {
                                pending_meta.mark(&exact, false);
                                entries.append(&mut exact);
                            }
                        }
                        total = total.max(entries.len());
                    }
                }
            }
        }

        // 盘符过滤兜底：内置引擎已按卷掩码查询；历史等其余来源的
        // 路径条目在这里统一裁掉（应用/命令无盘符，不受影响）。
        // 查询文本里的盘符 token 与输入框盘符 chip 求交。
        if let Some(mask) = filters::intersect_drive_masks(
            request.query.drive_mask(),
            request.filters.drive_mask(),
        ) {
            entries.retain(|e| {
                let Some(path) = &e.path else { return true };
                filters::path_in_drive_mask(path, mask)
            });
        }

        // 计算器:输入算式即出结果,钉在顶部(Enter 复制)。仅全部/命令范围、
        // 非正则模式;放在 scope 匹配之后以免被 keyword_rows 重赋值覆盖。
        if matches!(request.query.scope, Scope::All | Scope::Command)
            && !request.filters.matches.regex
        {
            if let Some(calc_row) = calc::calc_entry(&request.query.raw) {
                keyword_rows.push(calc_row);
            }
        }

        let t_finalize = Instant::now();
        // 惰性元数据:finalize 内部在 rank 之后、去重/截断之前只对最终候选
        // 逐条 find_meta(候选阶段零 FS);meta 耗时从 finalize 里单独拆出。
        let (entries, meta_ms) = finalize_entries_with_pending(
            entries,
            &request,
            &history,
            &config,
            keyword_rows,
            &noise,
            (!pending_meta.is_empty()).then_some(&pending_meta),
        );
        let diag_meta = meta_ms as f32;
        let diag_finalize =
            (t_finalize.elapsed().as_secs_f32() * 1000.0 - diag_meta).max(0.0);
        let diag_total = started.elapsed().as_secs_f32() * 1000.0;
        if (diag_total > 15.0 || profile_queries()) && !request.query.text.is_empty() {
            lstr_index::snapshot::debug_log(&format!(
                "search: stage gen={} split={:.1}ms apps={:.1}ms bm={:.1}ms exact={:.1}ms finalize={:.1}ms meta={:.1}ms total={:.1}ms text={:?}",
                request.generation,
                diag_split,
                diag_apps,
                diag_bookmarks,
                diag_exact,
                diag_finalize,
                diag_meta,
                diag_total,
                request.query.text
            ));
        }

send_response(&tx, &request, Response {
            generation: request.generation,
            entries,
            total,
            elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
            error,
        });
        ctx.request_repaint();
    }
    // 退出前把脏快照落盘(忽略节流),避免最后一次 journal 增量白丢。
    if let Some(b) = builtin.as_mut() {
        b.engine.flush_all();
    }
    // M4 + 退出收 worker:client 线程收到 shutdown 后会发 Shutdown 并等
    // worker 退出(≤3s),随后置 client_done;这里给它一个略宽的有界等待
    // (窗口还能覆盖管道半开/重连间隙的收尾),完成后向 on_exit 发出
    // worker_collected。
    shutdown_client.store(true, Ordering::Relaxed);
    {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !client_done.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !client_done.load(Ordering::Relaxed) {
            lstr_index::snapshot::debug_log("search: client thread did not finish worker shutdown in time");
        }
    }
    worker_collected.store(true, Ordering::Relaxed);
}

fn fetch_count(query: &Query, limit: usize, deliver: usize) -> usize {
    let short = query.text.chars().count() < 2
        && query.ext.is_none()
        && query.path_filter.is_none()
        && query.prefix.is_none()
        && query.suffix.is_none()
        && query.path_components.is_none();
    let base = if short {
        80
    } else {
        // 实测引擎查询评估本身是主要成本（与条数基本无关），
        // 300→500 只多花约 8ms，却能给排序多 66% 的候选，值得保留。
        (limit * 50).min(500)
    };
    if deliver <= MIN_DELIVER {
        return base;
    }
    // 加载更多：按交付条数的 2 倍抓候选——同名/id 去重在常见短查询里会
    // 吃掉近一半候选（实测搜 "a" 时 250 个候选只剩 122 条），只留 1/4 余量
    // 会让 deliver=1000 实际只加载到 ~520 条。
    base.max(deliver.saturating_mul(2)).min(MAX_FETCH)
}

/// 应用结果也要遵守匹配选项：严格模式下禁止拼音/模糊兜底，避免
/// 「区分大小写 + 全词」搜 ai 仍被 AI合成 / 拼音命中带进来。
fn app_matches_options(name: &str, query: &str, filters: &SearchFilters) -> bool {
    let matches = filters.matches;
    if matches.is_default() {
        return true;
    }
    if matches.regex {
        return lstr_index::query::regex_name_matches(name, query, matches.case_sensitive);
    }
    lstr_index::query::query_name_matches(
        name,
        query,
        matches.case_sensitive,
        matches.whole_word,
    )
}

fn scope_matches(scope: Scope, entry: &Entry) -> bool {
    match scope {
        Scope::All => entry.kind != Kind::Command,
        Scope::Folder => entry.kind == Kind::Folder,
        Scope::Program => entry.kind == Kind::Program,
        Scope::File => entry.kind == Kind::File,
        Scope::App => entry.kind == Kind::App,
        Scope::Bookmark => entry.kind == Kind::Bookmark,
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
        // 书签是 URL：浏览器的收藏夹文件可能暂时读不到（未安装/被占用），
        // 此时把历史条目杀掉反而像是丢了；URL 本身永远可尝试打开。
        Kind::Bookmark => false,
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

    /// SearchService 端到端测试串行化：`CANCEL_GEN` 是进程级全局，并行测试
    /// 的第二代查询会让另一个测试的首代响应被 send_response 丢弃。
    static SERVICE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 内置时间筛选的可用性:dir 首扫有 mtime;incremental="usn" 固定走 dir
    /// 首扫(同样带 mtime);纯 USN 首扫没有 mtime。
    #[test]
    fn builtin_time_filter_availability() {
        let index = crate::config::IndexConfig::default(); // auto + incremental=usn
        assert!(builtin_index_has_mtime(&index), "USN 增量层固定 dir 首扫,有 mtime");

        let mut index = crate::config::IndexConfig {
            incremental: "watch".into(),
            scan_mode: "dir".into(),
            ..Default::default()
        };
        assert!(builtin_index_has_mtime(&index), "dir 首扫写 mtime");

        index.scan_mode = "usn".to_string();
        assert!(!builtin_index_has_mtime(&index), "纯 USN 首扫无 mtime");
    }

    /// 书签进「全部」范围、不进应用页；历史里的书签不因浏览器文件暂时
    /// 读不到而被杀掉（URL 本身仍可打开）。
    #[test]
    fn bookmark_kind_scope_and_history_semantics() {
        let bookmark = Entry {
            id: "bm:https://github.com/".to_string(),
            kind: Kind::Bookmark,
            name: "GitHub".to_string(),
            path: Some(PathBuf::from("https://github.com/")),
            size: None,
            modified: None,
            pinyin: Some("github".to_string()),
            initial: "G".to_string(),
            tone: 0xE0A040,
        };
        assert!(scope_matches(Scope::All, &bookmark), "书签应出现在「全部」");
        assert!(
            scope_matches(Scope::Bookmark, &bookmark),
            "书签应属于「书签」页"
        );
        assert!(!scope_matches(Scope::App, &bookmark), "书签不属于应用页");
        assert!(
            !scope_matches(Scope::Bookmark, &Entry { kind: Kind::File, ..bookmark.clone() }),
            "文件不属于「书签」页"
        );
        assert!(!stale_history_entry(&bookmark, &[]), "书签历史不按磁盘存在性判断");
    }

    /// 测试隔离:测试进程不会拉起提权 worker(cfg(test) 下 spawn_client_thread
    /// 直接拒绝启动),引擎目录也走临时目录,不会触碰真实索引。
    fn test_config() -> crate::config::Config {
        crate::config::Config::default()
    }

    /// L6:Windows 路径/ID 大小写不敏感,索引与历史对同一路径给的
    /// 大小写可能不同,去重必须归一化,否则同一条目出现两行。
    #[test]
    fn duplicate_ids_are_deduped_case_insensitively() {
        fn entry(id: &str, name: &str) -> Entry {
            Entry {
                id: id.to_string(),
                kind: Kind::File,
                name: name.to_string(),
                path: Some(std::path::PathBuf::from(name)),
                size: None,
                modified: None,
                pinyin: None,
                initial: "f".to_string(),
                tone: 0,
            }
        }
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: Query {
                raw: "abc".to_string(),
                scope: Scope::All,
                text: "abc".to_string(),
                ext: None,
                path_filter: None,
                drives_include: 0,
                drives_exclude: 0,
                prefix: None,
                suffix: None,
                path_components: None,
            },
            limit: 10,
            deliver: MIN_DELIVER,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
            browse_root: None,
        };
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let entries = vec![
            entry("file:C:\\Temp\\ABC.txt", "ABC.txt"),
            entry("file:c:\\temp\\abc.txt", "abc.txt"),
        ];
        let out = finalize_entries(entries, &request, &history, &test_config(), Vec::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "ABC.txt");
    }

    /// 排除行硬过滤:排除目录的子孙不进结果(目录自身可见),对所有来源成立。
    #[test]
    fn finalize_hard_filters_excluded_paths() {
        let mut cfg = test_config();
        cfg.rank.noise_rules.push(crate::config::NoiseRule {
            pattern: r"D:\ProgramData".into(),
            penalty: 0,
            exclude: true,
        });
        let request = Request {
            generation: 1,
            sent_at: Instant::now(),
            query: Query {
                raw: "app".to_string(),
                scope: Scope::All,
                text: "app".to_string(),
                ext: None,
                path_filter: None,
                drives_include: 0,
                drives_exclude: 0,
                prefix: None,
                suffix: None,
                path_components: None,
            },
            limit: 10,
            deliver: MIN_DELIVER,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
            browse_root: None,
        };
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let mk = |id: &str, name: &str, path: &str, kind: Kind| Entry {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            path: Some(std::path::PathBuf::from(path)),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        };
        let entries = vec![
            mk("inside", "app.ini", r"D:\ProgramData\app.ini", Kind::File),
            mk("self", "ProgramData", r"D:\ProgramData", Kind::Folder),
            mk("outside", "app.ini", r"D:\Other\app.ini", Kind::File),
        ];
        let noise = lstr_index::engine::NoiseRules::from_static(vec![r"D:\ProgramData".into()]);
        let (out, _) = finalize_entries_with_pending(
            entries,
            &request,
            &history,
            &cfg,
            Vec::new(),
            &noise,
            None,
        );
        let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        assert!(!ids.contains(&"inside"), "排除目录子孙必须被剔除");
        assert!(ids.contains(&"self"), "排除目录自身可见(与索引层语义一致)");
        assert!(ids.contains(&"outside"), "无关路径保留");
    }

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
        let _guard = SERVICE_TEST_LOCK.lock().unwrap();
        // 路径模式只依赖 read_dir，可做端到端回归。
        fn wait(service: &SearchService, generation: u64) -> Response {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(response) = service.poll() {
                    if response.generation == generation {
                        return response;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "等待路径模式响应超时");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        let config = test_config();
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);
        let cwd = std::env::current_dir().expect("cwd");

        // 过滤词「carg」应命中 crate 根的 Cargo.toml。
        let query = format!("{}\\carg", cwd.display());
        let generation = service.search(&query, crate::model::Scope::All, false, &Default::default(), &Default::default(), true);
        let response = wait(&service, generation);
        assert!(
            response.entries.iter().any(|e| e.name == "Cargo.toml"),
            "路径模式应列出 Cargo.toml，实际: {:?}",
            response.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        // 完整目录路径（末尾无分隔符）应列出该目录内容，而不是把最后一段
        // 当成父目录的过滤词（那样只会显示这个目录自己）。
        let exact = cwd.display().to_string();
        let generation = service.search(&exact, crate::model::Scope::All, false, &Default::default(), &Default::default(), true);
        let response = wait(&service, generation);
        let names: Vec<&String> = response.entries.iter().map(|e| &e.name).collect();
        assert!(
            response.entries.iter().any(|e| e.name == "Cargo.toml")
                && response.entries.iter().any(|e| e.name == "src" && e.kind == Kind::Folder),
            "完整目录路径应列出子项（Cargo.toml/src），实际: {names:?}"
        );
    }

    #[test]
    fn app_results_respect_strict_match_options() {
        let mut filters = SearchFilters::default();
        assert!(app_matches_options("AI合成", "ai", &filters), "默认模式保留拼音/模糊");
        filters.matches.case_sensitive = true;
        filters.matches.whole_word = true;
        assert!(
            !app_matches_options("AI合成", "ai", &filters),
            "区分大小写时 AI合成 不应命中小写 ai"
        );
        assert!(
            app_matches_options("ai工具", "ai", &filters),
            "原始名包含 ai 且词边界成立时应命中"
        );
    }

    #[test]
    fn browse_root_empty_filter_lists_children() {        let _guard = SERVICE_TEST_LOCK.lock().unwrap();
        fn wait(service: &SearchService, generation: u64) -> Response {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(response) = service.poll() {
                    if response.generation == generation {
                        return response;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "等待浏览响应超时");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        let root = std::env::temp_dir().join(format!(
            "lstr-browse-root-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(root.join("alpha").join("inner.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.join("gamma")).unwrap();
        std::fs::write(root.join("beta.txt"), b"x").unwrap();

        let config = test_config();
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);

        // 空过滤词：列出 browse_root 的直接子项（3 项）。
        let generation = service.search_browse(
            "",
            crate::model::Scope::All,
            false,
            &Default::default(),
            &Default::default(),
            false,
            root.clone(),
        );
        let response = wait(&service, generation);
        let mut names: Vec<String> = response.entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta.txt", "gamma"]);

        // 非空过滤词不再走 browse_response：worker 改走正常引擎
        //（path_prefix=root）以支持递归子目录；resolve_exact 不再参与。
        // 递归 path_prefix 语义由 engine 的 path_prefix 测试覆盖。

        drop(service);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn more_doubles_deliver_until_cap_and_new_search_resets() {
        let _guard = SERVICE_TEST_LOCK.lock().unwrap();
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(test_config(), history, ctx);
        assert!(service.more().is_none(), "还没发过查询时不能续拉");
        let _ = service.search(
            "ai",
            crate::model::Scope::All,
            false,
            &SearchFilters::default(),
            &RankConfig::default(),
            false,
        );
        assert_eq!(service.deliver, MIN_DELIVER);
        assert!(service.more().is_some());
        assert_eq!(service.deliver, 200);
        assert!(service.more().is_some());
        assert_eq!(service.deliver, 400);
        assert!(service.more().is_some());
        assert_eq!(service.deliver, 800);
        assert!(service.more().is_some());
        assert_eq!(service.deliver, service.max_deliver);
        assert!(service.more().is_none(), "到上限后应停止");

        // 设置页把上限调到 3000：同一查询可以继续续拉。
        service.set_max_load(MAX_LOAD_RESULTS);
        assert!(service.more().is_some());
        assert_eq!(service.deliver, 2000);
        assert!(service.more().is_some());
        assert_eq!(service.deliver, MAX_LOAD_RESULTS);
        assert!(service.more().is_none(), "到新上限后应停止");

        // 新查询（输入/筛选变化）必须回到首屏 100，不能继承上一轮的上限。
        let _ = service.search(
            "ai2",
            crate::model::Scope::All,
            false,
            &SearchFilters::default(),
            &RankConfig::default(),
            false,
        );
        assert_eq!(service.deliver, MIN_DELIVER);
    }

    /// 回归(2026-11-09 截图):搜 "ai" 时精确同名文件夹必须出现在**唯一一次**
    /// 返回里并置顶。旧实现在精确补齐之前先发了一版未置顶的结果,而且那一版
    /// 里根本没有 E:\use\ai(文件夹候选预算 100,它在第 246 个),UI 一旦停在那
    /// 版上用户就看不到 ai。
    ///
    /// 依赖本机内置索引快照与 E:\use\ai;换机器时该测试自跳过。
    #[test]
    #[ignore = "requires the machine's builtin index snapshot (E:\\use\\ai)"]
    fn ai_folder_is_pinned_in_the_single_response() {
        if !std::path::Path::new(r"E:\use\ai").exists() {
            return;
        }
        let mut config = crate::config::Config::default();
        config.max_results = 10;
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);
        let filters = SearchFilters {
            location: Some(crate::search::filters::LocationFilter::Drive('E')),
            ..SearchFilters::default()
        };
        let generation = service.search(
            "ai",
            crate::model::Scope::All,
            false,
            &filters,
            &Default::default(),
            false,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let first = loop {
            if let Some(response) = service.poll() {
                if response.generation == generation {
                    break response;
                }
            }
            assert!(std::time::Instant::now() < deadline, "等待搜索结果超时");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let top = first.entries.first().expect("结果不能为空");
        assert_eq!(
            top.kind,
            Kind::Folder,
            "首行应是文件夹 ai,实际: {:?}",
            top.name
        );
        assert_eq!(
            top.name.to_lowercase(),
            "ai",
            "首行应是精确同名 ai,实际: {:?}",
            top.name
        );
        assert!(
            top.full_path().eq_ignore_ascii_case(r"E:\use\ai"),
            "首行应是 E:\\use\\ai,实际: {:?}",
            top.full_path()
        );
        // 旧两阶段实现会再发一版;现在必须只有一版,UI 不存在“停在未置顶中间结果”的窗口。
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(service.poll().is_none(), "不应再有第二版结果(两阶段遗留)");
    }

    /// 真实快照端到端:程序 tab 不得混入文件夹/普通文件(2026-11-09 回归)。
    /// 依赖本机内置索引快照;换机器或快照为空时该测试自跳过。
    #[test]
    #[ignore = "requires the machine's builtin index snapshot"]
    fn program_scope_only_returns_programs() {
        let mut config = crate::config::Config::default();
        config.max_results = 20;
        let history = std::sync::Arc::new(std::sync::Mutex::new(
            crate::history::History::in_memory(),
        ));
        let ctx = eframe::egui::Context::default();
        let mut service = SearchService::new(config, history, ctx);
        let generation = service.search(
            "remote",
            crate::model::Scope::Program,
            false,
            &SearchFilters::default(),
            &Default::default(),
            false,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let first = loop {
            if let Some(response) = service.poll() {
                if response.generation == generation {
                    break response;
                }
            }
            assert!(std::time::Instant::now() < deadline, "等待搜索结果超时");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        if first.entries.is_empty() {
            return; // 快照里没有 remote 程序:换机器时自跳过
        }
        for entry in &first.entries {
            assert_eq!(
                entry.kind,
                Kind::Program,
                "程序 tab 混入了非程序条目: {:?} ({:?})",
                entry.name,
                entry.full_path()
            );
        }
    }
}
