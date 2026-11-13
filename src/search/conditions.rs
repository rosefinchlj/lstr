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

use crate::search::filters::{DateRange, DriveFilter, SizeRange};

/// 一次扫描的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extract {
    /// 成功解析的日期条件（按出现顺序）。
    pub dates: Vec<DateRange>,
    /// 成功解析的大小条件（按出现顺序）。
    pub sizes: Vec<SizeRange>,
    /// 成功解析的盘符条件（按出现顺序，`C:` / `-C:`）。
    pub drives: Vec<DriveFilter>,
    /// 删除条件片段、折叠空白后的输入。
    pub cleaned: String,
    /// 面向用户的错误（只保留第一条给 UI）。
    pub errors: Vec<String>,
}

impl Extract {
    pub fn is_empty(&self) -> bool {
        self.dates.is_empty() && self.sizes.is_empty() && self.drives.is_empty()
    }
}

/// 单条已解析的条件（日期/大小两类共用一次扫描）。
enum ParsedCondition {
    Date(DateRange),
    Size(SizeRange),
}

/// 扫描整个输入，提取所有完整的日期/大小/盘符条件。
pub fn extract(raw: &str) -> Extract {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut dates = Vec::new();
    let mut sizes = Vec::new();
    let mut drives = Vec::new();
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
    scan_drives(raw, &mut spans, &mut drives);
    spans.sort_unstable_by_key(|&(start, _)| start);
    Extract {
        dates,
        sizes,
        drives,
        cleaned: remove_spans(raw, &spans),
        errors,
    }
}

/// 追加扫描盘符 token（`C:` / `-C:`）：整 token 精确匹配、词边界开始、
/// 后面必须是空白或行尾。`C:\foo` / `C:foo` 是路径，不算条件。
fn scan_drives(
    raw: &str,
    spans: &mut Vec<(usize, usize)>,
    drives: &mut Vec<DriveFilter>,
) {
    // 只收当前真实存在的盘符：`Y:` 这种不存在的盘不应变成条件 chip。
    let available = crate::search::filters::available_drives();
    let mut i = 0usize;
    while i < raw.len() {
        if !is_boundary(raw, i) {
            i = next_char(raw, i);
            continue;
        }
        let end = raw[i..]
            .find(char::is_whitespace)
            .map(|offset| i + offset)
            .unwrap_or(raw.len());
        if end > i {
            if let Some(drive) = parse_drive_token(&raw[i..end]) {
                if available.contains(&drive.letter) {
                    spans.push((i, end));
                    drives.push(drive);
                }
            }
        }
        i = end;
        while i < raw.len() {
            let Some(c) = raw[i..].chars().next() else {
                break;
            };
            if !c.is_whitespace() {
                break;
            }
            i += c.len_utf8();
        }
    }
}

/// `C:` / `-C:` / 全角 `C：`；其它形态返回 None。
fn parse_drive_token(token: &str) -> Option<DriveFilter> {
    let (body, exclude) = match token.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let mut chars = body.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    if !matches!(chars.next()?, ':' | '：') {
        return None;
    }
    if chars.next().is_some() {
        return None;
    }
    Some(DriveFilter {
        letter: letter.to_ascii_uppercase(),
        exclude,
    })
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
    // 已经能确定写错（例如 2026-13-01）时不暂停，交给错误态。
    if !extract(raw).errors.is_empty() {
        return false;
    }
    let Some((pos, field_len, compact, kind)) = last_field(raw) else {
        return false;
    };
    // 最后一个字段本身已经完整提交（有终结符）→ 不暂停。
    // 不能再用「extract 是否为空」判断：输入里已有盘符/更早的完整条件时，
    // 末尾半截条件仍应暂停查询。
    if parse_from(raw, pos, field_len, compact, kind).is_ok() {
        return false;
    }
    if compact {
        // `dm:` / `dm:>…` 都还在输入；空格后会由 extract 提交。
        return true;
    }
    let rest = raw[pos + field_len..].trim_start();
    if rest.is_empty() {
        // 只打了 `date`：仍允许当普通文件名搜（否则搜 "date" 永远出不来）。
        return false;
    }
    // 比较符之后的值，或裸值（没有比较符）：只要还可能成为日期值就暂停。
    let after = match parse_op(rest) {
        Some((_, op_len)) => &rest[skip_ws(rest, op_len)..],
        None => rest,
    };
    is_date_value_input(after)
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

/// 条件联想候选（目前只有 size / date）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// 展示用的关键字。
    pub keyword: &'static str,
    /// 回车后填入输入框的文本（带比较符，光标停在值前）。
    pub fill: String,
    pub description: &'static str,
}

const SUGGEST_KEYWORDS: [(&str, &str); 2] = [
    ("size", "文件大小，如 size>10M:"),
    ("date", "修改时间，如 date>2026-11-10:"),
];

