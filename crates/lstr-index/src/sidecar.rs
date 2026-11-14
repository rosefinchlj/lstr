//! 旁车游标/增量日志(cursor sidecar):快照旁的小文件,承载「最近一次全量
//! 快照之后」的游标推进与变更,让稳态下的持久化不再整卷重写。
//!
//! 背景(2026-11 实测):USN 稳态下就绪卷每 ~60s 全量保存一次,C 卷 117 万条
//! 每次占引擎线程 1.6-2s(tmp+rename 全量写 + compact + 拼音重建),周期性
//! 把同线程上的查询堵停 1-3 秒。改为:
//! - 稳态变更按序记进引擎内存的旁车日志,由 [`crate::engine`] 以 ~5s 节流
//!   **追加**写进本文件(只写新增的一小块,几 KB);
//! - 全量快照降频到 FULL_SAVE_INTERVAL(engine 侧常量)/对账收尾/优雅退出。
//!
//! 追加式(2026-11 二次改造):旧实现每次把「自快照以来的全部变更」重写一遍,
//! C 卷日志长到 1.6MB 时等于每 5s 写 1.6MB(实测日写入量与全量快照同量级)。
//! 现在文件只有「头 + 若干追加块」,写盘量 = 新增记录本身;块自带长度与校验,
//! 断电撕裂的尾块在加载时被丢弃并就地截断,不会污染后续追加。
//!
//! 崩溃恢复语义(与既有对账路径自洽):
//! - 快照(tmp+rename 原子)永远自洽,游标 = header.next_usn;
//! - 旁车基线 `base` 必须与快照游标完全一致才回放:回放 ops 后 store 即为
//!   `next_usn` 时刻的状态(运行期 apply 与回放共用同一套逐条语义);
//! - 基线不符/头部损坏 → 丢弃旁车,从快照游标出发走 journal 增量回放
//!   (游标失效窗口内的记录仍在 NTFS journal 里);journal 已回绕 → dirsync;
//! - 尾块撕裂(写一半断电)→ 只回放完好前缀,文件截断到该前缀末尾,
//!   缺口同样由 journal 回放补齐。
//!
//! 格式(小端):
//! ```text
//! 头(40B): "LCUR" | u32 version | u128 guid
//!          | u64 base_journal_id | i64 base_next_usn   // 所基于的全量快照游标
//! 块(24B 头 + 载荷):
//!   "LBAT" | u32 n_ops | i64 end_usn | u32 payload_len | u32 fnv1a(payload)
//!   payload: op*
//!     op: u8 tag
//!       1 = Journal  { u32 n + recs }                  // 逐条 upsert/remove(非对账语义)
//!       2 = Children { u64 parent_frn + u32 n + recs } // watcher 目录刷新(替换语义)
//! ```

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::proto::{decode_recs, encode_recs};
use crate::usn::Rec;

pub const MAGIC: &[u8; 4] = b"LCUR";
/// v3:追加块格式(头 40B + "LBAT" 块序列)。v2(一次性整文件重写)不兼容,
/// 加载时丢弃并从快照游标走 journal 回放(不会丢数据)。
pub const VERSION: u32 = 3;

const TAG_JOURNAL: u8 = 1;
const TAG_CHILDREN: u8 = 2;

const BLOCK_MAGIC: &[u8; 4] = b"LBAT";
/// "LCUR" | u32 | u128 | u64 | i64
const HEADER_LEN: usize = 4 + 4 + 16 + 8 + 8;
/// "LBAT" | u32 n_ops | i64 end_usn | u32 payload_len | u32 checksum
const BLOCK_HEADER_LEN: usize = 4 + 4 + 8 + 4 + 4;

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

fn header_bytes(guid: u128, base: (u64, i64)) -> [u8; HEADER_LEN] {
    let mut head = [0u8; HEADER_LEN];
    head[0..4].copy_from_slice(MAGIC);
    head[4..8].copy_from_slice(&VERSION.to_le_bytes());
    head[8..24].copy_from_slice(&guid.to_le_bytes());
    head[24..32].copy_from_slice(&base.0.to_le_bytes());
    head[32..40].copy_from_slice(&base.1.to_le_bytes());
    head
}

