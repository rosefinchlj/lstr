use crate::assets::{ActionDef, CommandDef, Favorite};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub hotkey: String,
    /// 一屏结果行数：决定窗口自动高度，也限制应用页/最近使用/Quick Switch
    /// 的条数（这些列表没有「滚动加载」）；文件列表首屏固定 100 条。
    pub max_results: usize,
    /// 滚到列表底部「加载更多」的候选上限（100..=3000）。首屏仍固定 100 条，
    /// 上限只决定最多能翻到多少条；越大单次响应越慢（逐条 find_meta + 抓候选）。
    #[serde(default = "default_max_load_results")]
    pub max_load_results: usize,
    pub width: f32,
    /// 用户拖拽后的窗口高度；None 表示按结果数量自动计算。
    #[serde(default)]
    pub height: Option<f32>,
    pub app_boost: i32,
    pub show_meta: bool,
    pub preview: bool,
    /// 前台出现标准文件对话框（#32770）时自动唤出对话框模式。
    #[serde(default = "default_true")]
    pub dialog_assist: bool,
    /// 空查询时显示「最近使用」历史；关掉后只显示启动项。
    #[serde(default = "default_true")]
    pub show_history: bool,
    pub theme: ThemeConfig,
    /// 自定义关键字：输入 "触发词 关键词" 生成网络搜索/命令行结果行。
    #[serde(default = "default_keywords")]
    pub keywords: Vec<KeywordConfig>,
    /// 排序规则（设置面板「排序」页可视化编辑）。
    #[serde(default)]
    pub rank: RankConfig,

    // —— P0-1 可积累资产 ——
    /// 收藏的文件夹/文件，Quick Switch 与菜单的数据源。
    #[serde(default)]
    pub favorites: Vec<Favorite>,
    /// 自定义动作：作用于选中的搜索结果，支持 {action_path}。
    #[serde(default)]
    pub actions: Vec<ActionDef>,
    /// 自定义命令：作用于搜索词/当前文件夹，支持 {query}/{current_folder}。
    #[serde(default)]
    pub commands: Vec<CommandDef>,
    // —— P0-2 / P0-3 开关 ——
    /// Explorer 内「直接键入搜索」：全局低级键盘钩子，默认关闭（需设置页显式开启）。
    /// 判定严格：仅 Explorer 前台 + 非编辑框 + 无修饰键/IME；设置页可随时关闭。
    #[serde(default)]
    pub explorer_typing: bool,

    /// 自研索引引擎（设置面板「搜索」页）。
    #[serde(default)]
    pub index: IndexConfig,
}

/// 滚到列表底部时「加载更多」的上限范围。首屏固定 100 条，这里只限制最多能翻到多少。
pub const MIN_LOAD_RESULTS: usize = 100;
pub const MAX_LOAD_RESULTS: usize = 3000;

fn default_max_load_results() -> usize {
    1000
}

/// 手写 config.toml 的越界值在 load 时夹到合法区间。
fn normalize_max_load_results(value: usize) -> usize {
    value.clamp(MIN_LOAD_RESULTS, MAX_LOAD_RESULTS)
}

