//! USN 记录解析(V2)与 reason 位。
//!
//! USN_RECORD_V2 布局(winioctl.h):
//! ```text
//! 0  u32 RecordLength
//! 4  u16 MajorVersion / u16 MinorVersion
//! 8  u64 FileReferenceNumber
//! 16 u64 ParentFileReferenceNumber
//! 24 i64 Usn
//! 32 i64 TimeStamp      ← 日志事件时间,不是文件 mtime!
//! 40 u32 Reason
//! 44 u32 SourceInfo
//! 48 u32 SecurityId
//! 52 u32 FileAttributes
//! 56 u16 FileNameLength / u16 FileNameOffset (=60)
//! 60 WCHAR[] FileName(变长,无 \0)
//! ```
//! 记录按 8 字节对齐,变长字段一律 try_into + from_le_bytes 读,
//! 不做指针转型(规避 unaligned UB)。

pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// 名字/存在性会变的 reason 集合,其余(DATA_* 等)忽略。
///
/// H3:`RENAME_OLD_NAME` **不**在此列 —— 改名是「原地更新」,旧名记录若翻译成
/// Remove 会把 slot 打成 DEAD,子项 parent 指向死槽、路径退化。旧名记录只在
/// 同一条记录同时带 NEW_NAME 时才随 Upsert 一起生效。
pub const REASON_LIFECYCLE: u32 =
    USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// journal 出错原因(journal 回收/删除 → 该卷需重扫)。
pub const ERROR_JOURNAL_ENTRY_DELETED: i32 = 0x0000_019D;
pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 0x0000_019E;
pub const ERROR_JOURNAL_NOT_ACTIVE: i32 = 0x0000_019C;

/// 引擎侧的记录操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// 新建或改名/改父(同 FRN 已存在则视为变更)。
    Upsert,
    /// 删除。
    Remove,
}

/// 一条引擎记录(已从 USN 原始记录翻译)。
#[derive(Debug, Clone, PartialEq)]
pub struct Rec {
    pub frn: u64,
    pub parent_frn: u64,
    pub attrs: u32,
    pub op: Op,
    pub name: String,
    /// 最后修改时间(unix 秒;0 = 未知)。只有目录枚举(dir scan)能拿到,
    /// USN 记录本身不含时间 —— 内置时间筛选依赖它。
    pub mtime: i64,
}

/// 解析一段 USN_RECORD_V2 缓冲(ENUM 输出从 offset 8 起,journal 输出同构)。
/// 返回 (记录, 消费字节数)。V3/V4 记录跳过并计数。
pub fn parse_records(buf: &[u8]) -> (Vec<RawRecord>, usize, usize) {
    let mut recs = Vec::new();
    let mut off = 0usize;
    let mut skipped = 0usize;
    while off + 60 <= buf.len() {
        let rec_len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if rec_len < 60 || off + rec_len > buf.len() {
            break;
        }
        let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
        if major == 2 {
            let frn = u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
            let parent = u64::from_le_bytes(buf[off + 16..off + 24].try_into().unwrap());
            let reason = u32::from_le_bytes(buf[off + 40..off + 44].try_into().unwrap());
            let attrs = u32::from_le_bytes(buf[off + 52..off + 56].try_into().unwrap());
            let name_len =
                u16::from_le_bytes(buf[off + 56..off + 58].try_into().unwrap()) as usize;
            let name_off =
                u16::from_le_bytes(buf[off + 58..off + 60].try_into().unwrap()) as usize;
            let name = if name_len > 0 && off + name_off + name_len <= off + rec_len {
                let u16s: Vec<u16> = buf[off + name_off..off + name_off + name_len]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                String::from_utf16_lossy(&u16s)
            } else {
                String::new()
            };
            recs.push(RawRecord { frn, parent, attrs, reason, name });
        } else {
            skipped += 1;
        }
        off += rec_len;
    }
    (recs, off, skipped)
}

/// 未翻译的原始 USN 记录(扫描/journal 共用)。
pub struct RawRecord {
    pub frn: u64,
    pub parent: u64,
    pub attrs: u32,
    pub reason: u32,
    pub name: String,
}

impl RawRecord {
    /// 翻译为引擎操作。返回 None = 与名字索引无关(纯内容写、CLOSE 等)。
    pub fn to_engine_op(&self) -> Option<Rec> {
        if self.name.is_empty() {
            return None;
        }
        // 排除元数据文件($MFT 等):FRN 低 48 位 < 0x100 且 $ 开头
        let mfrn = self.frn & 0x0000_FFFF_FFFF_FFFF;
        if mfrn < 0x100 && self.name.starts_with('$') {
            return None;
        }
        let op = if self.reason & USN_REASON_RENAME_NEW_NAME != 0
            || self.reason & USN_REASON_FILE_CREATE != 0
        {
            Op::Upsert
        } else if self.reason & USN_REASON_FILE_DELETE != 0 {
            Op::Remove
        } else {
            // H3:RENAME_OLD_NAME 单独出现时忽略 —— 新名记录(同一改名操作的
            // 后一条 USN)会以 Upsert 原地更新 slot,slot 身份保持不变。
            return None;
        };
        Some(Rec {
            frn: self.frn,
            parent_frn: self.parent,
            attrs: self.attrs,
            op,
            name: self.name.clone(),
            mtime: 0,
        })
    }
}

