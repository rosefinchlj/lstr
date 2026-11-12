//! 噪声排除:索引层语义是「噪声目录自身保留可搜,整棵子孙不进索引」(省内存):
//! 搜 `node_modules`/`temp`/`target` 能找到目录本身,但其内容不索引。
//!
//! 排除表来自「噪声」设置页的排除行(用户可增删,内置段名也作为普通行存在)。
//! 两种模式(按归一化后是否含 `/` 区分):
//! - 段名模式(`node_modules`):任意层级的同名目录。
//! - 锚定路径模式(`D:\ProgramData`):盘符锚定 + 从卷根起逐段连续匹配,
//!   只影响那棵树(不会误伤其它盘的同名目录)。
//!
//! 收录原则(内置默认):只排除「几乎必为垃圾/缓存/构建产物」且可能海量的
//! 高置信段名。`packages` 这类常见 monorepo 源码目录不在此列——裸段名无法
//! 区分源码与依赖,误排会让整片源码搜不到(见 A-2)。

/// 内置默认段名(小写、有序)。作为「噪声」页的初始排除行种子;用户可删。
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

/// 兼容旧调用点(内置默认段名判定)。生产分类走 [`ExcludeRules`]。
pub fn is_noise_segment(name_lc: &[u8]) -> bool {
    std::str::from_utf8(name_lc)
        .map(|s| NOISE_SEGMENTS.binary_search(&s).is_ok())
        .unwrap_or(false)
}

/// 归一化一个排除模式:trim、小写、反斜杠转正斜杠、去尾斜杠。空串返回 None。
pub fn normalize_pattern(raw: &str) -> Option<String> {
    let s = raw.trim().to_lowercase().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    (!s.is_empty()).then_some(s)
}

/// 归一化后的排除规则集(排除行为行)。
#[derive(Clone, Default, Debug)]
pub struct ExcludeRules {
    /// 段名模式(小写、有序二分)。归一化后不含 `/`。
    segments: Vec<String>,
    /// 锚定路径模式:Some(盘符) = 盘符锚定;None = 无盘符(任意卷)。
    /// segs = 剥掉盘符段后的路径段序列,从卷根起连续匹配。
    anchored: Vec<(Option<char>, Vec<String>)>,
}

impl ExcludeRules {
    /// 由原始模式表构建(已归一化/去重;顺序无关)。
    pub fn build(raw: &[String]) -> Self {
        let mut segments = Vec::new();
        let mut anchored: Vec<(Option<char>, Vec<String>)> = Vec::new();
        for s in raw {
            let Some(p) = normalize_pattern(s) else { continue };
            if p.contains('/') {
                let comps: Vec<&str> = p.split('/').collect();
                // `d:/a/b` → 盘符锚定 + [a,b];`foo/bar`(无盘符)→ 任意卷根 + [foo,bar]
                let (drive, segs): (Option<char>, Vec<String>) = if comps[0].len() == 2
                    && comps[0].as_bytes()[1] == b':'
                    && comps.len() >= 2
                {
                    (
                        // 盘符归一为大写(卷盘符在应用里是大写)
                        Some(comps[0].as_bytes()[0].to_ascii_uppercase() as char),
                        comps[1..].iter().map(|x| x.to_string()).collect(),
                    )
                } else {
                    (None, comps.iter().map(|x| x.to_string()).collect())
                };
                if segs.is_empty() {
                    continue; // 纯盘符(`d:`)不排整卷,不是这里的语义
                }
                anchored.push((drive, segs));
            } else {
                segments.push(p);
            }
        }
        segments.sort_unstable();
        segments.dedup();
        Self { segments, anchored }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.anchored.is_empty()
    }

    /// 是否含锚定路径模式(classify_chain 收集祖先链的开销门闸)。
    pub fn has_anchored(&self) -> bool {
        !self.anchored.is_empty()
    }

    /// 段名快查(热路径:逐条目逐祖先调用)。
    pub fn is_segment(&self, name_lc: &[u8]) -> bool {
        std::str::from_utf8(name_lc)
            .map(|s| self.segments.binary_search(&s.to_string()).is_ok())
            .unwrap_or(false)
    }

    /// 锚定匹配:祖先链(从卷根起、不含盘符段)是否落在某个锚定模式之下。
    /// `chain` = 该祖先的相对段名序列(根优先);`letter` = 本卷盘符。
    pub fn chain_anchored(&self, chain: &[&[u8]], letter: char) -> bool {
        if self.anchored.is_empty() || chain.is_empty() {
            return false;
        }
        for (drive, segs) in &self.anchored {
            if let Some(d) = drive {
                if *d != letter {
                    continue;
                }
            }
            if chain.len() < segs.len() {
                continue;
            }
            let ok = segs
                .iter()
                .zip(chain.iter())
                .all(|(pat, seg)| seg.eq_ignore_ascii_case(pat.as_bytes()));
            if ok {
                return true;
            }
        }
        false
    }

