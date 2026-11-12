//! 输入框日期/大小条件：把 `date>2026-11-10:`、`size>10M:` 这类短语
//! 解析成修改时间 / 文件大小区间。
//!
//! 语法（操作符前后空格可选，默认不写）：
//! - 日期（冒号提交，可出现在输入任意位置）：
//!   `date>2026-11-10:` / `date >= 2026-11-01:` / `date<2026-11-30:`
//!   `date 2026-11-01..2026-11-30:` / `dm=2026-11-10:`
//! - 大小：
//!   `size>10M:` / `size >= 1.5GB:` / `size<10KB:` / `size 10M..100M:`
//! - compact（要求后面有空白，空格后提交）：
//!   `date:>2026-11-10 ` / `dm:2026-11-01..2026-11-30 ` / `size:>10M `
//! - 字段别名：date / dm / mtime / 日期 / 时间 / 修改时间；
//!   size / 大小 / 文件大小。
//! - 日期格式：`YYYY-MM-DD`（也接受 `/`、`.` 分隔）；大小单位
//!   B/K/KB/M/MB/G/GB/T/TB 等（1 KB=1024 B，裸数字=字节）。
//! - 比较符也接受全角 `＞ ＜ ＝ ≥ ≤`。
//!
//! 解析成功才从输入里删掉片段；失败保留原文并返回错误文案，避免
//! 「打错一个字条件就消失」。`is_composing` 给 UI 判断「正在输入半截
//! 条件」，用于暂停查询（避免每敲一个字就搜一遍 `date >`）。

use crate::search::filters::{DateRange, SizeRange};

/// 一次扫描的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extract {
    /// 成功解析的日期条件（按出现顺序）。
    pub dates: Vec<DateRange>,
    /// 成功解析的大小条件（按出现顺序）。
    pub sizes: Vec<SizeRange>,
    /// 删除条件片段、折叠空白后的输入。
    pub cleaned: String,
    /// 面向用户的错误（只保留第一条给 UI）。
    pub errors: Vec<String>,
}

impl Extract {
    pub fn is_empty(&self) -> bool {
        self.dates.is_empty() && self.sizes.is_empty()
    }
}

/// 单条已解析的条件（日期/大小两类共用一次扫描）。
enum ParsedCondition {
    Date(DateRange),
    Size(SizeRange),
}

/// 扫描整个输入，提取所有完整的日期/大小条件。
pub fn extract(raw: &str) -> Extract {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut dates = Vec::new();
    let mut sizes = Vec::new();
    let mut errors = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        if !is_boundary(raw, i) {
            i = next_char(raw, i);
            continue;
        }
        let Some((field_len, is_compact, kind)) = field_at(raw, i) else {
            i = next_char(raw, i);
            continue;
        };
        match parse_from(raw, i, field_len, is_compact, kind) {
            Ok((end, condition)) => {
                spans.push((i, end));
                match condition {
                    ParsedCondition::Date(range) => dates.push(range),
                    ParsedCondition::Size(range) => sizes.push(range),
                }
                i = end;
            }
            Err(Some(err)) => {
                errors.push(err);
                i = next_char(raw, i);
            }
            Err(None) => {
                i = next_char(raw, i);
            }
        }
    }
    Extract {
        dates,
        sizes,
        cleaned: remove_spans(raw, &spans),
        errors,
    }
}

/// 当前位置是否是词边界：起点，或前一个字符是空白/常见分隔符。
/// 避免把 `updatedate` / `foo_date` 里的 date 当字段。
fn is_boundary(raw: &str, pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    raw[..pos]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, ':' | '：' | ',' | ';' | '(' | '['))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldKind {
    Date,
    Size,
}