/// 扫描记录(ENUM_USN_DATA):所有记录都是 Upsert。
pub fn scan_records(buf: &[u8]) -> (Vec<Rec>, usize) {
    let (raw, consumed, _skipped) = parse_records(buf);
    let recs = raw
        .into_iter()
        .filter(|r| !r.name.is_empty())
        .filter(|r| {
            let mfrn = r.frn & 0x0000_FFFF_FFFF_FFFF;
            !(mfrn < 0x100 && r.name.starts_with('$'))
        })
        .map(|r| Rec {
            frn: r.frn,
            parent_frn: r.parent,
            attrs: r.attrs,
            op: Op::Upsert,
            name: r.name,
            mtime: 0,
        })
        .collect();
    (recs, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工构造一条 V2 记录。
    fn build_v2(frn: u64, parent: u64, attrs: u32, reason: u32, name: &str) -> Vec<u8> {
        let name_u16: Vec<u8> = name
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let rec_len = 60 + name_u16.len();
        let mut buf = vec![0u8; rec_len];
        buf[0..4].copy_from_slice(&(rec_len as u32).to_le_bytes());
        buf[4..6].copy_from_slice(&2u16.to_le_bytes());
        buf[6..8].copy_from_slice(&0u16.to_le_bytes());
        buf[8..16].copy_from_slice(&frn.to_le_bytes());
        buf[16..24].copy_from_slice(&parent.to_le_bytes());
        buf[40..44].copy_from_slice(&reason.to_le_bytes());
        buf[52..56].copy_from_slice(&attrs.to_le_bytes());
        buf[56..58].copy_from_slice(&(name_u16.len() as u16).to_le_bytes());
        buf[58..60].copy_from_slice(&60u16.to_le_bytes());
        buf[60..].copy_from_slice(&name_u16);
        buf
    }

    #[test]
    fn parses_v2_record() {
        let mut buf = build_v2(0x0100_0000_0011, 5, 0x20, USN_REASON_FILE_CREATE, "hello.txt");
        // 前面垫 8 字节(ENUM 的 resume FRN 位)
        let mut framed = vec![0u8; 8];
        framed.append(&mut buf);
        let (recs, consumed, skipped) = parse_records(&framed[8..]);
        assert_eq!(skipped, 0);
        assert_eq!(consumed, 60 + 18); // "hello.txt" 9 chars × 2
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].frn, 0x0100_0000_0011);
        assert_eq!(recs[0].name, "hello.txt");
        assert_eq!(recs[0].attrs, 0x20);
    }

    #[test]
    fn lifecycle_translation() {
        let mk = |reason: u32, name: &str| {
            RawRecord {
                frn: 100,
                parent: 5,
                attrs: 0,
                reason,
                name: name.into(),
            }
        };
        assert_eq!(mk(USN_REASON_FILE_CREATE, "a").to_engine_op().unwrap().op, Op::Upsert);
        assert_eq!(
            mk(USN_REASON_RENAME_NEW_NAME, "a").to_engine_op().unwrap().op,
            Op::Upsert
        );
        assert_eq!(mk(USN_REASON_FILE_DELETE, "a").to_engine_op().unwrap().op, Op::Remove);
        // H3:旧名记录不再翻译成 Remove(否则改名=删+建,子项指向死槽)
        assert!(mk(USN_REASON_RENAME_OLD_NAME, "a").to_engine_op().is_none());
        // 同一条记录同时带 OLD|NEW(部分实现合并上报):仍是 Upsert
        assert_eq!(
            mk(USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME, "a")
                .to_engine_op()
                .unwrap()
                .op,
            Op::Upsert
        );
        // 纯内容写 / CLOSE:忽略
        assert!(mk(USN_REASON_DATA_EXTEND | USN_REASON_CLOSE, "a").to_engine_op().is_none());
        // 元数据文件:忽略
        assert!(
            mk(USN_REASON_FILE_CREATE, "$MFT")
                .to_engine_op()
                .is_none()
        );
    }

    #[test]
    fn scan_records_all_upsert_and_meta_filtered() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_v2(10, 5, 0, 0, "a.txt"));
        buf.extend_from_slice(&build_v2(0x10, 5, 0, 0, "$MFT"));
        let (recs, _) = scan_records(&buf);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].op, Op::Upsert);
        assert_eq!(recs[0].name, "a.txt");
    }
}
