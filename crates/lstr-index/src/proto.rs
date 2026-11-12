//! 管道帧协议:u32 长度前缀 | u8 类型 | 载荷(全部小端)。
//!
//! C→W:Hello(握手,带每卷续传位点)/ Rescan / Shutdown / SetVolumes
//! W→C:Welcome / VolumeMeta / ScanBegin / ScanProgress / Batch / VolumeReady
//!      / JournalReset / VolumeGone / VolumeAppeared / WorkerExit

use crate::usn::{Op, Rec};
use crate::usnwatch::UsnWatchState;
use crate::PROTO_VERSION;

pub mod frame_type {
    // C→W
    pub const HELLO: u8 = 1;
    pub const RESCAN: u8 = 12;
    pub const SHUTDOWN: u8 = 13;
    /// 运行期对账开始:引擎已有该卷快照,worker 在后台重扫并逐批 patch,
    /// 结束后删除本轮没见到的条目(离线期间被删/移走)。
    pub const DIR_SYNC_BEGIN: u8 = 14;
    /// 运行期对账结束:引擎删除未标记 SEEN 的槽位并 compact/落盘。
    pub const DIR_SYNC_END: u8 = 15;
    /// watcher 目录刷新:该父目录的最新子项集合。
    pub const DIR_CHILDREN: u8 = 16;
    /// 运行期切换索引卷:载荷 u32 位掩码(bit i = 盘符 `'A' + i`,0 = 未设置)。
    /// worker 只更新内存掩码,卷线程起停交给会话循环既有的 500ms 热插拔差异。
    pub const SET_VOLUMES: u8 = 17;
    /// USN 实时增量层每卷状态上报(usn 后端):Off/Watching/Backoff/Disabled。
    pub const USN_WATCH: u8 = 18;
    // W→C
    pub const WELCOME: u8 = 2;
    pub const VOLUME_META: u8 = 3;
    pub const SCAN_BEGIN: u8 = 4;
    pub const SCAN_PROGRESS: u8 = 5;
    pub const VOLUME_READY: u8 = 6;
    pub const BATCH: u8 = 7;
    pub const JOURNAL_RESET: u8 = 8;
    pub const VOLUME_GONE: u8 = 9;
    pub const VOLUME_APPEARED: u8 = 10;
    pub const WORKER_EXIT: u8 = 11;
}

/// 握手令牌长度(字节)。主进程每次启动生成,经 runas 命令行传给 worker,
/// Hello 里回传后由 worker 校验:管道名固定,只靠 exe 名挡不住同用户的其它进程。
pub const TOKEN_LEN: usize = 16;

