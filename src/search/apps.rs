use crate::model::{Entry, Kind};
use crate::search::{fuzzy, pinyin};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
};

const MAX_DEPTH: usize = 5;
const APP_PATHS_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths";
const EXCLUDED_KEYWORDS: [&str; 13] = [
    "uninstall",
    "卸载",
    "help",
    "帮助",
    "readme",
    "website",
    "主页",
    "administrative tools",
    "管理工具",
    "accessories",
    "附件",
    "system tools",
    "系统工具",
];
const TONES: [u32; 8] = [
    0xE0A040, 0x7C9CD6, 0x6BAF92, 0xD67C8A, 0x6FB7C4, 0xC58BD6, 0x9ABF6B, 0xD6B45C,
];

pub struct AppIndex {
    pub apps: Vec<Entry>,
}

impl AppIndex {
    pub fn load() -> Self {
        let mut apps = Vec::new();
        let mut seen = HashSet::new();

        for dir in start_menu_dirs() {
            collect_links(&dir, 0, &mut apps, &mut seen);
        }
        collect_app_paths(HKEY_CURRENT_USER, &mut apps, &mut seen);
        collect_app_paths(HKEY_LOCAL_MACHINE, &mut apps, &mut seen);
        apps.extend(system_entries());

        apps.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        Self { apps }
    }

    /// 后台加载完成后并入 UWP 应用；同名时保留已有 Win32 条目。
    pub fn append_uwp(&mut self, mut entries: Vec<Entry>) {
        let existing: HashSet<String> = self.apps.iter().map(|entry| entry.name.to_lowercase()).collect();
        entries.retain(|entry| !existing.contains(&entry.name.to_lowercase()));
        self.apps.append(&mut entries);
        self.apps.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
    }

    pub fn search(&self, q: &str, limit: usize) -> Vec<Entry> {
        let needle = q.trim().to_lowercase();
        if needle.is_empty() {
            return self.top(limit);
        }

        let mut matches: Vec<(u8, String, &Entry)> = self
            .apps
            .iter()
            .filter_map(|entry| {
                let name = entry.name.to_lowercase();
                // 不 clone 已有拼音串；只在缺失（罕见）时才现算。
                let py_storage;
                let py = match &entry.pinyin {
                    Some(pinyin) => pinyin.as_str(),
                    None => {
                        py_storage = pinyin::keys(&entry.name);
                        py_storage.as_str()
                    }
                };
                let py_prefix = py.split(' ').any(|token| token.starts_with(&needle));
                let py_contains = py.split(' ').any(|token| token.contains(&needle));
                let rank = if name.starts_with(&needle) {
                    0
                } else if name.contains(&needle) {
                    1
                } else if py_prefix {
                    2
                } else if py_contains {
                    3
                } else if fuzzy::score_gated(&entry.name, &needle).is_some() {
                    // 子序列模糊：vsc → Visual Studio Code（逐字符闸门：
                    // 散字母伪匹配如 rust → Blend for Visual Studio 直接拒绝）。
                    4
                } else {
                    // 拼音容错（wxin → weixin）：中文名的「首字母串」「全拼串」
                    // 逐段密度匹配。纯 ASCII 名的 pinyin key 只是名字小写无空格
                    // 副本：只保留「首字符命中开头」的散字母模式（ps → Photoshop），
                    // 从中间散拼的伪匹配拒绝（rust → blendforvisualstudio：
                    // b≠r，曾在此漏过逐字符闸门）。
                    if entry.name.is_ascii() {
                        let head_ok = py
                            .chars()
                            .next()
                            .zip(needle.chars().next())
                            .map(|(a, b)| a == b)
                            .unwrap_or(false);
                        if !head_ok {
                            return None;
                        }
                    }
                    let ok = py
                        .split(' ')
                        .any(|token| fuzzy::score_dense(token, &needle).is_some());
                    if ok {
                        4
                    } else {
                        return None;
                    }
                };
                Some((rank, name, entry))
            })
            .collect();

        // 小写名称已经算好，比较器不再重复转小写。
        matches.sort_by(|(rank_a, lower_a, a), (rank_b, lower_b, b)| {
            rank_a
                .cmp(rank_b)
                .then_with(|| lower_a.cmp(lower_b))
                .then_with(|| a.name.cmp(&b.name))
        });
        matches
            .into_iter()
            .take(limit)
            .map(|(_, _, entry)| entry.clone())
            .collect()
    }

