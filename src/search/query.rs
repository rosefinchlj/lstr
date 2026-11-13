use crate::model::{PathComponents, Query, Scope};

/// 正则模式：整串原样作为 pattern，不解析 `ext:` / `path:` / 盘符 / 作用域
/// 前缀（正则里的这些文本是模式的一部分）。作用域仍由当前 Tab 决定。
pub fn parse_regex(raw: &str, scope: Scope) -> Query {
    Query {
        raw: raw.to_string(),
        scope,
        text: raw.trim().to_string(),
        ext: None,
        path_filter: None,
        drives_include: 0,
        drives_exclude: 0,
        prefix: None,
        suffix: None,
        path_components: None,
    }
}

/// 浏览模式：输入框只放过滤词，按空白拆 token（多词 AND），但不解析
/// `ext:` / `path:` / 盘符（这些字符只是普通过滤文本）。
pub fn parse_plain(raw: &str, scope: Scope) -> Query {
    Query {
        raw: raw.to_string(),
        scope,
        text: raw.split_whitespace().collect::<Vec<_>>().join(" "),
        ext: None,
        path_filter: None,
        drives_include: 0,
        drives_exclude: 0,
        prefix: None,
        suffix: None,
        path_components: None,
    }
}

/// 相对路径分段（`DeepSeek\xxx` / `aa\aa\DeepSeek\`）：按分隔符拆段，
/// 交给引擎按「路径末尾连续分段」匹配。绝对路径（`C:\...` / `\\server`）
/// 由 browse 的路径浏览模式处理，这里不接管；带 `:` 的串（`path:`/`ext:`/
/// URL/盘符）一律不当作相对路径。
fn parse_path_components(text: &str) -> Option<PathComponents> {
    const SEPS: [char; 4] = ['\\', '/', '＼', '／'];
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    if text.contains([':', '：']) {
        return None;
    }
    // 盘符 / UNC 留给路径浏览。
    let bytes = text.as_bytes();
    if text.starts_with("\\\\")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return None;
    }
    if !text.contains(SEPS) {
        return None;
    }
    let components: Vec<String> = text
        .split(SEPS)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    if components.is_empty() {
        return None;
    }
    Some(PathComponents {
        components,
        trailing: text.ends_with(SEPS),
    })
}

/// 识别开头的范围前缀：`>`（不需要空格）、`a ` / `f `（必须有空格）。
///
/// UI 也用它把前缀收成范围 chip（`commit_conditions`），保证「文本前缀」与
/// 「chip」两处共用同一份规则，不会各写一套。
pub fn split_scope_prefix(raw: &str) -> Option<(Scope, &str)> {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix('>') {
        return Some((Scope::Command, rest.trim_start()));
    }
    if let Some(rest) = strip_prefix_ci(trimmed, "a ") {
        return Some((Scope::App, rest.trim_start()));
    }
    if let Some(rest) = strip_prefix_ci(trimmed, "f ") {
        return Some((Scope::File, rest.trim_start()));
    }
    None
}

