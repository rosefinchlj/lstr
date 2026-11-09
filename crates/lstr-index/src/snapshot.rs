//! 快照:卷内 store 的持久化。
//!
//! 格式(小端):
//! ```text
//! magic "LIDX" | u32 version | u128 guid | u8 letter | u8 flags(complete=1,noise=2)
//! u64 journal_id | i64 next_usn | u64 resume_frn | u64 est_records
//! u64 count
//! names  : u64 len + bytes(\0 结尾打包;name_off 由此重建,不落盘)
//! offs   : u64 len + u32×  (按小写名序)
//! frn    : u64 len + u64×  (按 FRN 序)
//! parent : u64 len + u32×
//! attrs  : u64 len + u16×
//! mtime  : u64 len + u32×  (unix 秒;0=未知,USN 路径为 0)
//! unres  : u64 len + (u32 slot, u64 parent_frn)×
//! ```
//! 只在 compact 后落盘(列处于一致有序态)。写盘走 tmp+rename 原子替换。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::store::{VolumeStore, NO_PARENT};

pub const MAGIC: &[u8; 4] = b"LIDX";
/// v3:未消解父链接 (slot, parent_frn) 也落盘(v2 没有,重启后这些条目永久退化)。
/// v4:增加 mtime 列(unix 秒),内置时间筛选/结果元数据使用。
pub const VERSION: u32 = 4;

/// 引擎调试日志(%APPDATA%\lstr\index\engine.log)。
/// worker 提权进程与 UI 都写同用户目录;失败静默。
pub fn debug_log(msg: &str) {
    let Ok(base) = std::env::var("APPDATA") else { return };
    let path = std::path::PathBuf::from(base).join("lstr").join("index").join("engine.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", chrono_now(), msg);
    }
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    format!("{}.{:03}", secs, d.subsec_millis())
}

pub const FLAG_COMPLETE: u8 = 0x01;
/// 快照生成时 `exclude_noise` 的取值(M3):NOISE 位是扫描/compact 期
/// 写进 attrs 的,开关切换后旧快照的可见性与新配置不符,必须拒绝重扫。
pub const FLAG_NOISE: u8 = 0x02;

#[derive(Clone, Debug)]
pub struct SnapshotHeader {
    pub guid: u128,
    pub letter: char,
    pub complete: bool,
    pub exclude_noise: bool,
    pub journal_id: u64,
    pub next_usn: i64,
    pub resume_frn: u64,
    pub est_records: u64,
}

pub fn snapshot_path(dir: &Path, guid: u128) -> PathBuf {
    dir.join(format!("vol_{guid:032x}.bin"))
}

/// `save` 的附加结果。
///
/// `compacted` 表示保存期间执行了 compact —— 那会重排/收缩 slot 编号。
/// 调用方只要持有以 slot 为键的缓存(engine 里的拼音候选缓存),就必须在
/// 保存成功后重建缓存;否则缓存里的旧 slot 会越界或指向别的条目
/// (m04259: `PinyinCache::search` 越界 panic,整个搜索线程死掉)。
pub struct SaveReport {
    pub bytes: u64,
    pub compacted: bool,
}

/// 序列化并原子写盘。count==0 且 !complete 的空卷不写。
/// 契约「列一致」由这里保证:有未归并 delta 时先 compact(早退使重复调用廉价)。
///
/// 需要知道保存期间是否发生过 compact 的调用方用 [`save_report`]。
pub fn save(
    dir: &Path,
    header: &SnapshotHeader,
    store: &mut VolumeStore,
) -> std::io::Result<u64> {
    save_report(dir, header, store).map(|report| report.bytes)
}