    pub fn top(&self, limit: usize) -> Vec<Entry> {
        const COMMON: [(&str, i32); 10] = [
            ("visual studio code", 100),
            ("微信", 96),
            ("wechat", 96),
            ("everything", 92),
            ("windows terminal", 88),
            ("文件资源管理器", 84),
            ("chrome", 78),
            ("edge", 74),
            ("notepad", 66),
            ("计算器", 60),
        ];
        let mut apps = self.apps.clone();
        apps.sort_by(|a, b| {
            let name_a = a.name.to_lowercase();
            let name_b = b.name.to_lowercase();
            let boost_a = COMMON
                .iter()
                .find(|(key, _)| name_a.contains(key))
                .map(|(_, boost)| *boost)
                .unwrap_or(0);
            let boost_b = COMMON
                .iter()
                .find(|(key, _)| name_b.contains(key))
                .map(|(_, boost)| *boost)
                .unwrap_or(0);
            boost_b
                .cmp(&boost_a)
                .then_with(|| name_a.cmp(&name_b))
                .then_with(|| a.name.cmp(&b.name))
        });
        apps.truncate(limit);
        apps
    }

    pub fn all(&self) -> &[Entry] {
        &self.apps
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.apps.len()
    }
}

fn start_menu_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for var in ["APPDATA", "PROGRAMDATA"] {
        if let Ok(base) = std::env::var(var) {
            if !base.is_empty() {
                dirs.push(PathBuf::from(base).join("Microsoft\\Windows\\Start Menu\\Programs"));
            }
        }
    }
    dirs
}

fn collect_links(dir: &Path, depth: usize, apps: &mut Vec<Entry>, seen: &mut HashSet<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };
    let mut children: Vec<PathBuf> = read_dir
        .filter_map(|item| item.ok())
        .map(|item| item.path())
        .collect();
    children.sort();

    for child in children {
        if child.is_dir() {
            collect_links(&child, depth + 1, apps, seen);
        } else if child
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
        {
            if let Some(stem) = child
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
            {
                push_app(apps, seen, &stem, child);
            }
        }
    }
}

fn push_app(apps: &mut Vec<Entry>, seen: &mut HashSet<String>, name: &str, path: PathBuf) {
    let name = name.trim();
    if name.is_empty() || is_excluded(name) {
        return;
    }
    if !seen.insert(name.to_lowercase()) {
        return;
    }
    apps.push(Entry {
        id: format!("app:{}", path.display()),
        kind: Kind::App,
        name: name.to_string(),
        path: Some(path),
        size: None,
        modified: None,
        pinyin: Some(pinyin::keys(name)),
        initial: initial_for(name),
        tone: tone_for(name),
    });
}

fn is_excluded(name: &str) -> bool {
    let lower = name.to_lowercase();
    EXCLUDED_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
}

fn initial_for(name: &str) -> String {
    let mut words = name.split_whitespace().filter(|word| !word.is_empty());
    let Some(first_word) = words.next() else {
        return "?".to_string();
    };
    let Some(first) = first_word.chars().next() else {
        return "?".to_string();
    };
    if !first.is_ascii_alphanumeric() {
        return first.to_string();
    }

    let mut initial = String::new();
    initial.push(first.to_ascii_uppercase());
    let second = words
        .next()
        .and_then(|word| word.chars().find(|ch| ch.is_ascii_alphanumeric()))
        .or_else(|| {
            first_word
                .chars()
                .skip(1)
                .find(|ch| ch.is_ascii_alphanumeric())
        });
    if let Some(ch) = second {
        initial.push(ch.to_ascii_uppercase());
    }
    initial
}