pub fn parse(raw: &str, scope: Scope) -> Query {
    let trimmed = raw.trim();

    let (scope, text) = match split_scope_prefix(trimmed) {
        Some((prefix_scope, rest)) => (prefix_scope, rest.trim()),
        None => (scope, trimmed),
    };

    // 相对路径分段：最后一段是名字锚点（`DeepSeek\xxx` → text=xxx），
    // 尾部分隔符表示「该目录下所有后代」（text 为空，交给引擎全量过滤）。
    // 应用/命令行作用域下 `\` 没有路径含义，保持普通词。
    if matches!(scope, Scope::All | Scope::File | Scope::Folder | Scope::Program) {
        if let Some(path) = parse_path_components(text) {
            let anchor = if path.trailing {
                String::new()
            } else {
                path.components.last().cloned().unwrap_or_default()
            };
            return Query {
                raw: raw.to_string(),
                scope,
                text: anchor,
                ext: None,
                path_filter: None,
                drives_include: 0,
                drives_exclude: 0,
                prefix: None,
                suffix: None,
                path_components: Some(path),
            };
        }
    }

    let mut ext = None;
    let mut path_filter = None;
    let mut drives_include = 0u32;
    let mut drives_exclude = 0u32;
    let mut prefix = None;
    let mut suffix = None;
    let mut words = Vec::new();

    for token in text.split_whitespace() {
        if let Some(value) = token.strip_prefix("*.") {
            // `*.html` 等价 ext:html（Everything 习惯）；多级后缀/其它通配
            // 暂按普通词处理，避免把 `*.tar.gz` 误当 ext:gz。
            if !value.is_empty() && !value.contains(['*', '?', '.', '\\', '/']) {
                ext = Some(value.to_ascii_lowercase());
            } else {
                words.push(token);
            }
        } else if let Some(value) = strip_prefix_ci(token, "ext:") {
            ext = Some(value.to_string());
        } else if let Some(value) = strip_prefix_ci(token, "path:") {
            path_filter = Some(value.to_string());
        } else if let Some((letter, exclude)) = parse_drive_token(token) {
            // `c:` / `C:` → 限定该盘；`-c:` → 排除该盘。可用空格分隔多个。
            let bit = 1u32 << (letter as u32 - 'A' as u32);
            if exclude {
                drives_exclude |= bit;
            } else {
                drives_include |= bit;
            }
        } else if let Some(value) = token.strip_suffix('*') {
            // `abc*` / `abc.*`：前缀通配。中间/多个通配暂按普通词处理。
            if !value.is_empty() && !value.contains(['*', '?']) {
                prefix = Some(value.to_ascii_lowercase());
            } else {
                words.push(token);
            }
        } else if let Some(value) = token.strip_prefix('*') {
            // `*abc`：后缀通配。
            if !value.is_empty() && !value.contains(['*', '?']) {
                suffix = Some(value.to_ascii_lowercase());
            } else {
                words.push(token);
            }
        } else {
            words.push(token);
        }
    }

    Query {
        raw: raw.to_string(),
        scope,
        text: words.join(" "),
        ext,
        path_filter,
        drives_include,
        drives_exclude,
        prefix,
        suffix,
        path_components: None,
    }
}

/// 盘符 token：`x:`（xy 仅单个 ASCII 字母，大小写均可）= 限定；
/// `-x:` = 排除。其余（`abc:`、`x:\foo` 等）不是盘符 token，按普通词处理
/// （`x:\foo` 走 path: 语义由 browse/调用方处理）。
fn parse_drive_token(token: &str) -> Option<(char, bool)> {
    let bytes = token.as_bytes();
    let (letter, exclude) = match bytes.len() {
        2 if bytes[1] == b':' => (bytes[0], false),
        3 if bytes[0] == b'-' && bytes[2] == b':' => (bytes[1], true),
        _ => return None,
    };
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let letter = letter.to_ascii_uppercase() as char;
    // 不存在的盘符（如 Y:）按普通搜索词处理，不能变成盘符过滤条件。
    if !crate::search::filters::available_drives().contains(&letter) {
        return None;
    }
    Some((letter, exclude))
}

