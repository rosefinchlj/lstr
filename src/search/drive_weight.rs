//! 盘符权重：把「C 盘调到 0/50」翻译成三个作用点。
//!
//! 1. **0 = 引擎级硬排除**（卷掩码）：整卷不扫，结果与总数都不含该盘；
//! 2. **1..=100 = 候选份额**：多个卷的命中按权重公平取样，避免字母序
//!    靠前的盘把候选窗口吃光（见 `builtin::interleave_hits`）；
//! 3. **1..=100 = 同档排序分**：`rank` 里 `-(100 - 权重)`，不改变匹配档位
//!    的大小关系，只影响同一档内谁更靠前。
//!
//! 显式条件（`C:` 文本/盘符 chip、`path:C:\…`、位置、浏览目录、Explorer
//! 就地搜索）会**锁定**该盘：本次请求按 100 处理，但不改写配置里的值，
//! 删掉条件后用户的偏好原样生效。

use std::path::Path;

use crate::config::RankConfig;
use crate::model::Query;
use crate::search::filters::SearchFilters;

/// 中性权重：与其它盘同等对待。
pub const NEUTRAL: u8 = 100;

/// 盘符 → 卷位（bit i = `'A' + i`）。非字母返回 0。
fn letter_bit(letter: char) -> u32 {
    let upper = letter.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        1u32 << (upper as u32 - 'A' as u32)
    } else {
        0
    }
}

fn bit_index(bit: u32) -> Option<usize> {
    (bit.count_ones() == 1).then(|| bit.trailing_zeros() as usize)
}

/// 路径首段的盘符位；UNC/相对路径返回 None（不参与权重）。
fn path_drive_bit(path: &str) -> Option<u32> {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut chars = path.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => Some(letter_bit(letter)),
        _ => None,
    }
}

/// 本次请求的最终盘符权重。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveWeights {
    /// 下标 0 = 'A'。
    weights: [u8; 26],
    /// 显式条件锁定、本次按 100 处理的盘位。
    pinned: u32,
}

impl Default for EffectiveWeights {
    fn default() -> Self {
        Self::neutral()
    }
}

impl EffectiveWeights {
    /// 全盘中性（测试基线 / 权重未启用时的等价形态）。
    pub fn neutral() -> Self {
        Self {
            weights: [NEUTRAL; 26],
            pinned: 0,
        }
    }

    /// 按配置 + 当前查询条件计算有效权重。
    ///
    /// `browse_root` = 深路径浏览目录；`base_folder` = Explorer 就地搜索目录。
    /// 两者都算「显式锁定」，因为它们已经把查询限定在某个具体目录里。
    pub fn new(
        cfg: &RankConfig,
        query: &Query,
        filters: &SearchFilters,
        browse_root: Option<&Path>,
        base_folder: Option<&Path>,
    ) -> Self {
        let mut weights = [NEUTRAL; 26];
        for (key, weight) in &cfg.drive_weights {
            let Some(letter) = key.trim().chars().next() else {
                continue;
            };
            let bit = letter_bit(letter);
            if let Some(index) = bit_index(bit) {
                weights[index] = (*weight).min(NEUTRAL);
            }
        }
        let pinned = pinned_mask(query, filters, browse_root, base_folder);
        for index in 0..weights.len() {
            if pinned & (1u32 << index) != 0 {
                weights[index] = NEUTRAL;
            }
        }
        Self { weights, pinned }
    }

    /// 某盘权重（0 = 不检索；缺省 100）。
    pub fn weight(&self, letter: char) -> u8 {
        bit_index(letter_bit(letter))
            .map(|index| self.weights[index])
            .unwrap_or(NEUTRAL)
    }

    /// 该盘是否被显式条件锁定（UI 用来置灰滑块）。
    pub fn is_pinned(&self, letter: char) -> bool {
        self.pinned & letter_bit(letter) != 0
    }

