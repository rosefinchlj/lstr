//! 索引卷选择掩码:bit i 对应盘符 `'A' + i`(0..26)。
//!
//! 语义只在这里定义一次,三处复用(设置页勾选 / worker 卷过滤 / 客户端启动参数):
//! - [`MASK_ALL`] = 全部可用卷(默认;配置里的空列表等价于此);
//! - `0` = 一个都不选(设置页禁止;worker 侧把它当「未设置」,回落到启动参数)。
//!
//! 用掩码而不是字符列表,是因为它要跨进程传(协议 `SetVolumes` 帧),
//! 而且勾选变化 / 热插拔时的差异比较是 O(1)。

/// 全部卷(默认;空选择等价于此)。
pub const MASK_ALL: u32 = u32::MAX;

/// 盘符 → 掩码位。非 ASCII 字母返回 `None`。
pub fn letter_bit(letter: char) -> Option<u32> {
    let up = letter.to_ascii_uppercase();
    up.is_ascii_uppercase().then(|| 1u32 << (up as u32 - 'A' as u32))
}

/// 盘符集合 → 掩码。空集合 = [`MASK_ALL`](与配置里「空 = 全部」同一语义)。
pub fn mask_from_letters<I>(letters: I) -> u32
where
    I: IntoIterator<Item = char>,
{
    let mut mask = 0u32;
    let mut any = false;
    for letter in letters {
        if let Some(bit) = letter_bit(letter) {
            mask |= bit;
            any = true;
        }
    }
    if any {
        mask
    } else {
        MASK_ALL
    }
}

/// 掩码是否包含该盘符。`MASK_ALL` 包含任何盘符。
pub fn has_letter(mask: u32, letter: char) -> bool {
    letter_bit(letter).is_some_and(|bit| mask & bit != 0)
}

/// 掩码 → 升序盘符列表。`MASK_ALL` 展开成 A..Z(仅调试/展示用;
/// 「全部」在配置里用空列表表示,见 [`mask_from_letters`])。
pub fn letters(mask: u32) -> Vec<char> {
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| char::from_u32('A' as u32 + i))
        .collect()
}

pub fn is_all(mask: u32) -> bool {
    mask == MASK_ALL
}

/// 人类可读描述:`全部` / `C,D` / `无`。
pub fn describe(mask: u32) -> String {
    if is_all(mask) {
        return "全部".to_string();
    }
    let list = letters(mask);
    if list.is_empty() {
        return "无".to_string();
    }
    list.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_letters_means_all() {
        assert_eq!(mask_from_letters([]), MASK_ALL);
        assert!(has_letter(mask_from_letters([]), 'C'));
        assert!(is_all(mask_from_letters([])));
    }

    #[test]
    fn letters_map_to_bits() {
        let mask = mask_from_letters(['c', 'F']);
        assert_eq!(mask, (1 << 2) | (1 << 5));
        assert!(has_letter(mask, 'C'));
        assert!(has_letter(mask, 'f'));
        assert!(!has_letter(mask, 'D'));
        assert_eq!(letters(mask), vec!['C', 'F']);
        assert_eq!(describe(mask), "C,F");
    }

    #[test]
    fn all_expands_to_a_to_z_and_describes() {
        assert_eq!(letters(MASK_ALL).len(), 26);
        assert_eq!(describe(MASK_ALL), "全部");
        assert_eq!(describe(0), "无");
    }

    #[test]
    fn non_letters_are_ignored() {
        assert_eq!(mask_from_letters(['1', ':']), MASK_ALL);
        assert_eq!(letter_bit('1'), None);
        assert!(!has_letter(mask_from_letters(['C']), '1'));
    }
}
