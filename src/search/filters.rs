//! 界面筛选条的数据模型：位置 / 时间 / 类型，以及输入框提交的日期/大小条件。
//! 用户只点按钮/输入条件，不直接写引擎查询语法；这些枚举由
//! `builtin::build_spec` 翻译成引擎查询规格。

use std::path::PathBuf;

/// 文件名匹配选项（与位置/时间/类型不同，不参与 `is_empty` 判断）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchOptions {
    /// 区分 ASCII 大小写（非 ASCII 本来就是字节精确）。
    pub case_sensitive: bool,
    /// 全词匹配：token 前后必须是词边界。
    pub whole_word: bool,
    /// 正则模式：整个输入作为正则表达式。
    pub regex: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchToggle {
    CaseSensitive,
    WholeWord,
    Regex,
}

impl MatchOptions {
    pub fn is_default(self) -> bool {
        !self.case_sensitive && !self.whole_word && !self.regex
    }

    pub fn get(self, toggle: MatchToggle) -> bool {
        match toggle {
            MatchToggle::CaseSensitive => self.case_sensitive,
            MatchToggle::WholeWord => self.whole_word,
            MatchToggle::Regex => self.regex,
        }
    }

    pub fn set(&mut self, toggle: MatchToggle, value: bool) {
        match toggle {
            MatchToggle::CaseSensitive => self.case_sensitive = value,
            MatchToggle::WholeWord => {
                self.whole_word = value;
                if value {
                    // 全词与正则互斥（正则有自己的边界语法）。
                    self.regex = false;
                }
            }
            MatchToggle::Regex => {
                self.regex = value;
                if value {
                    // 正则与全词互斥；区分大小写仍可与正则组合。
                    self.whole_word = false;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub location: Option<LocationFilter>,
    pub time: Option<TimeFilter>,
    pub group: Option<FileGroup>,
    /// 输入框里提交的日期条件（修改时间，unix 秒，含边界）。
    /// 与 `time` 预设是交集语义：两者都设置时取更紧的上下界。
    pub modified: Option<DateRange>,
    /// 输入框里提交的大小条件（字节，含边界）。目录不参与大小过滤。
    pub size: Option<SizeRange>,
    /// 输入框里提交的盘符条件（`C:` / `-C:`，可多个）。
    /// 与筛选条 `location` 独立：两者同时存在时在引擎层求交集。
    pub drives: Vec<DriveFilter>,
    /// 文件名匹配选项。`is_empty` 刻意忽略它：开启匹配选项不应让
    /// 应用/命令/关键字结果被当成“有筛选”而消失。
    pub matches: MatchOptions,
}

impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        self.location.is_none()
            && self.time.is_none()
            && self.group.is_none()
            && self.modified.is_none()
            && self.size.is_none()
            && self.drives.is_empty()
    }

    /// 输入框里是否有条件 chip（日期 / 大小 / 盘符）。
    pub fn has_input_conditions(&self) -> bool {
        self.modified.is_some() || self.size.is_some() || !self.drives.is_empty()
    }

    /// 盘符 chip 的卷掩码：None=不限制；Some(0)=所有卷都排除（结果为空）。
    pub fn drive_mask(&self) -> Option<u32> {
        if self.drives.is_empty() {
            return None;
        }
        let mut include = 0u32;
        let mut exclude = 0u32;
        for drive in &self.drives {
            if drive.exclude {
                exclude |= drive.bit();
            } else {
                include |= drive.bit();
            }
        }
        let base = if include == 0 {
            (1u32 << 26) - 1
        } else {
            include
        };
        Some(base & !exclude)
    }
}

/// 两个“可选卷掩码”求交：None=不限，与任何掩码求交都返回另一个。
pub fn intersect_drive_masks(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a & b),
        (Some(mask), None) | (None, Some(mask)) => Some(mask),
        (None, None) => None,
    }
}

/// 路径是否落在卷掩码内。非盘符路径（UNC/相对路径）不参与裁剪。
pub fn path_in_drive_mask(path: &std::path::Path, mask: u32) -> bool {
    let text = path.to_string_lossy();
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
            let bit = 1u32 << (letter.to_ascii_uppercase() as u32 - 'A' as u32);
            mask & bit != 0
        }
        _ => true,
    }
}