/// 字段名长度 + 是否 compact（字段后紧跟冒号）+ 条件类型。
fn field_at(raw: &str, pos: usize) -> Option<(usize, bool, FieldKind)> {
    // 长的在前，避免「修改时间」被「时间」抢先匹配。
    const FIELDS: [(&str, FieldKind); 9] = [
        ("修改时间", FieldKind::Date),
        ("文件大小", FieldKind::Size),
        ("日期", FieldKind::Date),
        ("时间", FieldKind::Date),
        ("大小", FieldKind::Size),
        ("mtime", FieldKind::Date),
        ("date", FieldKind::Date),
        ("size", FieldKind::Size),
        ("dm", FieldKind::Date),
    ];
    let rest = &raw[pos..];
    for (field, kind) in FIELDS {
        let Some(head) = rest.get(..field.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(field) {
            continue;
        }
        let after = &rest[field.len()..];
        let compact = after.starts_with(':') || after.starts_with('：');
        // 操作符前后空格都可省：`date>2026-11-10:`、`date >2026-11-10:` 都算。
        let prose = after.starts_with(char::is_whitespace)
            || after.chars().next().is_some_and(is_op_start);
        if !compact && !prose {
            continue;
        }
        return Some((field.len(), compact, kind));
    }
    None
}

fn is_op_start(c: char) -> bool {
    matches!(c, '>' | '<' | '=' | '≥' | '≤' | '＞' | '＜' | '＝')
}

/// 当前输入是否正在输入一条尚未提交的日期/大小条件（缺冒号/缺值/缺结尾）。
///
/// UI 据此暂停查询：`date >`、`date>2026-11-`、`size>10`、`dm:>2026-11-10`
/// 都不应该每敲一个字就把半截条件当文件名搜一遍。已经完整提交（extract
/// 有结果）或已经能确定写错（errors 非空）时返回 false，交给正常查询/错误态。
pub fn is_composing(raw: &str) -> bool {
    let out = extract(raw);
    if !out.is_empty() || !out.errors.is_empty() {
        return false;
    }
    let Some((pos, field_len, compact, _kind)) = last_field(raw) else {
        return false;
    };
    let rest = &raw[pos + field_len..];
    if compact {
        // `dm:` / `dm:>…` 都还在输入；空格后会由 extract 提交。
        return true;
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        // 只打了 `date`：仍允许当普通文件名搜（否则搜 "date" 永远出不来）。
        return false;
    }
    // 比较符之后必须是空白/数字/日期分隔符，才认为在输入日期；
    // `date>report.txt` 这类普通词不能把查询永久暂停。
    if let Some((_, op_len)) = parse_op(rest) {
        let after_pos = skip_ws(rest, op_len);
        let after = &rest[after_pos..];
        return after.is_empty()
            || after
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '.' | '/'));
    }
    // 闭区间：`date 2026-11-01..` 以数字开头。
    rest.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// 输入里最后一个字段起点（条件通常打在末尾，也比全局扫描便宜）。
fn last_field(raw: &str) -> Option<(usize, usize, bool, FieldKind)> {
    let mut found = None;
    for (idx, _) in raw.char_indices() {
        if is_boundary(raw, idx) {
            if let Some((len, compact, kind)) = field_at(raw, idx) {
                found = Some((idx, len, compact, kind));
            }
        }
    }
    found
}

/// 解析从 `start`（字段起点）开始的一条条件。
/// 返回 `Ok((片段终点, 条件))`；`Err(Some(msg))` = 明确写错（有冒号终结符），
/// `Err(None)` = 这里不是条件（继续扫）。
fn parse_from(
    raw: &str,
    start: usize,
    field_len: usize,
    compact: bool,
    kind: FieldKind,
) -> Result<(usize, ParsedCondition), Option<String>> {
    match kind {
        FieldKind::Date => parse_date_from(raw, start, field_len, compact)
            .map(|(end, range)| (end, ParsedCondition::Date(range))),
        FieldKind::Size => parse_size_from(raw, start, field_len, compact)
            .map(|(end, range)| (end, ParsedCondition::Size(range))),
    }
}

