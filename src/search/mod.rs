pub mod apps;
pub mod browse;
pub mod builtin;
pub mod everything;
pub mod filters;
pub mod fuzzy;
pub mod pinyin;
pub mod query;
pub mod rank;
pub mod uwp;

use crate::config::{Config, KeywordConfig, RankConfig};
use crate::history::History;
use lstr_index::{ClientCommand, EngineMsg};
use crate::model::{Entry, Kind, Query, Scope};
use crate::search::filters::SearchFilters;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use eframe::egui;
use std::time::{Duration, Instant};

/// 应用索引的刷新周期：新装/卸载应用后最多等这么久就能搜到/消失。
/// 重建成本是枚举开始菜单 + App Paths（本机 200+ 应用，几十 ms），放 worker 线程无感。
const APPS_RELOAD_INTERVAL: Duration = Duration::from_secs(90);

/// 等待中的查询请求数:UI 线程发送前 +1,搜索线程出队后 -1。
/// 引擎分批 apply 的块间让路钩子读它:>0 = 有打字请求在排队,对账/首扫的
/// 记录 apply 立即收手——0294602「打字时查询优先于索引维护」的 apply 中段
/// 扩展(此前只在 idle 轮边界检查,一轮 400 条大消息仍会把查询堵住秒级)。
/// 计数而非布尔的理由:UI +1 与 send 之间、worker 出队与下一请求之间存在
/// 窗口,布尔会出现「队列里有请求但标志已清」的假阴性;计数不会。
static QUERY_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

/// 出队了一个请求:等待计数 -1(saturating,防 debug 断言下溢)。
fn query_inflight_dec() {
    let _ = QUERY_INFLIGHT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(1))
    });
}

