//! 旁车游标/增量日志(cursor sidecar):快照旁的小文件,承载「最近一次全量
//! 快照之后」的游标推进与少量变更,让稳态下的持久化不再整卷重写。
//!
//! 背景(2026-11 实测):USN 稳态下就绪卷每 ~60s 全量保存一次,C 卷 117 万条
//! 每次占引擎线程 1.6-2s(tmp+rename 全量写 + compact + 拼音重建),周期性
//! 把同线程上的查询堵停 1-3 秒。改为:
//! - 稳态变更按序记进引擎内存的旁车日志,由 [`crate::engine`] 以 ~5s 节流
//!   原子写进本文件(几 KB,毫秒级);
//! - 全量快照降频到 FULL_SAVE_INTERVAL(engine 侧常量)/对账收尾/优雅退出。
//!
//! 崩溃恢复语义(与既有对账路径自洽):
//! - 快照(tmp+rename 原子)永远自洽,游标 = header.next_usn;
//! - 旁车基线 `base` 必须与快照游标完全一致才回放:回放 ops 后 store 即为
//!   `next_usn` 时刻的状态(运行期 apply 与回放共用同一套逐条语义);
//! - 基线不符/文件损坏 → 丢弃旁车,从快照游标出发走 journal 增量回放
//!   (游标失效窗口内的记录仍在 NTFS journal 里);journal 已回绕 → dirsync。
//! 任何时刻断电都落在「完整快照+旁车」或「快照+journal 回放」两态之一,
//! 不存在半态:旁车写盘同样 tmp+rename+fsync,且全量保存成功后先清内存
//! 日志再删旁车文件(基线守卫使旧旁车自动作废,双保险)。
//!
//! 格式(小端):
//! ```text
//! magic "LCUR" | u32 version | u128 guid
//! u64 base_journal_id | i64 base_next_usn   // 所基于的全量快照游标
//! i64 next_usn                              // 回放 ops 后的游标(>= base_next_usn)
//! u32 n_ops
//! op: u8 tag
//!   1 = Journal  { u32 n + recs }                  // 逐条 upsert/remove(非对账语义)
//!   2 = Children { u64 parent_frn + u32 n + recs } // watcher 目录刷新(替换语义)
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::proto::{decode_recs, encode_recs};
use crate::usn::Rec;

pub const MAGIC: &[u8; 4] = b"LCUR";
pub const VERSION: u32 = 1;

const TAG_JOURNAL: u8 = 1;
const TAG_CHILDREN: u8 = 2;

/// 一条旁车操作:与引擎运行期的 apply 语义一一对应,回放即恢复。
#[derive(Debug, Clone, PartialEq)]
pub enum SidecarOp {
    /// journal 批次(Records 帧,非对账路径):逐条 Upsert=insert_meta /
    /// Remove=打 DEAD 位,与 `engine::apply_rec(syncing=false)` 一致。
    Journal { recs: Vec<Rec> },
    /// watcher 目录刷新(DirChildren 帧):`parent_frn` 存 FRN 而非 slot,
    /// 回放时在加载后的 store 里重新解析(compact 后 slot 会变)。
    /// ROOT_FRN 走 apply_root_children;父解析失败时只 upsert 不删(宁多勿错删)。
    Children { parent_frn: u64, recs: Vec<Rec> },
}

impl SidecarOp {
    /// 变更记录数(溢出判断/诊断用)。
    pub fn recs_len(&self) -> usize {
        match self {
            SidecarOp::Journal { recs } | SidecarOp::Children { recs, .. } => recs.len(),
        }
    }
}

/// 读回的旁车文件内容。`base` 是该日志所基于的全量快照游标,加载方(引擎)
/// 必须校验它与快照 header 的 (journal_id, next_usn) 完全一致后才回放。
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarFile {
    pub guid: u128,
    pub base: (u64, i64),
    pub next_usn: i64,
    pub ops: Vec<SidecarOp>,
}

pub fn path(dir: &Path, guid: u128) -> PathBuf {
    dir.join(format!("cur_{guid:032x}.bin"))
}

fn tmp_path(dir: &Path, guid: u128) -> PathBuf {
    dir.join(format!("cur_{guid:032x}.tmp"))
}

