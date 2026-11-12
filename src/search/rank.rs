use crate::config::RankConfig;
use crate::history::History;
use crate::model::{Entry, Kind};

// 匹配档位分值来自 RankConfig（设置面板可调），这里只保留固定微调。
// 各档默认值：精确 1000 / 前缀 800 / 子串 600 / 多词 550 / 分隔符 520 /
// 拼音 450/350 / 模糊 300 / 仅路径 200。

// 历史分数换算：每 1.0 frecency 折算 10 分，上限来自 RankConfig（默认 150）。
const HISTORY_WEIGHT: f32 = 10.0;
// app_boost 的每个单位折算 10 分，只作用于 Kind::App。
const APP_BOOST_UNIT: i32 = 10;
// 同分时文件夹略微优先于文件。
const FOLDER_TIE_BONUS: i32 = 5;
// 命令的别名精确命中（如「设置」）要压过最近用过的同名应用，但不能
// 让「启动 Everything」这类命令在搜索 everything 时盖过应用本身。
const COMMAND_BOOST: i32 = 300;
// 精确同名时按路径深浅再排一次：E:\use 应压过
// C:\...\perl5\core_perl\Test\use 这种深埋的同名目录（home 加权会反过来）。
const EXACT_SHALLOW_DEPTH: i32 = 12;
const EXACT_SHALLOW_UNIT: i32 = 2;
// 名称子串匹配的额外加分：词首（下划线/连字符/点后）比词中更像用户要的；
// 出现位置越靠前越好。上限 40，保证不越过「名称前缀」。
const SUBSTRING_BOUNDARY_BONUS: i32 = 30;
const SUBSTRING_POSITION_BONUS: i32 = 10;

/// 位置/短名/浅路径这类启发式加分在**单个匹配档位内**的总上限。
/// 档位分值由 RankConfig 提供、用户可调，所以上限必须跟着档位走：
/// 取「相邻档位最小差 - 1」，否则位置更好的低档条目会反超高一档。
/// 默认档位里 tokens(550) 与 normalized(520) 只差 30，写死 80 时
/// cwd 里的 normalized 命中能反超 tokens 命中。
/// 下限 20：档位被调得非常接近时仍保留最低限度的同档排序能力。
fn heuristic_cap(cfg: &RankConfig) -> i32 {
    let mut tiers = [
        cfg.exact,
        cfg.prefix,
        cfg.substring,
        cfg.tokens,
        cfg.normalized,
        cfg.pinyin_prefix,
        cfg.pinyin_substring,
        cfg.fuzzy,
        cfg.path_only,
    ];
    tiers.sort_unstable();
    let min_gap = tiers
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|gap| *gap > 0)
        .min();
    match min_gap {
        // 留 1 分余量：档位内启发式最高只到下一档前一分。
        Some(gap) => (gap - 1).max(20),
        // 档位全相等时没有「相邻差」可言，给一个保守默认值。
        None => 80,
    }
}

/// 一次查询内复用的打分上下文：小写查询词、归一化噪声规则、历史批量快照。
/// 旧实现逐候选做 `q.trim().to_lowercase()`、逐候选逐规则 `to_lowercase`
/// （默认 70+ 条规则 × 数百候选）、逐候选两次 SQLite 查询，finalize 因此
/// 占 7ms。这里全部预计算一次，公式与逐条版本逐字节等价。
pub struct RankPlan {
    needle: String,
    /// (已 trim/小写/反斜杠→正斜杠, 原始 penalty)。规则表的模式是配置常量，
    /// 同一查询内不会变，预归一化不改变匹配结果。
    noise: Vec<(String, i32)>,
    snapshot: crate::history::RankSnapshot,
}

impl RankPlan {
    pub fn new(q: &str, history: &History, cfg: &RankConfig) -> Self {
        let needle = q.trim().to_lowercase();
        let noise = cfg
            .noise_rules
            .iter()
            // exclude 行是「整树排除」语义,索引层已不收录,不参与降权。
            .filter(|rule| !rule.exclude)
            .filter_map(|rule| {
                let pattern = rule.pattern.trim().to_lowercase().replace('\\', "/");
                (!pattern.is_empty()).then_some((pattern, rule.penalty))
            })
            .collect();
        Self {
            snapshot: history.rank_snapshot(&needle),
            needle,
            noise,
        }
    }
}

/// 按分数从高到低稳定排序（使用设置面板里的排序权重）。
pub fn rank_with(
    entries: &mut Vec<Entry>,
    q: &str,
    history: &History,
    app_boost: i32,
    cfg: &RankConfig,
) {
    let plan = RankPlan::new(q, history, cfg);
    entries.sort_by_cached_key(|entry| {
        std::cmp::Reverse(score_entry_planned(entry, &plan, app_boost, cfg))
    });
}

/// 测试用默认权重的包装。
#[cfg(test)]
pub fn rank(entries: &mut Vec<Entry>, q: &str, history: &History, app_boost: i32) {
    rank_with(entries, q, history, app_boost, &RankConfig::default());
}