/// 同 [`save`],但把「保存期间是否 compact 过」一并返回,供缓存维护使用。
pub fn save_report(
    dir: &Path,
    header: &SnapshotHeader,
    store: &mut VolumeStore,
) -> std::io::Result<SaveReport> {
    // 列一致契约:未归并 delta、扫描中、或有就地改名(名字区残留幽灵字节)
    // 都必须先 compact,否则 count 与 names 条目数对不上,load 直接拒绝
    // → 每次启动全量重扫(property 随机流里约 1/4 概率命中改名后落盘)。
    let compacted = if !store.delta_is_empty()
        || store.is_scanning()
        || store.has_pending_rename()
    {
        store.compact();
        true
    } else {
        false
    };
    std::fs::create_dir_all(dir)?;
    let final_path = snapshot_path(dir, header.guid);
    let tmp_path = dir.join(format!("vol_{:032x}.tmp", std::process::id()));
    let f = std::fs::File::create(&tmp_path)?;
    let mut w = std::io::BufWriter::with_capacity(8 << 20, f);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&header.guid.to_le_bytes())?;
    w.write_all(&[header.letter as u32 as u8])?;
    w.write_all(&[flags_of(header.complete, header.exclude_noise)])?;
    w.write_all(&header.journal_id.to_le_bytes())?;
    w.write_all(&header.next_usn.to_le_bytes())?;
    w.write_all(&header.resume_frn.to_le_bytes())?;
    w.write_all(&header.est_records.to_le_bytes())?;
    w.write_all(&(store.name_off.len() as u64).to_le_bytes())?;
    write_blob(&mut w, &store.names)?;
    write_col_u32(&mut w, &store.offs)?;
    write_col_u64(&mut w, &store.frn)?;
    write_col_u32(&mut w, &store.parent)?;
    write_col_u16(&mut w, &store.attrs)?;
    write_col_u32(&mut w, &store.mtime)?;
    // 未消解父链接:compact 会把 slot 重映射到最终编号,此时写入可直接恢复
    let pairs = store.unresolved_pairs();
    w.write_all(&(pairs.len() as u64).to_le_bytes())?;
    for (slot, parent_frn) in &pairs {
        w.write_all(&slot.to_le_bytes())?;
        w.write_all(&parent_frn.to_le_bytes())?;
    }
    w.flush()?;
    // 先 fsync 再 rename:否则崩溃/断电时 rename 可能先于数据落盘,
    // 留下「文件名在、内容空/截断」的快照(load 拒绝 → 全量重扫)。
    let f = w.into_inner().map_err(|e| e.into_error())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path)?;
    // rename 改的是父目录项:再 flush 一次目录,缩小「文件名已改、目录项未持久」
    // 的断电窗口。FAT/网络盘可能不支持,失败只记日志(最坏仍是一次重扫)。
    if let Some(dir) = final_path.parent() {
        if !crate::win::flush_dir(dir) {
            debug_log(&format!(
                "snapshot: flush_dir {} failed, dir entry may be non-durable",
                dir.display()
            ));
        }
    }
    Ok(SaveReport {
        bytes: std::fs::metadata(&final_path)?.len(),
        compacted,
    })
}

fn flags_of(complete: bool, exclude_noise: bool) -> u8 {
    (if complete { FLAG_COMPLETE } else { 0 }) | (if exclude_noise { FLAG_NOISE } else { 0 })
}

fn write_blob<W: Write>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)
}

fn write_col<W: Write, T: Copy>(w: &mut W, col: &[T]) -> std::io::Result<()> {
    let bytes = unsafe {
        std::slice::from_raw_parts(col.as_ptr() as *const u8, col.len() * std::mem::size_of::<T>())
    };
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)
}

fn write_col_u32<W: Write>(w: &mut W, col: &[u32]) -> std::io::Result<()> {
    write_col(w, col)
}
fn write_col_u64<W: Write>(w: &mut W, col: &[u64]) -> std::io::Result<()> {
    write_col(w, col)
}
fn write_col_u16<W: Write>(w: &mut W, col: &[u16]) -> std::io::Result<()> {
    write_col(w, col)
}

pub struct LoadedSnapshot {
    pub header: SnapshotHeader,
    pub store: VolumeStore,
}