/// 头是否属于「同一卷 + 同一快照基线」。只比 magic/version/guid/base,
/// 块内容由 [`load`] 严格校验。追加前据此判断要不要整文件重建。
fn header_matches(head: &[u8], guid: u128, base: (u64, i64)) -> bool {
    head.len() == HEADER_LEN
        && &head[0..4] == MAGIC
        && u32::from_le_bytes(head[4..8].try_into().unwrap()) == VERSION
        && u128::from_le_bytes(head[8..24].try_into().unwrap()) == guid
        && u64::from_le_bytes(head[24..32].try_into().unwrap()) == base.0
        && i64::from_le_bytes(head[32..40].try_into().unwrap()) == base.1
}

/// FNV-1a(32 位):块载荷的轻量校验。只防撕裂/位翻转,不防篡改。
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// 把一批 op 编成载荷(块体)。
fn encode_ops(ops: &[SidecarOp]) -> Vec<u8> {
    let mut payload: Vec<u8> = Vec::with_capacity(64 + ops.len() * 48);
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
    payload
}

/// 拼一个完整块(块头 + 载荷)。
fn encode_block(next_usn: i64, ops: &[SidecarOp]) -> Vec<u8> {
    let payload = encode_ops(ops);
    let mut block = Vec::with_capacity(BLOCK_HEADER_LEN + payload.len());
    block.extend_from_slice(BLOCK_MAGIC);
    block.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    block.extend_from_slice(&next_usn.to_le_bytes());
    block.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    block.extend_from_slice(&fnv1a(&payload).to_le_bytes());
    block.extend_from_slice(&payload);
    block
}

/// 全量重写旁车(tmp + fsync + rename + 目录 flush),与快照同一套崩溃安全纪律。
/// 只用于「重建」(基线变更/追加前置状态未知/测试):稳态写盘走 [`append`]。
pub fn save(
    dir: &Path,
    guid: u128,
    base: (u64, i64),
    next_usn: i64,
    ops: &[SidecarOp],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = tmp_path(dir, guid);
    let f = std::fs::File::create(&tmp)?;
    {
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(&header_bytes(guid, base))?;
        w.write_all(&encode_block(next_usn, ops))?;
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

/// 追加一批变更(稳态唯一写盘路径):文件不存在/长度不足/基线不符时先写头
/// (基线不符 = 旧快照的旁车,整文件重建)。写失败时把文件截回追加前长度,
/// 不留下半块给下次加载。
pub fn append(
    dir: &Path,
    guid: u128,
    base: (u64, i64),
    next_usn: i64,
    ops: &[SidecarOp],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let final_path = path(dir, guid);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&final_path)?;
    let mut created = false;
    let mut len = f.metadata()?.len();
    if len < HEADER_LEN as u64 {
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header_bytes(guid, base))?;
        len = HEADER_LEN as u64;
        created = true;
    } else {
        let mut head = [0u8; HEADER_LEN];
        f.seek(SeekFrom::Start(0))?;
        f.read_exact(&mut head)?;
        if !header_matches(&head, guid, base) {
            crate::snapshot::debug_log(&format!(
                "sidecar {guid:032x}: header base/guid mismatch on append, rebuilt"
            ));
            f.set_len(0)?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&header_bytes(guid, base))?;
            len = HEADER_LEN as u64;
            created = true;
        }
    }
    let block = encode_block(next_usn, ops);
    f.seek(SeekFrom::Start(len))?;
    if let Err(err) = f.write_all(&block) {
        let _ = f.set_len(len); // 截回追加前,别把半块留给下次加载
        return Err(err);
    }
    if let Err(err) = f.sync_data() {
        let _ = f.set_len(len);
        return Err(err);
    }
    drop(f);
    // 新建文件要 flush 父目录,条目本身才持久(appends 复用已存在的目录项)。
    if created {
        let _ = crate::win::flush_dir(dir);
    }
    Ok(())
}