/// 单个条目的排序分（默认权重，测试用）。
#[cfg(test)]
pub fn score_entry(entry: &Entry, q: &str, history: &History, app_boost: i32) -> i32 {
    score_entry_with(entry, q, history, app_boost, &RankConfig::default())
}

/// 单个条目的排序分（默认权重，测试用）。
#[cfg(test)]
pub fn score_entry_with(
    entry: &Entry,
    q: &str,
    history: &History,
    app_boost: i32,
    cfg: &RankConfig,
) -> i32 {
    let plan = RankPlan::new(q, history, cfg);
    score_entry_planned(entry, &plan, app_boost, cfg)
}

/// 与 [`score_entry_with`] 同公式，复用一次查询级上下文（见 [`RankPlan`]）。
pub fn score_entry_planned(
    entry: &Entry,
    plan: &RankPlan,
    app_boost: i32,
    cfg: &RankConfig,
) -> i32 {
    let needle = &plan.needle;
    let name_lc = entry.name.to_lowercase();
    let path_lc = full_path_lowercase(entry);
    let history_points = (plan.snapshot.score(&entry.id) * HISTORY_WEIGHT)
        .round()
        .clamp(0.0, cfg.history_cap.max(0) as f32) as i32;
    // 启动项（应用 + 可执行程序）加权：用户优先看到可以直接启动的东西。
    let launch_points = match entry.kind {
        Kind::App | Kind::Program => app_boost.max(0) * APP_BOOST_UNIT,
        Kind::Command => COMMAND_BOOST,
        _ => 0,
    };
    let folder_points = if entry.kind == Kind::Folder {
        FOLDER_TIE_BONUS
    } else {
        0
    };

    if needle.is_empty() {
        return history_points + launch_points + kind_preference(entry.kind) + folder_points;
    }

    let match_points = match_tier(entry, needle, cfg, &name_lc, &path_lc);
    if match_points == 0 {
        return 0;
    }
    // 精确同名：路径越浅越可能是用户要找的那个（E:\use vs Perl 测试目录）。
    // 同时压过 USERPROFILE 加权——深埋在用户目录里的同名目录不该赢过根目录。
    let exact_shallow_points = if match_points == cfg.exact {
        let depth = path_lc.matches('\\').count() as i32;
        (EXACT_SHALLOW_DEPTH - depth).max(0) * EXACT_SHALLOW_UNIT
    } else {
        0
    };
    // 拉丁字母查询命中中文应用/程序拼音时，启动目标应排到普通文件前面。
    let pinyin_app_points = if matches!(entry.kind, Kind::App | Kind::Program)
        && !name_lc.contains(needle)
    {
        let mut hit = false;
        if let Some(pinyin) = &entry.pinyin {
            hit = pinyin.to_lowercase().contains(needle);
        }
        if !hit {
            hit = entry.initial.to_lowercase().contains(needle);
        }
        if hit { 450 } else { 0 }
    } else {
        0
    };
    match_points
        + history_points
        + launch_points
        + folder_points
        + pinyin_app_points
        + exact_shallow_points
        + recency_points(entry.modified, cfg, plan.snapshot.now())
        + path_adjustment(entry, &path_lc, needle, cfg, &plan.noise)
        // P1-1：同一查询词下用户上次选过这个结果时加权。
        // 全局 frecency 回答「常用吗」，query_boost 回答「搜这个词时要的是哪个」。
        + plan.snapshot.boost(&entry.id)
}

/// `Entry::full_path()` 的小写版本，一次分配；旧实现每候选调用 2-3 次
/// `full_path()`（每次都 `display().to_string()`）。
fn full_path_lowercase(entry: &Entry) -> String {
    match &entry.path {
        Some(path) => path.display().to_string().to_lowercase(),
        None => entry.name.to_lowercase(),
    }
}

/// 最近修改过的文件更可能是目标（轻量加分，不压过匹配档位）。
fn recency_points(modified: Option<i64>, cfg: &RankConfig, now: i64) -> i32 {
    let Some(ts) = modified else {
        return 0;
    };
    let age = (now - ts).max(0);
    let boost = cfg.recent_boost.max(0);
    if age <= 7 * 86400 {
        boost
    } else if age <= 30 * 86400 {
        boost / 2
    } else {
        0
    }
}

/// 环境路径只算一次：之前每个候选都调 `std::env::var` + `current_dir`，
/// 500 个候选 × 每次击键就是上千次查询。
struct EnvPaths {
    home: Option<String>,
    cwd: Option<String>,
    cwd_parent: Option<String>,
}

fn env_paths() -> &'static EnvPaths {
    use std::sync::OnceLock;
    static ENV: OnceLock<EnvPaths> = OnceLock::new();
    ENV.get_or_init(|| {
        let home = std::env::var("USERPROFILE").ok().map(|home| home.to_lowercase());
        let cwd = std::env::current_dir().ok();
        let cwd_parent = cwd
            .as_ref()
            .and_then(|dir| dir.parent().map(|parent| parent.to_string_lossy().to_lowercase()));
        let cwd = cwd.map(|dir| dir.to_string_lossy().to_lowercase());
        EnvPaths { home, cwd, cwd_parent }
    })
}