/// 读快照;任何不一致(魔数/版本/长度)返回 None,由调用方触发全量重扫。
pub fn load(dir: &Path, guid: u128, noise_enabled: bool) -> Option<LoadedSnapshot> {
    let path = snapshot_path(dir, guid);
    let mut data = Vec::with_capacity(64 << 20);
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(err) => {
            debug_log(&format!("load {guid:032x}: open failed: {err}"));
            return None;
        }
    };
    if let Err(err) = f.read_to_end(&mut data) {
        debug_log(&format!("load {guid:032x}: read failed: {err}"));
        return None;
    }
    let mut cur = &data[..];

    if cur.len() < 4 + 4 || &cur[0..4] != MAGIC {
        debug_log(&format!("load {guid:032x}: bad magic/short ({} bytes)", cur.len()));
        return None;
    }
    cur = &cur[4..];
    let Some(version) = u32::from_le_array(&mut cur) else {
        debug_log(&format!("load {guid:032x}: truncated at version"));
        return None;
    };
    if version != VERSION {
        debug_log(&format!(
            "load {guid:032x}: version {version} != {VERSION}, stale snapshot removed (full rescan)"
        ));
        // 旧格式不做迁移:删掉它,避免每次启动都尝试加载;新扫描会重写。
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let Some(guid_h) = u128::from_le_array(&mut cur) else {
        debug_log(&format!("load {guid:032x}: truncated at guid"));
        return None;
    };
    if guid_h != guid {
        debug_log(&format!("load {guid:032x}: guid mismatch header={guid_h:032x}"));
        return None;
    }
    let letter = char::from_u32(*cur.first()? as u32)?;
    cur = &cur[1..];
    let flags = *cur.first()?;
    cur = &cur[1..];
    let noise_flag = flags & FLAG_NOISE != 0;
    if noise_flag != noise_enabled {
        debug_log(&format!(
            "load {guid:032x}: exclude_noise snapshot={noise_flag} cfg={noise_enabled}"
        ));
        return None;
    }
    let journal_id = u64::from_le_array(&mut cur)?;
    let next_usn = i64::from_le_array(&mut cur)?;
    let resume_frn = u64::from_le_array(&mut cur)?;
    let est_records = u64::from_le_array(&mut cur)?;
    let count = u64::from_le_array(&mut cur)? as usize;

    let names = read_blob(&mut cur)?;
    let offs = read_col_u32(&mut cur)?;
    let frn = read_col_u64(&mut cur)?;
    let parent = read_col_u32(&mut cur)?;
    let attrs = read_col_u16(&mut cur)?;
    let mtime = read_col_u32(&mut cur)?;
    let n_unresolved = u64::from_le_array(&mut cur)? as usize;
    let mut unresolved: Vec<(u32, u64)> = Vec::with_capacity(n_unresolved.min(1 << 20));
    for _ in 0..n_unresolved {
        let slot = u32::from_le_array(&mut cur)?;
        let parent_frn = u64::from_le_array(&mut cur)?;
        unresolved.push((slot, parent_frn));
    }
    if !cur.is_empty() {
        return None;
    }
    if offs.len() != count || frn.len() != count || parent.len() != count || attrs.len() != count || mtime.len() != count {
        debug_log(&format!(
            "load {guid:032x}: column len mismatch offs={} frn={} parent={} attrs={} count={count}",
            offs.len(), frn.len(), parent.len(), attrs.len()
        ));
        return None;
    }
    if n_unresolved > count
        || unresolved.iter().any(|&(slot, p)| slot as usize >= count || p == 0)
    {
        debug_log(&format!(
            "load {guid:032x}: invalid unresolved table (n={n_unresolved} count={count})"
        ));
        return None;
    }
    // 索引列必须落在 name 区/槽位范围内,否则后续 name(slot)/parent 追踪会越界。
    // (offs 的元素还应是名字起点;由 save 先 compact 保证,这里只做廉价边界校验。)
    if offs.iter().any(|&o| o as usize >= names.len()) {
        debug_log(&format!("load {guid:032x}: offs out of range (names={})", names.len()));
        return None;
    }
    if parent.iter().any(|&p| p != NO_PARENT && p as usize >= count) {
        debug_log(&format!("load {guid:032x}: parent out of range (count={count})"));
        return None;
    }
    // name_off 由 names 重建(\0 结尾 → 偏移即条目起点)
    let mut name_off: Vec<u32> = Vec::with_capacity(count);
    let mut off = 0usize;
    while off < names.len() {
        name_off.push(off as u32);
        match names[off..].iter().position(|&b| b == 0) {
            Some(p) => off += p + 1,
            None => {
                debug_log(&format!(
                    "load {guid:032x}: unterminated name at offset {off} (names={} count={count})",
                    names.len()
                ));
                return None;
            }
        }
    }
    if name_off.len() != count {
        debug_log(&format!(
            "load {guid:032x}: name_off {} != count {count}",
            name_off.len()
        ));
        return None;
    }
    let store = VolumeStore::from_parts(
        names,
        offs,
        frn,
        parent,
        attrs,
        mtime,
        name_off,
        unresolved,
        noise_enabled,
    );
    Some(LoadedSnapshot {
        header: SnapshotHeader {
            guid,
            letter,
            complete: flags & FLAG_COMPLETE != 0,
            exclude_noise: noise_flag,
            journal_id,
            next_usn,
            resume_frn,
            est_records,
        },
        store,
    })
}

