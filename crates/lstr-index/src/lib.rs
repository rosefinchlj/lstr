//! lstr-index:NTFS USN 支撑的文件名索引引擎。
//!
//! 架构(方案 docs/index-engine-plan.md §4):
//! - 提权 worker(`--index-worker`)读卷 ENUM/USN journal,经命名管道推记录流;
//! - 非提权主进程持有 [`engine::Engine`](内存列式存储),击键查询零 IPC;
//! - 握手携带 `{journal_id, next_usn}` 增量续传,仅 journal 回绕才全量重扫。

pub mod client;
pub mod dirscan;
pub mod engine;
pub mod noise;
pub mod pinyin;
pub mod proto;
pub mod query;
pub mod scan;
pub mod snapshot;
pub mod store;
pub mod usn;
pub mod watch;
pub mod win;
pub mod worker;

pub use engine::{Engine, EngineConfig, EngineMsg, IndexStatus, Phase, VolumeProgress};
pub use query::{Hit, KindFilter, QueryOutcome, QuerySpec};
pub use usn::{Op, Rec};

/// 协议版本。
/// v2:Hello 增加 16 字节能力令牌(worker 侧校验,防同用户其它进程连接固定
/// 管道名并驱动扫描)。
/// v3:Hello 每卷增加 resume_frn 与 complete 标记(中断续扫),布局不兼容 v2。
/// worker/client 必须同版本;升级后残留的旧提权 worker 会握手失败并退出
/// (见 worker::run_session),新客户端重拉即可。
pub const PROTO_VERSION: u32 = 3;
pub const PIPE_NAME: &str = r"\\.\pipe\lstr-index";

/// 引擎请求(主进程 → worker 的控制指令,经管道)。
#[derive(Debug)]
pub enum ClientCommand {
    /// 请求重建指定卷(设置页「重建」按钮)。
    Rescan(u128),
    Shutdown,
}