/// 令牌转 32 位小写十六进制(用于 `--index-token` 参数)。
pub fn token_hex(token: &[u8; TOKEN_LEN]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(TOKEN_LEN * 2);
    for b in token {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 解析 `--index-token` 的十六进制参数;长度/字符不合法返回 None。
pub fn token_from_hex(s: &str) -> Option<[u8; TOKEN_LEN]> {
    let s = s.trim();
    if s.len() != TOKEN_LEN * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; TOKEN_LEN];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello {
        proto: u32,
        /// 主进程启动时生成的能力令牌(worker 侧校验)。
        token: [u8; TOKEN_LEN],
        /// 每卷握手状态:(guid, journal_id, next_usn, resume_frn, complete)
        ///
        /// complete=false 且 resume_frn>0 表示「上次全量扫描中断」:worker 从
        /// resume_frn 续扫 ENUM,到 next_usn(扫描起点高水位)后再接 journal,
        /// 避免每次重启都从头全量扫(U 盘/大卷代价明显)。
        volumes: Vec<(u128, u64, i64, u64, bool)>,
    },
    Welcome {
        proto: u32,
    },
    VolumeMeta {
        guid: u128,
        letter: char,
        journal_id: u64,
        next_usn: i64,
        est_records: u64,
    },
    ScanBegin {
        guid: u128,
        resume_frn: u64,
    },
    ScanProgress {
        guid: u128,
        frn: u64,
        est: u64,
        count: u32,
    },
    VolumeReady {
        guid: u128,
        journal_id: u64,
        next_usn: i64,
        count: u32,
    },
    Batch {
        guid: u128,
        next_usn: i64,
        recs: Vec<Rec>,
    },
    JournalReset {
        guid: u128,
    },
    /// 运行期对账:已有快照时不重置 store,逐批 patch 现有条目。
    DirSyncBegin {
        guid: u128,
    },
    DirSyncEnd {
        guid: u128,
        /// true = 枚举完整结束,可以删除本轮没见到的条目;
        /// false = 中途停止/出错,只清 SEEN 位,不做删除。
        ok: bool,
    },
    /// 运行期 watcher:某个父目录的最新子项快照(替换该目录的子项集合)。
    DirChildren {
        guid: u128,
        parent_frn: u64,
        recs: Vec<Rec>,
    },
    /// USN 实时增量层每卷状态(usn 后端;worker 状态变化时上报,引擎据此
    /// 在设置页/横幅展示 Watching/Backoff/Disabled 与游标 age)。
    UsnWatch {
        guid: u128,
        state: UsnWatchState,
    },
    VolumeGone {
        guid: u128,
    },
    VolumeAppeared {
        guid: u128,
        letter: char,
    },
    WorkerExit {
        reason: u32,
    },
    /// 重建卷:mask = 盘符位掩码(bit i = `'A'+i`);0 = 全部卷。
    /// 噪声排除规则变更后用:盘符锚定的规则只需重扫对应盘。
    Rescan {
        mask: u32,
    },
    /// 运行期切换索引卷的掩码(bit i = 盘符 `'A' + i`,0 = 未设置)。
    SetVolumes {
        mask: u32,
    },
    Shutdown,
}

struct W {
    buf: Vec<u8>,
}

impl W {
    fn new(ty: u8) -> Self {
        Self { buf: vec![ty] }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u16(v.len() as u16);
        self.buf.extend_from_slice(v);
    }
}

fn write_rec(w: &mut W, r: &Rec) {
    w.u8(match r.op {
        Op::Upsert => 1,
        Op::Remove => 2,
    });
    w.u32(r.attrs);
    w.u64(r.frn);
    w.u64(r.parent_frn);
    w.u64(r.mtime as u64);
    w.u64(r.size);
    w.bytes(r.name.as_bytes());
}

/// 记录列表 → 线上字节(u32 数量 + 逐条 rec)。旁车增量日志复用同一编码,
/// 保证「管道帧里的记录」与「旁车里的记录」语义/布局完全一致。
pub(crate) fn encode_recs(recs: &[Rec]) -> Vec<u8> {
    let mut w = W { buf: Vec::new() };
    w.u32(recs.len() as u32);
    for r in recs {
        write_rec(&mut w, r);
    }
    w.buf
}

/// [`encode_recs`] 的逆操作;字节不完整、数量不符或有尾部残留 → None(损坏)。
pub(crate) fn decode_recs(bytes: &[u8]) -> Option<Vec<Rec>> {
    let mut r = R { buf: bytes };
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(100_000));
    for _ in 0..n {
        out.push(read_rec(&mut r)?);
    }
    if !r.buf.is_empty() {
        return None;
    }
    Some(out)
}

fn read_rec(r: &mut R) -> Option<Rec> {
    let op = match r.u8()? {
        1 => Op::Upsert,
        2 => Op::Remove,
        _ => return None,
    };
    let attrs = r.u32()?;
    let frn = r.u64()?;
    let parent_frn = r.u64()?;
    let mtime = r.u64()? as i64;
    let size = r.u64()?;
    let name = r.bytes()?;
    Some(Rec {
        frn,
        parent_frn,
        attrs,
        op,
        name: String::from_utf8_lossy(name).into_owned(),
        mtime,
        size,
    })
}

struct R<'a> {
    buf: &'a [u8],
}