    /// 权重为 0 的盘位（与卷掩码求交时取反）。
    pub fn zero_mask(&self) -> u32 {
        self.weights
            .iter()
            .enumerate()
            .filter(|(_, weight)| **weight == 0)
            .fold(0u32, |mask, (index, _)| mask | (1u32 << index))
    }

    /// 同档排序分：路径所在盘每降 1 点权重扣 1 分。
    ///
    /// 调用方只对 `File | Folder | Program` 使用；应用/命令/书签的 `path`
    /// 是启动目标或 URL，不该被盘符偏好连坐。
    pub fn penalty_for_path(&self, path: &str) -> i32 {
        let Some(index) = path_drive_bit(path).and_then(bit_index) else {
            return 0;
        };
        NEUTRAL as i32 - self.weights[index] as i32
    }

    /// 测试/诊断：从 `(盘符, 权重)` 列表直接构造（默认不锁定任何盘）。
    #[cfg(test)]
    pub fn from_pairs(pairs: &[(char, u8)]) -> Self {
        let mut weights = [NEUTRAL; 26];
        for (letter, weight) in pairs {
            if let Some(index) = bit_index(letter_bit(*letter)) {
                weights[index] = (*weight).min(NEUTRAL);
            }
        }
        Self { weights, pinned: 0 }
    }
}

