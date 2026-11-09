//! 界面筛选条的数据模型：位置 / 时间 / 类型。
//! 用户只点按钮，不输入 `path:` / `dm:` / `ext:` 语法；
//! 这些枚举负责把选择翻译成 Everything 查询段。

use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub location: Option<LocationFilter>,
    pub time: Option<TimeFilter>,
    pub group: Option<FileGroup>,
}

impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        self.location.is_none() && self.time.is_none() && self.group.is_none()
    }

    /// 生成 Everything 查询段。用户手输的 ext:/path: 优先于按钮选择，
    /// 避免「ext:rs + 类型=图片」这种自相矛盾的组合。
    pub fn query_parts(&self, typed_ext: bool, typed_path: bool) -> Vec<String> {
        let mut parts = Vec::new();
        if !typed_path {
            if let Some(location) = &self.location {
                parts.push(format!("path:{}", location.path().display()));
            }
        }
        if !typed_ext {
            if let Some(group) = &self.group {
                parts.push(format!("ext:{}", group.extensions().join(";")));
            }
        }
        if let Some(time) = &self.time {
            parts.push(time.query().to_string());
        }
        parts
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

    /// Everything 的日期函数（1.4 支持）。
    pub fn query(self) -> &'static str {
        match self {
            TimeFilter::Today => "dm:today",
            TimeFilter::Week => "dm:thisweek",
            TimeFilter::Month => "dm:thismonth",
            TimeFilter::Year => "dm:thisyear",
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
    fn empty_filters_produce_no_query_parts() {
        assert!(SearchFilters::default().is_empty());
        assert!(SearchFilters::default().query_parts(false, false).is_empty());
    }

    #[test]
    fn filters_translate_to_everything_syntax() {
        let filters = SearchFilters {
            location: Some(LocationFilter::Drive('D')),
            time: Some(TimeFilter::Week),
            group: Some(FileGroup::Images),
        };
        let parts = filters.query_parts(false, false);
        assert_eq!(parts[0], r"path:D:\");
        assert_eq!(parts[1], "ext:png;jpg;jpeg;gif;svg;webp;ico;bmp;tif;tiff");
        assert_eq!(parts[2], "dm:thisweek");
    }

    #[test]
    fn typed_syntax_wins_over_buttons() {
        let filters = SearchFilters {
            location: Some(LocationFilter::Drive('D')),
            time: None,
            group: Some(FileGroup::Code),
        };
        // 用户手输了 ext: 和 path: 时，按钮选择让位，避免自相矛盾的交集。
        let parts = filters.query_parts(true, true);
        assert_eq!(parts.len(), 0);
    }

    #[test]
    fn time_labels_and_queries_are_stable() {
        assert_eq!(TimeFilter::Today.query(), "dm:today");
        assert_eq!(TimeFilter::Year.label(), "一年内");
    }

    #[test]
    fn drives_are_letters() {
        for drive in available_drives() {
            assert!(drive.is_ascii_uppercase());
        }
    }

    #[test]
    fn user_locations_only_return_existing_dirs() {
        for location in user_locations() {
            assert!(location.path().is_dir());
            assert!(!location.label().is_empty());
        }
    }
}