/// 输入框提交的盘符条件（一个 chip 一项）。排除盘用 `-C:` 表示。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriveFilter {
    /// 大写盘符 A-Z。
    pub letter: char,
    pub exclude: bool,
}

impl DriveFilter {
    /// chip 显示文案：`C盘` / `排除C盘`。
    pub fn label(&self) -> String {
        if self.exclude {
            format!("排除{}盘", self.letter)
        } else {
            format!("{}盘", self.letter)
        }
    }

    /// 可编辑文本（还原到输入框时用）：`C:` / `-C:`。
    pub fn source(&self) -> String {
        format!("{}{}:", if self.exclude { "-" } else { "" }, self.letter)
    }

    pub fn bit(&self) -> u32 {
        1u32 << (self.letter.to_ascii_uppercase() as u32 - 'A' as u32)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationFilter {
    /// 盘符根目录，如 D:。
    Drive(char),
    /// 桌面/下载/文档等具体目录。
    Folder { path: PathBuf, label: String },
}

impl LocationFilter {
    pub fn label(&self) -> String {
        match self {
            LocationFilter::Drive(drive) => format!("{drive}:"),
            LocationFilter::Folder { label, .. } => label.clone(),
        }
    }

    pub fn path(&self) -> PathBuf {
        match self {
            LocationFilter::Drive(drive) => PathBuf::from(format!("{drive}:\\")),
            LocationFilter::Folder { path, .. } => path.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeFilter {
    Today,
    Week,
    Month,
    Year,
}

impl TimeFilter {
    pub fn label(self) -> &'static str {
        match self {
            TimeFilter::Today => "今天",
            TimeFilter::Week => "7 天内",
            TimeFilter::Month => "30 天内",
            TimeFilter::Year => "一年内",
        }
    }

    /// 内置索引用的时间过滤下界(unix 秒,含)。
    /// Today 取本地零点;其余按固定天数回退(不用日历月,够用且不引依赖)。
    pub fn since_unix(self) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match self {
            TimeFilter::Today => now - local_seconds_of_day().unwrap_or(0) as i64,
            TimeFilter::Week => now - 7 * 86_400,
            TimeFilter::Month => now - 30 * 86_400,
            TimeFilter::Year => now - 365 * 86_400,
        }
    }
}

fn local_seconds_of_day() -> Option<u32> {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    Some(st.wHour as u32 * 3600 + st.wMinute as u32 * 60 + st.wSecond as u32)
}

/// 输入框提交的修改时间区间。上下界都是 unix 秒且含边界（与引擎
/// `QuerySpec::mtime_after/before` 语义一致）；`None` = 该方向不限。
///
/// `after_text` / `before_text` 只用于 chip 文案，保留用户输入的比较符
/// （`> 2026-11-10` / `≤ 2026-11-30`），不参与查询。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DateRange {
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub after_text: Option<String>,
    pub before_text: Option<String>,
    /// 原始输入片段（含终结冒号），用于编辑 chip 时还原用户写法
    /// （`date>2026-11-10:` 而不是规范化的 `date >= 2026-11-11:`）。
    pub sources: Vec<String>,
}

impl DateRange {
    pub fn is_empty(&self) -> bool {
        self.after.is_none() && self.before.is_none()
    }

    /// chip 文案：单边保留原始比较符；双边用 ` · ` 连接。
    pub fn label(&self) -> String {
        match (&self.after_text, &self.before_text) {
            (Some(after), Some(before)) => format!("{after} · {before}"),
            (Some(after), None) => after.clone(),
            (None, Some(before)) => before.clone(),
            (None, None) => String::new(),
        }
    }

    /// 合并一条新条件：上下界取更紧的一侧，文案跟着被收紧的那一侧更新。
    /// 合并后为空区间（after > before）返回错误，调用方保留原状态。
    pub fn merge(&mut self, other: DateRange) -> Result<(), String> {
        let after = match (self.after, other.after) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let before = match (self.before, other.before) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let (Some(a), Some(b)) = (after, before) {
            if a > b {
                return Err("日期条件互相冲突，区间为空".to_string());
            }
        }
        if other.after.is_some_and(|b| self.after.is_none_or(|a| b > a)) {
            self.after_text = other.after_text;
        }
        if other
            .before
            .is_some_and(|b| self.before.is_none_or(|a| b < a))
        {
            self.before_text = other.before_text;
        }
        self.after = after;
        self.before = before;
        for source in other.sources {
            if !self.sources.contains(&source) {
                self.sources.push(source);
            }
        }
        Ok(())
    }
}

/// 输入框提交的文件大小区间（字节，含边界）。目录不参与大小过滤；
/// 大小未知的条目（索引里 `u64::MAX`）在条件下一律不通过。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SizeRange {
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub after_text: Option<String>,
    pub before_text: Option<String>,
    /// 原始输入片段（含终结冒号），编辑 chip 时优先还原用户写法
    /// （`size>10MB:` 而不是规范化的 `size >= 10485761B:`）。
    pub sources: Vec<String>,
}

impl SizeRange {
    pub fn is_empty(&self) -> bool {
        self.after.is_none() && self.before.is_none()
    }