/// 解析一条日期条件。
fn parse_date_from(
    raw: &str,
    start: usize,
    field_len: usize,
    compact: bool,
) -> Result<(usize, DateRange), Option<String>> {
    let mut i = start + field_len;
    if compact {
        let Some(colon) = colon_len(raw, i) else {
            return Err(None);
        };
        i += colon;
        // 还没有任何终结符（空格/冒号）时当作仍在输入，不报错也不提交。
        if !raw[i..]
            .chars()
            .any(|c| c.is_whitespace() || c == ':' || c == '：')
        {
            return Err(None);
        }
        let (end, condition) = parse_date_expr(raw, i, false)?;
        // compact 后面必须跟空白或冒号终结才提交，避免打到一半就被收走；
        // 行尾同样不提交（用户可能还要继续补字）。
        let next = raw[end..].chars().next();
        match next {
            Some(c) if c.is_whitespace() || c == ':' || c == '：' => {}
            _ => return Err(None),
        }
        let mut end = end;
        if let Some(colon) = colon_len(raw, end) {
            end += colon;
        }
        return Ok((end, condition));
    }
    i = skip_ws(raw, i);
    // prose 必须有冒号终结符；还没打冒号时（例如 date > 2026-11-1）只是
    // 输入中，不显示「日期无效」的瞬时错误。
    if !raw[i..].chars().any(|c| c == ':' || c == '：') {
        return Err(None);
    }
    let (end, condition) = parse_date_expr(raw, i, true)?;
    let after_ws = skip_ws(raw, end);
    let Some(colon) = colon_len(raw, after_ws) else {
        return Err(None);
    };
    Ok((after_ws + colon, condition))
}

/// 解析 `<op> date` 或 `date..date`。
/// `prose=true` 时日期后允许空白（后面还要找冒号）。
fn parse_date_expr(
    raw: &str,
    start: usize,
    prose: bool,
) -> Result<(usize, DateRange), Option<String>> {
    let mut i = skip_ws(raw, start);
    let rest = &raw[i..];
    // 先尝试区间：date..date
    if let Some(value_len) = date_len(rest) {
        let after_first = i + value_len;
        let j = skip_ws(raw, after_first);
        if raw[j..].starts_with("..") {
            let range_start = skip_ws(raw, j + 2);
            let Some(second_len) = date_len(&raw[range_start..]) else {
                return Err(Some("日期区间缺少结束日期（格式 YYYY-MM-DD..YYYY-MM-DD）".into()));
            };
            let first_text = &raw[i..after_first];
            let second_text = &raw[range_start..range_start + second_len];
            let first = parse_ymd(first_text)
                .ok_or_else(|| Some(format!("日期无效：{first_text}（格式 YYYY-MM-DD）")))?;
            let second = parse_ymd(second_text)
                .ok_or_else(|| Some(format!("日期无效：{second_text}（格式 YYYY-MM-DD）")))?;
            if first > second {
                return Err(Some("日期区间起点晚于终点".into()));
            }
            let after = local_midnight_unix(first)
                .ok_or_else(|| Some("无法换算本地时间（时区设置异常）".into()))?;
            let end_day = next_day(second);
            let before = local_midnight_unix(end_day)
                .and_then(|v| v.checked_sub(1))
                .ok_or_else(|| Some("无法换算本地时间（时区设置异常）".into()))?;
            return Ok((
                range_start + second_len,
                DateRange {
                    after: Some(after),
                    before: Some(before),
                    after_text: Some(format!("≥ {first_text}")),
                    before_text: Some(format!("≤ {second_text}")),
                },
            ));
        }
    }
    // 再尝试比较符。
    let (op, op_len) = parse_op(&raw[i..]).ok_or(None)?;
    i += op_len;
    i = skip_ws(raw, i);
    let Some(value_len) = date_len(&raw[i..]) else {
        return Err(Some(format!(
            "日期无效：{}（格式 YYYY-MM-DD）",
            token_at(raw, i)
        )));
    };
    let text = &raw[i..i + value_len];
    let date = parse_ymd(text).ok_or_else(|| Some(format!("日期无效：{text}（格式 YYYY-MM-DD）")))?;
    let end = i + value_len;
    if !prose && raw[end..].starts_with(':') {
        // compact 后直接跟冒号也可以（date:>2026-11-10:），由调用方消费。
    }
    let midnight = local_midnight_unix(date)
        .ok_or_else(|| Some("无法换算本地时间（时区设置异常）".into()))?;
    let next_midnight = local_midnight_unix(next_day(date))
        .ok_or_else(|| Some("无法换算本地时间（时区设置异常）".into()))?;
    let condition = match op {
        Op::Gt => DateRange {
            after: Some(next_midnight),
            before: None,
            after_text: Some(format!("> {text}")),
            before_text: None,
        },
        Op::Ge => DateRange {
            after: Some(midnight),
            before: None,
            after_text: Some(format!("≥ {text}")),
            before_text: None,
        },
        Op::Lt => DateRange {
            after: None,
            before: midnight.checked_sub(1),
            after_text: None,
            before_text: Some(format!("< {text}")),
        },
        Op::Le => DateRange {
            after: None,
            before: next_midnight.checked_sub(1),
            after_text: None,
            before_text: Some(format!("≤ {text}")),
        },
        Op::Eq => DateRange {
            after: Some(midnight),
            before: next_midnight.checked_sub(1),
            after_text: Some(format!("= {text}")),
            before_text: None,
        },
    };
    Ok((end, condition))
}