/// 显式锁定盘的位掩码：这些盘本次查询按 100 处理。
///
/// 只认「绝对路径带盘符」：`path:C:\xx` 锁定，`path:Users\me`（相对）不锁定；
/// `C\xx` 是相对路径分段语义（见 `query::parse_path_components`），同样不锁定。
pub fn pinned_mask(
    query: &Query,
    filters: &SearchFilters,
    browse_root: Option<&Path>,
    base_folder: Option<&Path>,
) -> u32 {
    let mut mask = query.drives_include;
    for drive in &filters.drives {
        if !drive.exclude {
            mask |= drive.bit();
        }
    }
    let mut add = |path: &Path| {
        if let Some(bit) = path_drive_bit(&path.to_string_lossy()) {
            mask |= bit;
        }
    };
    if let Some(path) = query.path_filter.as_deref() {
        add(Path::new(path));
    }
    if let Some(path) = filters.path.as_deref() {
        add(path);
    }
    if let Some(location) = &filters.location {
        // `LocationFilter::path()` 把盘符根也还原成 `D:\`，统一走盘符解析。
        add(&location.path());
    }
    if let Some(root) = browse_root {
        add(root);
    }
    if let Some(folder) = base_folder {
        add(folder);
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Scope;
    use crate::search::filters::{DriveFilter, LocationFilter};
    use crate::search::query;

    fn cfg_with(pairs: &[(char, u8)]) -> RankConfig {
        let mut cfg = RankConfig::default();
        cfg.drive_weights.clear();
        for (letter, weight) in pairs {
            cfg.set_drive_weight(*letter, *weight);
        }
        cfg
    }

    fn base_query() -> Query {
        query::parse("", Scope::All)
    }

    #[test]
    fn missing_letters_are_neutral() {
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 50)]), &base_query(), &Default::default(), None, None);
        assert_eq!(weights.weight('C'), 50);
        assert_eq!(weights.weight('D'), NEUTRAL);
        assert_eq!(weights.zero_mask(), 0);
    }

    #[test]
    fn zero_weight_excludes_the_drive() {
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &Default::default(), None, None);
        assert_eq!(weights.zero_mask(), 1 << 2);
        assert_eq!(weights.weight('C'), 0);
    }

    #[test]
    fn drive_chip_pins_the_drive_even_at_zero_weight() {
        let mut query = base_query();
        query.drives_include = 1 << 2;
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &query, &Default::default(), None, None);
        assert!(weights.is_pinned('C'));
        assert_eq!(weights.weight('C'), NEUTRAL, "显式 C: 必须压过权重 0");
        assert_eq!(weights.zero_mask(), 0);
    }

    #[test]
    fn drive_chip_from_filters_pins_too() {
        let mut filters = SearchFilters::default();
        filters.drives.push(DriveFilter {
            letter: 'C',
            exclude: false,
        });
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &filters, None, None);
        assert_eq!(weights.weight('C'), NEUTRAL);
    }

    #[test]
    fn excluded_drive_stays_excluded() {
        let mut filters = SearchFilters::default();
        filters.drives.push(DriveFilter {
            letter: 'C',
            exclude: true,
        });
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 50)]), &base_query(), &filters, None, None);
        assert!(!weights.is_pinned('C'), "排除不是锁定");
        assert_eq!(weights.weight('C'), 50, "排除由 drive_mask 处理，权重不参与");
    }

    #[test]
    fn path_filter_with_drive_pins() {
        let mut filters = SearchFilters::default();
        filters.path = Some(std::path::PathBuf::from(r"C:\Users\me\Desktop"));
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &filters, None, None);
        assert_eq!(weights.weight('C'), NEUTRAL);
        assert_eq!(weights.zero_mask(), 0);
    }

    #[test]
    fn relative_path_does_not_pin() {
        let mut filters = SearchFilters::default();
        filters.path = Some(std::path::PathBuf::from(r"Users\me"));
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &filters, None, None);
        assert_eq!(weights.weight('C'), 0, "相对路径没有盘符锚点");
    }

    #[test]
    fn location_filter_pins_the_drive() {
        let mut filters = SearchFilters::default();
        filters.location = Some(LocationFilter::Drive('C'));
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &filters, None, None);
        assert_eq!(weights.weight('C'), NEUTRAL);

        let mut filters = SearchFilters::default();
        filters.location = Some(LocationFilter::Folder {
            path: std::path::PathBuf::from(r"C:\Users\me"),
            label: "个人".to_string(),
        });
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &filters, None, None);
        assert_eq!(weights.weight('C'), NEUTRAL);
    }

    #[test]
    fn browse_and_base_folder_pin_the_drive() {
        let root = std::path::PathBuf::from(r"C:\Windows");
        let weights =
            EffectiveWeights::new(&cfg_with(&[('C', 0)]), &base_query(), &Default::default(), Some(&root), None);
        assert_eq!(weights.weight('C'), NEUTRAL);

        let folder = std::path::PathBuf::from(r"c:\work");
        let weights = EffectiveWeights::new(
            &cfg_with(&[('C', 0)]),
            &base_query(),
            &Default::default(),
            None,
            Some(&folder),
        );
        assert_eq!(weights.weight('C'), NEUTRAL, "盘符大小写不影响识别");
    }

    #[test]
    fn pinning_one_drive_keeps_others_weighted() {
        let mut query = base_query();
        query.drives_include = 1 << 2; // 只搜 C
        let weights = EffectiveWeights::new(&cfg_with(&[('C', 50), ('D', 20)]), &query, &Default::default(), None, None);
        assert_eq!(weights.weight('C'), NEUTRAL);
        assert_eq!(weights.weight('D'), 20);
    }

    #[test]
    fn penalty_matches_weight_drop() {
        let weights = EffectiveWeights::from_pairs(&[('C', 50), ('D', 100)]);
        assert_eq!(weights.penalty_for_path(r"C:\Users\me\a.txt"), 50);
        assert_eq!(weights.penalty_for_path(r"d:\work\a.txt"), 0);
        assert_eq!(weights.penalty_for_path(r"\\server\share\a.txt"), 0);
    }

    #[test]
    fn unc_and_drive_relative_paths_are_neutral() {
        let weights = EffectiveWeights::from_pairs(&[('C', 10)]);
        assert_eq!(weights.penalty_for_path(r"\\?\C:\x\a.txt"), 90);
        assert_eq!(weights.penalty_for_path("a.txt"), 0);
        assert_eq!(weights.penalty_for_path(r"C:a.txt"), 90, "C:relative 也是 C 盘");
    }
}