fn tone_for(name: &str) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    TONES[(hash % TONES.len() as u64) as usize]
}

fn collect_app_paths(hive: HKEY, apps: &mut Vec<Entry>, seen: &mut HashSet<String>) {
    let subkey = to_wide(APP_PATHS_SUBKEY);
    let mut root = HKEY::default();
    let status = unsafe { RegOpenKeyExW(hive, PCWSTR(subkey.as_ptr()), None, KEY_READ, &mut root) };
    if status != ERROR_SUCCESS {
        return;
    }

    let mut index = 0u32;
    loop {
        let mut name_buf = [0u16; 256];
        let mut name_len = name_buf.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                root,
                index,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            break;
        }
        index += 1;

        let key_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        if let Some(value) = default_value(root, &key_name) {
            push_app(apps, seen, strip_exe(&key_name), PathBuf::from(value));
        }
    }

    unsafe {
        let _ = RegCloseKey(root);
    }
}

fn default_value(parent: HKEY, subkey: &str) -> Option<String> {
    let name = to_wide(subkey);
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(parent, PCWSTR(name.as_ptr()), None, KEY_READ, &mut key) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let value = read_default_string(key);
    unsafe {
        let _ = RegCloseKey(key);
    }
    value
}

fn read_default_string(key: HKEY) -> Option<String> {
    let mut value_type = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR::null(),
            None,
            Some(&mut value_type),
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
        return None;
    }

    let mut buffer = vec![0u8; size as usize];
    let mut read = size;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR::null(),
            None,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr()),
            Some(&mut read),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    buffer.truncate(read as usize);

    let wide: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .take_while(|ch| *ch != 0)
        .collect();
    let value = String::from_utf16_lossy(&wide);
    let value = value.trim().trim_matches('"').trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn strip_exe(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::AppIndex;
    use crate::model::{Entry, Kind};
    use crate::search::pinyin;

    fn app(name: &str) -> Entry {
        Entry {
            id: format!("app:{name}"),
            kind: Kind::App,
            name: name.to_string(),
            path: None,
            size: None,
            modified: None,
            pinyin: Some(pinyin::keys(name)),
            initial: super::initial_for(name),
            tone: super::tone_for(name),
        }
    }

    fn sample() -> AppIndex {
        AppIndex {
            apps: vec![
                app("微信"),
                app("文件资源管理器"),
                app("计算器"),
                app("Visual Studio Code"),
            ],
        }
    }

    #[test]
    fn search_hits_pinyin_initials() {
        let index = sample();
        assert_eq!(index.search("wx", 10)[0].name, "微信");
        assert_eq!(index.search("wj", 10)[0].name, "文件资源管理器");
    }

    #[test]
    fn search_fuzzy_hits_english_names() {
        let index = sample();
        assert_eq!(index.search("vsc", 10)[0].name, "Visual Studio Code");
        // 乱序/漏字母：拼音容错。
        assert_eq!(index.search("wxin", 10)[0].name, "微信");
    }

    #[test]
    fn scattered_letters_do_not_match_english_names() {
        // rust 散布在 "Blend for Visual Studio"（r 在 for、u 在 Visual 中间）：
        // 名字模糊闸门与拼音回退都必须拒绝，不能因为 s/t 的连击加分过线。
        let index = AppIndex {
            apps: vec![app("Blend for Visual Studio"), app("RustDesk")],
        };
        let hits: Vec<String> = index.search("rust", 10).into_iter().map(|e| e.name).collect();
        assert_eq!(hits, vec!["RustDesk".to_string()], "只应命中真名匹配: {hits:?}");
        // 词首 + 连续模式仍然可用
        assert_eq!(index.search("blend", 10)[0].name, "Blend for Visual Studio");
    }

    #[test]
    fn search_hits_name_prefix_and_contains() {
        let index = sample();
        assert_eq!(index.search("code", 10)[0].name, "Visual Studio Code");
        assert_eq!(index.search("code", 10).len(), 1);
    }

    #[test]
    fn search_is_case_insensitive_and_limited() {
        let index = sample();
        assert_eq!(index.search("VISUAL", 10).len(), 1);
        assert_eq!(index.search("i", 1).len(), 1);
    }

    #[test]
    fn search_returns_nothing_for_unknown_query() {
        assert!(sample().search("zzz", 10).is_empty());
    }

    #[test]
    fn top_returns_first_apps_sorted_by_name() {
        let index = sample();
        let top = index.top(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "Visual Studio Code");
        assert_eq!(top[1].name, "微信");
        assert_eq!(index.top(100).len(), 4);
    }

    #[test]
    fn initial_uses_two_ascii_word_initials_or_first_cjk_char() {
        assert_eq!(super::initial_for("Visual Studio Code"), "VS");
        assert_eq!(super::initial_for("Notepad"), "NO");
        assert_eq!(super::initial_for("微信"), "微");
    }

    #[test]
    fn tone_is_stable() {
        assert_eq!(super::tone_for("微信"), super::tone_for("微信"));
        assert_ne!(super::tone_for("微信"), 0);
    }

    #[test]
    fn noise_names_are_excluded() {
        assert!(super::is_excluded("Uninstall 微信"));
        assert!(super::is_excluded("帮助文档"));
        assert!(super::is_excluded("README"));
        assert!(!super::is_excluded("WeChat"));
    }

    #[test]
    fn app_path_entries_drop_exe_suffix() {
        assert_eq!(super::strip_exe("chrome.exe"), "chrome");
        assert_eq!(super::strip_exe("Code.EXE"), "Code");
        assert_eq!(super::strip_exe("微信"), "微信");
    }

    #[test]
    fn uninstall_entry_opens_control_panel_applet() {
        let entries = super::system_entries();
        let uninstall = entries
            .iter()
            .find(|entry| entry.name == "卸载程序")
            .expect("应内置「卸载程序」");
        assert_eq!(
            uninstall.path.as_ref().unwrap().to_string_lossy(),
            "appwiz.cpl"
        );
        assert!(
            entries.iter().any(|entry| entry.name == "程序和功能")
        );
    }

    #[test]
    fn load_does_not_panic() {
        let index = AppIndex::load();
        println!("AppIndex loaded {} apps", index.len());
    }
}