/// 原子写旁车(tmp + fsync + rename + 目录 flush),与快照同一套崩溃安全纪律。
/// 空日志也允许写(游标推进但无条目级变更的极端情形)。
pub fn save(
    dir: &Path,
    guid: u128,
    base: (u64, i64),
    next_usn: i64,
    ops: &[SidecarOp],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut payload: Vec<u8> = Vec::with_capacity(64 + ops.len() * 48);
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&VERSION.to_le_bytes());
    payload.extend_from_slice(&guid.to_le_bytes());
    payload.extend_from_slice(&base.0.to_le_bytes());
    payload.extend_from_slice(&base.1.to_le_bytes());
    payload.extend_from_slice(&next_usn.to_le_bytes());
    payload.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    for op in ops {
        match op {
            SidecarOp::Journal { recs } => {
                payload.push(TAG_JOURNAL);
                payload.extend_from_slice(&encode_recs(recs));
            }
            SidecarOp::Children { parent_frn, recs } => {
                payload.push(TAG_CHILDREN);
                payload.extend_from_slice(&parent_frn.to_le_bytes());
                payload.extend_from_slice(&encode_recs(recs));
            }
        }
    }
    let tmp = tmp_path(dir, guid);
    let f = std::fs::File::create(&tmp)?;
    {
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(&payload)?;
        w.flush()?;
    }
    f.sync_all()?;
    drop(f);
    let final_path = path(dir, guid);
    std::fs::rename(&tmp, &final_path)?;
    if let Some(parent) = final_path.parent() {
        if !crate::win::flush_dir(parent) {
            crate::snapshot::debug_log(&format!(
                "sidecar: flush_dir {} failed, dir entry may be non-durable",
                parent.display()
            ));
        }
    }
    Ok(())
}

/// 读旁车。
/// - `Ok(None)`:文件不存在(从未写过/已被全量保存删除);
/// - `Ok(Some(..))`:内容完整有效(基线是否匹配由调用方对照快照 header 判断);
/// - `Err(())`:文件存在但损坏/版本不符/字段非法 —— 调用方应删除,
///   恢复路径回落到「快照游标 + journal 回放」。
pub fn load(dir: &Path, guid: u128) -> Result<Option<SidecarFile>, ()> {
    let p = path(dir, guid);
    let mut data = Vec::new();
    let mut f = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if f.read_to_end(&mut data).is_err() {
        return Err(());
    }
    match parse(&data, guid) {
        Some(sc) => Ok(Some(sc)),
        None => Err(()),
    }
}

/// 从缓冲头部取 n 字节并把游标前移;不足 n 字节返回 None(= 损坏/截断)。
fn take<'a>(cur: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if cur.len() < n {
        return None;
    }
    let (head, rest) = cur.split_at(n);
    *cur = rest;
    Some(head)
}

/// 严格解析;任何不完整/不一致都返回 None(= 损坏)。
fn parse(data: &[u8], guid: u128) -> Option<SidecarFile> {
    let mut cur: &[u8] = data;
    let head = take(&mut cur, 4 + 4 + 16 + 8 + 8 + 8 + 4)?;
    if &head[0..4] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(head[4..8].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let guid_h = u128::from_le_bytes(head[8..24].try_into().ok()?);
    if guid_h != guid {
        return None;
    }
    let base_jid = u64::from_le_bytes(head[24..32].try_into().ok()?);
    let base_usn = i64::from_le_bytes(head[32..40].try_into().ok()?);
    let next_usn = i64::from_le_bytes(head[40..48].try_into().ok()?);
    let n_ops = u32::from_le_bytes(head[48..52].try_into().ok()?) as usize;
    if next_usn < base_usn {
        return None;
    }
    let mut ops = Vec::with_capacity(n_ops.min(1 << 16));
    for _ in 0..n_ops {
        let tag = take(&mut cur, 1)?[0];
        match tag {
            TAG_JOURNAL => {
                let len = recs_block_len(cur)?;
                let body = take(&mut cur, len)?;
                ops.push(SidecarOp::Journal { recs: decode_recs(body)? });
            }
            TAG_CHILDREN => {
                let parent_frn = u64::from_le_bytes(take(&mut cur, 8)?.try_into().ok()?);
                let len = recs_block_len(cur)?;
                let body = take(&mut cur, len)?;
                ops.push(SidecarOp::Children { parent_frn, recs: decode_recs(body)? });
            }
            _ => return None,
        }
    }
    if !cur.is_empty() {
        return None;
    }
    Some(SidecarFile { guid, base: (base_jid, base_usn), next_usn, ops })
}

/// 一个 rec 块(4 字节计数 + 逐条 rec)的总字节数;逐条走一遍长度校验。
/// 布局:op(1) + attrs(4) + frn(8) + parent(8) + mtime(8) + name_len(2) + name。
/// 返回 None = 声称的条数与剩余字节矛盾(损坏)。
fn recs_block_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 {
        return None;
    }
    let n = u32::from_le_bytes(bytes[..4].try_into().ok()?) as usize;
    let mut rest = &bytes[4..];
    let mut total = 4usize;
    for _ in 0..n {
        if rest.len() < 31 {
            return None;
        }
        let name_len = u16::from_le_bytes(rest[29..31].try_into().ok()?) as usize;
        if rest.len() < 31 + name_len {
            return None;
        }
        rest = &rest[31 + name_len..];
        total += 31 + name_len;
    }
    Some(total)
}