/// 自研索引引擎配置。backend:
/// `auto` = worker 可用走内置,否则 Everything;`builtin` 强制内置(不碰
/// Everything);`everything` 只用 Everything。`off` 是历史值,load 时迁移为
/// `everything`(两者搜索行为相同)。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub backend: String,
    /// 排除噪声目录(node_modules/.git/target 等,实测省 53% 内存)。
    pub exclude_noise: bool,
    /// 单次查询候选收集上限(total 仍精确)。
    pub candidate_cap: usize,
    /// 首扫模式(历史字段):`dir` 目录枚举。仅旧配置手动写 `usn` 时保留旧行为。
    /// 新的实时层选择见 `incremental`。
    pub scan_mode: String,
    /// 历史字段：`dir` 首扫后是否轮询 journal（旧的 usn 首扫模式用）。
    /// 新配置请用 `incremental`。
    pub journal_enabled: bool,
    /// 内置索引的实时更新方式：
    /// - `usn`（默认）：目录首扫 + USN journal 轮询；快照游标可信时免扫直读。
    ///   卷打不开 / journal 不可用时，worker 按卷自动退回目录监视。
    /// - `watch`：只用 ReadDirectoryChangesW 目录监视，零 USN FSCTL。
    /// 仅在 backend 使用内置索引时生效（everything/off 忽略）。
    #[serde(default = "default_incremental")]
    pub incremental: String,
    /// 只索引这些盘(如 `["F"]`);空 = 全部本机固定 NTFS 卷。
    pub volumes: Vec<String>,
    /// 负载整形:dirscan 批次间隔(毫秒)。>0 时每个批次枚举后 sleep 一下,
    /// 削峰元数据 IO(2026-11-09 BSOD 取证:真正的风险模型是高并发元数据负载
    /// × ntfs QueryOpen × minifilter;默认 0 = 不限,行为与旧版完全一致)。
    #[serde(default)]
    pub scan_throttle_ms: u64,
    /// USN journal 轮询间隔(毫秒),最小 100,默认 500。incremental="usn" 使用。
    #[serde(default = "default_journal_poll_ms")]
    pub journal_poll_ms: u64,
}

fn default_incremental() -> String {
    "usn".to_string()
}

fn default_journal_poll_ms() -> u64 {
    500
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            backend: "auto".into(),
            exclude_noise: true,
            candidate_cap: 2000,
            scan_mode: "dir".into(),
            journal_enabled: false,
            incremental: default_incremental(),
            volumes: Vec::new(),
            scan_throttle_ms: 0,
            journal_poll_ms: default_journal_poll_ms(),
        }
    }
}

impl IndexConfig {
    /// 规范化盘符列表:大写、去重、升序。空列表仍表示「全部」。
    pub fn normalize_volumes(&mut self) {
        self.volumes = Self::normalize_letters(self.volumes.iter().filter_map(|v| v.trim().chars().next()));
    }

    /// 数值字段的归一化(Config::load 时调用):
    /// journal_poll_ms 最小 100 —— 过于激进的轮询间隔会放大 FSCTL 频率,
    /// 与 2026-11-09 之后的「降低 Ntfs.sys 触发面」约束冲突。
    pub fn normalize(&mut self) {
        if self.journal_poll_ms < 100 {
            self.journal_poll_ms = 100;
        }
        // 未知值按默认走 USN（有按卷回退，不会丢索引能力）。
        self.incremental = match self.incremental.trim().to_ascii_lowercase().as_str() {
            "watch" => "watch".to_string(),
            _ => "usn".to_string(),
        };
    }

    /// 旧 `backend` 值迁移（Config::load 时调用）：
    /// - `usn` → `auto` + `incremental = usn`（USN 现在只是内置索引的实时层，
    ///   不再是一个“后端”）；
    /// - `off` → `everything`（两者搜索行为相同，去掉重复选项）。
    pub fn migrate_legacy_backend(&mut self) {
        if self.backend == "usn" {
            self.backend = "auto".to_string();
            self.incremental = "usn".to_string();
        } else if self.backend == "off" {
            self.backend = "everything".to_string();
        }
    }