/// 读旁车。
/// - `Ok(None)`:文件不存在(从未写过/已被全量保存删除);
/// - `Ok(Some(..))`:头合法(基线是否匹配由调用方对照快照 header 判断);
///   尾部撕裂/校验失败的块被丢弃,文件**就地截断**到最后一个完好块末尾,
///   保证后续 [`append`] 不会排到垃圾后面;
/// - `Err(())`:头损坏/版本不符/guid 不符/游标非法 —— 调用方应删除,
///   恢复路径回落到「快照游标 + journal 回放」。
pub fn load(dir: &Path, guid: u128) -> Result<Option<SidecarFile>, ()> {
    let p = path(dir, guid);
    let data = match std::fs::read(&p) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if data.len() < HEADER_LEN {
        return Err(());
    }
    let head = &data[..HEADER_LEN];
    if &head[0..4] != MAGIC {
        return Err(());
    }
    let version = u32::from_le_bytes(head[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(());
    }
    let guid_h = u128::from_le_bytes(head[8..24].try_into().unwrap());
    if guid_h != guid {
        return Err(());
    }
    let base = (
        u64::from_le_bytes(head[24..32].try_into().unwrap()),
        i64::from_le_bytes(head[32..40].try_into().unwrap()),
    );
    let mut next_usn = base.1;
    let mut ops: Vec<SidecarOp> = Vec::new();
    let mut pos = HEADER_LEN;
    while pos < data.len() {
        let rest = &data[pos..];
        if rest.len() < BLOCK_HEADER_LEN || &rest[0..4] != BLOCK_MAGIC {
            break; // 撕裂/未知尾块:停在此处
        }
        let n_ops = u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize;
        let end_usn = i64::from_le_bytes(rest[8..16].try_into().unwrap());
        let payload_len = u32::from_le_bytes(rest[16..20].try_into().unwrap()) as usize;
        let checksum = u32::from_le_bytes(rest[20..24].try_into().unwrap());
        if rest.len() - BLOCK_HEADER_LEN < payload_len {
            break; // 载荷没写完
        }
        let payload = &rest[BLOCK_HEADER_LEN..BLOCK_HEADER_LEN + payload_len];
        if fnv1a(payload) != checksum {
            break; // 位翻转:按撕裂尾处理,只保留完好前缀
        }
        if end_usn < next_usn {
            return Err(()); // 校验通过但游标回退 = 文件不属于这条时间线
        }
        let Some(block_ops) = decode_ops(payload, n_ops) else {
            break;
        };
        ops.extend(block_ops);
        next_usn = end_usn;
        pos += BLOCK_HEADER_LEN + payload_len;
    }
    if pos != data.len() {
        crate::snapshot::debug_log(&format!(
            "sidecar {guid:032x}: torn tail {} bytes dropped (valid prefix {pos})",
            data.len() - pos
        ));
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) {
            let _ = f.set_len(pos as u64);
        }
    }
    if next_usn < base.1 {
        return Err(());
    }
    Ok(Some(SidecarFile { guid, base, next_usn, ops }))
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

/// 解析块载荷里的 n_ops 条 op;必须恰好消费完载荷,否则视为损坏。
fn decode_ops(mut cur: &[u8], n_ops: usize) -> Option<Vec<SidecarOp>> {
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
    Some(ops)
}

/// 一个 rec 块(4 字节计数 + 逐条 rec)的总字节数;逐条走一遍长度校验。
/// 布局:op(1) + attrs(4) + frn(8) + parent(8) + mtime(8) + size(8) +
/// name_len(2) + name。返回 None = 声称的条数与剩余字节矛盾(损坏)。
fn recs_block_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 {
        return None;
    }
    let n = u32::from_le_bytes(bytes[..4].try_into().ok()?) as usize;
    let mut rest = &bytes[4..];
    let mut total = 4usize;
    for _ in 0..n {
        if rest.len() < 39 {
            return None;
        }
        let name_len = u16::from_le_bytes(rest[37..39].try_into().ok()?) as usize;
        if rest.len() < 39 + name_len {
            return None;
        }
        rest = &rest[39 + name_len..];
        total += 39 + name_len;
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
            size: u64::MAX,
        }
    }

    fn sample_ops() -> Vec<SidecarOp> {
        vec![
            SidecarOp::Journal {
                recs: vec![
                    rec(11, 10, "alpha.txt", Op::Upsert),
                    Rec { frn: 12, parent_frn: 10, attrs: 0, op: Op::Remove, name: String::new(), mtime: 0, size: u64::MAX },
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
        // 头被截断
        save(&dir, 0x2, (7, 100), 300, &sample_ops()).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x2)).unwrap();
        bytes.truncate(30);
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
        // 块游标回退(头 base_usn 500 > 块 end_usn 300)
        save(&dir, 0x2, (7, 500), 300, &sample_ops()).unwrap();
        assert_eq!(load(&dir, 0x2), Err(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_is_dropped_and_file_truncated() {
        let dir = tmpdir("torn");
        let _ = std::fs::remove_dir_all(&dir);
        let ops = sample_ops();
        append(&dir, 0x5, (7, 100), 200, &ops[..1]).unwrap();
        append(&dir, 0x5, (7, 100), 300, &ops[1..2]).unwrap();
        let valid_len = std::fs::read(path(&dir, 0x5)).unwrap().len();
        // 第三块只写一半:截到「块头 + 半个载荷」
        append(&dir, 0x5, (7, 100), 400, &ops[2..]).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x5)).unwrap();
        bytes.truncate(valid_len + BLOCK_HEADER_LEN + 3);
        std::fs::write(path(&dir, 0x5), &bytes).unwrap();
        let sc = load(&dir, 0x5).unwrap().unwrap();
        assert_eq!(sc.next_usn, 300, "撕裂块之前的游标");
        assert_eq!(sc.ops.len(), 2, "撕裂块被丢弃");
        assert_eq!(
            std::fs::read(path(&dir, 0x5)).unwrap().len(),
            valid_len,
            "撕裂尾部应被就地截掉"
        );
        // 修复后追加不会排在垃圾后面
        append(&dir, 0x5, (7, 100), 400, &ops[2..]).unwrap();
        let sc = load(&dir, 0x5).unwrap().unwrap();
        assert_eq!(sc.next_usn, 400);
        assert_eq!(sc.ops.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_accumulates_blocks() {
        let dir = tmpdir("append");
        let _ = std::fs::remove_dir_all(&dir);
        let ops = sample_ops();
        append(&dir, 0x7, (1, 10), 20, &ops[..1]).unwrap();
        append(&dir, 0x7, (1, 10), 30, &ops[1..2]).unwrap();
        append(&dir, 0x7, (1, 10), 40, &ops[2..]).unwrap();
        let sc = load(&dir, 0x7).unwrap().unwrap();
        assert_eq!(sc.base, (1, 10));
        assert_eq!(sc.next_usn, 40);
        assert_eq!(sc.ops, ops);
        // 追加只增长文件,历史块字节原样保留
        let before = std::fs::read(path(&dir, 0x7)).unwrap();
        append(&dir, 0x7, (1, 10), 50, &ops[..1]).unwrap();
        let after = std::fs::read(path(&dir, 0x7)).unwrap();
        assert!(after.starts_with(&before), "历史块字节必须原样保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_rebuilds_on_base_mismatch() {
        let dir = tmpdir("rebase");
        let _ = std::fs::remove_dir_all(&dir);
        let ops = sample_ops();
        append(&dir, 0x8, (1, 10), 20, &ops[..1]).unwrap();
        // 新快照基线不同:旧块整文件重建,只保留新基线之后的内容
        append(&dir, 0x8, (2, 500), 600, &ops[1..2]).unwrap();
        let sc = load(&dir, 0x8).unwrap().unwrap();
        assert_eq!(sc.base, (2, 500));
        assert_eq!(sc.next_usn, 600);
        assert_eq!(sc.ops, ops[1..2].to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_checksum_stops_at_block() {
        let dir = tmpdir("crc");
        let _ = std::fs::remove_dir_all(&dir);
        let ops = sample_ops();
        append(&dir, 0x9, (1, 10), 20, &ops[..1]).unwrap();
        let first_len = std::fs::read(path(&dir, 0x9)).unwrap().len();
        append(&dir, 0x9, (1, 10), 30, &ops[1..2]).unwrap();
        let mut bytes = std::fs::read(path(&dir, 0x9)).unwrap();
        // 翻转第二个块载荷里的一个字节
        bytes[first_len + BLOCK_HEADER_LEN] ^= 0xff;
        std::fs::write(path(&dir, 0x9), &bytes).unwrap();
        let sc = load(&dir, 0x9).unwrap().unwrap();
        assert_eq!(sc.next_usn, 20, "校验失败块之前的游标");
        assert_eq!(sc.ops.len(), 1);
        assert_eq!(std::fs::read(path(&dir, 0x9)).unwrap().len(), first_len);
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
