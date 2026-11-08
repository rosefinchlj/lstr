use crate::assets::{ActionDef, CommandDef, Favorite};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub hotkey: String,
    pub max_results: usize,
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
    /// 文件对话框出现时自动跟随 Explorer 当前目录，默认关闭（会改动对话框路径）。
    #[serde(default)]
    pub dialog_auto_follow: bool,
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
            hotkey: "double-space".to_string(),
            max_results: 8,
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
            dialog_auto_follow: false,
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
        match toml::from_str::<Self>(&text) {
            // 补齐旧配置缺少的内置噪声规则（新增规则不靠用户手动「恢复默认」）。
            Ok(mut cfg) => {
                cfg.rank.backfill_missing_noise_rules();
                cfg
            }
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