    fn normalize_letters<I>(letters: I) -> Vec<String>
    where
        I: IntoIterator<Item = char>,
    {
        let mut out: Vec<char> = letters
            .into_iter()
            .filter_map(|c| {
                let up = c.to_ascii_uppercase();
                up.is_ascii_uppercase().then_some(up)
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out.into_iter().map(|c| c.to_string()).collect()
    }

    /// 显式配置的卷字母(升序);空 = 未指定 = 全部固定 NTFS 卷。
    pub fn volume_letters(&self) -> Vec<char> {
        let mut out: Vec<char> = self
            .volumes
            .iter()
            .filter_map(|v| v.trim().chars().next())
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.is_ascii_uppercase())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 运行时掩码:bit i = `'A' + i`;空配置 = 全部(`u32::MAX`)。
    /// worker 的卷过滤与 `SetVolumes` 控制帧都用这一份语义(见 `lstr_index::volmask`)。
    pub fn volume_mask(&self) -> u32 {
        lstr_index::volmask::mask_from_letters(self.volume_letters())
    }

    /// 该盘是否参与索引。空配置(全部)时任何盘都返回 true。
    pub fn volume_enabled(&self, letter: char) -> bool {
        lstr_index::volmask::has_letter(self.volume_mask(), letter)
    }

    /// 写入设置页的勾选结果。`selected` = 勾选中的盘,`known` = 设置页可见的全部盘。
    ///
    /// - 全选 → 清空 `volumes`(语义「全部」:以后新插的盘自动纳入,不用再改配置);
    /// - 部分选 → 写显式升序列表;
    /// - 全不选 → 拒绝并返回 `false`(至少保留一个盘,否则配置语义会退回「全部」)。
    pub fn set_selected_volumes(&mut self, selected: &[char], known: &[char]) -> bool {
        let want = Self::normalize_letters(selected.iter().copied());
        if want.is_empty() {
            return false;
        }
        let all = Self::normalize_letters(known.iter().copied());
        // 全集比较:勾满可见的全部盘 = 「全部」,落盘为空列表。
        let covers_all = !all.is_empty() && all.iter().all(|v| want.contains(v));
        self.volumes = if covers_all { Vec::new() } else { want };
        true
    }
}

/// 排序权重：设置面板里可以直接拖滑块调整，实时生效。
/// 数值越大越优先；同档内的位置/词首等微调仍是内置固定值。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RankConfig {
    /// 名称完全一致
    pub exact: i32,
    /// 名称前缀
    pub prefix: i32,
    /// 名称子串
    pub substring: i32,
    /// 多词全含（my report → my_report.pdf）
    pub tokens: i32,
    /// 分隔符不敏感（myreport → my_report.pdf）
    pub normalized: i32,
    /// 拼音前缀
    pub pinyin_prefix: i32,
    /// 拼音子串
    pub pinyin_substring: i32,
    /// 子序列模糊（应用/命令）
    pub fuzzy: i32,
    /// 仅路径包含
    pub path_only: i32,
    /// 历史使用分上限
    pub history_cap: i32,
    /// 最近 7 天修改加成（30 天内自动取一半）
    pub recent_boost: i32,
    /// 噪声目录/临时文件降权强度（百分比，100 = 默认）
    pub noise_strength: i32,
    /// 噪声规则表：路径片段命中即按分值降权。可在设置「噪声规则」页增删改。
    pub noise_rules: Vec<NoiseRule>,
}

/// 一条噪声规则：路径（不区分大小写）包含 `pattern` 时扣 `penalty` 分。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoiseRule {
    pub pattern: String,
    pub penalty: i32,
}

impl Default for RankConfig {
    fn default() -> Self {
        Self {
            exact: 1000,
            prefix: 800,
            substring: 600,
            tokens: 550,
            normalized: 520,
            pinyin_prefix: 450,
            pinyin_substring: 350,
            fuzzy: 300,
            path_only: 200,
            history_cap: 150,
            recent_boost: 15,
            noise_strength: 100,
            noise_rules: default_noise_rules(),
        }
    }
}