/// 解析一条大小条件。
fn parse_size_from(
    raw: &str,
    start: usize,
    field_len: usize,
    compact: bool,
) -> Result<(usize, SizeRange), Option<String>> {
    let mut i = start + field_len;
    if compact {
        let Some(colon) = colon_len(raw, i) else {
            return Err(None);
        };
        i += colon;
        if !raw[i..]
            .chars()
            .any(|c| c.is_whitespace() || c == ':' || c == '：')
        {
            return Err(None);
        }
        let (end, condition) = parse_size_expr(raw, i, false)?;
        let next = raw[end..].chars().next();
        match next {
            Some(c) if c.is_whitespace() || c == ':' || c == '：' => {}
            _ => return Err(None),
        }
        let mut end = end;
        if let Some(colon) = colon_len(raw, end) {
            end += colon;
        }
        return Ok((end, condition));
    }
    i = skip_ws(raw, i);
    if !raw[i..].chars().any(|c| c == ':' || c == '：') {
        return Err(None);
    }
    let (end, condition) = parse_size_expr(raw, i, true)?;
    let after_ws = skip_ws(raw, end);
    let Some(colon) = colon_len(raw, after_ws) else {
        return Err(None);
    };
    Ok((after_ws + colon, condition))
}

fn parse_size_expr(
    raw: &str,
    start: usize,
    _prose: bool,
) -> Result<(usize, SizeRange), Option<String>> {
    let mut i = skip_ws(raw, start);
    // 区间：10M..100M
    if let Ok((value_len, bytes, text)) = parse_size_value(&raw[i..]) {
        let after_first = i + value_len;
        let j = skip_ws(raw, after_first);
        if raw[j..].starts_with("..") {
            let second_start = skip_ws(raw, j + 2);
            let (second_len, second, second_text) =
                parse_size_value(&raw[second_start..]).map_err(Some)?;
            if bytes > second {
                return Err(Some("大小区间起点大于终点".into()));
            }
            return Ok((
                second_start + second_len,
                SizeRange {
                    after: Some(bytes),
                    before: Some(second),
                    after_text: Some(format!("≥ {text}")),
                    before_text: Some(format!("≤ {second_text}")),
                },
            ));
        }
    }
    let (op, op_len) = parse_op(&raw[i..]).ok_or(None)?;
    i += op_len;
    i = skip_ws(raw, i);
    let (value_len, bytes, text) = parse_size_value(&raw[i..]).map_err(Some)?;
    let end = i + value_len;
    let condition = match op {
        Op::Gt => SizeRange {
            after: Some(bytes.saturating_add(1)),
            before: None,
            after_text: Some(format!("> {text}")),
            before_text: None,
        },
        Op::Ge => SizeRange {
            after: Some(bytes),
            before: None,
            after_text: Some(format!("≥ {text}")),
            before_text: None,
        },
        Op::Lt => {
            // `< 0 B` 为空集：用 after=1/before=0 表示，引擎逐条比较自然 0 命中。
            let (after, before) = match bytes.checked_sub(1) {
                Some(b) => (None, Some(b)),
                None => (Some(1), Some(0)),
            };
            SizeRange {
                after,
                before,
                after_text: None,
                before_text: Some(format!("< {text}")),
            }
        }
        Op::Le => SizeRange {
            after: None,
            before: Some(bytes),
            after_text: None,
            before_text: Some(format!("≤ {text}")),
        },
        Op::Eq => SizeRange {
            after: Some(bytes),
            before: Some(bytes),
            after_text: Some(format!("= {text}")),
            before_text: None,
        },
    };
    Ok((end, condition))
}

