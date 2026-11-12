//! 子序列模糊匹配：needle 的字符按顺序出现在目标中即命中（`vsc` → Visual Studio Code）。
//! 用 DP 一次算出最优得分和命中位置：偏好词首/驼峰边界与连续匹配，
//! 这样高亮落在 "V""S""C" 上而不是 visual 中间的 s。
//!
//! 只用于本地索引（应用/命令）；文件结果由引擎按子串过滤，
//! 客户端模糊匹配既加不出新结果、也只会带来噪声。

/// 命中位置偏好加分：词首（空格/连字符后）或驼峰交界处。
const BOUNDARY_BONUS: i32 = 6;
/// 与上一个命中字符相邻。
const CONSECUTIVE_BONUS: i32 = 8;
/// 基础命中分。
const MATCH_SCORE: i32 = 10;

const NEG: i32 = i32::MIN / 2;

/// 返回 (得分, 命中字符的 char 下标)；needle 不是 text 的子序列时返回 None。
pub fn subsequence(text: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = needle
        .chars()
        .flat_map(|ch| ch.to_lowercase())
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let n = chars.len();
    let m = needle.len();
    if m == 0 || n == 0 || m > n || n > 256 || m > 32 {
        return None;
    }
    let lower: Vec<char> = chars
        .iter()
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    if lower.len() != n {
        return None;
    }

    // dp[i][j][p]：text[i..] 匹配 needle[j..] 的最大得分；p=上一个字符(i-1)是否已命中。
    // 倒序 DP，O(n*m)，应用名 ≤ 几十字符，开销可忽略。
    let stride = n + 1;
    let mut dp = vec![[NEG; 2]; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| j * stride + i;
    for p in 0..2 {
        dp[idx(n, m)][p] = 0;
    }
    for i in (0..=n).rev() {
        for j in (0..=m).rev() {
            if i == n && j == m {
                continue;
            }
            for p in 0..2 {
                let mut best = NEG;
                if i < n {
                    // 跳过 text[i]
                    best = dp[idx(i + 1, j)][0];
                    // 命中 text[i]
                    if j < m && lower[i] == needle[j] {
                        let gain = MATCH_SCORE
                            + if is_boundary(&chars, i) { BOUNDARY_BONUS } else { 0 }
                            + if p == 1 { CONSECUTIVE_BONUS } else { 0 };
                        let rest = dp[idx(i + 1, j + 1)][1];
                        if rest > NEG / 2 {
                            best = best.max(gain + rest);
                        }
                    }
                }
                dp[idx(i, j)][p] = best;
            }
        }
    }

    let total = dp[idx(0, 0)][0];
    if total <= NEG / 2 {
        return None;
    }
    // 回溯取命中下标。
    let mut positions = Vec::with_capacity(m);
    let (mut i, mut j, mut p) = (0usize, 0usize, 0usize);
    while j < m && i < n {
        let best = dp[idx(i, j)][p];
        if lower[i] == needle[j] {
            let gain = MATCH_SCORE
                + if is_boundary(&chars, i) { BOUNDARY_BONUS } else { 0 }
                + if p == 1 { CONSECUTIVE_BONUS } else { 0 };
            if best == gain + dp[idx(i + 1, j + 1)][1] {
                positions.push(i);
                i += 1;
                j += 1;
                p = 1;
                continue;
            }
        }
        i += 1;
        p = 0;
    }
    if positions.len() == m {
        Some((total, positions))
    } else {
        None
    }
}

/// 只取得分的便捷入口（丢弃命中位置）。
pub fn score(text: &str, needle: &str) -> Option<i32> {
    subsequence(text, needle).map(|(score, _)| score)
}

/// 密度门槛：平均每个 needle 字符至少 12 分。
/// 纯散布命中（无词首/无连续）只有 10 分/字符，视为噪声拒绝；
/// 词首(+6)/连续(+8)组合普遍 ≥13。用于拼音容错与应用名模糊，
/// 避免 "sz" 散布在长拼音串里造出假命中（Windows 安全中心 / 金太阳案例）。
const DENSE_MIN_PER_CHAR: i32 = 12;

pub fn score_dense(text: &str, needle: &str) -> Option<i32> {
    let score = score(text, needle)?;
    let chars = needle
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .count() as i32;
    if chars > 0 && score >= DENSE_MIN_PER_CHAR * chars {
        Some(score)
    } else {
        None
    }
}

/// 逐字符质量闸门：每个命中字符必须是词首/驼峰边界，或与前一个命中字符**连续**。
/// 拒绝「散字母」伪匹配——rust → "Blend for Visual Studio" 里 r 在 for 中间、
/// u 在 Visual 中间，即使 s(Studio 词首)+t(连续) 的加分让它过了密度门槛 。
/// 保留真实模式：vsc → Visual Studio Code（全词首）、code/chrome（连续子串）。
/// 仅用于应用名的模糊匹配；拼音容错仍走 score_dense（wxin → weixin）。
pub fn score_gated(text: &str, needle: &str) -> Option<i32> {
    let (score, positions) = subsequence(text, needle)?;
    let chars: Vec<char> = text.chars().collect();
    for (k, &pos) in positions.iter().enumerate() {
        let consecutive = k > 0 && pos == positions[k - 1] + 1;
        if !(consecutive || is_boundary(&chars, pos)) {
            return None;
        }
    }
    Some(score)
}

fn is_boundary(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = chars[i - 1];
    let curr = chars[i];
    if matches!(prev, ' ' | '-' | '_' | '.' | '/' | '\\' | '(' | ')' | '[' | ']' | '+' | '&') {
        return true;
    }
    // 驼峰：小写→大写；数字与字母交界也算。
    (prev.is_lowercase() && curr.is_uppercase())
        || (prev.is_ascii_digit() != curr.is_ascii_digit() && prev.is_alphanumeric() && curr.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positions(text: &str, needle: &str) -> Option<Vec<usize>> {
        subsequence(text, needle).map(|(_, pos)| pos)
    }

    #[test]
    fn vsc_hits_word_boundaries_of_visual_studio_code() {
        let pos = positions("Visual Studio Code", "vsc").expect("应命中");
        assert_eq!(pos, vec![0, 7, 14]);
    }

    #[test]
    fn contiguous_beats_scattered() {
        // "code" 连续命中应优先于散布的 c..o..d..e。
        let pos = positions("Visual Studio Code", "code").expect("应命中");
        assert_eq!(pos, vec![14, 15, 16, 17]);
    }

    #[test]
    fn no_match_returns_none() {
        assert!(positions("Notepad", "xyz").is_none());
        assert!(positions("ab", "abc").is_none());
        assert!(positions("", "a").is_none());
    }

    #[test]
    fn gated_rejects_scattered_false_positive() {
        // rust 散落在 "Blend for Visual Studio"（r 在 for、u 在 Visual 中间）→ 拒绝
        assert!(score_gated("Blend for Visual Studio", "rust").is_none());
        // 词首/连续构成的真实匹配保留
        assert!(score_gated("Visual Studio Code", "vsc").is_some());
        assert!(score_gated("Visual Studio Code", "code").is_some());
        assert!(score_gated("Google Chrome", "chrome").is_some());
        assert!(score_gated("RustDesk", "rust").is_some());
    }

    #[test]
    fn case_insensitive() {
        assert!(positions("Photoshop", "PS").is_some());
    }

    #[test]
    fn skips_whitespace_in_needle() {
        // "v c" 空白过滤后等价于 "vc"。
        let pos = positions("Visual Studio Code", "v c").expect("应命中");
        assert_eq!(pos, vec![0, 14]);
    }

    #[test]
    fn pinyin_typo_tolerance() {
        // wxin → weixin：漏字母也能中。
        assert!(positions("weixin", "wxin").is_some());
    }

    #[test]
    fn boundary_bonus_prefers_word_start() {
        let (boundary_score, _) = subsequence("Visual Studio Code", "s").unwrap();
        let (inline_score, _) = subsequence("aslide", "s").unwrap();
        assert!(boundary_score > inline_score);
    }
}

#[cfg(test)]
mod dense_tests {
    use super::*;

    #[test]
    fn dense_gate_accepts_meaningful_and_rejects_scattered() {
        // 词首/连续命中（≥13 分/字符）应通过。
        assert!(score_dense("Visual Studio Code", "vsc").is_some());
        assert!(score_dense("Photoshop", "ps").is_some());
        assert!(score_dense("weixin", "wxin").is_some());
        // 纯散布（10 分/字符）应拒绝。
        assert!(score_dense("windowsanquanzhongxin", "sz").is_none());
        assert!(score_dense("jtyjxzs", "sz").is_none());
        assert!(score_dense("jintaiyangjiaoxuezhushou", "sz").is_none());
        // 连续子串即使不在词首也保留（an→anquan 是真命中）。
        assert!(score_dense("anquanzhongxin", "an").is_some());
    }
}