/// 内置噪声规则：临时/系统目录、各语言依赖与构建产物、VCS、缓存、浏览器缓存，
/// 以及下载器/Office 留下的临时文件。用户可在设置里随时增删改。
/// 路径片段用正斜杠书写（匹配时会先把路径统一成 /），如 `node_modules/`。
pub fn default_noise_rules() -> Vec<NoiseRule> {
    let patterns: &[(&str, i32)] = &[
        // —— 临时 / 系统 ——
        ("appdata/local/temp/", 150),
        ("windows/temp/", 150),
        ("/temp/", 120),
        ("/tmp/", 100),
        ("$recycle.bin/", 200),
        ("system volume information/", 200),
        ("windows/winsxs/", 200),
        ("windows/installer/", 200),
        ("windows/assembly/", 150),
        // —— JS / TS ——
        ("node_modules/", 120),
        ("bower_components/", 120),
        (".pnpm-store/", 120),
        (".yarn/cache/", 120),
        (".next/", 100),
        (".nuxt/", 100),
        (".svelte-kit/", 100),
        (".turbo/", 100),
        (".parcel-cache/", 100),
        // —— Rust ——
        ("/target/", 120),
        // 目录名带后缀时 /target/ 匹配不到（lstr-wt-p0-target/）。
        // 不能写成裸 `-target/`：那会误伤 `acme-target/`、`build-target/` 这类
        // 正常源码目录。只列出 Cargo 真正会生成产物的子目录。
        ("-target/debug/", 120),
        ("-target/release/", 120),
        ("-target/deps/", 120),
        ("-target/.fingerprint/", 120),
        ("-target/incremental/", 120),
        ("-target/doc/", 120),
        // Cargo 指纹目录：num-traits-xxx 之类纯构建产物，短查询极易误命中。
        (".fingerprint/", 140),
        (".cargo/registry/", 120),
        (".cargo/git/", 120),
        (".rustup/toolchains/", 120),
        // —— Go ——
        ("/pkg/mod/", 120),
        // —— Python ——
        ("site-packages/", 120),
        ("__pycache__", 120),
        (".venv/", 120),
        ("/venv/", 100),
        (".tox/", 120),
        (".mypy_cache/", 100),
        (".pytest_cache/", 100),
        (".ruff_cache/", 100),
        // —— Java / .NET / C++ ——
        (".gradle/", 120),
        (".m2/repository/", 120),
        (".nuget/packages/", 120),
        ("cmakefiles/", 120),
        ("cmake-build-debug/", 120),
        ("cmake-build-release/", 120),
        ("/obj/", 80),
        ("/bin/debug/", 80),
        ("/bin/release/", 80),
        // —— 包管理器依赖 ——
        ("/vendor/", 100),
        // —— VCS / 缓存 ——
        ("/.git/", 120),
        ("/.svn/", 120),
        ("/.hg/", 120),
        ("/cache/", 100),
        ("/.cache/", 100),
        ("/.npm/", 100),
        // —— 浏览器 profile ——
        ("appdata/local/google/chrome/user data/", 120),
        ("appdata/local/microsoft/edge/user data/", 120),
        ("appdata/local/mozilla/firefox/profiles/", 120),
        ("appdata/roaming/mozilla/firefox/profiles/", 120),
        // —— 临时文件 ——
        ("~$", 80),
        (".tmp", 80),
        (".temp", 80),
        (".crdownload", 100),
        (".partial", 80),
        (".part", 60),
        (".download", 60),
        (".!ut", 60),
        ("thumbs.db", 100),
        ("desktop.ini", 100),
        (".ds_store", 100),
    ];
    patterns
        .iter()
        .map(|(pattern, penalty)| NoiseRule {
            pattern: pattern.to_string(),
            penalty: *penalty,
        })
        .collect()
}

impl RankConfig {
    /// 老配置文件不会自动获得新增的内置噪声规则：`noise_rules` 是持久化字段，
    /// 直接反序列化只会拿到旧列表。加载后按 pattern（忽略大小写、斜杠方向）补齐
    /// 缺失的内置规则；用户已有规则连同调过的强度原样保留。
    /// 这样存量用户也能拿到 `-target/debug/`、`.fingerprint/` 等后来新增的规则。
    pub fn backfill_missing_noise_rules(&mut self) {
        use std::collections::HashSet;
        let existing: HashSet<String> = self
            .noise_rules
            .iter()
            .map(|rule| rule.pattern.trim().to_lowercase().replace('\\', "/"))
            .collect();
        for rule in default_noise_rules() {
            let key = rule.pattern.trim().to_lowercase().replace('\\', "/");
            if !existing.contains(&key) {
                self.noise_rules.push(rule);
            }
        }
    }

