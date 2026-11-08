use crate::model::{Query, Scope};

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
    let mut words = Vec::new();

    for token in text.split_whitespace() {
        if let Some(value) = strip_prefix_ci(token, "ext:") {
            ext = Some(value.to_string());
        } else if let Some(value) = strip_prefix_ci(token, "path:") {
            path_filter = Some(value.to_string());
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
    }
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
}
