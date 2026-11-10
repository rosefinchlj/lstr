use pinyin::ToPinyin;

/// 把中文/英文名称转成小写拼音首字母串。
/// 例如：微信 → wx，文件资源管理器 → wjzyglq，Visual Studio Code → visualstudiocode。
pub fn initials(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(pinyin) = ch.to_pinyin() {
            out.push_str(pinyin.first_letter());
        }
    }
    out
}

/// 完整拼音（不带声调）：微信 → weixin，文件资源管理器 → wenjianziyuanguanliqi。
/// 让用户既可以用首字母（wx）也可以用全拼（weixin）命中应用。
pub fn full(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(pinyin) = ch.to_pinyin() {
            out.push_str(pinyin.plain());
        }
    }
    out
}

/// 展示用：拼音串（微信资料 → weixinziliao；LstrAcc-微信资料 → lstracc weixinziliao）。
/// 逐音节按原文顺序拼接、仅在分隔符处留空格（不按字切分，避免 "wei xin zi liao" 的碎读）；
/// 供结果行「拼音 xxx」标注，与 keys()（匹配用、含首字母备选）区分。
pub fn spaced(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(py) = ch.to_pinyin() {
            out.push_str(py.plain());
        } else if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out.trim_end().to_string()
}

/// 是否包含中日韩统一表意文字（用于决定是否值得生成拼音）。
pub fn has_cjk(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{4E00}'..='\u{9FFF}').contains(&ch))
}

/// 匹配用的拼音串：空格分隔的备选写法，例如 "wx weixin"。
/// 评分时按 token 分别判断前缀/子串，因此首字母和全拼都能拿到前缀分。
pub fn keys(name: &str) -> String {    let initials = initials(name);
    let full = full(name);
    if full.is_empty() || full == initials {
        initials
    } else {
        format!("{initials} {full}")
    }
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn chinese_words_use_pinyin_first_letters() {
        assert_eq!(initials("微信"), "wx");
        assert_eq!(initials("文件资源管理器"), "wjzyglq");
        assert_eq!(initials("计算器"), "jsq");
    }

    #[test]
    fn ascii_is_lowercased_without_spaces() {
        assert_eq!(initials("Visual Studio Code"), "visualstudiocode");
    }

    #[test]
    fn mixed_names_combine_both() {
        assert_eq!(initials("微信 WeChat"), "wxwechat");
    }

    #[test]
    fn full_pinyin_spells_each_syllable() {
        assert_eq!(super::full("微信"), "weixin");
        assert_eq!(super::full("文件资源管理器"), "wenjianziyuanguanliqi");
        assert_eq!(super::full("Visual Studio Code"), "visualstudiocode");
    }

    #[test]
    fn keys_contains_initials_and_full_pinyin() {
        assert_eq!(super::keys("微信"), "wx weixin");
        assert_eq!(super::keys("Visual Studio Code"), "visualstudiocode");
    }

    #[test]
    fn spaced_splits_syllables_for_display() {
        assert_eq!(super::spaced("微信资料"), "weixinziliao");
        assert_eq!(super::spaced("LstrAcc-微信资料"), "lstracc weixinziliao");
        assert_eq!(super::spaced("report.docx"), "report docx");
        assert_eq!(super::spaced(""), "");
    }

    #[test]
    fn detects_cjk_names() {
        assert!(super::has_cjk("报告.docx"));
        assert!(!super::has_cjk("report.docx"));
        assert!(!super::has_cjk(""));
    }
}
