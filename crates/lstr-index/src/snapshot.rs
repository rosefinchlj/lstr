//! 快照:卷内 store 的持久化。
//!
//! 格式(小端):
//! ```text
//! magic "LIDX" | u32 version | u128 guid | u8 letter | u8 flags(complete=1)
//! u64 journal_id | i64 next_usn | u64 resume_frn | u64 est_records
//! u64 count
//! names  : u64 len + bytes(\0 结尾打包;name_off 由此重建,不落盘)
//! offs   : u64 len + u32×  (按小写名序)
//! frn    : u64 len + u64×  (按 FRN 序)
//! parent : u64 len + u32×
//! attrs  : u64 len + u16×
//! ```
//! 只在 compact 后落盘(列处于一致有序态)。写盘走 tmp+rename 原子替换。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::store::VolumeStore;

pub const MAGIC: &[u8; 4] = b"LIDX";
pub const VERSION: u32 = 1;

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

#[derive(Clone, Debug)]
pub struct SnapshotHeader {
    pub guid: u128,
    pub letter: char,
    pub complete: bool,
    pub journal_id: u64,
    pub next_usn: i64,
    pub resume_frn: u64,
    pub est_records: u64,
}

pub fn snapshot_path(dir: &Path, guid: u128) -> PathBuf {
    dir.join(format!("vol_{guid:032x}.bin"))
}

/// 序列化并原子写盘。count==0 且 !complete 的空卷不写。
/// 契约「列一致」由这里保证:有未归并 delta 时先 compact(早退使重复调用廉价)。
pub fn save(
    dir: &Path,
    header: &SnapshotHeader,
    store: &mut VolumeStore,
) -> std::io::Result<u64> {
    if !store.delta_is_empty() || store.is_scanning() {
        store.compact();
    }
    std::fs::create_dir_all(dir)?;
    let final_path = snapshot_path(dir, header.guid);
    let tmp_path = dir.join(format!("vol_{:032x}.tmp", std::process::id()));
    let f = std::fs::File::create(&tmp_path)?;
    let mut w = std::io::BufWriter::with_capacity(8 << 20, f);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&header.guid.to_le_bytes())?;
    w.write_all(&[header.letter as u32 as u8])?;
    w.write_all(&flags_of(header.complete))?;
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
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(std::fs::metadata(&final_path)?.len())
}

fn flags_of(complete: bool) -> [u8; 1] {
    [if complete { FLAG_COMPLETE } else { 0 }]
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
        debug_log(&format!("load {guid:032x}: version {version} != {VERSION}"));
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
    if !cur.is_empty() {
        return None;
    }
    if offs.len() != count || frn.len() != count || parent.len() != count || attrs.len() != count {
        debug_log(&format!(
            "load {guid:032x}: column len mismatch offs={} frn={} parent={} attrs={} count={count}",
            offs.len(), frn.len(), parent.len(), attrs.len()
        ));
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
        name_off,
        noise_enabled,
    );
    Some(LoadedSnapshot {
        header: SnapshotHeader {
            guid,
            letter,
            complete: flags & FLAG_COMPLETE != 0,
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
    use crate::store::VolumeStore;

    #[test]
    fn snapshot_roundtrip() {
        let mut s = VolumeStore::new(true);
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.insert(11, 5, 0, "报告.docx".as_bytes());
        s.insert(12, 5, 0x10, b"node_modules");
        s.insert(13, 12, 0, b"dep.txt");
        s.compact();
        let header = SnapshotHeader {
            guid: 0x1234_5678_9abc_def0_1122_3344,
            letter: 'C',
            complete: true,
            journal_id: 0xfeed,
            next_usn: 0x42,
            resume_frn: 0,
            est_records: 999,
        };
        let dir = std::env::temp_dir().join(format!("lstr-idx-test-{}", std::process::id()));
        let size = save(&dir, &header, &mut s).unwrap();
        assert!(size > 0);
        let loaded = load(&dir, header.guid, true).unwrap();
        assert_eq!(loaded.header.journal_id, 0xfeed);
        assert_eq!(loaded.header.next_usn, 0x42);
        assert!(loaded.header.complete);
        let ls = &loaded.store;
        assert_eq!(ls.name_off.len(), 3); // node_modules 子树被排除
        assert!(ls.find_slot(10).is_some());
        let slot = ls.find_slot(10).unwrap();
        assert_eq!(ls.name(slot), b"alpha.txt");
        assert_eq!(ls.offs.len(), ls.name_off.len());
        // 有序性保持
        let names: Vec<&[u8]> = ls.offs.iter().map(|&x| ls.name_lc(x)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("lstr-idx-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let guid = 42u128;
        let p = snapshot_path(&dir, guid);
        std::fs::write(&p, b"garbage-data-here").unwrap();
        assert!(load(&dir, guid, false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
