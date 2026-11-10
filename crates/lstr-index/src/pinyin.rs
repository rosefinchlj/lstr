//! 拼音缓存:只为非 ASCII 名字生成(实测占比 ~0.2%,量级 <10 万),
//! 线性扫描即可。语义对齐 lstr `search/pinyin.rs`(initials / full)。

use pinyin::ToPinyin;

use crate::store::VolumeStore;

pub struct PinyinCache {
    /// slot 升序。
    pub slots: Vec<u32>,
    /// 与 slots 平行:首字母串 / 全拼串(小写,无空格)。
    pub initials: Vec<String>,
    pub full: Vec<String>,
}

impl PinyinCache {
    pub fn empty() -> Self {
        Self {
            slots: Vec::new(),
            initials: Vec::new(),
            full: Vec::new(),
        }
    }

    /// 从 store 全量重建(compact 后调用;增量 slot 等 compact 一并进缓存)。
    pub fn rebuild(store: &VolumeStore) -> Self {
        let mut cache = Self::empty();
        for slot in 0..store.name_off.len() as u32 {
            if store.attrs[slot as usize] & crate::store::ATTR_INTERNAL != 0 {
                continue;
            }
            let name = store.name(slot);
            if name.is_ascii() {
                continue;
            }
            let Ok(s) = std::str::from_utf8(name) else {
                continue;
            };
            let ini = initials_of(s);
            let full = full_of(s);
            if ini.is_empty() {
                continue;
            }
            cache.slots.push(slot);
            cache.initials.push(ini);
            cache.full.push(full);
        }
        cache
    }

    /// slot → (initials, full) 二分查找。
    pub fn get(&self, slot: u32) -> Option<(&str, &str)> {
        let i = self.slots.binary_search(&slot).ok()?;
        Some((&self.initials[i], &self.full[i]))
    }

    /// 局部刷新:只重算 `slots` 里这些 slot 的拼音条目(新增/改名/删除)。
    ///
    /// 分段保存不 compact,不能靠 compact 后的全量 rebuild 把新条目带进缓存,
    /// 也不该在引擎线程上重建 250 万条。缓存只含非 ASCII 名(<1%),局部刷新
    /// 规模 = 上次 compact 以来的 delta/改名数(受 COMPACT_DELTA 约束),
    /// 毫秒级;保存期间查询延迟不受影响。
    pub fn refresh_slots(&mut self, store: &VolumeStore, slots: &[u32]) {
        let mut affected: Vec<u32> = slots
            .iter()
            .copied()
            .filter(|&s| (s as usize) < store.name_off.len())
            .collect();
        affected.sort_unstable();
        affected.dedup();
        if affected.is_empty() {
            return;
        }
        let mut rows: Vec<(u32, String, String)> = self
            .slots
            .iter()
            .copied()
            .zip(self.initials.iter().cloned())
            .zip(self.full.iter().cloned())
            .filter_map(|((slot, ini), full)| {
                (affected.binary_search(&slot).is_err()).then_some((slot, ini, full))
            })
            .collect();
        for slot in affected {
            if store.attrs[slot as usize] & crate::store::ATTR_INTERNAL != 0 {
                continue;
            }
            let name = store.name(slot);
            if name.is_ascii() {
                continue;
            }
            let Ok(s) = std::str::from_utf8(name) else {
                continue;
            };
            let ini = initials_of(s);
            let full = full_of(s);
            if ini.is_empty() {
                continue;
            }
            rows.push((slot, ini, full));
        }
        rows.sort_unstable_by_key(|&(slot, _, _)| slot);
        self.slots = rows.iter().map(|&(slot, _, _)| slot).collect();
        self.initials = rows.iter().map(|(_, i, _)| i.clone()).collect();
        self.full = rows.iter().map(|(_, _, f)| f.clone()).collect();
    }
    /// 拼音候选:全部 token 命中(前缀或子串)。
    pub fn search<'a>(
        &'a self,
        store: &'a VolumeStore,
        tokens: &[Vec<u8>],
        cap: usize,
    ) -> impl Iterator<Item = u32> + 'a {
        let mut hits: Vec<u32> = Vec::new();
        if !tokens.is_empty() {
            for (i, &slot) in self.slots.iter().enumerate() {
                if hits.len() >= cap {
                    break;
                }
                // 缓存与 store 的 slot 编号可能短暂不同步(例如 save 内部 compact
                // 之后、调用方重建缓存之前)。宁可漏掉这一条,也不能越界 panic:
                // 搜索线程一 panic,整个搜索服务就永久失联(m04259)。
                let Some(&attrs) = store.attrs.get(slot as usize) else {
                    continue;
                };
                // 拼音命中还须通过 store 位过滤(DEAD/NOISE 可能晚于缓存更新)
                if attrs & crate::store::ATTR_INTERNAL != 0 {
                    continue;
                }
                let ini = self.initials[i].as_bytes();
                let full = self.full[i].as_bytes();
                if crate::query::pinyin_match(ini, full, tokens) {
                    hits.push(slot);
                }
            }
        }
        hits.into_iter()
    }
}