    /// 当前档位从高到低的展示顺序（设置面板里用来「一眼看明白规则」）。
    pub fn ladder(&self) -> Vec<(&'static str, i32)> {
        let mut tiers = vec![
            ("名称完全一致", self.exact),
            ("名称前缀", self.prefix),
            ("名称子串", self.substring),
            ("多词全含", self.tokens),
            ("分隔符不敏感", self.normalized),
            ("拼音前缀", self.pinyin_prefix),
            ("拼音子串", self.pinyin_substring),
            ("模糊匹配", self.fuzzy),
            ("仅路径包含", self.path_only),
        ];
        tiers.sort_by(|a, b| b.1.cmp(&a.1));
        tiers
    }
}

/// `url` 与 `exec` 二选一；模板里的 {query} 会被替换（url 做百分号编码）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeywordConfig {
    pub trigger: String,
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub exec: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_keywords() -> Vec<KeywordConfig> {
    let url = |trigger: &str, name: &str, template: &str| KeywordConfig {
        trigger: trigger.to_string(),
        name: name.to_string(),
        url: Some(template.to_string()),
        exec: None,
    };
    vec![
        url("g", "Google", "https://www.google.com/search?q={query}"),
        url("bd", "百度", "https://www.baidu.com/s?wd={query}"),
        url("b", "Bing", "https://www.bing.com/search?q={query}"),
        url("gh", "GitHub", "https://github.com/search?q={query}"),
        url("so", "Stack Overflow", "https://stackoverflow.com/search?q={query}"),
    ]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThemeConfig {
    pub mode: String,
    pub accent: String,
    pub opacity: f32,
    pub radius: f32,
    pub row_height: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 默认唤起热键：双击 Ctrl（Listary 式）；double-space 仍是合法选项。
            hotkey: crate::hotkey::DEFAULT_HOTKEY.to_string(),
            max_results: 8,
            max_load_results: default_max_load_results(),
            width: 640.0,
            height: None,
            app_boost: 3,
            show_meta: true,
            preview: true,
            dialog_assist: true,
            show_history: true,
            theme: ThemeConfig::default(),
            keywords: default_keywords(),
            rank: RankConfig::default(),
            favorites: Vec::new(),
            actions: Vec::new(),
            commands: Vec::new(),
            explorer_typing: true,
            index: IndexConfig::default(),
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            mode: "frost".to_string(),
            accent: "#E0A040".to_string(),
            opacity: 0.84,
            radius: 14.0,
            row_height: 44.0,
        }
    }
}

impl ThemeConfig {
    pub fn accent_rgb(&self) -> [u8; 3] {
        parse_hex(&self.accent).unwrap_or([0xE0, 0xA0, 0x40])
    }

    pub fn is_light(&self) -> bool {
        self.mode == "frost"
    }
}

pub fn attach_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join("lstr")
}