trait LeRead: Sized {
    fn from_le_array(cur: &mut &[u8]) -> Option<Self>;
}
macro_rules! le_read {
    ($($t:ty),*) => {
        $(impl LeRead for $t {
            fn from_le_array(cur: &mut &[u8]) -> Option<Self> {
                let n = std::mem::size_of::<$t>();
                if cur.len() < n {
                    return None;
                }
                let v = <$t>::from_le_bytes(cur[..n].try_into().unwrap());
                *cur = &cur[n..];
                Some(v)
            }
        })*
    };
}
le_read!(u16, u32, u64, i64, u128);

fn read_blob(cur: &mut &[u8]) -> Option<Vec<u8>> {
    let len = u64::from_le_array(cur)? as usize;
    if cur.len() < len {
        return None;
    }
    let v = cur[..len].to_vec();
    *cur = &cur[len..];
    Some(v)
}

fn read_col_u32(cur: &mut &[u8]) -> Option<Vec<u32>> {
    let bytes = read_blob(cur)?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

fn read_col_u64(cur: &mut &[u8]) -> Option<Vec<u64>> {
    let bytes = read_blob(cur)?;
    if bytes.len() % 8 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

fn read_col_u16(cur: &mut &[u8]) -> Option<Vec<u16>> {
    let bytes = read_blob(cur)?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QuerySpec;
    use crate::store::VolumeStore;

    #[test]
    fn snapshot_roundtrip() {
        let mut s = VolumeStore::new(true);
        s.insert(5, 0, 0x10, b"root");
        s.insert_meta(10, 5, 0, b"alpha.txt", 1_700_000_123);
        s.insert(11, 5, 0, "报告.docx".as_bytes());
        s.insert(12, 5, 0x10, b"node_modules");
        s.insert(13, 12, 0, b"dep.txt");
        s.compact();
        let header = SnapshotHeader {
            guid: 0x1234_5678_9abc_def0_1122_3344,
            letter: 'C',
            complete: true,
            exclude_noise: true,
            journal_id: 0xfeed,
            next_usn: 0x42,
            resume_frn: 0,
            est_records: 999,
        };
        let dir = std::env::temp_dir().join(format!("lstr-idx-test-{}", std::process::id()));
        let size = save(&dir, &header, &mut s).unwrap();
        assert!(size > 0);
        let loaded = load(&dir, header.guid, true).unwrap();
        // M3:噪声排除开关与快照生成时不符 → 拒绝加载(触发全量重扫),
        // 否则切换开关后旧快照里的 NOISE 位会让结果与设置页承诺不一致。
        assert!(load(&dir, header.guid, false).is_none());
        assert_eq!(loaded.header.journal_id, 0xfeed);
        assert_eq!(loaded.header.next_usn, 0x42);
        assert!(loaded.header.complete);
        let ls = &loaded.store;
        assert_eq!(ls.name_off.len(), 3); // node_modules 子树被排除
        assert!(ls.find_slot(10).is_some());
        let slot = ls.find_slot(10).unwrap();
        assert_eq!(ls.name(slot), b"alpha.txt");
        // v4:mtime 列落盘/读回(0=未知)。
        assert_eq!(ls.mtime[slot as usize], 1_700_000_123);
        assert_eq!(ls.mtime[ls.find_slot(11).unwrap() as usize], 0);
        assert_eq!(ls.char_mask.len(), ls.name_off.len());
        assert_eq!(ls.offs.len(), ls.name_off.len());
        // 有序性保持
        let names: Vec<&[u8]> = ls.offs.iter().map(|&x| ls.name_lc(x)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v3:未消解父链接落盘。旧实现 load 后 unresolved 为空,父项永远等不到、
    /// 子项路径永久退化为 NO_PARENT,只能靠一次全量重扫碰运气。
    #[test]
    fn unresolved_parents_survive_roundtrip() {
        let dir = tmpdir("unres");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        // 子项先到、父 FRN 99 未到 → 进 unresolved
        s.insert(20, 99, 0, b"orphan.txt");
        s.compact();
        assert_eq!(s.unresolved_pairs().len(), 1);
        let h = header(0x1234, false);
        save(&dir, &h, &mut s).unwrap();
        let mut ls = load(&dir, h.guid, false).expect("load").store;
        assert_eq!(ls.unresolved_pairs().len(), 1);
        // 父项在后续 journal 事件里补到 → compact 消解,不需要全量重扫
        ls.insert(99, 5, 0x10, b"late-dir");
        ls.compact();
        assert!(ls.unresolved_pairs().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lstr-idx-{tag}-{}", std::process::id()))
    }

    fn header(guid: u128, complete: bool) -> SnapshotHeader {
        SnapshotHeader {
            guid,
            letter: 'C',
            complete,
            // 这些用例的 store 都是 VolumeStore::new(false),load 也传 false;
            // 指纹必须一致,否则 M3 的加载校验会按设计拒绝(v1 快照语义)。
            exclude_noise: false,
            journal_id: 7,
            next_usn: 900,
            resume_frn: 0,
            est_records: 0,
        }
    }

    /// H1 回归:compact → insert×N(含就地改名)→ save → load,
    /// 计数/FRN 定位/名字查询/继续插入/再次 compact 全部自洽。
    ///
    /// 旧实现 save 按 `name_off.len()` 写 count 却落未 compact 的 offs,
    /// 增量 insert 后 load 因 `offs.len() != count` 永久拒绝。
    #[test]
    fn incremental_state_survives_roundtrip() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.compact();
        for i in 0..5u64 {
            s.insert(100 + i, 5, 0, format!("delta{i}.txt").as_bytes());
        }
        s.insert(10, 5, 0, b"alpha-renamed.txt"); // 就地改名:base 名序失效
        let n = s.len();

        let dir = tmpdir("h1-incr");
        let _ = std::fs::remove_dir_all(&dir);
        let h = header(0xabc, true);
        save(&dir, &h, &mut s).unwrap();
        let loaded = load(&dir, h.guid, false).unwrap();
        let mut ls = loaded.store;
        assert!(loaded.header.complete);
        assert_eq!(ls.len(), n);
        // save 接管列一致契约:delta/改名已归并,offs 覆盖全部 slot
        assert_eq!(ls.offs.len(), ls.len());
        assert_eq!(
            ls.find_slot(10).map(|x| ls.name(x).to_vec()),
            Some(b"alpha-renamed.txt".to_vec())
        );
        assert_eq!(
            ls.find_slot(103).map(|x| ls.name(x).to_vec()),
            Some(b"delta3.txt".to_vec())
        );

        // 名字查询:子串与精确都必须命中
        let spec = QuerySpec { tokens: vec!["delta3".into()], ..Default::default() };
        let (slots, total, _) = crate::query::query_volume(&ls, &spec, 100);
        assert_eq!(total, 1);
        assert_eq!(ls.name(slots[0]), b"delta3.txt");
        let spec = QuerySpec {
            tokens: vec!["delta4.txt".into()],
            exact: Some("delta4.txt".into()),
            ..Default::default()
        };
        let (_, total, _) = crate::query::query_volume(&ls, &spec, 100);
        assert_eq!(total, 1);

        // 再插入不 panic,FRN 定位可用;compact 后恢复全覆盖 offs
        assert!(ls.insert(200, 5, 0, b"after-load.txt"));
        assert!(ls.find_slot(200).is_some());
        ls.insert(10, 5, 0, b"alpha.txt");
        ls.compact();
        assert_eq!(ls.offs.len(), ls.len());
        let names: Vec<&[u8]> = ls.offs.iter().map(|&x| ls.name_lc(x)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H1 回归(改名无 delta):compact 后只做一次就地改名,delta 为空。
    /// 旧实现 save 的 compact 条件只看 delta/scanning → 不 compact 就落盘,
    /// names 里的幽灵旧名使 name_off 条目数 != count,load 拒绝 → 启动全量重扫
    /// (property 随机流约 1/4 概率踩中,表现为偶发 load=None)。
    #[test]
    fn rename_without_delta_survives_roundtrip() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.compact();
        assert!(s.delta_is_empty());
        assert!(s.insert(10, 5, 0, b"alpha-renamed.txt"));
        assert!(s.delta_is_empty(), "就地改名不进 delta");
        assert!(s.has_pending_rename());
        let dir = tmpdir("h1-rename-no-delta");
        let _ = std::fs::remove_dir_all(&dir);
        let h = header(0xfeed, true);
        save(&dir, &h, &mut s).unwrap();
        let loaded = load(&dir, h.guid, false).expect("改名后落盘的快照必须可 load");
        let ls = loaded.store;
        assert_eq!(ls.len(), 2);
        assert_eq!(
            ls.find_slot(10).map(|x| ls.name(x).to_vec()),
            Some(b"alpha-renamed.txt".to_vec())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H1 回归:扫描中断的部分快照必须可落盘/可 load。
    ///
    /// ZCode 方案的语义:save 时先归并(compact),partial 快照被归一化成
    /// ready 列布局;header.complete 仍为 false,重启握手拿不到增量位点,
    /// worker 走全量重扫,已有 entry 按 FRN 去重。因此不需要还原
    /// `is_scanning` 本身,但 count/名字/父链必须完整可用,后续 append 不坏。
    #[test]
    fn partial_scan_snapshot_roundtrip() {
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0x10, b"users");
        s.insert(11, 10, 0, b"a.txt");
        assert!(s.is_scanning());
        let dir = tmpdir("h1-partial");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = header(0xdef, false);
        h.resume_frn = 1234;
        save(&dir, &h, &mut s).unwrap();
        let loaded = load(&dir, h.guid, false).unwrap();
        assert!(!loaded.header.complete);
        assert_eq!(loaded.header.resume_frn, 1234);
        let mut ls = loaded.store;
        // save-time 归一秒变 ready 布局(旧实现这里 offs 为空 → load 拒绝)
        assert!(!ls.is_scanning());
        assert_eq!(ls.len(), 3);
        assert_eq!(ls.offs.len(), ls.len());
        assert_eq!(ls.name(ls.find_slot(10).unwrap()), b"users");
        // 继续 append:目录索引仍在,子项父链可解析
        assert!(ls.insert(12, 10, 0, b"b.txt"));
        assert_eq!(ls.len(), 4);
        let b = ls.len() - 1;
        assert_eq!(ls.parent[b as usize], ls.find_slot(10).unwrap());
        ls.compact();
        assert_eq!(ls.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// save → load → insert → save → load:两轮往返后状态仍一致。
    #[test]
    fn snapshot_roundtrip_twice() {
        let dir = tmpdir("h1-twice");
        let _ = std::fs::remove_dir_all(&dir);
        let guid = 0x5150;
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"one.txt");
        s.compact();
        save(&dir, &header(guid, true), &mut s).unwrap();
        let mut s = load(&dir, guid, false).unwrap().store;
        s.insert(11, 5, 0, b"two.txt");
        save(&dir, &header(guid, true), &mut s).unwrap();
        let ls = load(&dir, guid, false).unwrap().store;
        assert_eq!(ls.len(), 3);
        assert_eq!(
            ls.find_slot(11).map(|x| ls.name(x).to_vec()),
            Some(b"two.txt".to_vec())
        );
        assert_eq!(
            ls.find_slot(10).map(|x| ls.name(x).to_vec()),
            Some(b"one.txt".to_vec())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1 等旧格式不迁移:load 拒绝并删除文件,避免每次启动重复尝试。
    #[test]
    fn stale_version_snapshot_is_removed() {
        let dir = tmpdir("stale-ver");
        let _ = std::fs::remove_dir_all(&dir);
        let guid = 0x7777;
        let mut s = VolumeStore::new(false);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"a.txt");
        s.compact();
        let h = header(guid, true);
        save(&dir, &h, &mut s).unwrap();
        let p = snapshot_path(&dir, guid);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes()); // 伪造成 v1
        std::fs::write(&p, &bytes).unwrap();
        assert!(load(&dir, guid, false).is_none());
        assert!(!p.exists(), "旧版本快照加载失败后应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_garbage() {        let dir = std::env::temp_dir().join(format!("lstr-idx-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let guid = 42u128;
        let p = snapshot_path(&dir, guid);
        std::fs::write(&p, b"garbage-data-here").unwrap();
        assert!(load(&dir, guid, false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