fn strip_prefix_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let head = input.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &input[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        let query = parse("  hello world  ", Scope::All);
        assert_eq!(query.raw, "  hello world  ");
        assert_eq!(query.scope, Scope::All);
        assert_eq!(query.text, "hello world");
        assert_eq!(query.ext, None);
        assert_eq!(query.path_filter, None);
    }

    #[test]
    fn app_prefix() {
        let query = parse("a 微信", Scope::All);
        assert_eq!(query.raw, "a 微信");
        assert_eq!(query.scope, Scope::App);
        assert_eq!(query.text, "微信");
    }

    #[test]
    fn file_prefix_with_ext() {
        let query = parse("f ext:rs", Scope::All);
        assert_eq!(query.scope, Scope::File);
        assert_eq!(query.text, "");
        assert_eq!(query.ext, Some("rs".to_string()));
        assert_eq!(query.path_filter, None);
    }

    #[test]
    fn drive_tokens_include_and_exclude() {
        // c: d: 限定两盘，-e: 排除第三盘，三者都不进搜索词
        let q = parse("c: d: ai -e:", Scope::All);
        assert_eq!(q.text, "ai");
        assert_eq!(q.drives_include, (1 << 2) | (1 << 3));
        assert_eq!(q.drives_exclude, 1 << 4);
        assert_eq!(q.drive_mask(), Some(((1 << 2) | (1 << 3)) & !(1 << 4)));
        // 只有排除：默认在全部盘里排除 C
        let q = parse("-C: foo", Scope::All);
        assert_eq!(q.text, "foo");
        assert_eq!(q.drive_mask(), Some(((1u32 << 26) - 1) & !(1 << 2)));
        // 只有盘符没有词：不视为空查询（进入筛选式浏览）
        assert!(!parse("c:", Scope::All).is_empty());
        // 非法 token 按普通词处理：多字符前缀 / 带反斜杠的路径
        let q = parse("abc: c:\\foo", Scope::All);
        assert_eq!(q.text, "abc: c:\\foo");
        assert!(q.drive_mask().is_none());
    }

    #[test]
    fn unavailable_drive_tokens_are_text() {
        let available = crate::search::filters::available_drives();
        let Some(missing) = ('A'..='Z').find(|letter| !available.contains(letter)) else {
            return;
        };
        let input = format!("{missing}: foo");
        let q = parse(&input, Scope::All);
        assert_eq!(q.text, input, "不存在的盘符应留作普通搜索词");
        assert!(q.drive_mask().is_none());
    }

    #[test]
    fn path_token() {
        let query = parse(r"path:E:\foo", Scope::All);
        assert_eq!(query.scope, Scope::All);
        assert_eq!(query.text, "");
        assert_eq!(query.ext, None);
        assert_eq!(query.path_filter, Some(r"E:\foo".to_string()));
    }

    #[test]
    fn command_prefix() {
        let query = parse("> quit", Scope::Folder);
        assert_eq!(query.raw, "> quit");
        assert_eq!(query.scope, Scope::Command);
        assert_eq!(query.text, "quit");

        let bare = parse(">", Scope::All);
        assert_eq!(bare.scope, Scope::Command);
        assert_eq!(bare.text, "");
    }

    #[test]
    fn prefixes_are_case_insensitive() {
        assert_eq!(parse("A foo", Scope::All).scope, Scope::App);
        assert_eq!(parse("F foo", Scope::All).scope, Scope::File);
    }

    #[test]
    fn tokens_are_case_insensitive_and_removed() {
        let query = parse(r"EXT:RS Path:C:\Windows foo bar", Scope::All);
        assert_eq!(query.scope, Scope::All);
        assert_eq!(query.text, "foo bar");
        assert_eq!(query.ext, Some("RS".to_string()));
        assert_eq!(query.path_filter, Some(r"C:\Windows".to_string()));
    }

    #[test]
    fn prefix_needs_separator() {
        let query = parse("apple", Scope::All);
        assert_eq!(query.scope, Scope::All);
        assert_eq!(query.text, "apple");
    }

    #[test]
    fn split_scope_prefix_matches_parse_rules() {
        // `>` 不要求空格；`a `/`f ` 必须有空格；只认开头。
        assert_eq!(split_scope_prefix(">quit"), Some((Scope::Command, "quit")));
        assert_eq!(split_scope_prefix("> quit"), Some((Scope::Command, "quit")));
        assert_eq!(split_scope_prefix("a 微信"), Some((Scope::App, "微信")));
        assert_eq!(split_scope_prefix("F x"), Some((Scope::File, "x")));
        assert_eq!(split_scope_prefix("  f   x  "), Some((Scope::File, "x  ")));
        assert_eq!(split_scope_prefix("a"), None);
        assert_eq!(split_scope_prefix("apple"), None);
        assert_eq!(split_scope_prefix("x a foo"), None);
        assert_eq!(split_scope_prefix(""), None);
        // 与 parse 的结果保持一致（同一份规则的两种用法）。
        for raw in [">x", "a x", "F  y", "apple", "> a foo"] {
            let (scope, text) = match split_scope_prefix(raw) {
                Some((scope, rest)) => (scope, rest.trim()),
                None => (Scope::All, raw.trim()),
            };
            let parsed = parse(raw, Scope::All);
            assert_eq!(parsed.scope, scope, "{raw:?}");
            assert_eq!(parsed.text, text, "{raw:?}");
        }
    }

    #[test]
    fn relative_path_components_bind_last_segment_to_name() {
        let query = parse(r"DeepSeek\xxx", Scope::All);
        {
            let path = query.path_components.as_ref().expect("路径分段");
            assert_eq!(path.components, vec!["DeepSeek", "xxx"]);
            assert!(!path.trailing);
        }
        assert_eq!(query.text, "xxx", "最后一段是名字锚点");
        assert!(!query.is_empty());
        assert_eq!(query.path_anchor(), Some("xxx"));

        // 多段、正斜杠、全角分隔符都支持。
        let multi = parse(r"aa\aa\DeepSeek\xxx", Scope::All);
        assert_eq!(
            multi
                .path_components
                .as_ref()
                .expect("多段")
                .components,
            vec!["aa", "aa", "DeepSeek", "xxx"]
        );
        let slash = parse("aa/DeepSeek/xxx", Scope::All);
        assert_eq!(
            slash
                .path_components
                .as_ref()
                .expect("斜杠")
                .components,
            vec!["aa", "DeepSeek", "xxx"]
        );
        let fullwidth = parse("所有DeepSeek＼xxx", Scope::All);
        let path = fullwidth.path_components.as_ref().expect("全角分隔符");
        assert_eq!(path.components, vec!["所有DeepSeek", "xxx"]);
    }

    #[test]
    fn relative_path_trailing_separator_means_descendants() {
        let query = parse(r"DeepSeek\", Scope::All);
        {
            let path = query.path_components.as_ref().expect("路径分段");
            assert!(path.trailing);
            assert_eq!(path.components, vec!["DeepSeek"]);
        }
        assert_eq!(query.text, "", "尾部分隔符没有名字锚点");
        assert!(query.path_anchor().is_none());
        assert!(!query.is_empty(), "只有路径也是有效查询");
    }

    #[test]
    fn absolute_paths_stay_out_of_relative_path_mode() {
        // 盘符路径与 UNC 由 browse 的路径浏览处理。
        assert!(parse(r"C:\Windows\System32", Scope::All)
            .path_components
            .is_none());
        assert!(parse(r"\\server\share\x", Scope::All)
            .path_components
            .is_none());
        // 含空白不是路径表达式（多词查询）。
        assert!(parse(r"DeepSeek\xxx foo", Scope::All)
            .path_components
            .is_none());
        // 普通查询/文件名不受影响。
        assert!(parse("report.txt", Scope::All).path_components.is_none());
        assert!(parse(r"date>2026-11-10:", Scope::All)
            .path_components
            .is_none());
    }

    #[test]
    fn star_dot_ext_is_extension_filter() {
        let query = parse("*.html", Scope::All);
        assert_eq!(query.ext.as_deref(), Some("html"));
        assert_eq!(query.text, "");
        assert!(!query.is_empty(), "只有后缀过滤也是有效查询");

        let upper = parse("*.HTML", Scope::All);
        assert_eq!(upper.ext.as_deref(), Some("html"));

        // 多级后缀不擅自降级成最后一段 ext，按普通词保留。
        let multi = parse("*.tar.gz", Scope::All);
        assert_eq!(multi.ext, None);
        assert_eq!(multi.text, "*.tar.gz");
    }

    #[test]
    fn wildcard_prefix_and_suffix_are_parsed() {
        let prefix = parse("abc.*", Scope::All);
        assert_eq!(prefix.prefix.as_deref(), Some("abc."));
        assert_eq!(prefix.text, "");

        let prefix2 = parse("abc*", Scope::All);
        assert_eq!(prefix2.prefix.as_deref(), Some("abc"));

        let suffix = parse("*abc", Scope::All);
        assert_eq!(suffix.suffix.as_deref(), Some("abc"));
        assert_eq!(suffix.text, "");

        // 多个/中间通配暂不支持，按普通词保留。
        let middle = parse("a*b", Scope::All);
        assert_eq!(middle.text, "a*b");
        assert_eq!(middle.prefix, None);
        assert_eq!(middle.suffix, None);
    }

    #[test]
    fn regex_parse_keeps_raw_pattern_and_ignores_tokens() {
        let query = parse_regex(r"ext:rs .*\.dll$", Scope::File);
        assert_eq!(query.scope, Scope::File);
        assert_eq!(query.text, r"ext:rs .*\.dll$");
        assert_eq!(query.ext, None, "正则里的 ext: 是模式的一部分");
        assert_eq!(query.path_filter, None);
        assert_eq!(query.drive_mask(), None);
    }

    #[test]
    fn browse_parse_keeps_filter_literal() {
        // 浏览模式的输入是过滤词：ext:/path:/盘符都只是普通文本，不能当语法。
        let query = parse_plain("ext:rs D: alpha", Scope::All);
        assert_eq!(query.text, "ext:rs D: alpha");
        assert_eq!(query.ext, None);
        assert_eq!(query.path_filter, None);
        assert_eq!(query.drive_mask(), None);
    }
}