/// 首字母串:ASCII 字母数字原样小写,CJK 取拼音首字母。例:微信 → wx。
pub fn initials_of(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(py) = ch.to_pinyin() {
            out.push_str(py.first_letter());
        }
    }
    out
}

/// 全拼串(不带声调)。例:微信 → weixin。
pub fn full_of(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(py) = ch.to_pinyin() {
            out.push_str(py.plain());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_initials_and_full() {
        assert_eq!(initials_of("微信"), "wx");
        assert_eq!(full_of("微信"), "weixin");
        // 语义对齐 lstr:非字母数字非 CJK 的字符(如 .)跳过
        assert_eq!(initials_of("报告.docx"), "bgdocx");
        assert_eq!(initials_of("Visual Studio Code"), "visualstudiocode");
    }

    #[test]
    fn cache_build_and_search() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, "报告.docx".as_bytes());
        s.insert(11, 5, 0, "文件.txt".as_bytes());
        s.compact();
        let cache = PinyinCache::rebuild(&s);
        assert_eq!(cache.slots.len(), 2);
        let tokens: Vec<Vec<u8>> = vec![b"bg".to_vec()];
        let hits: Vec<u32> = cache.search(&s, &tokens, 100).collect();
        assert_eq!(hits, vec![s.find_slot(10).unwrap()]);
        let tokens: Vec<Vec<u8>> = vec![b"wenjian".to_vec()];
        let hits: Vec<u32> = cache.search(&s, &tokens, 100).collect();
        assert_eq!(hits.len(), 1);
    }

    /// 分段保存不 compact:新增/改名的非 ASCII 条目必须靠局部刷新进缓存,
    /// 改成 ASCII 名的槽必须从缓存移除(否则旧拼音会误命中)。
    #[test]
    fn refresh_slots_tracks_new_rename_and_ascii() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, "报告.docx".as_bytes());
        s.compact();
        let mut cache = PinyinCache::rebuild(&s);
        let slot = s.find_slot(10).unwrap();

        // 改名(仍是 CJK):旧拼音被替换
        s.insert(10, 5, 0, "文件.txt".as_bytes());
        cache.refresh_slots(&s, &[slot]);
        let hits: Vec<u32> = cache
            .search(&s, &[b"wenjian".to_vec()], 100)
            .collect();
        assert_eq!(hits, vec![slot]);
        let hits: Vec<u32> = cache.search(&s, &[b"bg".to_vec()], 100).collect();
        assert!(hits.is_empty(), "旧拼音条目必须被移除");

        // 新增 delta 非 ASCII slot
        s.insert(11, 5, 0, "图片.png".as_bytes());
        let new_slot = s.find_slot(11).unwrap();
        cache.refresh_slots(&s, &[new_slot]);
        let hits: Vec<u32> = cache.search(&s, &[b"tp".to_vec()], 100).collect();
        assert_eq!(hits, vec![new_slot]);

        // 再改成 ASCII:缓存移除,不再命中拼音
        s.insert(10, 5, 0, b"report.txt");
        cache.refresh_slots(&s, &[slot]);
        assert!(cache.get(slot).is_none());
    }
}
