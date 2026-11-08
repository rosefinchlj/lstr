//! 管道帧协议:u32 长度前缀 | u8 类型 | 载荷(全部小端)。
//!
//! C→W:Hello(握手,带每卷续传位点)/ Rescan / Shutdown
//! W→C:Welcome / VolumeMeta / ScanBegin / ScanProgress / Batch / VolumeReady
//!      / JournalReset / VolumeGone / VolumeAppeared / WorkerExit

use crate::usn::{Op, Rec};
use crate::PROTO_VERSION;

pub mod frame_type {
    // C→W
    pub const HELLO: u8 = 1;
    pub const RESCAN: u8 = 12;
    pub const SHUTDOWN: u8 = 13;
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

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello {
        proto: u32,
        /// (guid, journal_id, next_usn)
        volumes: Vec<(u128, u64, i64)>,
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
    Rescan {
        guid: u128,
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
            Frame::Hello { proto, volumes } => {
                let mut w = W::new(frame_type::HELLO);
                w.u32(*proto);
                w.u32(volumes.len() as u32);
                for (guid, jid, usn) in volumes {
                    w.u128(*guid);
                    w.u64(*jid);
                    w.i64(*usn);
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
                    w.u8(match r.op {
                        Op::Upsert => 1,
                        Op::Remove => 2,
                    });
                    w.u32(r.attrs);
                    w.u64(r.frn);
                    w.u64(r.parent_frn);
                    w.bytes(r.name.as_bytes());
                }
                w
            }
            Frame::JournalReset { guid } => {
                let mut w = W::new(frame_type::JOURNAL_RESET);
                w.u128(*guid);
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
            Frame::Rescan { guid } => {
                let mut w = W::new(frame_type::RESCAN);
                w.u128(*guid);
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
                let n = r.u32()? as usize;
                let mut volumes = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let guid = r.u128()?;
                    let jid = r.u64()?;
                    let usn = r.i64()?;
                    volumes.push((guid, jid, usn));
                }
                Frame::Hello { proto, volumes }
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
                    let op = match r.u8()? {
                        1 => Op::Upsert,
                        2 => Op::Remove,
                        _ => return None,
                    };
                    let attrs = r.u32()?;
                    let frn = r.u64()?;
                    let parent = r.u64()?;
                    let name = r.bytes()?;
                    recs.push(Rec {
                        frn,
                        parent_frn: parent,
                        attrs,
                        op,
                        name: String::from_utf8_lossy(name).into_owned(),
                    });
                }
                Frame::Batch { guid, next_usn, recs }
            }
            frame_type::JOURNAL_RESET => Frame::JournalReset { guid: r.u128()? },
            frame_type::VOLUME_GONE => Frame::VolumeGone { guid: r.u128()? },
            frame_type::VOLUME_APPEARED => Frame::VolumeAppeared {
                guid: r.u128()?,
                letter: char::from_u32(r.u8()? as u32)?,
            },
            frame_type::WORKER_EXIT => Frame::WorkerExit { reason: r.u32()? },
            frame_type::RESCAN => Frame::Rescan { guid: r.u128()? },
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
                volumes: vec![(1, 2, 3), (u128::MAX, u64::MAX, i64::MIN)],
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
                    Rec { frn: 1, parent_frn: 2, attrs: 0x10, op: Op::Upsert, name: "a.txt".into() },
                    Rec { frn: 3, parent_frn: 4, attrs: 0, op: Op::Remove, name: "删除我.txt".into() },
                ],
            },
            Frame::JournalReset { guid: 7 },
            Frame::VolumeGone { guid: 7 },
            Frame::VolumeAppeared { guid: 7, letter: 'E' },
            Frame::WorkerExit { reason: 0 },
            Frame::Rescan { guid: 7 },
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
        let f = Frame::Hello { proto: 999, volumes: vec![] };
        let bytes = f.encode();
        let decoded = Frame::decode(&bytes[4..]).unwrap();
        assert!(!decoded.proto_ok());
    }
}