/// 输入是条件关键字的严格前缀时返回候选。只在整串是纯字母、无空白时匹配，
/// 避免把正常文件名搜索（`screenshot`）一直当条件联想。
pub fn suggestions(raw: &str) -> Vec<Suggestion> {
    // 有空白就说明用户已经在输入查询词（`s ` / `s foo`），不再弹联想。
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        return Vec::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    let mut out = Vec::new();
    for (keyword, description) in SUGGEST_KEYWORDS {
        if lower.len() < keyword.len() && keyword.starts_with(&lower) {
            out.push(Suggestion {
                keyword,
                fill: format!("{keyword}>"),
                description,
            });
        }
    }
    out
}

/// 日期 chip 的可编辑文本：由区间反推一组精确等价的规范条件。
/// 合并过的 chip 也用单个区间表达式还原，避免多个片段只有最后一个能
/// 被终结冒号提交。日期按本地日粒度反查。
pub fn date_edit_text(range: &DateRange) -> Option<String> {
    match (range.after, range.before) {
        (Some(after), Some(before)) => {
            let start = local_date_text(after)?;
            let end = local_date_text(before)?;
            Some(format!("date {start}..{end}:"))
        }
        (Some(after), None) => Some(format!("date >= {}:", local_date_text(after)?)),
        (None, Some(before)) => Some(format!("date <= {}:", local_date_text(before)?)),
        (None, None) => None,
    }
}

/// 日期条件的绝对区间文案（tooltip 用）：别名条件显示换算后的真实日期。
pub fn date_range_text(range: &DateRange) -> Option<String> {
    let after = range.after.and_then(local_date_text);
    let before = range.before.and_then(local_date_text);
    match (after, before) {
        (Some(a), Some(b)) => Some(format!("{a} ~ {b}")),
        (Some(a), None) => Some(format!("≥ {a}")),
        (None, Some(b)) => Some(format!("≤ {b}")),
        (None, None) => None,
    }
}

/// 输入条件时的字段感知提示（空态面板第二行）。
pub fn composing_hint(raw: &str) -> &'static str {
    match last_field(raw) {
        Some((_, _, _, FieldKind::Date)) => {
            "冒号或回车提交；日期可用 今天/本周/今年/近N天 或 2025"
        }
        Some((_, _, _, FieldKind::Size)) => "冒号或回车提交；单位 B/KB/MB/GB/TB，如 10M、1.5GB",
        None => "冒号或回车提交，例如 date>2026-11-10: / size>10M: / C:",
    }
}

/// 大小 chip 的可编辑文本：用字节边界还原，保证重新解析后区间逐字节一致。
pub fn size_edit_text(range: &SizeRange) -> Option<String> {
    match (range.after, range.before) {
        (Some(after), Some(before)) => Some(format!("size {after}B..{before}B:")),
        (Some(after), None) => Some(format!("size >= {after}B:")),
        (None, Some(before)) => Some(format!("size <= {before}B:")),
        (None, None) => None,
    }
}

