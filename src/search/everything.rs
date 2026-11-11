use crate::model::{Entry, Kind, Query, Scope};
use crate::search::filters::SearchFilters;
use everything_ipc::wm::{EverythingClient, RequestFlags, Sort};
use std::path::PathBuf;

/// Everything 返回的 WIN32 属性位：用索引里的属性判断目录，
/// 避免每个结果一次 `is_dir()` 磁盘 syscall（500 条结果就是 500 次 stat）。
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

pub struct EverythingProvider {
    client: EverythingClient,
}

impl EverythingProvider {
    pub fn new() -> Result<Self, String> {
        EverythingClient::new()
            .map(|client| Self { client })
            .map_err(|err| format!("Everything 未运行: {err}"))
    }

    pub fn search(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<(Vec<Entry>, usize), String> {
        let search = build_search(query, filters);
        self.run(&search, limit, |kind| kept_in_scope(query.scope, kind), query)
    }

    /// 「全部」分路取候选：Everything 在返回前就按排序截断，某一类结果可能
    /// 整类缺席（实测 "rust" 最近 400 条里 0 个文件夹），精确命中的文件夹
    /// 根本进不了排序。因此文件夹/非文件夹各取一次再合并。
    pub fn search_split(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
        folders: bool,
    ) -> Result<(Vec<Entry>, usize), String> {
        let search = build_search_split(query, filters, folders);
        self.run(
            &search,
            limit,
            move |kind| {
                if folders {
                    kind == Kind::Folder
                } else {
                    kind != Kind::Folder
                }
            },
            query,
        )
    }

    /// 完全同名兜底（Everything 的 `wfn:`）。常规候选在 Everything 侧就按
    /// 修改时间截断，老目录（实测 `E:\use` 搜 `use`）会被一堆「最近修改过的
    /// User*」挤出候选集，精确命中反而看不到。`wfn:` 只返回同名项，
    /// 结果集很小（实测 40 条 / 46ms），用来保证精确匹配一定进排序。
    pub fn search_exact(
        &self,
        query: &Query,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<(Vec<Entry>, usize), String> {
        let search = build_search_wfn(query, filters);
        self.run(&search, limit, |kind| kept_in_scope(query.scope, kind), query)
    }

    fn run(
        &self,
        search: &str,
        limit: usize,
        keep: impl Fn(Kind) -> bool,
        query: &Query,
    ) -> Result<(Vec<Entry>, usize), String> {
        let list = self
            .client
            .query_wait(search)
            .request_flags(
                RequestFlags::FileName
                    | RequestFlags::FullPathAndFileName
                    | RequestFlags::Size
                    | RequestFlags::DateModified
                    | RequestFlags::Attributes,
            )
            .sort(Sort::DateModifiedDescending)
            .max_results(limit as u32)
            .call()
            .map_err(|err| format!("Everything 查询失败: {err}"))?;

        let total = list.total_len();
        let mut entries = Vec::with_capacity(list.len());
        // 中文查询不需要拼音匹配；ASCII 查询才为中文名生成拼音。
        let want_pinyin = query.text.is_ascii();

        for item in list.iter() {
            let name = item.get_string(RequestFlags::FileName).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            // FullPathAndFileName 一次取全路径，省去父路径 + join 的两次分配。
            let full_str = item
                .get_string(RequestFlags::FullPathAndFileName)
                .unwrap_or_default();
            if full_str.is_empty() {
                continue;
            }
            let is_dir = item
                .get_u32(RequestFlags::Attributes)
                .map(|attrs| attrs & FILE_ATTRIBUTE_DIRECTORY != 0)
                .unwrap_or(false);
            let kind = kind_of(&name, is_dir);
            if !keep(kind) {
                continue;
            }
            let modified = item.get_time(RequestFlags::DateModified).and_then(|time| {
                let ticks = ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
                filetime_ticks_to_unix(ticks)
            });
            // 中文文件名也生成拼音，用户可以直接敲 bg 命中「报告.docx」。
            let pinyin = (want_pinyin && crate::search::pinyin::has_cjk(&name))
                .then(|| crate::search::pinyin::keys(&name));
            entries.push(Entry {
                tone: tone_for(&full_str),
                initial: initial_of(&name),
                id: full_str.clone(),
                kind,
                name,
                path: Some(PathBuf::from(full_str)),
                // 文件夹的 Size 在 Everything 里是 u64::MAX，展示无意义，直接不存。
                size: (!is_dir)
                    .then(|| item.get_size(RequestFlags::Size))
                    .flatten(),
                modified,
                pinyin,
            });
        }

        Ok((entries, total))
    }
}

fn build_search(query: &Query, filters: &SearchFilters) -> String {
    let mut parts = Vec::new();

    push_scope_prefix(query.scope, &mut parts);
    push_filters(query, filters, &mut parts, text_token(query));
    parts.join(" ")
}

/// 「全部」分路查询：folder: / !folder: 前缀，其余过滤条件与普通查询一致。
fn build_search_split(query: &Query, filters: &SearchFilters, folders: bool) -> String {
    let mut parts = Vec::new();
    parts.push(if folders { "folder:" } else { "!folder:" }.to_string());
    push_filters(query, filters, &mut parts, text_token(query));
    parts.join(" ")
}

/// 完全同名查询：text 用 `wfn:` 包起来（带空格时加引号保持整体）。
fn build_search_wfn(query: &Query, filters: &SearchFilters) -> String {
    let mut parts = Vec::new();
    push_scope_prefix(query.scope, &mut parts);
    let token = if query.text.is_empty() {
        None
    } else if query.text.contains(char::is_whitespace) {
        Some(format!("wfn:\"{}\"", query.text))
    } else {
        Some(format!("wfn:{}", query.text))
    };
    push_filters(query, filters, &mut parts, token);
    parts.join(" ")
}

fn text_token(query: &Query) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !query.text.is_empty() {
        parts.push(query.text.clone());
    }
    if let Some(prefix) = &query.prefix {
        parts.push(format!("{prefix}*"));
    }
    if let Some(suffix) = &query.suffix {
        parts.push(format!("*{suffix}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn push_scope_prefix(scope: Scope, parts: &mut Vec<String>) {
    match scope {
        Scope::Folder => parts.push("folder:".to_string()),
        Scope::Program => parts.push("ext:exe".to_string()),
        Scope::File => {
            parts.push("!folder:".to_string());
            parts.push("!ext:exe".to_string());
        }
        _ => {}
    }
}

/// 用户手输的 ext:/path: 与界面按钮的筛选合并；
/// 手输优先（避免 ext:rs + 类型=图片 这种自相矛盾的组合）。
/// `text` 是文本部分的查询串（普通搜索是原文，wfn 查询是 `wfn:...`）。
fn push_filters(
    query: &Query,
    filters: &SearchFilters,
    parts: &mut Vec<String>,
    text: Option<String>,
) {
    if let Some(ext) = &query.ext {
        parts.push(format!("ext:{ext}"));
    }
    if let Some(text) = text {
        parts.push(text);
    }
    if let Some(path) = &query.path_filter {
        parts.push(format!("path:{}", quote_path_token(path)));
    }
    parts.extend(filters.query_parts(query.ext.is_some(), query.path_filter.is_some()));
}

/// Everything 的 path: 参数含空格时必须整体加引号；结尾不留路径分隔符，
/// 否则引号前的分隔符会被解析成转义引号，整条查询语法失效。
fn quote_path_token(path: &str) -> String {
    // 92 = 反斜杠，47 = 正斜杠；用码点避免与 shell / 字符串转义打架。
    let trimmed = path.trim_end_matches(|c: char| c as u32 == 92 || c as u32 == 47);
    if trimmed.chars().any(char::is_whitespace) {
        format!("\"{trimmed}\"")
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn kept_in_scope(scope: Scope, kind: Kind) -> bool {
    match scope {
        Scope::Folder => kind == Kind::Folder,
        Scope::Program => kind == Kind::Program,
        Scope::File => kind == Kind::File,
        _ => true,
    }
}

pub(crate) fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        Kind::Folder
    } else if name
        .rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        Kind::Program
    } else {
        Kind::File
    }
}

pub(crate) fn initial_of(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

pub(crate) fn tone_for(value: &str) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash & 0x00FF_FFFF
}

const FILETIME_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

fn filetime_ticks_to_unix(ticks: u64) -> Option<i64> {
    if ticks == 0 {
        None
    } else {
        Some(((ticks as i128 - FILETIME_UNIX_EPOCH_TICKS as i128) / 10_000_000) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_tokens_are_quoted_and_have_no_trailing_separator() {
        let s = char::from(92);
        let plain = format!("D:{s}work{s}src");
        assert_eq!(quote_path_token(&plain), plain);
        assert_eq!(quote_path_token(&format!("{plain}{s}")), plain);
        let spaced = format!("C:{s}Program Files{s}");
        assert_eq!(
            quote_path_token(&spaced),
            format!("\"C:{s}Program Files\"")
        );
    }

    fn query(scope: Scope, text: &str, ext: Option<&str>, path: Option<&str>) -> Query {
        Query {
            raw: String::new(),
            scope,
            text: text.to_string(),
            ext: ext.map(str::to_string),
            path_filter: path.map(str::to_string),
            drives_include: 0,
            drives_exclude: 0,
            prefix: None,
            suffix: None,
        }
    }

    #[test]
    fn search_string_puts_filters_before_text_and_path_last() {
        let q = query(Scope::File, "foo bar", Some("rs"), Some(r"E:\code"));
        assert_eq!(
            build_search(&q, &SearchFilters::default()),
            r"!folder: !ext:exe ext:rs foo bar path:E:\code"
        );
    }

    #[test]
    fn search_string_scope_filters() {
        assert_eq!(build_search(&query(Scope::All, "foo", None, None), &SearchFilters::default()), "foo");
        assert_eq!(
            build_search(&query(Scope::Folder, "foo", None, None), &SearchFilters::default()),
            "folder: foo"
        );
        assert_eq!(
            build_search(&query(Scope::Program, "foo", None, None), &SearchFilters::default()),
            "ext:exe foo"
        );
    }

    #[test]
    fn split_search_covers_both_kinds() {
        // 「全部」分两路取候选：一路只要文件夹、一路只要非文件夹，
        // 两路的过滤条件都保留 ext:/path:。
        let q = query(Scope::All, "rust", Some("rs"), Some(r"E:\code"));
        assert_eq!(
            build_search_split(&q, &SearchFilters::default(), true),
            r"folder: ext:rs rust path:E:\code"
        );
        assert_eq!(
            build_search_split(&q, &SearchFilters::default(), false),
            r"!folder: ext:rs rust path:E:\code"
        );
        // 非文件夹一路必须包含 exe（Program），不能套用 Scope::File 的 !ext:exe。
        assert!(!build_search_split(&q, &SearchFilters::default(), false).contains("!ext:exe"));
    }

    #[test]
    fn exact_search_uses_wfn() {
        let q = query(Scope::All, "use", None, None);
        assert_eq!(build_search_wfn(&q, &SearchFilters::default()), "wfn:use");
        // 单类别 Tab 也带 scope 前缀。
        let q = query(Scope::Folder, "use", None, None);
        assert_eq!(build_search_wfn(&q, &SearchFilters::default()), "folder: wfn:use");
        // 带空格的名字整体加引号，避免被拆成两个条件。
        let q = query(Scope::All, "new folder", None, None);
        assert_eq!(build_search_wfn(&q, &SearchFilters::default()), "wfn:\"new folder\"");
        // ext:/path: 条件保留。
        let q = query(Scope::File, "use", Some("rs"), Some(r"E:\code"));
        assert_eq!(
            build_search_wfn(&q, &SearchFilters::default()),
            r"!folder: !ext:exe ext:rs wfn:use path:E:\code"
        );
    }

    #[test]
    fn search_string_omits_empty_text() {
        assert_eq!(
            build_search(&query(Scope::All, "", Some("rs"), None), &SearchFilters::default()),
            "ext:rs"
        );
        assert_eq!(
            build_search(&query(Scope::All, "", None, Some(r"C:\Windows")), &SearchFilters::default()),
            r"path:C:\Windows"
        );
    }

    #[test]
    fn initial_is_first_char_uppercased() {
        assert_eq!(initial_of("微信"), "微");
        assert_eq!(initial_of("readme"), "R");
        assert_eq!(initial_of(""), "");
    }

    #[test]
    fn tone_is_stable_and_rgb_sized() {
        assert_eq!(tone_for(r"C:\a.rs"), tone_for(r"C:\a.rs"));
        assert!(tone_for(r"C:\a.rs") <= 0x00FF_FFFF);
    }

    #[test]
    fn kind_detection_uses_attributes_and_extension() {
        assert_eq!(kind_of("foo", true), Kind::Folder);
        assert_eq!(kind_of("foo.exe", false), Kind::Program);
        assert_eq!(kind_of("FOO.EXE", false), Kind::Program);
        assert_eq!(kind_of("readme", false), Kind::File);
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_ticks_to_unix(0), None);
        assert_eq!(filetime_ticks_to_unix(FILETIME_UNIX_EPOCH_TICKS), Some(0));
        assert_eq!(
            filetime_ticks_to_unix(FILETIME_UNIX_EPOCH_TICKS + 10_000_000),
            Some(1)
        );
    }

    #[test]
    #[ignore = "requires a running Everything instance"]
    fn perf_probe() {
        let provider = EverythingProvider::new().expect("Everything 未运行");
        for text in ["c", "rs", "docx", "clip"] {
            let query = query(Scope::All, text, None, None);
            let started = std::time::Instant::now();
            let (entries, total) = provider.search(&query, &SearchFilters::default(), 300).expect("Everything 查询失败");
            println!(
                "{text}: {} entries of {} total in {:.1}ms",
                entries.len(),
                total,
                started.elapsed().as_secs_f32() * 1000.0
            );
        }

        // 对比不同排序方式与条数的成本。
        let q = query(Scope::All, "rs", None, None);
        let search = build_search(&q, &SearchFilters::default());
        for (sort, max, label) in [
            (Sort::DateModifiedDescending, 300, "date 300"),
            (Sort::NameAscending, 300, "name 300"),
            (Sort::DateModifiedDescending, 100, "date 100"),
            (Sort::NameAscending, 500, "name 500"),
        ] {
            let started = std::time::Instant::now();
            let list = provider
                .client
                .query_wait(&search)
                .request_flags(
                    RequestFlags::FileName
                        | RequestFlags::FullPathAndFileName
                        | RequestFlags::Size
                        | RequestFlags::DateModified
                        | RequestFlags::Attributes,
                )
                .sort(sort)
                .max_results(max)
                .call()
                .expect("查询失败");
            println!(
                "{label}: {} entries in {:.1}ms",
                list.len(),
                started.elapsed().as_secs_f32() * 1000.0
            );
        }
    }

    #[test]
    #[ignore = "requires a running Everything instance"]
    fn searches_rust_files() {
        let provider = EverythingProvider::new().expect("Everything 未运行");
        let query = query(Scope::All, "", Some("rs"), None);

        let (entries, total) = provider.search(&query, &SearchFilters::default(), 5).expect("Everything 查询失败");

        assert!(total > 0, "expected ext:rs matches");
        assert!(!entries.is_empty(), "expected at least one entry");
        assert!(entries.len() <= 5);
        for entry in &entries {
            let path = entry.path.as_ref().expect("path");
            assert_eq!(entry.id, entry.full_path());
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some(entry.name.as_str())
            );
            assert!(path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs")));
            assert!(!entry.initial.is_empty());
            assert!(entry.pinyin.is_none());
        }
    }
}

#[cfg(test)]
mod split_integration {
    use super::*;
    use crate::model::Kind;

    #[test]
    #[ignore = "requires a running Everything instance"]
    fn split_query_returns_both_kinds() {
        // 回归：单次查询按修改时间截断时 "rust" 可能一个文件夹都拿不到；
        // 分路查询必须保证两路都有候选。
        let provider = EverythingProvider::new().expect("Everything 未运行");
        let q = crate::search::query::parse("rust", Scope::All);
        let (folders, _) = provider.search_split(&q, &SearchFilters::default(), 80, true).expect("文件夹查询失败");
        let (files, _) = provider.search_split(&q, &SearchFilters::default(), 320, false).expect("非文件夹查询失败");
        assert!(folders.iter().all(|e| e.kind == Kind::Folder));
        assert!(files.iter().all(|e| e.kind != Kind::Folder));
        assert!(!folders.is_empty(), "文件夹一路应至少返回一条");
        assert!(!files.is_empty(), "非文件夹一路应至少返回一条");
        println!("folders={} files={}", folders.len(), files.len());
    }
}

#[cfg(test)]
mod filter_integration {
    use super::*;
    use crate::search::filters::{FileGroup, LocationFilter, TimeFilter};

    fn query(text: &str) -> Query {
        crate::search::query::parse(text, Scope::All)
    }

    #[test]
    #[ignore = "requires a running Everything instance"]
    fn button_filters_restrict_results() {
        let provider = EverythingProvider::new().expect("Everything 未运行");
        let q = query("rust");

        // 位置筛选：结果必须全部位于 E:\use 下。
        let filters = SearchFilters {
            location: Some(LocationFilter::Folder {
                path: std::path::PathBuf::from(r"E:\use"),
                label: r"E:\use".to_string(),
            }),
            time: None,
            group: None,
            ..Default::default()
        };
        let (entries, _) = provider.search(&q, &filters, 20).expect("查询失败");
        assert!(!entries.is_empty(), "位置筛选不应返回空集");
        for entry in &entries {
            let path = entry.full_path().to_lowercase();
            assert!(path.starts_with(r"e:\use"), "结果应在目标目录下: {path}");
        }

        // 类型筛选：结果扩展名必须是代码组。
        let filters = SearchFilters {
            location: None,
            time: None,
            group: Some(FileGroup::Code),
            ..Default::default()
        };
        let (entries, _) = provider.search(&q, &filters, 20).expect("查询失败");
        assert!(!entries.is_empty(), "类型筛选不应返回空集");
        for entry in &entries {
            let ext = entry
                .name
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_lowercase())
                .unwrap_or_default();
            assert!(
                FileGroup::Code.extensions().contains(&ext.as_str()),
                "扩展名不在代码组: {ext}"
            );
        }

        // 时间筛选（今天）：修改时间必须在今天附近。
        let filters = SearchFilters {
            location: None,
            time: Some(TimeFilter::Today),
            group: None,
            ..Default::default()
        };
        let (entries, _) = provider.search(&q, &filters, 5).expect("查询失败");
        for entry in &entries {
            if let Some(modified) = entry.modified {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
                assert!(now - modified < 2 * 86400, "今天筛选出现旧文件: {modified}");
            }
        }
    }
}