struct Request {
    generation: u64,
    /// 诊断用：UI 发出时刻。worker 出队时用它区分「排队慢」还是「执行慢」。
    sent_at: Instant,
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
    /// 自研索引状态(设置页/状态行用);worker 线程随时刷新。
    pub index_status: Arc<Mutex<Option<builtin::IndexUiStatus>>>,
    /// 「参与索引的卷」掩码(lstr_index::volmask 位掩码,bit i='A'+i),
    /// 设置页勾选/取消盘时更新;worker 线程在每次拉起与每帧指令里读取。
    volume_mask: Arc<AtomicU32>,
    /// 发给索引 worker 的控制指令(运行期切卷/重建),由 client 线程非阻塞写帧。
    index_cmd: mpsc::Sender<ClientCommand>,
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
        let status_slot: Arc<Mutex<Option<builtin::IndexUiStatus>>> = Arc::new(Mutex::new(None));
        let worker_status = Arc::clone(&status_slot);
        // 卷选择:启动时从配置取快照(空列表 = 全部盘),之后设置页改掩码即可
        // 在运行期切卷,不需要重启 lstr。
        let volume_mask = Arc::new(AtomicU32::new(config.index.volume_mask()));
        let worker_volume_mask = Arc::clone(&volume_mask);
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
                    worker_status,
                    index_cmd_rx,
                    worker_volume_mask,
                    done_s,
                    done_w,
                    done_c,
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
            index_status: status_slot,
            volume_mask,
            index_cmd: index_cmd_tx,
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
    /// worker 没在跑(后端 off / Everything / UAC 被拒)时指令会静默失败,
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
        // 通知查询线程:更旧的查询可以立即放弃(打字时 "a"→"ai" 不再排队 600ms)。
        lstr_index::query::CANCEL_GEN
            .store(self.generation, std::sync::atomic::Ordering::Relaxed);
        let query = query::parse(raw, scope);
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
                keywords: self.keywords.clone(),
                dialog_quick,
                filters: filters.clone(),
                rank: rank.clone(),
                show_history,
                favorites: self.favorites.clone(),
                base_folder,
            })
            .is_err()
        {
            // 搜索线程已退出:计数回退,避免遗留计数让后续会话误判
            query_inflight_dec();
        }
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
fn pin_exact_name_matches(entries: &mut Vec<Entry>, needle: &str) {
    if needle.is_empty() {
        return;
    }
    // needle 由调用方统一 to_lowercase 过，这里也用 to_lowercase 比较，
    // 与去重/rank 的归一化保持一致（eq_ignore_ascii_case 只覆盖 ASCII）。
    let is_exact = |entry: &Entry| {
        if entry.name.to_lowercase() == needle {
            return true;
        }
        entry.kind == Kind::Command
            && command_aliases(&entry.id)
                .iter()
                .any(|alias| alias.to_lowercase() == needle)
    };
    // 没有命中就不搬动元素：短查询每次都全量 drain 没有必要。
    if !entries.iter().any(|entry| is_exact(entry)) {
        return;
    }
    let mut exact = Vec::new();
    let mut rest = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if is_exact(&entry) {
            exact.push(entry);
        } else {
            rest.push(entry);
        }
    }
    // 组内保持原有分数顺序（exact.append 不重排）。
    exact.append(&mut rest);
    *entries = exact;
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
        pin_exact_name_matches(&mut entries, "ai");
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["exact1", "exact2", "sub", "other"]
        );
    }

    #[test]
    fn empty_needle_keeps_the_original_order() {
        let mut entries = vec![item("a", "a"), item("b", "b")];
        pin_exact_name_matches(&mut entries, "");
        assert_eq!(entries[0].id, "a");
    }

    #[test]
    fn pin_uses_unicode_lowercase_not_ascii_only() {
        // L2：去重/rank 都按 to_lowercase 归一化，置顶也必须一致。
        let mut entries = vec![item("sub", "x"), item("exact", "ÄI")];
        pin_exact_name_matches(&mut entries, "äi");
        assert_eq!(entries[0].id, "exact");
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
            },
            limit,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: RankConfig::default(),
            show_history: true,
            favorites: Vec::new(),
            base_folder: None,
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
    fn finalize_keeps_at_most_six_exact_names_and_pins_them() {
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
        assert_eq!(exact_count, 6, "同名精确命中最多保留 6 条");
        assert!(
            out.iter().take(6).all(|entry| entry.name == "ai"),
            "6 条精确同名都应被置顶"
        );
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
}

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

    // 两遍去重：先算 keep 掩码再 retain（借用不跨 retain）。
    // Windows 路径/ID 大小写不敏感，内置索引与 Everything 对同一个路径给的
    // 大小写可能不同，所以 id 先归一化成小写再比较，否则同一条目出现两行。
    let keep_ids: Vec<bool> = {
        let mut seen_ids: HashSet<String> = HashSet::new();
        entries
            .iter()
            .map(|entry| seen_ids.insert(entry.id.to_lowercase()))
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
    // 名称完全一致的结果硬置顶：档位权重可能被用户调整，或被历史/位置加权翻过。
    pin_exact_name_matches(&mut entries, &needle_lower);
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

/// 内置索引能否自带时间筛选:目录枚举(dir 首扫)会把 mtime 写进快照;
/// backend="usn" 实验后端固定走 dir 首扫(见 config 注释),同样有 mtime。
/// 纯 USN 首扫(scan_mode="usn")没有 mtime,时间筛选仍需 Everything。
fn builtin_index_has_mtime(index: &crate::config::IndexConfig) -> bool {
    index.scan_mode == "dir" || index.backend == "usn"
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    rx: Receiver<Request>,
    tx: Sender<Response>,
    history: Arc<Mutex<History>>,
    config: Config,
    ctx: egui::Context,
    apps_dirty: Arc<AtomicBool>,
    status_slot: Arc<Mutex<Option<builtin::IndexUiStatus>>>,
    index_cmd_rx: Receiver<ClientCommand>,
    volume_mask: Arc<AtomicU32>,
    shutdown_client: Arc<AtomicBool>,
    worker_collected: Arc<AtomicBool>,
    client_done: Arc<AtomicBool>,
) {
    // 显式选择「仅内置索引」:此时无论引擎是否就绪,都不再回落 Everything,
    // 否则与设置页文案矛盾(旧实现在 Fallback 时仍把查询发给 Everything)。
    let force_builtin = config.index.backend == "builtin";
    // 目录扫描(dir)会把 mtime 写进快照,所以内置索引自己能做时间筛选;
    // 纯 USN 首扫没有 mtime,时间筛选仍需 Everything(或直接报错)。
    let builtin_time_ok = builtin_index_has_mtime(&config.index);
    // 严格模式:用户选了「仅内置索引」就完全不碰 Everything —— 连 IPC 探测
    // 都不做(既避免 IPC 超时拖慢首次查询,也保证观察不到一切 Everything 结果)。
    let mut everything = if force_builtin {
        None
    } else {
        match everything::EverythingProvider::new() {
            Ok(provider) => Some(provider),
            Err(err) => {
                eprintln!("lstr: Everything 不可用: {err}");
                None
            }
        }
    };
    // 「Everything 不可用」的重试节流：首次探测可能等 IPC 超时(~2s)，
    // 未运行时常驻用户每次查询都重试会把交互拖慢；最多 10s 试一次。
    let mut last_everything_probe = Instant::now();
    // —— 自研索引后端(方案 §4/§6):engine 由本线程独占;client 线程推 EngineMsg ——
    // 有界通道(背压):client 线程跑得快时会阻塞在 send,防止整个卷的记录
    // 积压在通道内存里(实测无界时 UI 进程冲到 877MB)。
    let (engine_tx, engine_rx) = mpsc::sync_channel::<EngineMsg>(16);
    let mut builtin: Option<builtin::BuiltinProvider> = None;
    let handshake_slot: Arc<Mutex<Vec<(u128, u64, i64, u64, bool)>>> =
        Arc::new(Mutex::new(Vec::new()));
    // client 线程是否在跑(优雅退出时要等它确认 worker 已收尾;off/everything
    // 后端没有 client 线程,收尾等待直接跳过,退出不空转)。
    let mut client_spawned = false;
    match config.index.backend.as_str() {
        // 这两个后端不拉 worker:控制通道直接丢弃(发送端失败即忽略)。
        "off" | "everything" => {
            drop(index_cmd_rx);
        }
        _ => {
            let mut engine = builtin::make_engine(&config.index);
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
            client_spawned = true;
            builtin = Some(builtin::BuiltinProvider::new(engine));
        }
    }
    let publish_status = |builtin: Option<&builtin::BuiltinProvider>,
                          everything: Option<&everything::EverythingProvider>,
                          slot: &Arc<Mutex<Option<builtin::IndexUiStatus>>>| {
        if let Some(b) = builtin {
            let mut st = b.status();
            st.everything_ok = everything.is_some();
            *slot.lock().unwrap() = Some(st);
        } else if let Some(_e) = everything {
            *slot.lock().unwrap() = None;
        }
    };
    publish_status(builtin.as_ref(), everything.as_ref(), &status_slot);
    let mut apps = apps::AppIndex::load();
    let mut apps_loaded = Instant::now();
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
        if pending.is_none() {
            let idle_start = Instant::now();
            // 1. 应用自研索引的增量 + 维护(仅空闲路径)
            if let Some(b) = &mut builtin {
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
                    if QUERY_INFLIGHT.load(Ordering::Relaxed) > 0 || b.engine.has_pending_apply() {
                        break;
                    }
                }
                b.engine.maintain();
                *handshake_slot.lock().unwrap() = b.engine.handshake_state();
            }
            let idle_ms = idle_start.elapsed().as_millis();
            if idle_ms > 100 {
                lstr_index::snapshot::debug_log(&format!(
                    "search: idle maintenance took {idle_ms}ms"
                ));
            }
        }
        publish_status(builtin.as_ref(), everything.as_ref(), &status_slot);
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
            None => match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(r) => {
                    query_inflight_dec();
                    r
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        // 用户可能刚点了提示条启动 Everything；每次查询前尝试重连，
        // 而不是启动时连不上就永久报错。
        // 但 IPC 探测在 Everything 未运行时要等超时(实测 ~2s)，而 UI 显示的
        // elapsed_ms 从请求出队后才开始计，用户就会看到「显示 70ms、实际等 2s」。
        // 因此：只有可能用到 Everything 的阶段才重试，且最多 10s 一次。
        if everything.is_none() {
            let phase_ready = builtin
                .as_ref()
                .map(|b| b.status().phase == lstr_index::Phase::Ready)
                .unwrap_or(false);
            // 严格「仅内置索引」模式:任何阶段都不探测/回落 Everything。
            let need_everything = !force_builtin
                && (config.index.backend == "everything"
                    || !phase_ready
                    // dir 扫描的索引自己带 mtime:Ready 后时间筛选不再需要 Everything
                    || (request.filters.time.is_some() && !builtin_time_ok));
            if need_everything && last_everything_probe.elapsed() >= Duration::from_secs(10) {
                last_everything_probe = Instant::now();
                everything = everything::EverythingProvider::new().ok();
            }
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
        // 诊断:记录本次请求的解析结果与索引阶段,用来解释「为什么走了 Everything」。
        {
            let phase = builtin
                .as_ref()
                .map(|b| b.status().phase)
                .unwrap_or(lstr_index::Phase::Fallback);
            lstr_index::snapshot::debug_log(&format!(
                "search: req gen={} text={:?} scope={:?} phase={:?} builtin={} everything={}",
                request.generation,
                request.query.text,
                request.query.scope,
                phase,
                builtin.is_some(),
                everything.is_some()
            ));
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
            send_response(&tx, &request, Response {
                generation: request.generation,
                entries,
                total,
                elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                error: None,
            });            ctx.request_repaint();
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
                    let phase = builtin
                        .as_ref()
                        .map(|b| b.engine.status().phase)
                        .unwrap_or(lstr_index::Phase::Fallback);
                    let use_builtin = force_builtin || phase != lstr_index::Phase::Fallback;
                    // 时间筛选依赖文件 mtime:USN 不含(方案 §3.2),降级走 Everything
                    let time_forced = request.filters.time.is_some();
                    // 构建期与 Everything 合并(方案 §5.2.2);Ready 后纯内置
                    let merge_everything = use_builtin
                        && !force_builtin
                        && phase != lstr_index::Phase::Ready
                        && !time_forced
                        && everything.is_some();
                    let fetch = fetch_count(&request.query, request.limit);
                    let folder_fetch = (fetch / 5).clamp(40, 120);
                    let file_fetch = fetch.saturating_sub(folder_fetch).max(60);
                    let mut truncation = 0usize;
                    if use_builtin && (!time_forced || builtin_time_ok) {
                        // 内置索引:本地引擎,双路语义在 kind 过滤层保持
                        if let Some(b) = &builtin {
                            // H4:内置双路都用同一 spec 时,旧实现把同一个 All-kind
                            // 总数加了两次(total=2N),且 truncation 恒 >0 → 每次
                            // All 查询都走“先发一版 + exact 补”两阶段。
                            // 现 folders 路按 KindFilter::Folders 统计,files 路按
                            // All 统计后扣掉文件夹数,两路 total 同口径且不重复。
                            let mut folder_total = 0usize;
                            let mut file_total = 0usize;
                            match b.search_split(&request.query, &request.filters, folder_fetch, true)
                            {
                                Ok((mut folders, count)) => {
                                    folder_total = count;
                                    truncation += count.saturating_sub(folders.len());
                                    entries.append(&mut folders);
                                }
                                Err(err) => error = Some(err),
                            }
                            match b.search_split(&request.query, &request.filters, file_fetch, false)
                            {
                                Ok((mut files, count_all)) => {
                                    file_total = count_all.saturating_sub(folder_total);
                                    truncation += file_total.saturating_sub(files.len());
                                    entries.append(&mut files);
                                }
                                Err(err) => error = error.or(Some(err)),
                            }
                            total += folder_total + file_total;
                            needs_exact = truncation > 0;
                        }
                    } else if let Some(provider) = everything.as_ref().filter(|_| !force_builtin) {
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
                    } else if time_forced {
                        error = Some(if force_builtin {
                            "仅内置索引模式不支持时间筛选(USN 不含修改时间)".to_string()
                        } else {
                            "时间筛选需要 Everything(内置索引暂不含文件修改时间)".to_string()
                        });
                    } else {
                        error = Some("索引未启用且 Everything 未运行".to_string());
                    }
                    if merge_everything {
                        if let Some(provider) = &everything {
                            if let Ok((mut folders, _)) = provider.search_split(
                                &request.query,
                                &request.filters,
                                folder_fetch,
                                true,
                            ) {
                                entries.append(&mut folders);
                            }
                            if let Ok((mut files, _)) = provider.search_split(
                                &request.query,
                                &request.filters,
                                file_fetch,
                                false,
                            ) {
                                entries.append(&mut files);
                            }
                        }
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
                    // M7:合并进来的 Everything 结果与应用/命令命中也要计入总数,
                    // 否则会出现「显示 0 个结果但列表里有内容」。
                    total = total.max(entries.len());
                    // 精确同名补齐必须在唯一一次返回之前完成。旧实现把补齐放在
                    // 「先发一版」之后:先发的那版里精确同名可能根本不在候选窗口里
                    // (如搜 "ai" 时文件夹 ai 排在第 246 个候选,而文件夹预算只有
                    // 100),UI 会停在这一版上,用户就看不到 ai。现在只发一次,
                    // 每次返回都经过 pin 置顶,不存在“中间错列表”的窗口。
                    if needs_exact {
                        let phase = builtin
                            .as_ref()
                            .map(|b| b.engine.status().phase)
                            .unwrap_or(lstr_index::Phase::Fallback);
                        if let Some(b) = &builtin {
                            if (force_builtin || phase != lstr_index::Phase::Fallback)
                                && request.filters.time.is_none()
                            {
                                if let Ok((mut exact, _)) =
                                    b.search_exact(&request.query, &request.filters, 50)
                                {
                                    entries.append(&mut exact);
                                }
                            } else if let Some(provider) = everything.as_ref().filter(|_| !force_builtin) {
                                if let Ok((mut exact, _)) =
                                    provider.search_exact(&request.query, &request.filters, 50)
                                {
                                    entries.append(&mut exact);
                                }
                            }
                        } else if let Some(provider) = &everything {
                            if let Ok((mut exact, _)) =
                                provider.search_exact(&request.query, &request.filters, 50)
                            {
                                entries.append(&mut exact);
                            }
                        }
                        total = total.max(entries.len());
                    }
                }
                _ => {
                    let mut needs_exact = false;
                    let phase = builtin
                        .as_ref()
                        .map(|b| b.engine.status().phase)
                        .unwrap_or(lstr_index::Phase::Fallback);
                    let time_forced = request.filters.time.is_some();
                    let used_builtin = (force_builtin || phase != lstr_index::Phase::Fallback)
                        && (!time_forced || builtin_time_ok);
                    if let Some(b) = &builtin {
                        if used_builtin {
                            let fetch = fetch_count(&request.query, request.limit);
                            match b.search(&request.query, &request.filters, fetch) {
                                Ok((mut files, count)) => {
                                    total = count;
                                    needs_exact = count > files.len();
                                    entries.append(&mut files);
                                }
                                Err(err) => error = Some(err),
                            }
                        } else if let Some(provider) = everything.as_ref().filter(|_| !force_builtin) {
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
                            error = Some(if time_forced {
                                "时间筛选需要 Everything(内置索引暂不含文件修改时间)".to_string()
                            } else {
                                "内置索引尚未就绪(需管理员授权或等待首次构建)".to_string()
                            });
                        }
                    } else if let Some(provider) = &everything {
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
                        error = Some("索引未启用且 Everything 未运行".to_string());
                    }
                    // M5:内置索引还没 Ready(Fallback 以上的构建期)时,单 scope
                    // 也要像 All scope 一样合并 Everything 的结果;否则构建期
                    // 只能看到半成品索引,查不到尚未扫到的文件。
                    if used_builtin && phase != lstr_index::Phase::Ready {
                        if let Some(provider) = everything.as_ref().filter(|_| !force_builtin) {
                            let fetch = fetch_count(&request.query, request.limit);
                            if let Ok((mut extra, count)) =
                                provider.search(&request.query, &request.filters, fetch)
                            {
                                total = total.max(count);
                                entries.append(&mut extra);
                            }
                        }
                    }
                    // M7:合并的 Everything 结果与精确补齐都要计入总数。
                    total = total.max(entries.len());
                    // 同 All 分支:精确补齐在唯一一次返回之前完成,避免 UI 停在
                    // 未置顶的中间版本上。
                    if needs_exact {
                        let phase = builtin
                            .as_ref()
                            .map(|b| b.engine.status().phase)
                            .unwrap_or(lstr_index::Phase::Fallback);
                        if let Some(b) = &builtin {
                            if (force_builtin || phase != lstr_index::Phase::Fallback)
                                && request.filters.time.is_none()
                            {
                                if let Ok((mut exact, _)) =
                                    b.search_exact(&request.query, &request.filters, 50)
                                {
                                    entries.append(&mut exact);
                                }
                            } else if let Some(provider) = everything.as_ref().filter(|_| !force_builtin) {
                                if let Ok((mut exact, _)) =
                                    provider.search_exact(&request.query, &request.filters, 50)
                                {
                                    entries.append(&mut exact);
                                }
                            }
                        } else if let Some(provider) = &everything {
                            if let Ok((mut exact, _)) =
                                provider.search_exact(&request.query, &request.filters, 50)
                            {
                                entries.append(&mut exact);
                            }
                        }
                        total = total.max(entries.len());
                    }
                }
            }
        }

        let entries = finalize_entries(entries, &request, &history, &config, keyword_rows);

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
    // worker_collected。没有 client 线程(off/everything 后端)直接跳过。
    shutdown_client.store(true, Ordering::Relaxed);
    if client_spawned {
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

    /// 内置时间筛选的可用性:dir 首扫有 mtime;backend="usn" 固定走 dir
    /// 首扫(同样带 mtime);纯 USN 首扫没有 mtime。
    #[test]
    fn builtin_time_filter_availability() {
        let mut index = crate::config::IndexConfig::default(); // backend=auto, scan_mode=dir
        assert!(builtin_index_has_mtime(&index), "dir 首扫写 mtime");
        index.scan_mode = "usn".to_string();
        assert!(!builtin_index_has_mtime(&index), "纯 USN 首扫无 mtime");
        index.backend = "usn".to_string();
        assert!(builtin_index_has_mtime(&index), "usn 实验后端固定 dir 首扫,有 mtime");
    }

    /// 测试隔离:默认配置 backend=auto 会让服务线程真去探活/拉起 worker
    /// (runas → 可能弹 UAC),在测试机器上既慢又打扰用户。单测一律关掉
    /// 内置索引;需要 Everything 的用例本来就标了 #[ignore]。
    fn test_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.index.backend = "off".to_string();
        config
    }

    /// L6:Windows 路径/ID 大小写不敏感,内置索引与 Everything 对同一路径给的
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
            },
            limit: 10,
            keywords: Vec::new(),
            dialog_quick: false,
            filters: SearchFilters::default(),
            rank: RankConfig::default(),
            show_history: false,
            favorites: Vec::new(),
            base_folder: None,
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
        let config = test_config();
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
        let config = test_config();
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
        // 强制纯内置索引:不依赖 Everything,也不会有双后端合并干扰。
        config.index.backend = "builtin".to_string();
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
        // 强制纯内置索引:Everything 合并会掩盖分类过滤的回归。
        config.index.backend = "builtin".to_string();
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
