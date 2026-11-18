//! 计算器：输入算式即出结果（Enter 复制）。支持 `+ - * / % ^`、括号、小数、
//! 一元正负号，纯本地递归下降求值，无外部依赖。只在输入「看起来是算式」时
//! 触发，避免把文件名/日期误当算式（见 [`looks_like_math`]）。

use crate::model::{Entry, Kind};

/// 整体是合法算式且结果有限时返回 Some；否则 None（含除零、非法字符、剩余尾缀）。
pub fn evaluate(expr: &str) -> Option<f64> {
    let cleaned: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    let mut p = Parser { s: cleaned.as_bytes(), pos: 0 };
    let v = p.expr()?;
    if p.pos != p.s.len() {
        return None; // 有剩余字符 → 不是纯算式
    }
    if !v.is_finite() {
        return None;
    }
    Some(v)
}

/// 输入是否「看起来是算式」：仅数字与运算符/括号/小数点，含数字与至少一个
/// 运算符，且排除纯日期（`2024-01-01` 这类只用 `-` 连接、含四位年份的串）。
fn looks_like_math(s: &str) -> bool {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() < 3 {
        return false; // 至少形如 "1+2"
    }
    if !cleaned.chars().all(|c| c.is_ascii_digit() || "+-*/%^().".contains(c)) {
        return false;
    }
    if !cleaned.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    if !cleaned.chars().any(|c| "+-*/%^".contains(c)) {
        return false;
    }
    if is_date_like(&cleaned) {
        return false;
    }
    true
}

/// 两/三段纯数字以 `-` 连接且含四位年份 → 视为日期（`2024-01`、`2024-01-01`），
/// 避免「年月」输入被当成减法算出 2023。
fn is_date_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    (parts.len() == 2 || parts.len() == 3)
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 4 && p.chars().all(|c| c.is_ascii_digit()))
        && parts.iter().any(|p| p.len() == 4)
}

/// 结果格式化：整数不带小数点；小数最多 10 位并去尾零。
pub fn format_result(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{:.10}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 由原始输入构造计算器结果行（Command 特殊行，id `calc:`，path 存结果供复制）。
/// 非算式返回 None。
pub fn calc_entry(raw: &str) -> Option<Entry> {
    let trimmed = raw.trim();
    if !looks_like_math(trimmed) {
        return None;
    }
    let v = evaluate(trimmed)?;
    let result = format_result(v);
    Some(Entry {
        id: "calc:".to_string(),
        kind: Kind::Command,
        name: format!("= {result}"),
        path: Some(std::path::PathBuf::from(&result)),
        size: None,
        modified: None,
        pinyin: None,
        initial: "=".to_string(),
        tone: 0x40A0E0,
    })
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    // expr = term (('+'|'-') term)*
    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(c) = self.peek() {
            match c {
                b'+' => {
                    self.bump();
                    v += self.term()?;
                }
                b'-' => {
                    self.bump();
                    v -= self.term()?;
                }
                _ => break,
            }
        }
        Some(v)
    }
    // term = factor (('*'|'/'|'%') factor)*
    fn term(&mut self) -> Option<f64> {
        let mut v = self.factor()?;
        while let Some(c) = self.peek() {
            match c {
                b'*' => {
                    self.bump();
                    v *= self.factor()?;
                }
                b'/' => {
                    self.bump();
                    let d = self.factor()?;
                    if d == 0.0 {
                        return None;
                    }
                    v /= d;
                }
                b'%' => {
                    self.bump();
                    let d = self.factor()?;
                    if d == 0.0 {
                        return None;
                    }
                    v %= d;
                }
                _ => break,
            }
        }
        Some(v)
    }
    // factor = unary ('^' factor)?（右结合）
    fn factor(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if self.peek() == Some(b'^') {
            self.bump();
            let exp = self.factor()?;
            Some(base.powf(exp))
        } else {
            Some(base)
        }
    }
    // unary = ('+'|'-') unary | primary
    fn unary(&mut self) -> Option<f64> {
        match self.peek() {
            Some(b'-') => {
                self.bump();
                Some(-self.unary()?)
            }
            Some(b'+') => {
                self.bump();
                self.unary()
            }
            _ => self.primary(),
        }
    }
    // primary = number | '(' expr ')'
    fn primary(&mut self) -> Option<f64> {
        match self.peek() {
            Some(b'(') => {
                self.bump();
                let v = self.expr()?;
                if self.bump() != Some(b')') {
                    return None;
                }
                Some(v)
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => self.number(),
            _ => None,
        }
    }
    fn number(&mut self) -> Option<f64> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' {
                self.bump();
            } else {
                break;
            }
        }
        let txt = std::str::from_utf8(&self.s[start..self.pos]).ok()?;
        if txt.matches('.').count() > 1 {
            return None;
        }
        txt.parse::<f64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_arithmetic() {
        assert_eq!(evaluate("1+2*3"), Some(7.0));
        assert_eq!(evaluate("(1+2)*3"), Some(9.0));
        assert_eq!(evaluate("2^10"), Some(1024.0));
        assert_eq!(evaluate("2^3^2"), Some(512.0)); // 右结合
        assert_eq!(evaluate("10/4"), Some(2.5));
        assert_eq!(evaluate("10%3"), Some(1.0));
        assert_eq!(evaluate("-5+3"), Some(-2.0));
        assert_eq!(evaluate(" 1 + 2 * 3 "), Some(7.0));
        assert_eq!(evaluate("1.5*2"), Some(3.0));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(evaluate("1/0"), None);
        assert_eq!(evaluate("1+"), None);
        assert_eq!(evaluate("(1+2"), None);
        assert_eq!(evaluate("1+2abc"), None);
        assert_eq!(evaluate("report"), None);
        assert_eq!(evaluate("1..2+3"), None);
        assert_eq!(evaluate(""), None);
    }

    #[test]
    fn detection_filters_filenames_and_dates() {
        assert!(looks_like_math("1+2*3"));
        assert!(looks_like_math("2^10"));
        assert!(!looks_like_math("report-2024")); // 含字母
        assert!(!looks_like_math("2024-01-01")); // 日期
        assert!(!looks_like_math("01-01-2024")); // 日期
        assert!(!looks_like_math("2024-01")); // 年月（此前被当减法算出 2023）
        assert!(!looks_like_math("2024-1")); // 年月
        assert!(!looks_like_math("123")); // 无运算符
        assert!(!looks_like_math("ext:rs"));
        assert!(looks_like_math("1-2-3")); // 非日期(无四位年)→是算式
    }

    #[test]
    fn formats_results() {
        assert_eq!(format_result(7.0), "7");
        assert_eq!(format_result(1024.0), "1024");
        assert_eq!(format_result(2.5), "2.5");
        assert_eq!(format_result(10.0 / 3.0), "3.3333333333");
    }

    #[test]
    fn calc_entry_builds_command_row() {
        let e = calc_entry("1+2*3").expect("应识别为算式");
        assert_eq!(e.id, "calc:");
        assert_eq!(e.kind, Kind::Command);
        assert_eq!(e.name, "= 7");
        assert_eq!(e.path.unwrap().to_string_lossy(), "7");
        assert!(calc_entry("report.txt").is_none());
        assert!(calc_entry("2024-01-01").is_none());
        assert!(calc_entry("2024-01").is_none()); // 年月不触发计算器
    }
}