    /// 全路径判定(排序层硬过滤兜底 Everything/历史等来源)。
    /// `norm_path` = 小写正斜杠的完整路径(含 `d:` 盘符段)。
    /// 语义与索引层一致:目录自身可见,只有**严格位于**排除目录之下的条目被剔除。
    pub fn path_excluded(&self, norm_path: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let comps: Vec<&str> = norm_path.split('/').collect();
        if comps.len() < 3 {
            return false; // `d:/xxx` 顶层条目没有更深的祖先可判
        }
        let drive = if comps[0].len() == 2 && comps[0].as_bytes()[1] == b':' {
            Some(comps[0].as_bytes()[0].to_ascii_uppercase() as char)
        } else {
            None
        };
        // 祖先段 = 去掉盘符段与自身名的部分
        let ancestors = &comps[1..comps.len() - 1];
        for (d, segs) in &self.anchored {
            if let Some(d) = d {
                if *d != drive.unwrap_or('\0') {
                    continue;
                }
            }
            if ancestors.len() >= segs.len()
                && segs
                    .iter()
                    .zip(ancestors.iter())
                    .all(|(pat, seg)| seg.eq_ignore_ascii_case(pat))
            {
                return true;
            }
        }
        ancestors.iter().any(|seg| self.is_segment(seg.as_bytes()))
    }

    /// 指纹(快照头部):规则表内容变了,旧快照的 NOISE 位全部失真,必须重扫。
    /// 排序后哈希,顺序无关。
    pub fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.segments.hash(&mut h);
        for (d, segs) in &self.anchored {
            d.hash(&mut h);
            segs.hash(&mut h);
        }
        h.finish()
    }
}

/// 内置默认的原始模式表(迁移种子/「恢复默认」用)。
pub fn builtin_patterns() -> Vec<String> {
    NOISE_SEGMENTS.iter().map(|s| s.to_string()).collect()
}

/// 单个归一化模式影响哪些盘:None = 全部盘;Some = 仅这些盘符。
/// 盘符锚定路径(`d:/x`)只影响 d 盘;段名(`node_modules`)与无盘符
/// 锚定(`foo/bar`)影响所有盘。用于「立即重建」只重扫受影响卷。
pub fn affected_drives_for_pattern(normalized: &str) -> Option<Vec<char>> {
    if !normalized.contains('/') {
        return None; // 段名 → 全部盘
    }
    let comps: Vec<&str> = normalized.split('/').collect();
    if comps[0].len() == 2 && comps[0].as_bytes()[1] == b':' && comps.len() >= 2 {
        Some(vec![comps[0].as_bytes()[0].to_ascii_uppercase() as char])
    } else {
        None // 无盘符锚定 → 全部盘
    }
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
        let r = ExcludeRules::build(&["node_modules".into(), ".git".into()]);
        assert!(r.is_segment(b"node_modules"));
        assert!(r.is_segment("Node_Modules".to_lowercase().as_bytes()));
        assert!(!r.is_segment(b"my-project"));
        assert!(!r.is_segment(b"targeted")); // 只匹配整段名
    }

    #[test]
    fn normalize_keeps_backslash_and_case() {
        assert_eq!(normalize_pattern(r"D:\ProgramData\"), Some("d:/programdata".into()));
        assert_eq!(normalize_pattern(" node_modules "), Some("node_modules".into()));
        assert_eq!(normalize_pattern("///"), None);
    }

    #[test]
    fn anchored_pattern_scopes_to_drive() {
        let r = ExcludeRules::build(&[r"D:\ProgramData".into()]);
        // D 盘下命中
        assert!(r.chain_anchored(&[b"programdata"], 'D'));
        assert!(r.chain_anchored(&[b"programdata", b"foo"], 'D'));
        // C 盘的同名目录不受影响
        assert!(!r.chain_anchored(&[b"programdata"], 'C'));
        assert!(!r.chain_anchored(&[], 'D'));
        // 全路径判定:子孙剔除、目录自身与深层无关路径保留
        assert!(r.path_excluded("d:/programdata/foo/bar.txt"));
        assert!(!r.path_excluded("d:/programdata")); // 自身不剔(语义:目录可见)
        assert!(!r.path_excluded("c:/programdata/x"));
        assert!(!r.path_excluded("d:/programdata-backup/x")); // 边界:不误伤
    }

    #[test]
    fn segment_pattern_matches_any_depth_via_path() {
        let r = ExcludeRules::build(&["node_modules".into()]);
        assert!(r.path_excluded("d:/proj/node_modules/pkg/index.js"));
        assert!(r.path_excluded("e:/a/b/node_modules/x")); // 子孙剔除
        assert!(!r.path_excluded("e:/a/b/node_modules")); // 目录自身可见
        assert!(!r.path_excluded("d:/node_modules_pkg/x")); // 段名整段匹配
        assert!(!r.path_excluded("d:/proj/node_modules")); // 目录自身可见
    }

    #[test]
    fn hash_changes_with_content() {
        let a = ExcludeRules::build(&["node_modules".into()]);
        let b = ExcludeRules::build(&["node_modules".into(), "x".into()]);
        let a2 = ExcludeRules::build(&["NODE_MODULES".into(), "".into()]);
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash(), a2.hash()); // 归一化后等价 → 同指纹
    }
}