/// 构建产物目录规则集合：这些目录里产出的 `.exe` 是正常程序，不是用户要找的
/// 临时/缓存文件，因此对 `Kind::Program` 不按噪声降权。
/// 与 `config::default_noise_rules` 里的构建规则保持同步；用户自定义的
/// `xxx-target/` 这类目录也一并豁免（`is_under_path` 式的边界规则在这里用
/// 子串集合表达，因为噪声规则本身就是路径片段匹配）。
fn is_build_dir_pattern(pattern: &str) -> bool {
    matches!(
        pattern,
        "/target/"
            | "-target/"
            | "/target/debug/"
            | "/target/release/"
            | "/target/deps/"
            | "/target/.fingerprint/"
            | "/target/incremental/"
            | "/target/doc/"
            | "-target/debug/"
            | "-target/release/"
            | "-target/deps/"
            | "-target/.fingerprint/"
            | "-target/incremental/"
            | "-target/doc/"
            | ".fingerprint/"
            | "/bin/debug/"
            | "/bin/release/"
            | "/obj/"
            | "cmake-build-debug/"
            | "cmake-build-release/"
    )
}

/// 噪声惩罚（测试用：验证规则命中的回归口径）。生产热路径走
/// [`noise_penalty_prepared`]。
#[cfg(test)]
fn noise_penalty(path: &str, needle: &str, kind: Kind, cfg: &RankConfig) -> i32 {
    // 规则表按小写正斜杠保存，路径也统一归一化（含大小写）后再匹配。
    let normalized_path = path.replace('\\', "/").to_lowercase();
    let rules: Vec<(String, i32)> = cfg
        .noise_rules
        .iter()
        .filter_map(|rule| {
            let pattern = rule.pattern.trim().to_lowercase().replace('\\', "/");
            (!pattern.is_empty()).then_some((pattern, rule.penalty))
        })
        .collect();
    noise_penalty_prepared(&normalized_path, needle, kind, cfg, &rules)
}

/// [`noise_penalty`] 的预归一化规则版本（rank 热路径用，避免逐候选重复
/// trim/lowercase 70+ 条规则）。
fn noise_penalty_prepared(
    normalized_path: &str,
    needle: &str,
    kind: Kind,
    cfg: &RankConfig,
    rules: &[(String, i32)],
) -> i32 {
    let mut worst = 0;
    for (pattern, penalty) in rules {
        if !normalized_path.contains(pattern) {
            continue;
        }
        // 构建目录下的 .exe 是正常程序产物，不罚。
        if kind == Kind::Program && is_build_dir_pattern(pattern) {
            continue;
        }
        // 「用户在找噪声目录本身」：查询词包含在片段里，如 cache / temp。
        if needle.len() >= 2 && pattern.contains(needle) {
            continue;
        }
        worst = worst.max((*penalty).clamp(0, 500));
    }
    worst * cfg.noise_strength.clamp(0, 200) / 100
}

fn path_adjustment(
    entry: &Entry,
    path: &str,
    needle: &str,
    cfg: &RankConfig,
    rules: &[(String, i32)],
) -> i32 {
    let normalized_path = path.replace('\\', "/");
    let cap = heuristic_cap(cfg);
    // 分成「条目自身细节」和「环境上下文」两块额度：
    // - 细节（短名、浅路径）决定同一档位内谁更靠前，不能被上下文挤掉；
    // - 上下文（home/parent/cwd）只该拉近与当前目录的距离，不能把档位抹平。
    // 两块合计严格小于 cap，因此永远跨不过下一档。
    let detail_cap = cap / 2;
    let name_raw = (30 - (entry.name.chars().count() as i32).min(30)).clamp(0, 30);
    let depth_raw = (20 - (path.matches('\\').count() as i32).min(20)).clamp(0, 20);
    let mut hint = (name_raw + depth_raw) * detail_cap / 50;

    let env = env_paths();
    let mut context = 0;
    if let Some(home) = &env.home {
        if is_under_path(&path, home) {
            context += 20;
        }
    }
    if let Some(parent) = &env.cwd_parent {
        if is_under_path(&path, parent) {
            context += 60;
        }
    }
    if let Some(cwd) = &env.cwd {
        if is_under_path(&path, cwd) {
            context += 90;
        }
    }
    hint += context.min(cap - detail_cap);

    // 噪声惩罚是「排除」信号，不受上界限制：该沉下去的就要沉下去。
    hint - noise_penalty_prepared(&normalized_path, needle, entry.kind, cfg, rules)
}

/// 路径边界匹配：dir 本身，或 dir 后紧跟路径分隔符才算在目录下。
/// 裸 starts_with 会把 `lstr-wt-p0-target` 误当成 `lstr-wt-p0` 的子目录。
fn is_under_path(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches(|c| c == '\\' || c == '/');
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('\\') || rest.starts_with('/'))
}