/// 解析 `10M` / `1.5GB` / `1024`（裸数字=字节）。
/// 返回 `(消费字节数, 字节数, 展示文案)`。
fn parse_size_value(input: &str) -> Result<(usize, u64, String), String> {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let int_len = i;
    if bytes.get(i) == Some(&b'.') {
        let dot = i;
        i += 1;
        let frac_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == frac_start {
            i = dot; // `10.` 只按整数 10 解析
        }
    }
    if int_len == 0 {
        return Err(format!(
            "大小无效：{}（示例：10M、1.5GB）",
            token_at(input, 0)
        ));
    }
    let number_text = &input[..i];
    // 单位：紧贴数字，或数字后一个空格再接单位（`10 MB`）。
    let mut unit_start = i;
    if bytes.get(unit_start).is_some_and(u8::is_ascii_alphabetic) {
        // 紧贴
    } else {
        let mut j = unit_start;
        while bytes.get(j).is_some_and(|b| b.is_ascii_whitespace()) {
            j += 1;
        }
        if j > unit_start && bytes.get(j).is_some_and(u8::is_ascii_alphabetic) {
            unit_start = j;
        } else {
            // 没有单位：裸数字=字节
            let value: u64 = number_text
                .parse()
                .map_err(|_| format!("大小无效：{number_text}"))?;
            return Ok((i, value, format!("{number_text} B")));
        }
    }
    let mut unit_end = unit_start;
    while bytes.get(unit_end).is_some_and(u8::is_ascii_alphabetic) {
        unit_end += 1;
    }
    let unit_raw = &input[unit_start..unit_end];
    let (factor, unit_label) = size_unit(unit_raw)
        .ok_or_else(|| format!("大小单位无效：{unit_raw}（支持 B/KB/MB/GB/TB）"))?;
    let value: f64 = number_text
        .parse()
        .map_err(|_| format!("大小无效：{number_text}"))?;
    let bytes_value = value * factor as f64;
    if !bytes_value.is_finite() || bytes_value < 0.0 || bytes_value >= u64::MAX as f64 {
        return Err(format!("大小超出范围：{number_text}{unit_raw}"));
    }
    Ok((
        unit_end,
        bytes_value.round() as u64,
        format!("{number_text} {unit_label}"),
    ))
}