/// 内置系统功能入口：控制面板/管理工具等文件索引覆盖不到的常用功能。
fn system_entries() -> Vec<Entry> {
    const ENTRIES: &[(&str, &str, &str)] = &[
        // (名称, 启动目标, 拼音提示——首字母由 initials 自动生成)
        // 控制面板的「程序和功能」在中文系统里显示为「卸载程序」，指向 appwiz.cpl；
        // ms-settings:appsfeatures 是「设置 → 应用」，两个入口都保留。
        ("卸载程序", "appwiz.cpl", ""),
        ("程序和功能", "appwiz.cpl", ""),
        ("应用和功能", "ms-settings:appsfeatures", ""),
        ("设备管理器", "devmgmt.msc", ""),
        ("磁盘管理", "diskmgmt.msc", ""),
        ("服务", "services.msc", ""),
        ("任务管理器", "taskmgr", ""),
        ("控制面板", "control", ""),
        ("网络连接", "ncpa.cpl", ""),
        ("系统属性", "sysdm.cpl", ""),
        ("电源选项", "powercfg.cpl", ""),
        ("声音设置", "mmsys.cpl", ""),
        ("防火墙", "wf.msc", ""),
        ("事件查看器", "eventvwr.msc", ""),
        ("注册表编辑器", "regedit", ""),
        ("系统信息", "msinfo32", ""),
    ];
    ENTRIES
        .iter()
        .map(|(name, target, _)| Entry {
            id: format!("sys:{name}"),
            kind: Kind::App,
            name: name.to_string(),
            path: Some(PathBuf::from(*target)),
            size: None,
            modified: None,
            pinyin: Some(crate::search::pinyin::keys(name)),
            initial: name.chars().next().unwrap_or('系').to_string(),
            tone: 0x6FB7C4,
        })
        .collect()
}