impl Config {
    pub fn path() -> PathBuf {
        attach_dir().join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let Ok(text) = fs::read_to_string(&path) else {
            let cfg = Self::default();
            let _ = cfg.save();
            return cfg;
        };
        match toml::from_str::<Config>(&text) {
            // 补齐旧配置缺少的内置噪声规则（新增规则不靠用户手动「恢复默认」）。
            Ok(mut cfg) => {
                cfg.rank.backfill_missing_noise_rules();
                cfg.max_load_results = normalize_max_load_results(cfg.max_load_results);
                cfg.index.backend = normalize_backend(&cfg.index.backend);
                cfg.index.migrate_legacy_backend();
                cfg.index.normalize_volumes();
                cfg.index.normalize();
                // 唤起热键：手写 config.toml 的非法值（如裸字母）回退默认，
                // 避免静默变成「永不生效」或全局吞键。
                cfg.hotkey = crate::hotkey::normalize_hotkey_or_default(&cfg.hotkey);
                cfg
            },
            Err(err) => {
                eprintln!("lstr: config.toml 解析失败（使用默认值）: {err}");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = attach_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
        let text = toml::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;
        // 原子写：先写同目录临时文件再 rename。直接 fs::write 会先截断目标文件，
        // 写到一半崩溃/磁盘满会留下半截 TOML，下次 load 解析失败回落默认值，
        // 用户的收藏/动作/命令会整体「消失」。
        let path = Self::path();
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).map_err(|e| format!("写入配置失败: {e}"))?;
        fs::rename(&tmp, &path).map_err(|e| format!("替换配置失败: {e}"))
    }
}

/// 后端名归一化:接受 auto/builtin/everything;`usn`/`off` 是旧值,
/// 由 [`IndexConfig::migrate_legacy_backend`] 迁移。未知值一律回 auto,
/// 避免手写 config.toml 里的错拼被静默当成「建内置引擎」。
pub fn normalize_backend(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => "auto".into(),
        "builtin" => "builtin".into(),
        "usn" => "usn".into(),
        "everything" => "everything".into(),
        "off" => "off".into(),
        _ => "auto".into(),
    }
}

