//! lstr-index:NTFS USN 支撑的文件名索引引擎。
//!
//! 架构(方案 docs/index-engine-plan.md §4):
//! - 提权 worker(`--index-worker`)读卷 ENUM/USN journal,经命名管道推记录流;
//! - 非提权主进程持有 [`engine::Engine`](内存列式存储),击键查询零 IPC;
//! - 握手携带 `{journal_id, next_usn}` 增量续传,仅 journal 回绕才全量重扫。

pub mod engine;
pub mod noise;
pub mod pinyin;
pub mod proto;
pub mod query;
pub mod snapshot;
pub mod store;
pub mod usn;

pub use engine::{Engine, EngineConfig, EngineMsg, IndexStatus, Phase, VolumeProgress};
pub use query::{Hit, KindFilter, QueryOutcome, QuerySpec};
pub use usn::{Op, Rec};

pub const PROTO_VERSION: u32 = 1;
pub const PIPE_NAME: &str = r"\\.\pipe\lstr-index";

/// 引擎请求(主进程 → worker 的控制指令,经管道)。
#[derive(Debug)]
pub enum ClientCommand {
    /// 请求重建指定卷(设置页「重建」按钮)。
    Rescan(u128),
    Shutdown,
}
