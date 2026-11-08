//! 噪声排除:与 lstr 的 `default_noise_rules` 同源的目录段名。
//! 索引层的语义是「整棵子树不进索引」(省内存),比打分层降权更彻底。
//! 有序数组 + 二分:单次判断 ~3 次比较。

/// 目录段名(小写)。来源 = lstr 噪声规则中「单段可判」的部分。
pub const NOISE_SEGMENTS: &[&str] = &[
    "$recycle.bin",
    ".cachestorage",
    ".cargo",
    ".git",
    ".gradle",
    ".m2",
    ".next",
    ".npm",
    ".nuxt",
    ".parcel-cache",
    ".pnpm-store",
    ".rustup",
    ".svelte-kit",
    ".turbo",
    ".venv",
    ".yarn",
    "__pycache__",
    "assembly",
    "bower_components",
    "cache2",
    "cachestorage",
    "crashpad",
    "gpucache",
    "installer",
    "node_modules",
    "packages",
    "scoped_dir",
    "service worker",
    "shadercache",
    "site-packages",
    "system volume information",
    "target",
    "temp",
    "tmp",
    "venv",
    "winsxs",
];

pub fn is_noise_segment(name_lc: &[u8]) -> bool {
    std::str::from_utf8(name_lc)
        .map(|s| NOISE_SEGMENTS.binary_search(&s).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_sorted() {
        let mut sorted = NOISE_SEGMENTS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, NOISE_SEGMENTS.to_vec());
    }

    #[test]
    fn matches_common_noise() {
        assert!(is_noise_segment(b"node_modules"));
        assert!(is_noise_segment("Node_Modules".to_lowercase().as_bytes()));
        assert!(is_noise_segment(b".git"));
        assert!(is_noise_segment(b"$recycle.bin"));
        assert!(!is_noise_segment(b"my-project"));
        assert!(!is_noise_segment(b"targeted")); // 只匹配整段名
    }
}