    pub fn label(&self) -> String {
        match (&self.after_text, &self.before_text) {
            (Some(after), Some(before)) => format!("{after} · {before}"),
            (Some(after), None) => after.clone(),
            (None, Some(before)) => before.clone(),
            (None, None) => String::new(),
        }
    }

    /// 合并一条新条件：上下界取更紧的一侧，文案跟着被收紧的那一侧更新。
    /// 合并后为空区间（after > before）返回错误，调用方保留原状态。
    pub fn merge(&mut self, other: SizeRange) -> Result<(), String> {
        let after = match (self.after, other.after) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let before = match (self.before, other.before) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let (Some(a), Some(b)) = (after, before) {
            if a > b {
                return Err("大小条件互相冲突，区间为空".to_string());
            }
        }
        if other.after.is_some_and(|b| self.after.is_none_or(|a| b > a)) {
            self.after_text = other.after_text;
        }
        if other
            .before
            .is_some_and(|b| self.before.is_none_or(|a| b < a))
        {
            self.before_text = other.before_text;
        }
        self.after = after;
        self.before = before;
        for source in other.sources {
            if !self.sources.contains(&source) {
                self.sources.push(source);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileGroup {
    Documents,
    Code,
    Images,
    Videos,
    Audio,
}

impl FileGroup {
    pub const ALL: [FileGroup; 5] = [
        FileGroup::Documents,
        FileGroup::Code,
        FileGroup::Images,
        FileGroup::Videos,
        FileGroup::Audio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileGroup::Documents => "文档",
            FileGroup::Code => "代码",
            FileGroup::Images => "图片",
            FileGroup::Videos => "视频",
            FileGroup::Audio => "音频",
        }
    }

    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            FileGroup::Documents => &[
                "doc", "docx", "pdf", "md", "txt", "rtf", "xls", "xlsx", "ppt", "pptx", "csv",
                "wps",
            ],
            FileGroup::Code => &[
                "rs", "ts", "tsx", "js", "jsx", "vue", "py", "go", "java", "c", "h", "cpp",
                "hpp", "cs", "json", "toml", "yaml", "yml", "xml", "html", "css", "sh", "ps1",
                "sql",
            ],
            FileGroup::Images => &["png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "bmp", "tif", "tiff"],
            FileGroup::Videos => &["mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v"],
            FileGroup::Audio => &["mp3", "flac", "wav", "m4a", "aac", "ogg", "wma"],
        }
    }
}

/// 用户常用目录（只返回真实存在的）。
pub fn user_locations() -> Vec<LocationFilter> {
    let Ok(profile) = std::env::var("USERPROFILE") else {
        return Vec::new();
    };
    let profile = PathBuf::from(profile);
    let candidates = [
        ("桌面", "Desktop"),
        ("下载", "Downloads"),
        ("文档", "Documents"),
    ];
    candidates
        .into_iter()
        .filter_map(|(label, sub)| {
            let path = profile.join(sub);
            path.is_dir().then(|| LocationFilter::Folder {
                path,
                label: label.to_string(),
            })
        })
        .collect()
}

/// 当前可用的盘符（C、D、E……）。
pub fn available_drives() -> Vec<char> {
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Vec::new();
    }
    (0..26u32)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| (b'A' + bit as u8) as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_filters_are_empty() {
        assert!(SearchFilters::default().is_empty());
    }

    #[test]
    fn time_labels_are_stable() {
        assert_eq!(TimeFilter::Year.label(), "一年内");
    }

    #[test]
    fn date_range_merges_tighter_bounds() {
        let mut range = DateRange {
            after: Some(100),
            before: Some(300),
            after_text: Some("> 2026-11-01".into()),
            before_text: Some("< 2026-11-30".into()),
            sources: Vec::new(),
        };
        range
            .merge(DateRange {
                after: Some(200),
                before: None,
                after_text: Some("≥ 2026-11-10".into()),
                before_text: None,
                sources: Vec::new(),
            })
            .expect("tighter lower bound");
        assert_eq!(range.after, Some(200));
        assert_eq!(range.before, Some(300));
        assert_eq!(range.label(), "≥ 2026-11-10 · < 2026-11-30");

        // 更松的边界不会改动既有区间。
        range
            .merge(DateRange {
                after: Some(50),
                before: None,
                after_text: Some("> 2026-10-01".into()),
                before_text: None,
                sources: Vec::new(),
            })
            .expect("looser lower bound is ignored");
        assert_eq!(range.after, Some(200));
        assert_eq!(range.label(), "≥ 2026-11-10 · < 2026-11-30");
    }

    #[test]
    fn conflicting_date_range_is_rejected() {
        let mut range = DateRange {
            after: Some(200),
            before: None,
            after_text: Some("≥ 2026-11-10".into()),
            before_text: None,
            sources: Vec::new(),
        };
        let before = range.clone();
        let err = range
            .merge(DateRange {
                after: None,
                before: Some(100),
                after_text: None,
                before_text: Some("< 2026-11-01".into()),
                sources: Vec::new(),
            })
            .expect_err("empty interval must be rejected");
        assert!(err.contains("冲突"));
        assert_eq!(range, before, "失败时不得改动原区间");
    }

    #[test]
    fn drives_are_letters() {
        for drive in available_drives() {
            assert!(drive.is_ascii_uppercase());
        }
    }

    #[test]
    fn size_range_merges_tighter_bounds() {
        let mut range = SizeRange {
            after: Some(1024),
            before: Some(10 * 1024 * 1024),
            after_text: Some("≥ 1 KB".into()),
            before_text: Some("≤ 10 MB".into()),
            sources: Vec::new(),
        };
        range
            .merge(SizeRange {
                after: Some(2 * 1024 * 1024),
                before: None,
                after_text: Some("≥ 2 MB".into()),
                before_text: None,
                sources: Vec::new(),
            })
            .expect("tighter lower bound");
        assert_eq!(range.after, Some(2 * 1024 * 1024));
        assert_eq!(range.before, Some(10 * 1024 * 1024));
        assert_eq!(range.label(), "≥ 2 MB · ≤ 10 MB");

        // 更松的边界不改动既有区间。
        range
            .merge(SizeRange {
                after: Some(1),
                before: None,
                after_text: Some("> 1 B".into()),
                before_text: None,
                sources: Vec::new(),
            })
            .expect("looser lower bound is ignored");
        assert_eq!(range.after, Some(2 * 1024 * 1024));
    }

    #[test]
    fn conflicting_size_range_is_rejected() {
        let mut range = SizeRange {
            after: Some(10 * 1024 * 1024),
            before: None,
            after_text: Some("≥ 10 MB".into()),
            before_text: None,
            sources: Vec::new(),
        };
        let before = range.clone();
        let err = range
            .merge(SizeRange {
                after: None,
                before: Some(1024),
                after_text: None,
                before_text: Some("< 1 KB".into()),
                sources: Vec::new(),
            })
            .expect_err("empty interval must be rejected");
        assert!(err.contains("冲突"));
        assert_eq!(range, before, "失败时不得改动原区间");
    }

    #[test]
    fn size_condition_makes_filters_non_empty() {
        let mut filters = SearchFilters::default();
        filters.size = Some(SizeRange {
            after: Some(1024),
            ..Default::default()
        });
        assert!(!filters.is_empty());
    }

    #[test]
    fn drive_filters_make_filters_non_empty_and_build_mask() {
        let mut filters = SearchFilters {
            drives: vec![
                DriveFilter {
                    letter: 'C',
                    exclude: false,
                },
                DriveFilter {
                    letter: 'd', // 大小写都应归一
                    exclude: false,
                },
            ],
            ..Default::default()
        };
        assert!(!filters.is_empty());
        assert_eq!(filters.drive_mask(), Some((1 << 2) | (1 << 3)));

        // 只排除：从全部卷里去掉 C。
        filters.drives = vec![DriveFilter {
            letter: 'C',
            exclude: true,
        }];
        assert_eq!(filters.drive_mask(), Some(((1u32 << 26) - 1) & !(1 << 2)));

        // include 与 exclude 冲突时排除优先（与 Query::drive_mask 一致）。
        filters.drives = vec![
            DriveFilter {
                letter: 'C',
                exclude: false,
            },
            DriveFilter {
                letter: 'C',
                exclude: true,
            },
        ];
        assert_eq!(filters.drive_mask(), Some(0));

        filters.drives.clear();
        assert!(filters.drive_mask().is_none());
        assert!(filters.is_empty());
    }

    #[test]
    fn drive_masks_intersect_with_query_mask() {
        assert_eq!(intersect_drive_masks(None, None), None);
        assert_eq!(intersect_drive_masks(Some(0b110), None), Some(0b110));
        assert_eq!(intersect_drive_masks(None, Some(0b101)), Some(0b101));
        assert_eq!(
            intersect_drive_masks(Some(0b110), Some(0b101)),
            Some(0b100)
        );
        assert_eq!(intersect_drive_masks(Some(0b110), Some(0b001)), Some(0));
    }

    #[test]
    fn path_drive_mask_matches_letter() {
        use std::path::Path;
        assert!(path_in_drive_mask(Path::new(r"C:\foo"), 1 << 2));
        assert!(!path_in_drive_mask(Path::new(r"d:\foo"), 1 << 2));
        // UNC/相对路径不参与盘符裁剪。
        assert!(path_in_drive_mask(Path::new(r"\\server\share"), 0));
    }

    #[test]
    fn user_locations_only_return_existing_dirs() {
        for location in user_locations() {
            assert!(location.path().is_dir());
            assert!(!location.label().is_empty());
        }
    }

    /// 匹配选项不能算“文件筛选”：否则只开区分大小写时，应用/命令/关键字
    /// 会因 `is_empty() == false` 被整段跳过。
    #[test]
    fn match_options_do_not_make_filters_non_empty() {
        let mut filters = SearchFilters::default();
        filters.matches.case_sensitive = true;
        assert!(filters.is_empty(), "匹配选项不应改变 is_empty 语义");
    }

    /// 输入框日期条件属于文件筛选：激活后应用/命令/关键字应退出。
    #[test]
    fn typed_date_condition_makes_filters_non_empty() {
        let mut filters = SearchFilters::default();
        filters.modified = Some(DateRange {
            after: Some(1),
            ..Default::default()
        });
        assert!(!filters.is_empty());
    }

    #[test]
    fn whole_word_and_regex_are_mutually_exclusive() {
        let mut matches = MatchOptions::default();
        matches.set(MatchToggle::Regex, true);
        matches.set(MatchToggle::WholeWord, true);
        assert!(matches.whole_word);
        assert!(!matches.regex, "开全词应关闭正则");
        matches.set(MatchToggle::Regex, true);
        assert!(matches.regex);
        assert!(!matches.whole_word, "开正则应关闭全词");
        assert!(!matches.is_default());
    }
}