impl<'a> R<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.buf.first()?;
        self.buf = &self.buf[1..];
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        if self.buf.len() < 2 {
            return None;
        }
        let v = u16::from_le_bytes(self.buf[..2].try_into().unwrap());
        self.buf = &self.buf[2..];
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        if self.buf.len() < 4 {
            return None;
        }
        let v = u32::from_le_bytes(self.buf[..4].try_into().unwrap());
        self.buf = &self.buf[4..];
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        if self.buf.len() < 8 {
            return None;
        }
        let v = u64::from_le_bytes(self.buf[..8].try_into().unwrap());
        self.buf = &self.buf[8..];
        Some(v)
    }
    fn i64(&mut self) -> Option<i64> {
        self.u64().map(|v| v as i64)
    }
    fn u128(&mut self) -> Option<u128> {
        if self.buf.len() < 16 {
            return None;
        }
        let v = u128::from_le_bytes(self.buf[..16].try_into().unwrap());
        self.buf = &self.buf[16..];
        Some(v)
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.u16()? as usize;
        if self.buf.len() < len {
            return None;
        }
        let v = &self.buf[..len];
        self.buf = &self.buf[len..];
        Some(v)
    }
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = match self {
            Frame::Hello { proto, token, volumes } => {
                let mut w = W::new(frame_type::HELLO);
                w.u32(*proto);
                w.buf.extend_from_slice(token);
                w.u32(volumes.len() as u32);
                for (guid, jid, usn, resume_frn, complete) in volumes {
                    w.u128(*guid);
                    w.u64(*jid);
                    w.i64(*usn);
                    w.u64(*resume_frn);
                    w.u8(u8::from(*complete));
                }
                w
            }
            Frame::Welcome { proto } => {
                let mut w = W::new(frame_type::WELCOME);
                w.u32(*proto);
                w
            }
            Frame::VolumeMeta { guid, letter, journal_id, next_usn, est_records } => {
                let mut w = W::new(frame_type::VOLUME_META);
                w.u128(*guid);
                w.u8(*letter as u8);
                w.u64(*journal_id);
                w.i64(*next_usn);
                w.u64(*est_records);
                w
            }
            Frame::ScanBegin { guid, resume_frn } => {
                let mut w = W::new(frame_type::SCAN_BEGIN);
                w.u128(*guid);
                w.u64(*resume_frn);
                w
            }
            Frame::ScanProgress { guid, frn, est, count } => {
                let mut w = W::new(frame_type::SCAN_PROGRESS);
                w.u128(*guid);
                w.u64(*frn);
                w.u64(*est);
                w.u32(*count);
                w
            }
            Frame::VolumeReady { guid, journal_id, next_usn, count } => {
                let mut w = W::new(frame_type::VOLUME_READY);
                w.u128(*guid);
                w.u64(*journal_id);
                w.i64(*next_usn);
                w.u32(*count);
                w
            }
            Frame::Batch { guid, next_usn, recs } => {
                let mut w = W::new(frame_type::BATCH);
                w.u128(*guid);
                w.i64(*next_usn);
                w.u32(recs.len() as u32);
                for r in recs {
                    write_rec(&mut w, r);
                }
                w
            }
            Frame::JournalReset { guid } => {
                let mut w = W::new(frame_type::JOURNAL_RESET);
                w.u128(*guid);
                w
            }
            Frame::DirSyncBegin { guid } => {
                let mut w = W::new(frame_type::DIR_SYNC_BEGIN);
                w.u128(*guid);
                w
            }
            Frame::DirSyncEnd { guid, ok } => {
                let mut w = W::new(frame_type::DIR_SYNC_END);
                w.u128(*guid);
                w.u8(u8::from(*ok));
                w
            }
            Frame::DirChildren { guid, parent_frn, recs } => {
                let mut w = W::new(frame_type::DIR_CHILDREN);
                w.u128(*guid);
                w.u64(*parent_frn);
                w.u32(recs.len() as u32);
                for r in recs {
                    write_rec(&mut w, r);
                }
                w
            }
            Frame::VolumeGone { guid } => {
                let mut w = W::new(frame_type::VOLUME_GONE);
                w.u128(*guid);
                w
            }
            Frame::VolumeAppeared { guid, letter } => {
                let mut w = W::new(frame_type::VOLUME_APPEARED);
                w.u128(*guid);
                w.u8(*letter as u8);
                w
            }
            Frame::WorkerExit { reason } => {
                let mut w = W::new(frame_type::WORKER_EXIT);
                w.u32(*reason);
                w
            }
            Frame::Rescan { mask } => {
                let mut w = W::new(frame_type::RESCAN);
                w.u32(*mask);
                w
            }
            Frame::SetVolumes { mask } => {
                let mut w = W::new(frame_type::SET_VOLUMES);
                w.u32(*mask);
                w
            }
            Frame::UsnWatch { guid, state } => {
                let mut w = W::new(frame_type::USN_WATCH);
                w.u128(*guid);
                w.u8(state.to_u8());
                w
            }
            Frame::Shutdown => W::new(frame_type::SHUTDOWN),
        };
        // 前置长度(含类型字节)
        let mut out = Vec::with_capacity(4 + w.buf.len());
        out.extend_from_slice(&(w.buf.len() as u32).to_le_bytes());
        out.append(&mut w.buf);
        out
    }

    pub fn decode(payload: &[u8]) -> Option<Frame> {
        let ty = *payload.first()?;
        let mut r = R { buf: &payload[1..] };
        Some(match ty {
            frame_type::HELLO => {
                let proto = r.u32()?;
                if r.buf.len() < TOKEN_LEN {
                    return None;
                }
                let mut token = [0u8; TOKEN_LEN];
                token.copy_from_slice(&r.buf[..TOKEN_LEN]);
                r.buf = &r.buf[TOKEN_LEN..];
                let n = r.u32()? as usize;
                let mut volumes = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let guid = r.u128()?;
                    let jid = r.u64()?;
                    let usn = r.i64()?;
                    let resume_frn = r.u64()?;
                    let complete = r.u8()? != 0;
                    volumes.push((guid, jid, usn, resume_frn, complete));
                }
                Frame::Hello { proto, token, volumes }
            }
            frame_type::WELCOME => Frame::Welcome { proto: r.u32()? },
            frame_type::VOLUME_META => Frame::VolumeMeta {
                guid: r.u128()?,
                letter: char::from_u32(r.u8()? as u32)?,
                journal_id: r.u64()?,
                next_usn: r.i64()?,
                est_records: r.u64()?,
            },
            frame_type::SCAN_BEGIN => Frame::ScanBegin { guid: r.u128()?, resume_frn: r.u64()? },
            frame_type::SCAN_PROGRESS => Frame::ScanProgress {
                guid: r.u128()?,
                frn: r.u64()?,
                est: r.u64()?,
                count: r.u32()?,
            },
            frame_type::VOLUME_READY => Frame::VolumeReady {
                guid: r.u128()?,
                journal_id: r.u64()?,
                next_usn: r.i64()?,
                count: r.u32()?,
            },
            frame_type::BATCH => {
                let guid = r.u128()?;
                let next_usn = r.i64()?;
                let n = r.u32()? as usize;
                let mut recs = Vec::with_capacity(n.min(100_000));
                for _ in 0..n {
                    recs.push(read_rec(&mut r)?);
                }
                Frame::Batch { guid, next_usn, recs }
            }
            frame_type::DIR_CHILDREN => {
                let guid = r.u128()?;
                let parent_frn = r.u64()?;
                let n = r.u32()? as usize;
                let mut recs = Vec::with_capacity(n.min(100_000));
                for _ in 0..n {
                    recs.push(read_rec(&mut r)?);
                }
                Frame::DirChildren { guid, parent_frn, recs }
            }
            frame_type::JOURNAL_RESET => Frame::JournalReset { guid: r.u128()? },
            frame_type::DIR_SYNC_BEGIN => Frame::DirSyncBegin { guid: r.u128()? },
            frame_type::DIR_SYNC_END => Frame::DirSyncEnd { guid: r.u128()?, ok: r.u8()? != 0 },
            frame_type::VOLUME_GONE => Frame::VolumeGone { guid: r.u128()? },
            frame_type::VOLUME_APPEARED => Frame::VolumeAppeared {
                guid: r.u128()?,
                letter: char::from_u32(r.u8()? as u32)?,
            },
            frame_type::WORKER_EXIT => Frame::WorkerExit { reason: r.u32()? },
            frame_type::RESCAN => Frame::Rescan { mask: r.u32()? },
            frame_type::SET_VOLUMES => Frame::SetVolumes { mask: r.u32()? },
            frame_type::USN_WATCH => Frame::UsnWatch {
                guid: r.u128()?,
                state: UsnWatchState::from_u8(r.u8()?),
            },
            frame_type::SHUTDOWN => Frame::Shutdown,
            _ => return None,
        })
    }

    pub fn proto_ok(&self) -> bool {
        match self {
            Frame::Hello { proto, .. } | Frame::Welcome { proto } => *proto == PROTO_VERSION,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::Op;

    #[test]
    fn roundtrip_all_frames() {
        let frames = vec![
            Frame::Hello {
                proto: PROTO_VERSION,
                token: [0x0f; TOKEN_LEN],
                volumes: vec![
                    (1, 2, 3, 4, true),
                    (u128::MAX, u64::MAX, i64::MIN, u64::MAX, false),
                ],
            },
            Frame::Welcome { proto: PROTO_VERSION },
            Frame::VolumeMeta { guid: 7, letter: 'C', journal_id: 9, next_usn: -5, est_records: 100 },
            Frame::ScanBegin { guid: 7, resume_frn: 0xdead },
            Frame::ScanProgress { guid: 7, frn: 5, est: 10, count: 3 },
            Frame::VolumeReady { guid: 7, journal_id: 1, next_usn: 2, count: 999 },
            Frame::Batch {
                guid: 7,
                next_usn: 42,
                recs: vec![
                    Rec { frn: 1, parent_frn: 2, attrs: 0x10, op: Op::Upsert, name: "a.txt".into(), mtime: 0, size: u64::MAX },
                    Rec { frn: 3, parent_frn: 4, attrs: 0, op: Op::Remove, name: "删除我.txt".into(), mtime: 0, size: u64::MAX },
                ],
            },
            Frame::JournalReset { guid: 7 },
            Frame::DirSyncBegin { guid: 7 },
            Frame::DirSyncEnd { guid: 7, ok: true },
            Frame::DirChildren {
                guid: 7,
                parent_frn: 5,
                recs: vec![Rec {
                    frn: 9,
                    parent_frn: 5,
                    attrs: 0x20,
                    op: Op::Upsert,
                    name: "new.txt".into(),
                    mtime: 0,
                    size: u64::MAX,
                }],
            },
            Frame::VolumeGone { guid: 7 },
            Frame::VolumeAppeared { guid: 7, letter: 'E' },
            Frame::WorkerExit { reason: 0 },
            Frame::Rescan { mask: 0b100 },
            Frame::SetVolumes { mask: (1 << 2) | (1 << 3) },
            Frame::SetVolumes { mask: u32::MAX },
            Frame::UsnWatch { guid: 7, state: crate::usnwatch::UsnWatchState::Watching },
            Frame::UsnWatch { guid: u128::MAX, state: crate::usnwatch::UsnWatchState::Disabled },
            Frame::Shutdown,
        ];
        for f in frames {
            let bytes = f.encode();
            let (len, rest) = bytes.split_at(4);
            let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
            assert_eq!(len, rest.len());
            let decoded = Frame::decode(rest).unwrap();
            assert_eq!(f, decoded);
        }
    }

    #[test]
    fn proto_mismatch_survives_decode() {
        let f = Frame::Hello { proto: 999, token: [0u8; TOKEN_LEN], volumes: vec![] };
        let bytes = f.encode();
        let decoded = Frame::decode(&bytes[4..]).unwrap();
        assert!(!decoded.proto_ok());
    }

    #[test]
    fn token_hex_roundtrip_and_rejects_bad_input() {
        let t = [0x00u8, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let hex = token_hex(&t);
        assert_eq!(hex, "00112233445566778899aabbccddeeff");
        assert_eq!(token_from_hex(&hex).unwrap(), t);
        // 大写也接受
        assert_eq!(token_from_hex(&hex.to_uppercase()).unwrap(), t);
        assert!(token_from_hex("abcd").is_none());
        assert!(token_from_hex(&"z".repeat(32)).is_none());
    }
}