fn match_tier(
    entry: &Entry,
    needle: &str,
    cfg: &RankConfig,
    name: &str,
    path_lc: &str,
) -> i32 {
    if entry.kind == Kind::Command {
        for alias in crate::search::command_aliases(&entry.id) {
            let alias = alias.to_lowercase();
            if alias == needle {
                return cfg.exact;
            }
            if alias.starts_with(needle) {
                return cfg.prefix;
            }
            if alias.contains(needle) {
                return cfg.substring;
            }
        }
    }
    if name == needle {
        return cfg.exact;
    }
    if name.starts_with(needle) {
        return cfg.prefix;
    }
    if let Some(byte_pos) = name.find(needle) {
        // 词首匹配（app_config → 命中 config）比词中匹配更像目标；
        // 出现位置越靠前越好。
        let char_pos = name[..byte_pos].chars().count() as i32;
        let position_bonus = (SUBSTRING_POSITION_BONUS - char_pos.min(SUBSTRING_POSITION_BONUS)).max(0);
        let boundary = byte_pos == 0
            || name[..byte_pos]
                .chars()
                .last()
                .is_some_and(|ch| matches!(ch, '_' | '-' | '.' | ' ' | '(' | '['));
        let boundary_bonus = if boundary { SUBSTRING_BOUNDARY_BONUS } else { 0 };
        return cfg.substring + boundary_bonus + position_bonus;
    }

    // 多词查询：所有词都出现在名称里（`my report` → `my_report.pdf`）。
    // Everything 已按 AND 过滤，这里给个合理档位，别让它们沉底。
    if needle.contains(char::is_whitespace) {
        let tokens: Vec<&str> = needle.split_whitespace().collect();
        if tokens.len() > 1 && tokens.iter().all(|token| name.contains(token)) {
            return cfg.tokens;
        }
    }

    // 分隔符不敏感：`myreport` → `my_report.pdf`。
    let normalize = |text: &str| -> String {
        text.chars()
            .filter(|ch| !matches!(ch, '_' | '-' | '.' | ' '))
            .collect()
    };
    let normalized_needle = normalize(needle);
    if normalized_needle.chars().count() >= 3
        && normalize(&name).contains(&normalized_needle)
    {
        return cfg.normalized;
    }

    let mut pinyin_substring = false;
    let candidates = entry
        .pinyin
        .iter()
        .flat_map(|pinyin| pinyin.split(' '))
        .chain(std::iter::once(entry.initial.as_str()));
    for candidate in candidates {
        let candidate = candidate.to_lowercase();
        if candidate.is_empty() {
            continue;
        }
        if candidate.starts_with(needle) {
            return cfg.pinyin_prefix;
        }
        if candidate.contains(needle) {
            pinyin_substring = true;
        }
    }
    if pinyin_substring {
        return cfg.pinyin_substring;
    }

    // 子序列模糊只作用于本地索引能新产出结果的类别（应用/命令/程序）；
    // 文件类由 Everything 按子串过滤，模糊档加不出新结果只会带来噪声。
    if matches!(entry.kind, Kind::App | Kind::Command | Kind::Program) {
        let mut best = crate::search::fuzzy::score_dense(&entry.name, needle);
        if best.is_none() {
            if let Some(pinyin) = &entry.pinyin {
                // 拼音容错（wxin → weixin）：逐段密度匹配，得分打八折；
                // 不做跨段拼接、拒绝纯散布命中（噪声）。
                best = pinyin
                    .split(' ')
                    .filter_map(|token| crate::search::fuzzy::score_dense(token, needle))
                    .map(|s| s * 4 / 5)
                    .max();
            }
        }
        if let Some(score) = best {
            return cfg.fuzzy + score.clamp(0, 49);
        }
    }

    if path_lc.contains(needle) {
        return cfg.path_only;
    }

    0
}