/// 删除旁车文件(全量保存成功后/基线失效时)。不存在时静默成功。
pub fn remove(dir: &Path, guid: u128) {
    let _ = std::fs::remove_file(path(dir, guid));
    let _ = std::fs::remove_file(tmp_path(dir, guid));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::Op;

    fn tmpdir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lstr-sidecar-{tag}-{}", std::process::id()))
    }

    fn rec(frn: u64, parent: u64, name: &str, op: Op) -> Rec {
        Rec {
            frn,
            parent_frn: parent,
            attrs: if name.ends_with('/') { 0x10 } else { 0 },
            op,
            name: name.trim_end_matches('/').to_string(),
            mtime: 1_700_000_000,
        }
    }

    fn sample_ops() -> Vec<SidecarOp> {
        vec![
            SidecarOp::Journal {
                recs: vec![
                    rec(11, 10, "alpha.txt", Op::Upsert),
                    Rec { frn: 12, parent_frn: 10, attrs: 0, op: Op::Remove, name: String::new(), mtime: 0 },
                ],
            },
            SidecarOp::Children { parent_frn: crate::store::ROOT_FRN, recs: vec![rec(20, 5, "root-new.txt", Op::Upsert)] },
            SidecarOp::Journal { recs: vec![rec(30, 10, "中文-改.txt", Op::Upsert)] },
        ]
    }

    #[test]
    fn sidecar_roundtrip() {
        let dir = tmpdir("rt");
        let _ = std::fs::remove_dir_all(&dir);
        let ops = sample_ops();
        save(&dir, 0xa11c, (7, 100), 300, &ops).unwrap();
        let sc = load(&dir, 0xa11c).unwrap().expect("file exists");
        assert_eq!(sc.guid, 0xa11c);
        assert_eq!(sc.base, (7, 100));
        assert_eq!(sc.next_usn, 300);
        assert_eq!(sc.ops, ops);
        assert_eq!(sc.ops.iter().map(|o| o.recs_len()).sum::<usize>(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sidecar_is_ok_none() {
        let dir = tmpdir("missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir, 0x1), Ok(None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_sidecar_is_err() {
        let dir = tmpdir("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 非法魔数
        std::fs::write(path(&dir, 0x2), b"XXXX-garbage").unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        // 魔数对但截断
        save(&dir, 0x2, (7, 100), 300, &sample_ops()).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x2)).unwrap();
        bytes.truncate(30);
        std::fs::write(path(&dir, 0x2), &bytes).unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        // 尾部多字节
        save(&dir, 0x2, (7, 100), 300, &sample_ops()).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x2)).unwrap();
        bytes.push(0xff);
        std::fs::write(path(&dir, 0x2), &bytes).unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        // 未知 op tag
        save(&dir, 0x2, (7, 100), 300, &sample_ops()).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x2)).unwrap();
        let at = 4 + 4 + 16 + 8 + 8 + 8 + 4; // 头部之后第一个 op tag
        bytes[at] = 0x7f;
        std::fs::write(path(&dir, 0x2), &bytes).unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        // 版本不符
        save(&dir, 0x2, (7, 100), 300, &sample_ops()).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x2)).unwrap();
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        std::fs::write(path(&dir, 0x2), &bytes).unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        // guid 不符
        save(&dir, 0x2, (7, 100), 300, &sample_ops()).unwrap();
        let bytes = std::fs::read(path(&dir, 0x2)).unwrap();
        std::fs::write(path(&dir, 0x3), &bytes).unwrap();
        assert_eq!(load(&dir, 0x3), Err(()));
        // next_usn < base_usn
        save(&dir, 0x2, (7, 500), 300, &sample_ops()).unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_log_roundtrip_and_remove() {
        let dir = tmpdir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        save(&dir, 0x9, (3, 42), 42, &[]).unwrap();
        let sc = load(&dir, 0x9).unwrap().unwrap();
        assert_eq!(sc.next_usn, 42);
        assert!(sc.ops.is_empty());
        remove(&dir, 0x9);
        assert_eq!(load(&dir, 0x9), Ok(None));
        // remove 幂等(文件不存在也 OK)
        remove(&dir, 0x9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
