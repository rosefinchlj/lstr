use crate::model::{Query, Scope};

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
    }
}

pub fn parse(raw: &str, scope: Scope) -> Query {
    let trimmed = raw.trim();

    let (scope, text) = if let Some(rest) = trimmed.strip_prefix('>') {
        (Scope::Command, rest.trim())
    } else if let Some(rest) = strip_prefix_ci(trimmed, "a ") {
        (Scope::App, rest.trim())
    } else if let Some(rest) = strip_prefix_ci(trimmed, "f ") {
        (Scope::File, rest.trim())
    } else {
        (scope, trimmed)
    };

    let mut ext = None;
    let mut path_filter = None;
    let mut drives_include = 0u32;
    let mut drives_exclude = 0u32;
    let mut words = Vec::new();

    for token in text.split_whitespace() {
        if let Some(value) = strip_prefix_ci(token, "ext:") {
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
    Some((letter.to_ascii_uppercase() as char, exclude))
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
    fn regex_parse_keeps_raw_pattern_and_ignores_tokens() {
        let query = parse_regex(r"ext:rs .*\.dll$", Scope::File);
        assert_eq!(query.scope, Scope::File);
        assert_eq!(query.text, r"ext:rs .*\.dll$");
        assert_eq!(query.ext, None, "正则里的 ext: 是模式的一部分");
        assert_eq!(query.path_filter, None);
        assert_eq!(query.drive_mask(), None);
    }
}