/// 单位 → 字节倍数（1 KB = 1024 B，与 Windows 一致）。
fn size_unit(raw: &str) -> Option<(u64, &'static str)> {
    match raw.to_ascii_uppercase().as_str() {
        "B" => Some((1, "B")),
        "K" | "KB" | "KIB" => Some((1024, "KB")),
        "M" | "MB" | "MIB" => Some((1024 * 1024, "MB")),
        "G" | "GB" | "GIB" => Some((1024 * 1024 * 1024, "GB")),
        "T" | "TB" | "TIB" => Some((1024u64.pow(4), "TB")),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

fn parse_op(input: &str) -> Option<(Op, usize)> {
    // 长的在前；中文输入法下常见的全角比较符也接受。
    for (text, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("==", Op::Eq),
        ("＞=", Op::Ge),
        ("＜=", Op::Le),
        ("＝", Op::Eq),
        ("≥", Op::Ge),
        ("≤", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
        ("＞", Op::Gt),
        ("＜", Op::Lt),
    ] {
        if input.starts_with(text) {
            return Some((op, text.len()));
        }
    }
    None
}

/// 当前 token（到空白/冒号为止），用于错误文案，避免把后面整段输入带进去。
fn token_at(raw: &str, pos: usize) -> &str {
    let end = raw[pos..]
        .find(|c: char| c.is_whitespace() || c == ':' || c == '：')
        .map(|offset| pos + offset)
        .unwrap_or(raw.len());
    &raw[pos..end]
}

fn colon_len(raw: &str, pos: usize) -> Option<usize> {    if raw[pos..].starts_with(':') {
        Some(1)
    } else if raw[pos..].starts_with('：') {
        Some('：'.len_utf8())
    } else {
        None
    }
}

fn skip_ws(raw: &str, mut pos: usize) -> usize {
    while let Some(c) = raw[pos..].chars().next() {
        if !c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

fn date_len(input: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    // YYYY-MM-DD / YYYY/MM/DD / YYYY.MM.DD = 10 字节。
    if bytes.len() < 10 {
        return None;
    }
    let sep = bytes[4];
    if !matches!(sep, b'-' | b'/' | b'.') || bytes[7] != sep {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| {
        bytes[range].iter().all(u8::is_ascii_digit)
    };
    if digits(0..4) && digits(5..7) && digits(8..10) {
        Some(10)
    } else {
        None
    }
}

/// 解析 YYYY-MM-DD（也接受 / 或 . 分隔），校验真实日历日。
fn parse_ymd(text: &str) -> Option<(i32, u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    let sep = bytes[4];
    if !matches!(sep, b'-' | b'/' | b'.') || bytes[7] != sep {
        return None;
    }
    let year: i32 = text[0..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some((year, month, day))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn next_day((year, month, day): (i32, u32, u32)) -> (i32, u32, u32) {
    if day < days_in_month(year, month) {
        (year, month, day + 1)
    } else if month < 12 {
        (year, month + 1, 1)
    } else {
        (year + 1, 1, 1)
    }
}

/// 本地日期零点 → unix 秒。用 `TzSpecificLocalTimeToSystemTime` 按目标
/// 日期自身的夏令时规则换算，不用「当前时区偏置」（跨 DST 会差一小时）。
fn local_midnight_unix((year, month, day): (i32, u32, u32)) -> Option<i64> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};

    let local = SYSTEMTIME {
        wYear: year as u16,
        wMonth: month as u16,
        wDay: day as u16,
        wDayOfWeek: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    let mut utc = SYSTEMTIME::default();
    unsafe {
        TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).ok()?;
    }
    let mut file_time = FILETIME::default();
    unsafe {
        SystemTimeToFileTime(&utc, &mut file_time).ok()?;
    }
    let ticks = ((file_time.dwHighDateTime as u64) << 32) | file_time.dwLowDateTime as u64;
    const EPOCH: i128 = 116_444_736_000_000_000;
    Some(((ticks as i128 - EPOCH) / 10_000_000) as i64)
}

fn remove_spans(raw: &str, spans: &[(usize, usize)]) -> String {
    if spans.is_empty() {
        return raw.trim().to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0usize;
    for &(start, end) in spans {
        out.push_str(&raw[cursor..start]);
        out.push(' ');
        cursor = end;
    }
    out.push_str(&raw[cursor..]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn next_char(raw: &str, pos: usize) -> usize {
    raw[pos..]
        .chars()
        .next()
        .map(|c| pos + c.len_utf8())
        .unwrap_or(raw.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn midnight(y: i32, m: u32, d: u32) -> i64 {
        local_midnight_unix((y, m, d)).expect("local midnight")
    }

    #[test]
    fn parses_gt_and_removes_fragment() {
        let out = extract("date > 2026-11-10: a.log");
        assert_eq!(out.cleaned, "a.log");
        assert!(out.errors.is_empty());
        assert_eq!(out.dates.len(), 1);
        let cond = &out.dates[0];
        assert_eq!(cond.after, Some(midnight(2026, 11, 11)), "> 不含当天");
        assert_eq!(cond.before, None);
        assert_eq!(cond.label(), "> 2026-11-10");
    }

    #[test]
    fn comparison_operators_use_day_granularity() {
        let ge = extract("date >= 2026-11-10:").dates.remove(0);
        assert_eq!(ge.after, Some(midnight(2026, 11, 10)));
        let lt = extract("date < 2026-11-10:").dates.remove(0);
        assert_eq!(lt.before, Some(midnight(2026, 11, 10) - 1));
        let le = extract("date <= 2026-11-10:").dates.remove(0);
        assert_eq!(le.before, Some(midnight(2026, 11, 11) - 1));
        let eq = extract("date = 2026-11-10:").dates.remove(0);
        assert_eq!(eq.after, Some(midnight(2026, 11, 10)));
        assert_eq!(eq.before, Some(midnight(2026, 11, 11) - 1));
    }

    #[test]
    fn range_includes_both_days() {
        let out = extract("date 2026-11-01..2026-11-30: foo");
        assert_eq!(out.cleaned, "foo");
        let cond = &out.dates[0];
        assert_eq!(cond.after, Some(midnight(2026, 11, 1)));
        assert_eq!(cond.before, Some(midnight(2026, 12, 1) - 1));
        assert_eq!(cond.label(), "≥ 2026-11-01 · ≤ 2026-11-30");
    }

    #[test]
    fn chinese_alias_and_fullwidth_colon() {
        let out = extract("修改时间 >= 2026-02-28：报告.docx");
        assert_eq!(out.cleaned, "报告.docx");
        assert_eq!(out.dates.len(), 1);
        assert_eq!(out.dates[0].after, Some(midnight(2026, 2, 28)));
        let alias = extract("时间 < 2026/03/01: a.txt");
        assert_eq!(alias.cleaned, "a.txt");
        assert!(alias.errors.is_empty());
    }

    #[test]
    fn multiple_conditions_merge() {
        let out = extract("date > 2026-11-01: date < 2026-11-30: log");
        assert_eq!(out.cleaned, "log");
        assert_eq!(out.dates.len(), 2);
        let mut merged = out.dates[0].clone();
        merged.merge(out.dates[1].clone()).expect("merge");
        assert_eq!(merged.after, Some(midnight(2026, 11, 2)));
        assert_eq!(merged.before, Some(midnight(2026, 11, 30) - 1));
    }

    #[test]
    fn compact_requires_trailing_space() {
        // 未跟空白 = 还在输入，不提交。
        let typing = extract("dm:>2026-11-10");
        assert!(typing.dates.is_empty());
        assert_eq!(typing.cleaned, "dm:>2026-11-10");
        // 空格后提交。
        let done = extract("dm:>2026-11-10 a.log");
        assert_eq!(done.cleaned, "a.log");
        assert_eq!(done.dates.len(), 1);
    }

    #[test]
    fn invalid_date_keeps_text_and_reports_error() {
        let out = extract("date > 2026-13-01: a.log");
        assert!(out.dates.is_empty());
        assert_eq!(out.cleaned, "date > 2026-13-01: a.log");
        assert!(out.errors[0].contains("2026-13-01"), "{:?}", out.errors);
    }

    #[test]
    fn incomplete_input_does_not_flash_errors() {
        // 还没打冒号 = 输入中；不能因为日期没打完就弹错。
        let typing = extract("date > 2026-11-1");
        assert!(typing.errors.is_empty(), "{:?}", typing.errors);
        assert_eq!(typing.cleaned, "date > 2026-11-1");
        let compact = extract("date:2026-13");
        assert!(compact.errors.is_empty(), "{:?}", compact.errors);
    }

    #[test]
    fn range_with_reversed_dates_is_rejected() {
        let out = extract("date 2026-11-30..2026-11-01:");
        assert!(out.dates.is_empty());
        assert!(out.errors[0].contains("起点晚于终点"));
    }

    #[test]
    fn word_boundary_prevents_false_positive() {
        let out = extract("updatedate > 2026-11-10: x");
        assert!(out.dates.is_empty());
        assert_eq!(out.cleaned, "updatedate > 2026-11-10: x");
    }

    #[test]
    fn operators_do_not_require_spaces() {
        for input in [
            "date>2026-11-10: a.log",
            "date> 2026-11-10: a.log",
            "date >2026-11-10: a.log",
            "日期>=2026-11-10: a.log",
            "date≥2026-11-10: a.log",
        ] {
            let out = extract(input);
            assert!(out.errors.is_empty(), "{input:?}: {:?}", out.errors);
            assert_eq!(out.cleaned, "a.log", "{input:?}");
            assert_eq!(out.dates.len(), 1, "{input:?}");
        }
    }

    #[test]
    fn composing_pauses_uncommitted_condition_input() {
        for input in [
            "date>",
            "date>2026-11-10",
            "date > 2026-11-",
            "date 2026-11-01..",
            "dm:>2026-11-10",
            "a.log date>2026-11",
        ] {
            assert!(is_composing(input), "{input:?} 应视为条件输入中");
        }
        for input in [
            "date",
            "date foo",
            "date>report.txt",
            "a.log",
            "date > 2026-11-10: a.log",
            "date > 2026-13-01:",
        ] {
            assert!(!is_composing(input), "{input:?} 不应暂停查询");
        }
    }

    #[test]
    fn fullwidth_operators_are_accepted() {
        let out = extract("date ＞ 2026-11-10: a.log");
        assert_eq!(out.cleaned, "a.log");
        assert_eq!(out.dates[0].after, Some(midnight(2026, 11, 11)));
        let ge = extract("date ≥ 2026-11-10: a.log");
        assert_eq!(ge.dates[0].after, Some(midnight(2026, 11, 10)));
        let le = extract("date ≤ 2026-11-10: a.log");
        assert_eq!(le.dates[0].before, Some(midnight(2026, 11, 11) - 1));
    }

    #[test]
    fn leap_day_is_valid_only_in_leap_year() {
        assert!(extract("date = 2024-02-29:").errors.is_empty());
        assert!(!extract("date = 2025-02-29:").errors.is_empty());
    }

    #[test]
    fn condition_can_appear_after_search_text() {
        let out = extract("a.log date > 2026-11-10:");
        assert_eq!(out.cleaned, "a.log");
        assert_eq!(out.dates.len(), 1);
    }

    #[test]
    fn size_operators_and_units() {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * 1024 * 1024;

        let gt = extract("size>10M: a.log");
        assert_eq!(gt.cleaned, "a.log");
        assert_eq!(gt.sizes[0].after, Some(10 * MB + 1), "> 严格大于");
        assert_eq!(gt.sizes[0].label(), "> 10 MB");

        let ge = extract("size >= 1.5GB: a.log").sizes.remove(0);
        assert_eq!(ge.after, Some(GB + GB / 2));
        assert_eq!(ge.label(), "≥ 1.5 GB");

        let lt = extract("size < 10KB: a.log").sizes.remove(0);
        assert_eq!(lt.before, Some(10 * KB - 1));
        let le = extract("size <= 10KB: a.log").sizes.remove(0);
        assert_eq!(le.before, Some(10 * KB));
        let eq = extract("size = 10KB: a.log").sizes.remove(0);
        assert_eq!(eq.after, Some(10 * KB));
        assert_eq!(eq.before, Some(10 * KB));

        // 裸数字 = 字节；数字和单位之间允许空格。
        let raw = extract("size>1024: a.log").sizes.remove(0);
        assert_eq!(raw.after, Some(1025));
        let spaced = extract("size>10 MB: a.log").sizes.remove(0);
        assert_eq!(spaced.after, Some(10 * MB + 1));
        assert_eq!(spaced.label(), "> 10 MB");
    }

    #[test]
    fn size_range_alias_and_compact() {
        let range = extract("大小 10M..100M: a.log").sizes.remove(0);
        assert_eq!(range.after, Some(10 * 1024 * 1024));
        assert_eq!(range.before, Some(100 * 1024 * 1024));
        assert_eq!(range.label(), "≥ 10 MB · ≤ 100 MB");

        let alias = extract("文件大小>=1G: a.log").sizes.remove(0);
        assert_eq!(alias.after, Some(1024 * 1024 * 1024));

        // compact 需要末尾空白才提交；EOF 时仍在输入。
        assert!(extract("size:>10M").sizes.is_empty());
        let compact = extract("size:>10M a.log");
        assert_eq!(compact.cleaned, "a.log");
        assert_eq!(compact.sizes.len(), 1);
    }

    #[test]
    fn size_errors_and_composing() {
        let bad_unit = extract("size>10X: a.log");
        assert!(bad_unit.sizes.is_empty());
        assert!(bad_unit.errors[0].contains("单位无效"), "{:?}", bad_unit.errors);
        let reversed = extract("size 100M..10M:");
        assert!(reversed.errors[0].contains("起点大于终点"));
        let missing = extract("size>:");
        assert!(!missing.errors.is_empty());

        for input in ["size>", "size>10", "size>10M", "size 10M..", "size:>10M"] {
            assert!(is_composing(input), "{input:?} 应视为条件输入中");
        }
        for input in ["size", "size>abc", "size report.txt", "size>10M: a.log"] {
            assert!(!is_composing(input), "{input:?} 不应暂停查询");
        }
    }

    #[test]
    fn date_and_size_conditions_can_combine() {
        let out = extract("date>2026-11-01: size>10M: a.log");
        assert_eq!(out.cleaned, "a.log");
        assert_eq!(out.dates.len(), 1);
        assert_eq!(out.sizes.len(), 1);
        // 顺序无关
        let swapped = extract("size>10M: date>2026-11-01: a.log");
        assert_eq!(swapped.cleaned, "a.log");
        assert_eq!(swapped.dates.len(), 1);
        assert_eq!(swapped.sizes.len(), 1);
    }
}