/// unix 秒 → 本地日期 `YYYY-MM-DD`（用目标时刻自身的时区/DST 规则）。
fn local_date_text(unix: i64) -> Option<String> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    const EPOCH: i128 = 116_444_736_000_000_000;
    let ticks = EPOCH + unix as i128 * 10_000_000;
    if !(0..=u64::MAX as i128).contains(&ticks) {
        return None;
    }
    let ticks = ticks as u64;
    let utc_file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&utc_file, &mut utc).ok()?;
    }
    let mut local = SYSTEMTIME::default();
    unsafe {
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(format!(
        "{:04}-{:02}-{:02}",
        local.wYear, local.wMonth, local.wDay
    ))
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
/// 值支持显式 `YYYY-MM-DD` 与相对别名（今天/昨天/本周/上周/本月/上月/近N天）；
/// 裸值（没有比较符）默认「等于」该天/该区间。
fn parse_date_expr(
    raw: &str,
    start: usize,
    _prose: bool,
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
    // 比较符可省略：`date 2026-11-10:` / `date 本周:` 默认「等于」该天/该区间。
    let (op, has_op) = match parse_op(&raw[i..]) {
        Some((op, len)) => {
            i += len;
            i = skip_ws(raw, i);
            (op, true)
        }
        None => (Op::Eq, false),
    };
    let value_start = i;
    let token = token_at(raw, value_start);
    let (value_len, first, second) = if token.len() == 10 && date_len(token).is_some() {
        let ymd = parse_ymd(token)
            .ok_or_else(|| Some(format!("日期无效：{token}（格式 YYYY-MM-DD）")))?;
        (token.len(), ymd, ymd)
    } else if let Some((a, b)) = resolve_date_value(token) {
        (token.len(), a, b)
    } else if has_op {
        return Err(Some(format!(
            "日期无效：{token}（YYYY-MM-DD 或 今天/本周/近N天）"
        )));
    } else {
        // 裸值不是日期也不是别名：这里不是条件，继续扫描。
        return Err(None);
    };
    let end = value_start + value_len;
    let text = token;
    let start_midnight = local_midnight_unix(first)
        .ok_or_else(|| Some("无法换算本地时间（时区设置异常）".into()))?;
    let end_exclusive = local_midnight_unix(next_day(second))
        .ok_or_else(|| Some("无法换算本地时间（时区设置异常）".into()))?;
    let condition = match op {
        Op::Gt => DateRange {
            after: Some(end_exclusive),
            before: None,
            after_text: Some(format!("> {text}")),
            before_text: None,
        },
        Op::Ge => DateRange {
            after: Some(start_midnight),
            before: None,
            after_text: Some(format!("≥ {text}")),
            before_text: None,
        },
        Op::Lt => DateRange {
            after: None,
            before: start_midnight.checked_sub(1),
            after_text: None,
            before_text: Some(format!("< {text}")),
        },
        Op::Le => DateRange {
            after: None,
            before: end_exclusive.checked_sub(1),
            after_text: None,
            before_text: Some(format!("≤ {text}")),
        },
        Op::Eq => DateRange {
            after: Some(start_midnight),
            before: end_exclusive.checked_sub(1),
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

/// 输入框大小遮罩的单位显示顺序（与 `size_unit` 的解析保持一致）。
pub const SIZE_UNIT_LABELS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// 单位文本 → 显示下标（B/KB/MB/GB/TB）。
pub fn size_unit_index(raw: &str) -> Option<usize> {
    match raw.to_ascii_uppercase().as_str() {
        "B" => Some(0),
        "K" | "KB" | "KIB" => Some(1),
        "M" | "MB" | "MIB" => Some(2),
        "G" | "GB" | "GIB" => Some(3),
        "T" | "TB" | "TIB" => Some(4),
        _ => None,
    }
}

/// 输入是否整串就是「大小字段 + 操作符」且值还没开始输入：
/// 是则返回 (字段, 操作符文本)，供输入框大小分段遮罩激活。
pub fn size_mask_prefix(input: &str) -> Option<(String, String)> {
    let (field_len, compact, kind) = field_at(input, 0)?;
    if kind != FieldKind::Size {
        return None;
    }
    let mut i = field_len;
    if compact {
        i += colon_len(input, i)?;
        i = skip_ws(input, i);
    } else {
        i = skip_ws(input, i);
        if parse_op(&input[i..]).is_none() {
            return None;
        }
    }
    let mut op = String::new();
    if let Some((_, op_len)) = parse_op(&input[i..]) {
        op = input[i..i + op_len].to_string();
        i += op_len;
        i = skip_ws(input, i);
    }
    if i != input.len() {
        return None;
    }
    Some((input[..field_len].to_string(), op))
}

/// 解析 `size_edit_text` 生成的规范文本（无终结冒号）：
/// 返回 (操作符, (数字文本, 单位下标), 第二个大小 Option)。
pub fn parse_size_edit_parts(
    text: &str,
) -> Option<(String, (String, usize), Option<(String, usize)>)> {
    let rest = text.trim_start().strip_prefix("size")?;
    let rest = rest.trim_start();
    let (op, value_start) = match parse_op(rest) {
        Some((_, op_len)) => (rest[..op_len].to_string(), skip_ws(rest, op_len)),
        None => (String::new(), 0),
    };
    let value = rest[value_start..].trim();
    if let Some((a, b)) = value.split_once("..") {
        Some((
            op,
            split_size_display(a.trim())?,
            Some(split_size_display(b.trim())?),
        ))
    } else {
        Some((op, split_size_display(value)?, None))
    }
}

fn split_size_display(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let number = text[..i].to_string();
    let unit = text[i..].trim();
    let unit = if unit.is_empty() {
        0
    } else {
        size_unit_index(unit)?
    };
    Some((number, unit))
}

/// 当前 token（到空白/冒号为止），用于错误文案，避免把后面整段输入带进去。
fn token_at(raw: &str, pos: usize) -> &str {
    let end = raw[pos..]
        .find(|c: char| c.is_whitespace() || c == ':' || c == '：')
        .map(|offset| pos + offset)
        .unwrap_or(raw.len());
    &raw[pos..end]
}

// ---------------------------------------------------------------------------
// 相对日期别名：今天 / 昨天 / 本周 / 上周 / 本月 / 上月 / 近N天
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AliasKind {
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    ThisYear,
    LastYear,
}

const DATE_ALIASES: &[(&str, AliasKind)] = &[
    ("今天", AliasKind::Today),
    ("today", AliasKind::Today),
    ("昨天", AliasKind::Yesterday),
    ("yesterday", AliasKind::Yesterday),
    ("本周", AliasKind::ThisWeek),
    ("这周", AliasKind::ThisWeek),
    ("thisweek", AliasKind::ThisWeek),
    ("上周", AliasKind::LastWeek),
    ("lastweek", AliasKind::LastWeek),
    ("本月", AliasKind::ThisMonth),
    ("这个月", AliasKind::ThisMonth),
    ("thismonth", AliasKind::ThisMonth),
    ("上月", AliasKind::LastMonth),
    ("上个月", AliasKind::LastMonth),
    ("lastmonth", AliasKind::LastMonth),
    ("今年", AliasKind::ThisYear),
    ("thisyear", AliasKind::ThisYear),
    ("去年", AliasKind::LastYear),
    ("lastyear", AliasKind::LastYear),
];

/// 把日期值 token 解析成闭区间 `(首日, 末日)`。
/// 显式日期 → 单天；别名/近N天 → 按本地今天换算的区间；
/// `2025` / `2025年` → 整年；`2023..2025` → 年区间；
/// `A..B`（完整日期）→ 日期区间（`date >= A..B:` 这类带操作符的区间用）。
fn resolve_date_value(token: &str) -> Option<((i32, u32, u32), (i32, u32, u32))> {
    let today = local_today();
    for (name, kind) in DATE_ALIASES {
        if name.eq_ignore_ascii_case(token) {
            return Some(alias_range(*kind, today));
        }
    }
    if let Some((a, b)) = token.split_once("..") {
        let av = resolve_single_or_year(a)?;
        let bv = resolve_single_or_year(b)?;
        if av.0 > bv.1 {
            return None;
        }
        return Some((av.0, bv.1));
    }
    if let Some(range) = resolve_single_or_year(token) {
        return Some(range);
    }
    relative_days_range(token, today)
}

/// 单个值：完整日期（单天）或 4 位年份（整年）。
fn resolve_single_or_year(text: &str) -> Option<((i32, u32, u32), (i32, u32, u32))> {
    let t = text.trim();
    if t.len() == 10 && date_len(t).is_some() {
        let day = parse_ymd(t)?;
        return Some((day, day));
    }
    let year_text = t.strip_suffix('年').unwrap_or(t);
    if year_text.len() == 4 && year_text.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(year) = year_text.parse::<i32>() {
            if (1970..=9999).contains(&year) {
                return Some(((year, 1, 1), (year, 12, 31)));
            }
        }
    }
    None
}

fn alias_range(kind: AliasKind, today: (i32, u32, u32)) -> ((i32, u32, u32), (i32, u32, u32)) {
    match kind {
        AliasKind::Today => (today, today),
        AliasKind::Yesterday => {
            let y = add_days(today, -1);
            (y, y)
        }
        AliasKind::ThisWeek => {
            let monday = add_days(today, -(weekday(today) as i64));
            (monday, add_days(monday, 6))
        }
        AliasKind::LastWeek => {
            let monday = add_days(today, -(weekday(today) as i64) - 7);
            (monday, add_days(monday, 6))
        }
        AliasKind::ThisMonth => (
            (today.0, today.1, 1),
            (today.0, today.1, days_in_month(today.0, today.1)),
        ),
        AliasKind::LastMonth => {
            let (year, month) = if today.1 == 1 {
                (today.0 - 1, 12)
            } else {
                (today.0, today.1 - 1)
            };
            ((year, month, 1), (year, month, days_in_month(year, month)))
        }
        AliasKind::ThisYear => ((today.0, 1, 1), (today.0, 12, 31)),
        AliasKind::LastYear => ((today.0 - 1, 1, 1), (today.0 - 1, 12, 31)),
    }
}

/// `近7天` / `最近30天` / `7天内` → 含今天的近 N 天区间。
/// 与别名不同，这里的形态不合法时返回 None（不当作输入中）。
fn relative_days_range(token: &str, today: (i32, u32, u32)) -> Option<((i32, u32, u32), (i32, u32, u32))> {
    const MAX_DAYS: i64 = 36_500;
    let n: i64 = if let Some(rest) = token
        .strip_prefix("最近")
        .or_else(|| token.strip_prefix("近"))
    {
        rest.strip_suffix('天')?.parse().ok()?
    } else {
        token.strip_suffix("天内")?.parse().ok()?
    };
    if n == 0 || n > MAX_DAYS {
        return None;
    }
    Some((add_days(today, -(n - 1)), today))
}

/// token 是否可能成为日期值（别名或近N天的前缀）：`date>今`、`date 近7` 期间
/// 要暂停查询，而不是把半截串当文件名搜。
fn is_date_value_input(after: &str) -> bool {
    if after.is_empty() {
        return true;
    }
    let token = token_at(after, 0);
    if token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    if DATE_ALIASES
        .iter()
        .any(|(name, _)| name.starts_with(token) || name.to_ascii_lowercase().starts_with(&token.to_ascii_lowercase()))
    {
        return true;
    }
    is_relative_days_prefix(token)
}

/// `近` / `近7` / `7天` / `7天内` 这类还在打一半的形态。
fn is_relative_days_prefix(token: &str) -> bool {
    let had_prefix = token.starts_with("最近") || token.starts_with("近");
    let rest = if had_prefix {
        token
            .strip_prefix("最近")
            .or_else(|| token.strip_prefix("近"))
            .unwrap_or(token)
    } else {
        token
    };
    let bytes = rest.as_bytes();
    let mut digits = 0usize;
    while digits < bytes.len() && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    let tail = &rest[digits..];
    match tail {
        "" => digits > 0 || had_prefix,
        "天" | "天内" => digits > 0,
        _ => false,
    }
}

/// 当前本地日期（今天），供相对别名换算。
pub fn today_ymd() -> (i32, u32, u32) {
    local_today()
}

/// 某年某月的天数（输入框日期分段遮罩用）。
pub fn days_in_month_of(year: i32, month: u32) -> u32 {
    days_in_month(year, month)
}

/// 输入是否整串就是「日期字段 + 操作符」且值还没开始输入：
/// 是则返回 (字段, 操作符文本)，供输入框日期分段遮罩激活。
/// 裸字段（`date` / `date `，无操作符）返回 None——那仍是普通文件名搜索。
pub fn date_mask_prefix(input: &str) -> Option<(String, String)> {
    let (field_len, compact, kind) = field_at(input, 0)?;
    if kind != FieldKind::Date {
        return None;
    }
    let mut i = field_len;
    let mut op = String::new();
    if compact {
        i += colon_len(input, i)?;
        i = skip_ws(input, i);
    } else {
        i = skip_ws(input, i);
        // prose 裸字段（`date`/`date `，无操作符）不进遮罩。
        if parse_op(&input[i..]).is_none() {
            return None;
        }
    }
    if let Some((_, op_len)) = parse_op(&input[i..]) {
        op = input[i..i + op_len].to_string();
        i += op_len;
        i = skip_ws(input, i);
    }
    if i != input.len() {
        return None;
    }
    Some((input[..field_len].to_string(), op))
}

/// 解析 `date_edit_text` 生成的规范文本（无终结冒号）：
/// 返回 (操作符文本, 第一日期, 第二日期 Option)。
pub fn parse_date_edit_parts(
    text: &str,
) -> Option<(String, (i32, u32, u32), Option<(i32, u32, u32)>)> {
    let rest = text.trim_start().strip_prefix("date")?;
    let rest = rest.trim_start();
    let (op, value_start) = match parse_op(rest) {
        Some((_, op_len)) => (rest[..op_len].to_string(), skip_ws(rest, op_len)),
        None => (String::new(), 0),
    };
    let value = rest[value_start..].trim();
    if let Some((a, b)) = value.split_once("..") {
        let first = parse_ymd(a.trim())?;
        let second = parse_ymd(b.trim())?;
        Some((op, first, Some(second)))
    } else {
        Some((op, parse_ymd(value)?, None))
    }
}

/// 当前本地日期（今天），供相对别名换算。
fn local_today() -> (i32, u32, u32) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    (st.wYear as i32, st.wMonth as u32, st.wDay as u32)
}

/// 平锐算法：civil 日期 → 自 1970-01-01 的天数（无时区/DST 参与，加减天安全）。
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe - 719_468
}

/// `days_from_civil` 的逆变换。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// 日期 ± N 天。
fn add_days(date: (i32, u32, u32), delta: i64) -> (i32, u32, u32) {
    civil_from_days(days_from_civil(date.0, date.1, date.2) + delta)
}

/// 星期一 = 0 … 星期日 = 6。
fn weekday(date: (i32, u32, u32)) -> u32 {
    (days_from_civil(date.0, date.1, date.2) + 3).rem_euclid(7) as u32
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

    #[test]
    fn drive_tokens_are_extracted() {
        let out = extract("C: d: log");
        assert_eq!(out.cleaned, "log");
        assert_eq!(
            out.drives,
            vec![
                DriveFilter {
                    letter: 'C',
                    exclude: false,
                },
                DriveFilter {
                    letter: 'D',
                    exclude: false,
                },
            ]
        );
        assert!(out.errors.is_empty());

        let exclude = extract("ap -c:");
        assert_eq!(exclude.cleaned, "ap");
        assert_eq!(
            exclude.drives,
            vec![DriveFilter {
                letter: 'C',
                exclude: true,
            }]
        );

        // 全角冒号也收；词边界防止把路径/带过滤词的 token 当条件。
        assert_eq!(extract("C： foo").cleaned, "foo");
        assert_eq!(extract("C： foo").drives.len(), 1);
        for input in ["C:\\foo", "C:/foo", "c:foo", "abc:", "-cc:"] {
            let out = extract(input);
            assert!(out.drives.is_empty(), "{input:?}: {:?}", out.drives);
            assert_eq!(out.cleaned, input);
        }
    }

    #[test]
    fn drives_mix_with_date_and_size() {
        let out = extract("C: date>2026-11-10: size>10M: log");
        assert_eq!(out.cleaned, "log");
        assert_eq!(out.drives.len(), 1);
        assert_eq!(out.dates.len(), 1);
        assert_eq!(out.sizes.len(), 1);
        assert!(!out.is_empty());
    }

    #[test]
    fn composing_is_not_confused_by_complete_drives() {
        // 盘符条件已完整，但末尾日期还在输入 → 仍要暂停查询。
        assert!(is_composing("C: date>2026-11"));
        assert!(is_composing("C: size>10"));
        assert!(!is_composing("C:"));
        assert!(!is_composing("C: date>2026-11-10:"));
    }

    #[test]
    fn unavailable_drives_are_not_conditions() {
        let available = crate::search::filters::available_drives();
        let Some(missing) = ('A'..='Z').find(|letter| !available.contains(letter)) else {
            return;
        };
        let input = format!("{missing}: foo");
        let out = extract(&input);
        assert!(out.drives.is_empty(), "{missing}: 不应成为盘符条件");
        assert_eq!(out.cleaned, input, "不存在的盘符要保留原文");
    }

    #[test]
    fn suggestions_match_strict_keyword_prefixes() {
        let size = suggestions("s");
        assert_eq!(size.len(), 1);
        assert_eq!(size[0].keyword, "size");
        assert_eq!(size[0].fill, "size>");
        assert_eq!(suggestions("SIZ")[0].keyword, "size");
        let date = suggestions("d");
        assert_eq!(date.len(), 1);
        assert_eq!(date[0].fill, "date>");
        assert_eq!(suggestions("da")[0].keyword, "date");
        for input in ["size", "date", "sc", "x", "", "s d", "size>10", "大小", "s "] {
            assert!(suggestions(input).is_empty(), "{input:?}");
        }
    }

    #[test]
    fn date_edit_text_roundtrips_every_shape() {
        for input in [
            "date>2026-11-10:",
            "date>=2026-11-10:",
            "date<2026-11-10:",
            "date<=2026-11-10:",
            "date=2026-11-10:",
            "date 2026-11-01..2026-11-30:",
        ] {
            let range = extract(input).dates.remove(0);
            let text = date_edit_text(&range).expect("date edit text");
            let again = extract(&text).dates.remove(0);
            assert_eq!(again.after, range.after, "{input:?} -> {text:?}");
            assert_eq!(again.before, range.before, "{input:?} -> {text:?}");
        }
    }

    #[test]
    fn date_edit_text_roundtrips_merged_range() {
        let mut range = extract("date>2026-11-01:").dates.remove(0);
        range
            .merge(extract("date<2026-11-30:").dates.remove(0))
            .expect("merge");
        let text = date_edit_text(&range).expect("date edit text");
        let again = extract(&text).dates.remove(0);
        assert_eq!(again.after, range.after, "{text:?}");
        assert_eq!(again.before, range.before, "{text:?}");
    }

    #[test]
    fn size_edit_parts_parse_canonical_text() {
        let (op, first, second) = parse_size_edit_parts("size >= 10485761B").expect("ge");
        assert_eq!(op, ">=");
        assert_eq!(first, ("10485761".to_string(), 0));
        assert_eq!(second, None);

        let (op, first, second) = parse_size_edit_parts("size 10M..100M").expect("range");
        assert!(op.is_empty());
        assert_eq!(first, ("10".to_string(), 2));
        assert_eq!(second, Some(("100".to_string(), 2)));

        assert!(parse_size_edit_parts("date 2026-11-13").is_none());
    }

    #[test]
    fn size_mask_prefix_activation() {
        assert_eq!(
            size_mask_prefix("size>"),
            Some(("size".to_string(), ">".to_string()))
        );
        assert_eq!(
            size_mask_prefix("size >= "),
            Some(("size".to_string(), ">=".to_string()))
        );
        assert_eq!(
            size_mask_prefix("大小≥"),
            Some(("大小".to_string(), "≥".to_string()))
        );
        assert_eq!(
            size_mask_prefix("size:"),
            Some(("size".to_string(), String::new()))
        );
        for input in ["size", "size ", "size>10", "date>", ""] {
            assert!(size_mask_prefix(input).is_none(), "{input:?}");
        }
    }

    #[test]
    fn size_unit_index_maps_aliases() {
        assert_eq!(size_unit_index("B"), Some(0));
        assert_eq!(size_unit_index("k"), Some(1));
        assert_eq!(size_unit_index("MB"), Some(2));
        assert_eq!(size_unit_index("GiB"), Some(3));
        assert_eq!(size_unit_index("tb"), Some(4));
        assert_eq!(size_unit_index("X"), None);
    }

    #[test]
    fn size_edit_text_roundtrips_every_shape() {
        for input in [
            "size>10M:",
            "size>=10M:",
            "size<10M:",
            "size<=10M:",
            "size=10M:",
            "size 10M..100M:",
        ] {
            let range = extract(input).sizes.remove(0);
            let text = size_edit_text(&range).expect("size edit text");
            let again = extract(&text).sizes.remove(0);
            assert_eq!(again.after, range.after, "{input:?} -> {text:?}");
            assert_eq!(again.before, range.before, "{input:?} -> {text:?}");
        }

        let mut merged = extract("size>10M:").sizes.remove(0);
        merged
            .merge(extract("size<100M:").sizes.remove(0))
            .expect("merge");
        let text = size_edit_text(&merged).expect("size edit text");
        let again = extract(&text).sizes.remove(0);
        assert_eq!(again.after, merged.after, "{text:?}");
        assert_eq!(again.before, merged.before, "{text:?}");
    }

    #[test]
    fn civil_date_helpers_match_known_anchors() {
        // 1970-01-01 周四、2024-01-01 周一（真实历法锚点）。
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(weekday((1970, 1, 1)), 3);
        assert_eq!(weekday((2024, 1, 1)), 0);
        // 往返与跨月加减。
        assert_eq!(add_days((2026, 3, 1), -1), (2026, 2, 28));
        assert_eq!(add_days((2024, 2, 28), 1), (2024, 2, 29), "闰年");
        assert_eq!(add_days((2026, 12, 31), 1), (2027, 1, 1));
        for day in [(2026, 1, 1), (2024, 2, 29), (2026, 11, 13)] {
            assert_eq!(civil_from_days(days_from_civil(day.0, day.1, day.2)), day);
        }
    }

    #[test]
    fn relative_date_aliases_expand_to_day_ranges() {
        let today = local_today();
        assert_eq!(resolve_date_value("今天"), Some((today, today)));
        assert_eq!(resolve_date_value("today"), Some((today, today)));
        let yesterday = add_days(today, -1);
        assert_eq!(resolve_date_value("昨天"), Some((yesterday, yesterday)));

        let monday = add_days(today, -(weekday(today) as i64));
        assert_eq!(resolve_date_value("本周"), Some((monday, add_days(monday, 6))));
        assert_eq!(
            resolve_date_value("上周"),
            Some((add_days(monday, -7), add_days(monday, -1)))
        );

        let first = (today.0, today.1, 1);
        assert_eq!(
            resolve_date_value("本月"),
            Some((first, (today.0, today.1, days_in_month(today.0, today.1))))
        );
        let (py, pm) = if today.1 == 1 {
            (today.0 - 1, 12)
        } else {
            (today.0, today.1 - 1)
        };
        assert_eq!(
            resolve_date_value("上月"),
            Some(((py, pm, 1), (py, pm, days_in_month(py, pm))))
        );

        assert_eq!(
            resolve_date_value("近7天"),
            Some((add_days(today, -6), today))
        );
        assert_eq!(
            resolve_date_value("14天内"),
            Some((add_days(today, -13), today))
        );
        assert_eq!(resolve_date_value("近1天"), Some((today, today)));
        assert_eq!(resolve_date_value("最近30天"), Some((add_days(today, -29), today)));
        assert!(resolve_date_value("近0天").is_none());
        assert!(resolve_date_value("近x天").is_none());
        assert!(resolve_date_value("7天").is_none(), "裸数字+天 不是完整形态（要 天内）");
    }

    #[test]
    fn alias_conditions_parse_with_labels() {
        let today = local_today();
        let out = extract("date>今天: a.log");
        assert_eq!(out.cleaned, "a.log");
        assert_eq!(out.dates.len(), 1);
        assert_eq!(
            out.dates[0].after,
            Some(local_midnight_unix(add_days(today, 1)).unwrap()),
            "> 今天 = 明天零点起"
        );
        assert_eq!(out.dates[0].label(), "> 今天");

        let week = extract("date 本周:").dates.remove(0);
        let monday = add_days(today, -(weekday(today) as i64));
        assert_eq!(week.after, Some(local_midnight_unix(monday).unwrap()));
        assert_eq!(
            week.before,
            Some(local_midnight_unix(add_days(monday, 7)).unwrap() - 1)
        );
        assert_eq!(week.label(), "= 本周");

        let recent = extract("date>=近7天:").dates.remove(0);
        assert_eq!(
            recent.after,
            Some(local_midnight_unix(add_days(today, -6)).unwrap())
        );
        assert_eq!(recent.label(), "≥ 近7天");

        // 别名与显式日期可合并（谁紧用谁）。
        let mut merged = extract("date>今天:").dates.remove(0);
        merged
            .merge(extract("date<2099-01-01:").dates.remove(0))
            .expect("merge");
        assert_eq!(
            merged.after,
            Some(local_midnight_unix(add_days(today, 1)).unwrap())
        );
    }

    #[test]
    fn bare_date_defaults_to_that_day() {
        let bare = extract("date 2026-11-10:").dates.remove(0);
        let eq = extract("date=2026-11-10:").dates.remove(0);
        assert_eq!(bare.after, eq.after);
        assert_eq!(bare.before, eq.before);
        assert_eq!(bare.label(), "= 2026-11-10");
        // 裸值不是日期也不是别名：仍不算条件。
        let out = extract("date foo:");
        assert!(out.dates.is_empty());
        assert!(out.errors.is_empty());
        assert_eq!(out.cleaned, "date foo:");
    }

    #[test]
    fn composing_covers_alias_and_relative_prefixes() {
        for input in [
            "date>今",
            "date>今天",
            "date 本",
            "date 本周",
            "date 近",
            "date 近7",
            "date 7",
            "date 7天",
            "date 7天内",
            "date>t",
            "date>最近",
            "a.log date>今",
        ] {
            assert!(is_composing(input), "{input:?} 应视为条件输入中");
        }
        for input in [
            "date>今天foo",
            "date foo",
            "date>report.txt",
            "date>2026-11-10:",
        ] {
            assert!(!is_composing(input), "{input:?} 不应暂停查询");
        }
    }

    #[test]
    fn composing_hint_is_field_aware() {
        assert!(composing_hint("date>今").contains("今年"));
        assert!(composing_hint("size>10").contains("B/KB"));
        assert!(composing_hint("C").contains("C:"));
        assert!(composing_hint("").contains("date"));
    }

    #[test]
    fn date_range_text_resolves_absolute_dates() {
        let range = extract("date 本周:").dates.remove(0);
        let text = date_range_text(&range).expect("absolute text");
        // 两个端点都是合法 YYYY-MM-DD 且逐日递增。
        let parts: Vec<&str> = text.split(" ~ ").collect();
        assert_eq!(parts.len(), 2);
        for part in parts {
            assert_eq!(part.len(), 10);
            assert!(parse_ymd(part).is_some(), "{part:?}");
        }

        let single = extract("date>今天:").dates.remove(0);
        assert!(date_range_text(&single).unwrap().starts_with("≥ "));
        assert!(date_range_text(&DateRange::default()).is_none());
    }

    #[test]
    fn year_values_filter_whole_years() {
        let y = extract("date 2025:").dates.remove(0);
        assert_eq!(y.after, Some(midnight(2025, 1, 1)));
        assert_eq!(y.before, Some(midnight(2026, 1, 1) - 1));
        assert_eq!(y.label(), "= 2025");

        let gt = extract("date>2025:").dates.remove(0);
        assert_eq!(gt.after, Some(midnight(2026, 1, 1)), "> 2025 = 2026 年起");
        assert_eq!(gt.label(), "> 2025");

        let cn = extract("date 2024年:").dates.remove(0);
        assert_eq!(cn.after, Some(midnight(2024, 1, 1)));

        let span = extract("date 2023..2025:").dates.remove(0);
        assert_eq!(span.after, Some(midnight(2023, 1, 1)));
        assert_eq!(span.before, Some(midnight(2026, 1, 1) - 1));

        // 带操作符的日期区间（date >= A..B）按区间语义取边界。
        let op_range = extract("date >= 2026-11-07..2026-11-13:").dates.remove(0);
        assert_eq!(op_range.after, Some(midnight(2026, 11, 7)));
        assert_eq!(op_range.before, None);

        let this_year = local_today().0;
        let this = extract("date 今年:").dates.remove(0);
        assert_eq!(this.after, Some(midnight(this_year, 1, 1)));
        assert_eq!(this.label(), "= 今年");
        let last = extract("date 去年:").dates.remove(0);
        assert_eq!(last.after, Some(midnight(this_year - 1, 1, 1)));
        let le = extract("date<=今年:").dates.remove(0);
        assert_eq!(le.before, Some(midnight(this_year + 1, 1, 1) - 1));

        // 越界年份：裸值不算条件，带操作符报错。
        let out_of_range = extract("date 1969:");
        assert!(out_of_range.dates.is_empty() && out_of_range.errors.is_empty());
        let bad = extract("date>1969:");
        assert!(bad.dates.is_empty() && !bad.errors.is_empty());

        // 年 chip 的规范编辑文本往返一致。
        let text = date_edit_text(&y).expect("edit text");
        let again = extract(&text).dates.remove(0);
        assert_eq!(again.after, y.after, "{text:?}");
        assert_eq!(again.before, y.before, "{text:?}");
    }

    #[test]
    fn date_mask_prefix_activation() {
        assert_eq!(
            date_mask_prefix("date>"),
            Some(("date".to_string(), ">".to_string()))
        );
        assert_eq!(
            date_mask_prefix("date >= "),
            Some(("date".to_string(), ">=".to_string()))
        );
        assert_eq!(
            date_mask_prefix("日期≥"),
            Some(("日期".to_string(), "≥".to_string()))
        );
        assert_eq!(
            date_mask_prefix("date:"),
            Some(("date".to_string(), String::new()))
        );
        // 裸字段仍是文件名搜索；有值/其它字段不激活。
        assert!(date_mask_prefix("date").is_none());
        assert!(date_mask_prefix("date ").is_none());
        assert!(date_mask_prefix("date>2026").is_none());
        assert!(date_mask_prefix("size>").is_none());
        assert!(date_mask_prefix("").is_none());
    }

    #[test]
    fn date_edit_parts_parse_canonical_text() {
        let (op, first, second) =
            parse_date_edit_parts("date 2026-11-07..2026-11-13").expect("range");
        assert!(op.is_empty());
        assert_eq!(first, (2026, 11, 7));
        assert_eq!(second, Some((2026, 11, 13)));

        let (op, first, second) = parse_date_edit_parts("date >= 2026-11-11").expect("ge");
        assert_eq!(op, ">=");
        assert_eq!(first, (2026, 11, 11));
        assert_eq!(second, None);
        assert!(parse_date_edit_parts("size >= 10M").is_none());
    }
}