/// 空查询时的类型偏好；最近使用分会叠加在这之上。
fn kind_preference(kind: Kind) -> i32 {
    match kind {
        Kind::App => 20,
        Kind::Folder => 10,
        Kind::Program => 8,
        Kind::File => 5,
        Kind::Command => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs()
    }

    fn entry(id: &str, name: &str, kind: Kind) -> Entry {
        Entry {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            path: Some(PathBuf::from(format!("C:\\tools\\{name}"))),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        }
    }

    fn history(entries: &[(&str, u32, u64)]) -> History {
        let mut history = History::in_memory();
        for (id, count, last) in entries {
            history.seed(&entry(id, id, Kind::File), *count, *last);
        }
        history
    }

    #[test]
    fn noise_directories_are_downranked() {
        let mk = |id: &str, path: &str| {
            let mut e = entry(id, "settings.json", Kind::File);
            e.path = Some(std::path::PathBuf::from(path));
            e
        };
        let history = History::in_memory();
        let clean = mk("clean", r"D:\proj\pkg\settings.json");
        for path in [
            r"D:\proj\node_modules\pkg\settings.json",
            r"D:\proj\target\debug\settings.json",
            r"D:\project\.git\settings.json",
            r"C:\Users\x\AppData\Local\Temp\settings.json",
            r"C:\Windows\WinSxS\settings.json",
            r"D:\py\Lib\site-packages\pkg\settings.json",
            r"D:\go\pkg\mod\pkg\settings.json",
        ] {
            let noisy = mk("noisy", path);
            let delta = score_entry(&clean, "settings", &history, 3)
                - score_entry(&noisy, "settings", &history, 3);
            assert!(delta >= 60, "噪声目录应明显降权: {path} (delta={delta})");
        }
    }

    #[test]
    fn searching_noise_keyword_disables_penalty() {
        // 用户明确搜 "cache" 时，.cache 目录不该被降权。
        let mut noisy = entry("noisy", "cache", Kind::Folder);
        noisy.path = Some(std::path::PathBuf::from(r"D:\proj\.cache\cache"));
        let mut clean = entry("clean", "cache", Kind::Folder);
        clean.path = Some(std::path::PathBuf::from(r"D:\proj\cache"));
        let history = History::in_memory();
        let a = score_entry(&noisy, "cache", &history, 0);
        let b = score_entry(&clean, "cache", &history, 0);
        assert!((b - a).abs() <= 4, "搜 cache 不应降权: noisy={a} clean={b}");
    }

    #[test]
    fn temp_file_names_are_downranked() {
        let cfg = RankConfig::default();
        assert!(noise_penalty(r"C:\x\~$report.docx", "report", Kind::File, &cfg) > 0);
        assert!(noise_penalty(r"C:\x\data.tmp", "data", Kind::File, &cfg) > 0);
        assert_eq!(
            noise_penalty(r"C:\x\video.mp4.crdownload", "video", Kind::File, &cfg),
            100
        );
        // 用户明确在找它时不罚。
        assert_eq!(noise_penalty(r"C:\x\Thumbs.db", "thumbs", Kind::File, &cfg), 0);
        assert_eq!(noise_penalty(r"C:\x\data.tmp", "tmp", Kind::File, &cfg), 0);
        assert_eq!(noise_penalty(r"C:\x\report.docx", "report", Kind::File, &cfg), 0);
    }

    #[test]
    fn adjacent_target_dirs_are_not_penalized_but_build_artifacts_are() {
        // M4 误伤反例：目录名以 -target 结尾，但不是 Cargo 构建目录。
        let cfg = RankConfig::default();
        assert_eq!(
            noise_penalty(r"D:\work\acme-target\src\main.rs", "main", Kind::File, &cfg),
            0
        );
        assert_eq!(
            noise_penalty(r"D:\work\build-target\src\lib.rs", "lib", Kind::File, &cfg),
            0
        );
        // 真实产物子目录仍应明显降权。
        for path in [
            r"D:\work\lstr-wt-p0-target\debug\deps\meta.json",
            r"D:\work\lstr-wt-p0-target\release\notes.txt",
            r"D:\work\lstr-wt-p0-target\deps\meta.json",
            r"D:\work\lstr-wt-p0-target\.fingerprint\abc\bin-lstr.json",
            r"D:\work\lstr-wt-p0-target\incremental\xyz.bin",
            r"D:\work\lstr-wt-p0-target\doc\lstr\index.html",
        ] {
            assert!(
                noise_penalty(path, "lstr", Kind::File, &cfg) >= 100,
                "真实构建产物应降权: {path}"
            );
        }
    }

    #[test]
    fn program_in_build_dirs_is_not_penalized() {
        // L1：构建目录里的 exe 是正常程序产物，不该按噪声降权。
        let cfg = RankConfig::default();
        for path in [
            r"D:\work\lstr-wt-p0-target\debug\lstr.exe",
            r"D:\work\lstr-wt-p0-target\.fingerprint\app.exe",
            r"D:\work\cmake-build-debug\app.exe",
            r"D:\work\bin\Debug\app.exe",
            r"D:\work\obj\x64\app.exe",
        ] {
            assert_eq!(
                noise_penalty(path, "lstr", Kind::Program, &cfg),
                0,
                "构建产物 exe 应豁免: {path}"
            );
        }
        // 普通临时目录里的 exe 仍然降权（这些 exe 不是构建产物）。
        assert!(
            noise_penalty(r"C:\Users\x\AppData\Local\Temp\app.exe", "lstr", Kind::Program, &cfg) > 0
        );
    }

    #[test]
    fn word_boundary_matches_rank_higher() {
        // app_config 命中 config 应比 appxconfig 命中更靠前。
        let boundary = entry("b", "app_config.json", Kind::File);
        let middle = entry("m", "appxconfig.json", Kind::File);
        let history = History::in_memory();
        assert!(
            score_entry(&boundary, "config", &history, 0)
                > score_entry(&middle, "config", &history, 0)
        );
    }

    #[test]
    fn multi_word_queries_match_separated_names() {
        // my report → my_report_2024.pdf：所有词都在名称里，不该沉底。
        let entry = entry("e", "my_report_2024.pdf", Kind::File);
        let score = score_entry(&entry, "my report", &History::in_memory(), 0);
        assert!(score >= RankConfig::default().tokens, "多词全含应有 550 档: {score}");
    }

    #[test]
    fn separator_insensitive_match() {
        // myreport → my_report.pdf（忽略下划线/连字符/点）。
        let entry = entry("e", "my_report.pdf", Kind::File);
        let score = score_entry(&entry, "myreport", &History::in_memory(), 0);
        assert!(score >= RankConfig::default().normalized, "分隔符不敏感匹配: {score}");
    }

    #[test]
    fn recent_files_get_a_small_boost() {
        let now = now() as i64;
        let mut fresh = entry("fresh", "notes.md", Kind::File);
        fresh.modified = Some(now);
        let mut stale = entry("stale", "notes.md", Kind::File);
        stale.modified = Some(now - 400 * 86400);
        let history = History::in_memory();
        assert!(
            score_entry(&fresh, "notes", &history, 0)
                > score_entry(&stale, "notes", &history, 0)
        );
        // 只是轻微加成，不能压过匹配档位。
        assert!(score_entry(&fresh, "notes", &history, 0)
            - score_entry(&stale, "notes", &history, 0)
            <= RankConfig::default().recent_boost);
    }

    /// 修复 FILETIME 换算后候选带索引 mtime:其余分数完全相同的条目由
    /// recency 决定顺序,分差严格等于 config 的 recent_boost(而非写死)。
    #[test]
    fn newer_mtime_wins_among_otherwise_equal_entries() {
        let now = now() as i64;
        let cfg = RankConfig::default();
        let mut fresh = entry("fresh", "notes.md", Kind::File);
        fresh.modified = Some(now - 3600);
        let mut stale = entry("stale", "notes.md", Kind::File);
        stale.modified = Some(now - 400 * 86400);
        let history = History::in_memory();

        let fresh_score = score_entry_with(&fresh, "notes", &history, 0, &cfg);
        let stale_score = score_entry_with(&stale, "notes", &history, 0, &cfg);
        assert_eq!(
            fresh_score - stale_score,
            cfg.recent_boost,
            "同匹配档位下只应差 recency 权重"
        );

        // 输入顺序故意把旧的放前面,rank 后新的必须排第一。
        let mut entries = vec![stale, fresh];
        rank_with(&mut entries, "notes", &history, 0, &cfg);
        assert_eq!(entries[0].id, "fresh");
        assert_eq!(entries[1].id, "stale");
    }

    /// 索引 mtime=0 时候选回落 None:recency 不计分,不改变同分条目的顺序。
    #[test]
    fn unknown_mtime_falls_back_to_none_without_reordering() {
        let now = now() as i64;
        let cfg = RankConfig::default();
        let unknown = entry("unknown", "notes.md", Kind::File); // modified=None
        let mut stale = entry("stale", "notes.md", Kind::File);
        stale.modified = Some(now - 400 * 86400); // 超过 30 天,recency=0
        let history = History::in_memory();

        assert_eq!(
            score_entry_with(&unknown, "notes", &history, 0, &cfg),
            score_entry_with(&stale, "notes", &history, 0, &cfg),
            "未知 mtime 不能凭空加分"
        );
        let mut entries = vec![unknown, stale];
        rank_with(&mut entries, "notes", &history, 0, &cfg);
        assert_eq!(entries[0].id, "unknown", "同分稳定排序应保持输入顺序");
        assert_eq!(entries[1].id, "stale");
    }

    #[test]
    fn exact_match_prefers_shallow_paths() {
        // E:\use 与深埋的同名目录都能精确命中时，浅路径优先。
        let mut shallow = entry("shallow", "use", Kind::Folder);
        shallow.path = Some(std::path::PathBuf::from(r"E:\use"));
        let mut deep = entry("deep", "use", Kind::Folder);
        deep.path = Some(std::path::PathBuf::from(
            r"C:\Users\someone\.workbuddy\vendor\PortableGit\usr\share\perl5\Test\use",
        ));
        let mut entries = vec![deep, shallow];
        rank(&mut entries, "use", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "shallow");
    }

    #[test]
    fn fuzzy_app_match_scores_above_zero() {
        let mut entries = vec![
            entry("app", "Visual Studio Code", Kind::App),
            entry("file", "notes.txt", Kind::File),
        ];
        rank(&mut entries, "vsc", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "app");
    }

    #[test]
    fn name_substring_beats_fuzzy() {
        let mut entries = vec![
            entry("fuzzy", "Visual Studio Code", Kind::App),
            entry("sub", "vsc-backup.zip", Kind::File),
        ];
        rank(&mut entries, "vsc", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "sub");
    }

    #[test]
    fn files_do_not_fuzzy_match() {
        // 文件类结果由 Everything 按子串过滤，客户端不再做模糊，避免噪声。
        let file = entry("f", "calendar.ics", Kind::File);
        assert_eq!(score_entry(&file, "cdr", &History::in_memory(), 3), 0);
    }

    #[test]
    fn name_prefix_beats_name_substring() {
        let mut entries = vec![
            entry("sub", "my-report.txt", Kind::File),
            entry("prefix", "report.txt", Kind::File),
        ];
        rank(&mut entries, "report", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "prefix");
    }

    #[test]
    fn name_exact_beats_pinyin_prefix() {
        let exact = entry("exact", "wx", Kind::File);
        let mut pinyin = entry("pinyin", "微信", Kind::File);
        pinyin.pinyin = Some("weixin".to_string());
        pinyin.initial = "wx".to_string();
        let mut entries = vec![pinyin, exact];
        rank(&mut entries, "wx", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "exact");
    }

    #[test]
    fn pinyin_prefix_beats_path_substring() {
        let mut pinyin = entry("pinyin", "微信", Kind::File);
        pinyin.pinyin = Some("weixin".to_string());
        pinyin.initial = "wx".to_string();
        let mut path = entry("path", "readme.txt", Kind::File);
        path.path = Some(PathBuf::from("C:\\wx-tools\\readme.txt"));
        let mut entries = vec![path, pinyin];
        rank(&mut entries, "wx", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "pinyin");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let entry = entry("upper", "REPORT.TXT", Kind::File);
        assert!(score_entry(&entry, "report", &History::in_memory(), 3) > 0);
    }

    #[test]
    fn history_boosts_equal_matches() {
        let mut entries = vec![
            entry("cold", "report-old.txt", Kind::File),
            entry("hot", "report-new.txt", Kind::File),
        ];
        let history = history(&[("hot", 5, now())]);
        rank(&mut entries, "report", &history, 3);
        assert_eq!(entries[0].id, "hot");
    }

    #[test]
    fn empty_query_puts_most_recent_first() {
        let t = now();
        let mut entries = vec![
            entry("stale", "stale.txt", Kind::File),
            entry("fresh", "fresh.txt", Kind::File),
        ];
        let history = history(&[
            ("stale", 1, t.saturating_sub(30 * 24 * 60 * 60)),
            ("fresh", 1, t),
        ]);
        rank(&mut entries, "", &history, 3);
        assert_eq!(entries[0].id, "fresh");
    }

    #[test]
    fn app_boost_prefers_apps_on_equal_match() {
        let mut entries = vec![
            entry("file", "notes", Kind::File),
            entry("app", "notes", Kind::App),
        ];
        rank(&mut entries, "notes", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "app");
    }

    #[test]
    fn folder_beats_file_on_tie() {
        let mut entries = vec![
            entry("file", "docs", Kind::File),
            entry("folder", "docs", Kind::Folder),
        ];
        rank(&mut entries, "docs", &History::in_memory(), 0);
        assert_eq!(entries[0].id, "folder");
    }

    #[test]
    fn equal_scores_keep_input_order() {
        let mut entries = vec![
            entry("first", "a.txt", Kind::File),
            entry("second", "b.txt", Kind::File),
        ];
        rank(&mut entries, "txt", &History::in_memory(), 0);
        assert_eq!(entries[0].id, "first");
        assert_eq!(entries[1].id, "second");
    }

    #[test]
    fn heuristic_cap_derives_from_adjacent_tiers() {
        // M1：cap 必须小于最小相邻档位差，否则低一档靠位置分就能反超。
        let cfg = RankConfig::default();
        assert_eq!(heuristic_cap(&cfg), 29, "默认 tokens 550 vs normalized 520 差 30");
        let mut tight = RankConfig::default();
        tight.tokens = 410;
        tight.normalized = 400;
        assert_eq!(heuristic_cap(&tight), 20, "档位差很小时保底 20");
    }

    fn adj(entry: &Entry, needle: &str, cfg: &RankConfig) -> i32 {
        let path = full_path_lowercase(entry);
        let rules: Vec<(String, i32)> = cfg
            .noise_rules
            .iter()
            .filter_map(|rule| {
                let pattern = rule.pattern.trim().to_lowercase().replace('\\', "/");
                (!pattern.is_empty()).then_some((pattern, rule.penalty))
            })
            .collect();
        path_adjustment(entry, &path, needle, cfg, &rules)
    }

    #[test]
    fn path_context_is_bounded_by_heuristic_cap() {
        // 旧用例名不副实：49 分永远碰不到 80。这里让 cwd + 短名 + 浅路径
        // 拉满，仍必须 ≤ cap（旧实现会直接饱和到 80）。
        let cfg = RankConfig::default();
        let cap = heuristic_cap(&cfg);
        let cwd = std::env::current_dir().expect("cwd");
        let mut hot = entry("hot", "a", Kind::Folder);
        hot.path = Some(cwd.join("a"));
        let hint = adj(&hot, "a", &cfg);
        assert!(
            hint <= cap,
            "位置启发式不应跨越匹配档位: hint={hint}, cap={cap}"
        );
        assert!(hint > 0);
    }

    #[test]
    fn cwd_context_keeps_name_and_depth_differential() {
        // M2：context 不再把 cwd 内所有条目抹平成同一个 cap，
        // 短名/浅路径仍应略高于长名/深路径。
        let cfg = RankConfig::default();
        let cwd = std::env::current_dir().expect("cwd");
        let mut short = entry("short", "a", Kind::Folder);
        short.path = Some(cwd.join("a"));
        let mut deep = entry("deep", "a-very-long-folder-name", Kind::Folder);
        deep.path = Some(cwd.join("deep").join("deeper").join("deeper").join("x"));
        let short_hint = adj(&short, "a", &cfg);
        let deep_hint = adj(&deep, "a", &cfg);
        assert!(
            short_hint > deep_hint,
            "cwd 内短名/浅路径仍应更靠前: short={short_hint}, deep={deep_hint}"
        );
    }

    #[test]
    fn tokens_match_not_overtaken_by_cwd_normalized_match() {
        // M1 回归：「my report」多词命中（tokens 550）vs cwd 里只靠分隔符
        // 不敏感的命中（normalized 520）。旧 cap=80 时 cwd 的 +80 让后者反超。
        let cfg = RankConfig::default();
        let cwd = std::env::current_dir().expect("cwd");
        let mut tokens = entry("tokens", "my_report_2024.pdf", Kind::File);
        tokens.path = Some(PathBuf::from(r"D:\archive\my_report_2024.pdf"));
        let mut normalized = entry("normalized", "myre_port.pdf", Kind::File);
        normalized.path = Some(cwd.join("myre_port.pdf"));
        let mut entries = vec![normalized, tokens];
        rank_with(&mut entries, "my report", &History::in_memory(), 3, &cfg);
        assert_eq!(entries[0].id, "tokens");
    }

    #[test]
    fn path_context_requires_a_directory_boundary() {
        let dir = r"e:/use/ai/deepseek/rust/lstr-wt-p0";
        assert!(is_under_path(r"e:/use/ai/deepseek/rust/lstr-wt-p0/src", dir));
        assert!(!is_under_path(
            r"e:/use/ai/deepseek/rust/lstr-wt-p0-target/debug",
            dir
        ));
        assert!(is_under_path(dir, dir));
    }

    #[test]
    fn exact_name_beats_substring_in_current_working_directory() {
        // 复现截图：cwd 下名称蹭到 ai 的构建产物 vs 名称恰好叫 ai 的目录。
        // 再加历史 + query 学习把构建产物顶高——精确同名仍须靠档位差赢。
        let cwd = std::env::current_dir().expect("cwd");
        let mut noisy = entry("noisy", "num-traits-dab2dc0fb3c75e7", Kind::Folder);
        noisy.path = Some(cwd.join("target").join("debug").join("num-traits"));
        let mut exact = entry("exact", "ai", Kind::Folder);
        exact.path = Some(PathBuf::from(r"D:\arch\ai"));
        let mut history = History::in_memory();
        history.seed(&noisy, 40, now());
        for _ in 0..50 {
            history.record_query("ai", &noisy);
        }
        let mut entries = vec![noisy, exact];
        rank(&mut entries, "ai", &history, 3);
        assert_eq!(entries[0].id, "exact");
    }

    #[test]
    fn control_panel_uninstall_beats_file_matches() {
        // 控制面板「卸载程序」：名称精确命中，应压过同名文件和设置页。
        let mut uninstall = entry("sys:卸载程序", "卸载程序", Kind::App);
        uninstall.path = Some(PathBuf::from("appwiz.cpl"));
        let mut entries = vec![
            entry("file:1", "卸载程序.txt", Kind::File),
            entry("file:2", "如何卸载程序.pdf", Kind::File),
            entry("sys:应用和功能", "应用和功能", Kind::App),
            uninstall,
        ];
        rank(&mut entries, "卸载程序", &History::in_memory(), 3);
        assert_eq!(entries[0].id, "sys:卸载程序");
        assert_eq!(
            entries[0].path.as_ref().unwrap().to_string_lossy(),
            "appwiz.cpl"
        );
    }

    #[test]
    fn settings_alias_outranks_frequently_used_app_named_settings() {
        let mut command = entry("cmd:settings", "打开设置", Kind::Command);
        command.path = None;
        let mut settings_app = entry("app:settings", "设置", Kind::App);
        settings_app.path = Some(PathBuf::from(
            r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\设置.lnk",
        ));
        let mut entries = vec![settings_app, command];
        // 应用最近用了 20 次，仍应输给「设置」这个明确命令意图。
        rank(
            &mut entries,
            "设置",
            &history(&[("app:settings", 20, now())]),
            3,
        );
        assert_eq!(entries[0].id, "cmd:settings");
    }
}