pub fn parse_hex(input: &str) -> Option<[u8; 3]> {
    let s = input.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

#[cfg(test)]
mod tests {
    use super::*;


    // —— 索引卷选择(index.volumes)——

    #[test]
    fn empty_volumes_means_all_drives() {
        let cfg = IndexConfig::default();
        assert!(cfg.volume_letters().is_empty());
        assert_eq!(cfg.volume_mask(), u32::MAX);
        assert!(cfg.volume_enabled('C'));
        assert!(cfg.volume_enabled('Z'));
    }

    #[test]
    fn volumes_are_normalized_to_upper_sorted_unique() {
        let mut cfg = IndexConfig { volumes: vec!["f".into(), " C ".into(), "c".into(), "1".into()], ..IndexConfig::default() };
        cfg.normalize_volumes();
        assert_eq!(cfg.volumes, vec!["C".to_string(), "F".to_string()]);
        assert_eq!(cfg.volume_letters(), vec!['C', 'F']);
        let mask = cfg.volume_mask();
        assert!(lstr_index::volmask::has_letter(mask, 'C'));
        assert!(!lstr_index::volmask::has_letter(mask, 'D'));
    }

    #[test]
    fn selecting_every_visible_drive_collapses_to_all() {
        let mut cfg = IndexConfig { volumes: vec!["C".into(), "F".into()], ..IndexConfig::default() };
        // 勾满 C/D/E/F → 落盘为空列表(全部),新插盘自动纳入
        assert!(cfg.set_selected_volumes(&['c', 'D', 'e', 'F'], &['C', 'D', 'E', 'F']));
        assert!(cfg.volumes.is_empty());
        assert_eq!(cfg.volume_mask(), u32::MAX);
    }

    #[test]
    fn unchecking_a_drive_writes_explicit_sorted_list() {
        let mut cfg = IndexConfig::default();
        // 从「全部」取消 F:其余 C/D/E 写成显式列表
        assert!(cfg.set_selected_volumes(&['D', 'C', 'E'], &['C', 'D', 'E', 'F']));
        assert_eq!(cfg.volumes, vec!["C".to_string(), "D".to_string(), "E".to_string()]);
        assert!(!cfg.volume_enabled('F'));
        assert!(cfg.volume_enabled('C'));
    }

    #[test]
    fn unchecking_the_last_drive_is_rejected() {
        let mut cfg = IndexConfig { volumes: vec!["F".into()], ..IndexConfig::default() };
        assert!(!cfg.set_selected_volumes(&[], &['C', 'D', 'E', 'F']));
        assert_eq!(cfg.volumes, vec!["F".to_string()]);
    }

    #[test]
    fn backend_names_are_normalized() {
        assert_eq!(normalize_backend("auto"), "auto");
        assert_eq!(normalize_backend("  BuIlTiN "), "builtin");
        assert_eq!(normalize_backend("everything"), "everything");
        assert_eq!(normalize_backend("off"), "off");
        // USN 增量后端:归一化接受,大小写/空白容忍
        assert_eq!(normalize_backend("usn"), "usn");
        assert_eq!(normalize_backend(" USN "), "usn");
        // 手写错拼不再静默创建内置引擎,回落 auto
        assert_eq!(normalize_backend("bogus"), "auto");
        assert_eq!(normalize_backend(""), "auto");
    }

    /// 提交约束:backend 默认值(auto)与既有字段默认值不变;新增字段默认
    /// 是「零行为差异」(不限流 + 500ms 轮询)。
    #[test]
    fn throttle_and_poll_defaults_are_neutral() {
        let cfg = IndexConfig::default();
        assert_eq!(cfg.backend, "auto");
        assert_eq!(cfg.scan_mode, "dir");
        assert!(!cfg.journal_enabled);
        assert_eq!(cfg.scan_throttle_ms, 0, "默认不限流,行为与旧版一致");
        assert_eq!(cfg.journal_poll_ms, 500);
    }

    /// 旧配置文件缺新字段 → serde default 补齐(向后兼容读取)。
    #[test]
    fn legacy_index_section_gets_new_field_defaults() {
        let cfg: IndexConfig =
            toml::from_str("backend = 'builtin'\n").expect("parse minimal index section");
        assert_eq!(cfg.backend, "builtin");
        assert_eq!(cfg.scan_throttle_ms, 0);
        assert_eq!(cfg.journal_poll_ms, 500);
        assert_eq!(cfg.incremental, "usn", "缺 incremental 时默认走 USN");
    }

    /// 实时层默认 USN；旧 backend=usn 迁移为 auto+usn，off 迁移为 everything。
    #[test]
    fn incremental_defaults_and_legacy_backend_migrate() {
        let cfg = IndexConfig::default();
        assert_eq!(cfg.backend, "auto");
        assert_eq!(cfg.incremental, "usn");

        let mut cfg = IndexConfig { backend: "usn".into(), ..Default::default() };
        cfg.migrate_legacy_backend();
        assert_eq!(cfg.backend, "auto");
        assert_eq!(cfg.incremental, "usn");

        let mut cfg = IndexConfig { backend: "off".into(), ..Default::default() };
        cfg.migrate_legacy_backend();
        assert_eq!(cfg.backend, "everything");

        // 手写值归一化：大小写/空白容忍，未知值回默认 USN。
        let mut cfg = IndexConfig { incremental: " WATCH ".into(), ..Default::default() };
        cfg.normalize();
        assert_eq!(cfg.incremental, "watch");
        let mut cfg = IndexConfig { incremental: "bogus".into(), ..Default::default() };
        cfg.normalize();
        assert_eq!(cfg.incremental, "usn");
    }

    /// 加载更多上限：默认 1000；手写越界值在 load 归一化时夹到 100..=3000；
    /// 旧配置缺少该键时用 serde default 补齐，不会整份回落默认值。
    #[test]
    fn max_load_results_defaults_and_clamps() {
        assert_eq!(Config::default().max_load_results, 1000);
        assert_eq!(normalize_max_load_results(10), MIN_LOAD_RESULTS);
        assert_eq!(normalize_max_load_results(10_000), MAX_LOAD_RESULTS);
        assert_eq!(normalize_max_load_results(1500), 1500);

        let text = toml::to_string(&Config::default()).expect("serialize config");
        let legacy: String = text
            .lines()
            .filter(|line| !line.starts_with("max_load_results"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Config = toml::from_str(&legacy).expect("legacy config without new key");
        assert_eq!(parsed.max_load_results, 1000);
    }

    /// journal_poll_ms 下限 100:配置里手写更小的值在 load 归一化时被抬到 100。
    #[test]
    fn journal_poll_ms_clamped_to_minimum() {
        let mut cfg = IndexConfig { journal_poll_ms: 50, ..IndexConfig::default() };
        cfg.normalize();
        assert_eq!(cfg.journal_poll_ms, 100);
        // 合法值不动
        let mut cfg = IndexConfig { journal_poll_ms: 800, ..IndexConfig::default() };
        cfg.normalize();
        assert_eq!(cfg.journal_poll_ms, 800);
    }

    /// 唤起热键：默认是 double-ctrl；非法配置串 load 时回退默认。
    #[test]
    fn hotkey_defaults_to_double_ctrl_and_falls_back_on_garbage() {
        let cfg = Config::default();
        assert_eq!(cfg.hotkey, "double-ctrl", "默认唤起热键回归：双击 Ctrl");

        // 归一化路径：合法值保持规范形（double-space 仍是合法选项）。
        assert_eq!(
            crate::hotkey::normalize_hotkey_or_default("Double-Space"),
            "double-space"
        );
        assert_eq!(
            crate::hotkey::normalize_hotkey_or_default("ALT+Q"),
            "alt+q"
        );
        // 非法输入回退默认，而不是静默失效。
        assert_eq!(
            crate::hotkey::normalize_hotkey_or_default("q"),
            "double-ctrl"
        );
        assert_eq!(
            crate::hotkey::normalize_hotkey_or_default("whatever"),
            "double-ctrl"
        );
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let text = toml::to_string(&Config::default()).expect("serialize config");
        let parsed: Config = toml::from_str(&text).expect("deserialize config");
        assert!(parsed.height.is_none());
        assert_eq!(parsed.theme.mode, "frost");
        assert!(parsed.explorer_typing);
        assert!(parsed.favorites.is_empty());
    }

    #[test]
    fn legacy_config_without_noise_rules_gets_builtin_rules() {
        // M3：模拟存量配置——rank 段存在但没有规则（新规则从未写入过）。
        // load 走的是同一个 backfill 调用，因此这里覆盖「加载后补齐」。
        let mut value = toml::Value::try_from(Config::default()).expect("config → toml value");
        value["rank"]["noise_rules"] = toml::Value::Array(Vec::new());
        let text = toml::to_string(&value).expect("serialize legacy config");
        let mut cfg: Config = toml::from_str(&text).expect("deserialize legacy config");
        assert!(cfg.rank.noise_rules.is_empty());

        cfg.rank.backfill_missing_noise_rules();

        let defaults = default_noise_rules();
        for rule in &defaults {
            assert!(
                cfg.rank
                    .noise_rules
                    .iter()
                    .any(|existing| existing.pattern.eq_ignore_ascii_case(&rule.pattern)),
                "加载后缺少内置规则: {}",
                rule.pattern
            );
        }
        assert_eq!(cfg.rank.noise_rules.len(), defaults.len());
    }

    #[test]
    fn backfill_keeps_user_rules_and_adjusted_strength() {
        let mut cfg = RankConfig::default();
        cfg.noise_rules.clear();
        cfg.noise_rules.push(NoiseRule {
            pattern: "node_modules/".to_string(),
            penalty: 42,
        });

        cfg.backfill_missing_noise_rules();
        cfg.backfill_missing_noise_rules();

        let hits: Vec<i32> = cfg
            .noise_rules
            .iter()
            .filter(|rule| rule.pattern == "node_modules/")
            .map(|rule| rule.penalty)
            .collect();
        assert_eq!(hits, vec![42], "用户调过的强度必须保留且不重复追加");
    }
}