#[cfg(test)]
mod pinyin_fuzzy_tests {
    use super::AppIndex;

    #[test]
    fn scattered_pinyin_match_is_rejected() {
        // 回归：sz 散布在 jtyjxzs + jintaiyang… 里曾让金太阳教学助手混进结果。
        let mut app = crate::model::Entry {
            id: "app:test".to_string(),
            kind: crate::model::Kind::App,
            name: "金太阳教学助手".to_string(),
            path: None,
            size: None,
            modified: None,
            pinyin: Some("jtyjxzs jintaiyangjiaoxuezhushou".to_string()),
            initial: "金".to_string(),
            tone: 0,
        };
        app.initial = "金".to_string();
        let index = AppIndex { apps: vec![app.clone()] };
        assert!(
            index.search("sz", 20).is_empty(),
            "sz 不应命中金太阳教学助手"
        );
        // 合法的拼音容错仍然工作。
        assert_eq!(index.search("jty", 5)[0].name, "金太阳教学助手");
    }
}

#[cfg(test)]
mod pinyin_matrix {
    use super::AppIndex;
    use crate::model::{Entry, Kind};
    use crate::search::pinyin;

    fn app(name: &str) -> Entry {
        Entry {
            id: format!("app:{name}"),
            kind: Kind::App,
            name: name.to_string(),
            path: None,
            size: None,
            modified: None,
            pinyin: Some(pinyin::keys(name)),
            initial: super::initial_for(name),
            tone: 0,
        }
    }

    fn index(names: &[&str]) -> AppIndex {
        AppIndex {
            apps: names.iter().map(|n| app(n)).collect(),
        }
    }

    #[test]
    fn sz_matrix_hits_and_rejects() {
        let idx = index(&[
            "设置",
            "时钟",
            "声音设置",
            "快速助手",
            "Windows 安全中心",
            "金太阳教学助手",
            "截图工具",
        ]);
        let hit = |q: &str| -> Vec<String> {
            idx.search(q, 50)
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        // 应命中：真拼音（前缀/子串）
        for name in ["设置", "时钟", "声音设置", "快速助手"] {
            let hits = hit("sz");
            assert!(
                hits.iter().any(|n| n == name),
                "sz 应命中 {name}，实际: {hits:?}"
            );
        }
        // 不应命中：散布子序列噪声
        for name in ["Windows 安全中心", "金太阳教学助手", "截图工具"] {
            let hits = hit("sz");
            assert!(
                !hits.iter().any(|n| n == name),
                "sz 不应命中 {name}，实际: {hits:?}"
            );
        }
    }

    #[test]
    fn fuzzy_tolerance_matrix() {
        let idx = index(&["微信", "Visual Studio Code", "Photoshop", "金太阳教学助手"]);
        // 合法容错：漏字母（wxin→weixin）、词首（vsc、ps）、前缀（jty）
        for (q, name) in [
            ("wxin", "微信"),
            ("vsc", "Visual Studio Code"),
            ("ps", "Photoshop"),
            ("jty", "金太阳教学助手"),
        ] {
            let hits: Vec<String> = idx
                .search(q, 20)
                .into_iter()
                .map(|e| e.name)
                .collect();
            assert!(
                hits.iter().any(|n| n == name),
                "{q} 应命中 {name}，实际: {hits:?}"
            );
        }
        // 噪声容错：两个散布字母不应命中长拼音/长名称
        for (q, name) in [("sz", "金太阳教学助手"), ("wz", "微信")] {
            let hits: Vec<String> = idx
                .search(q, 20)
                .into_iter()
                .map(|e| e.name)
                .collect();
            assert!(
                !hits.iter().any(|n| n == name),
                "{q} 不应命中 {name}，实际: {hits:?}"
            );
        }
    }
}
