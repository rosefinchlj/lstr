//! USN 实时增量层的护栏(纯函数,不碰 Win32,单测覆盖)。
//!
//! 背景:2026-11-09 两次 BSOD 取证(`.dsh-monitor/bsod-rootcause.md`)证明根因是
//! ntfs.sys QueryOpen 类型混淆 × 第三方 minifilter,与 USN FSCTL 参数无关,USN
//! journal 用户态方案可以带护栏回归。本模块集中定义两类护栏:
//!
//! - [`UsnCircuit`]:每卷独立的轮询熔断状态机 Off → Watching → Backoff →
//!   Disabled(session)。任何 FSCTL 错误按 1s→2s→4s→…→60s 指数退避重试;
//!   journal 生命周期错误(0x19C/19D/19E)立即调度 DirSync 对账;连续
//!   [`UsnCircuit::MAX_FAILURES`] 次失败 → 该卷本 session 停用,不重试风暴。
//! - [`cursor_trusted`]:重启后的游标可信判定 —— journal_id 匹配且
//!   FirstUsn <= next_usn <= NextUsn 才信任,否则回退 dirscan 全量。
//!
//! 状态机本身不做 IO、不睡眠:调用方(worker 卷线程)拿到 [`UsnDirective`]
//! 后自行执行,保证全部转移逻辑可离线单测。

/// 每卷 USN 层的会话内状态(worker → 引擎经 `UsnWatch` 帧上报,UI 展示)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsnWatchState {
    /// 未启用(非 usn 后端,或尚未进入轮询)。
    Off,
    /// 正常轮询 journal。
    Watching,
    /// 出错退避中(含 DirSync 对账期间)。
    Backoff,
    /// 连续失败超限,本 session 停用该卷 USN 层。
    Disabled,
}

impl UsnWatchState {
    /// 线上协议编码(`UsnWatch` 帧载荷)。
    pub fn to_u8(self) -> u8 {
        match self {
            UsnWatchState::Off => 0,
            UsnWatchState::Watching => 1,
            UsnWatchState::Backoff => 2,
            UsnWatchState::Disabled => 3,
        }
    }

    /// 解码;未知值回落 Off(协议向前兼容)。
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => UsnWatchState::Watching,
            2 => UsnWatchState::Backoff,
            3 => UsnWatchState::Disabled,
            _ => UsnWatchState::Off,
        }
    }

    /// UI/日志用的中文短标签。
    pub fn label(self) -> &'static str {
        match self {
            UsnWatchState::Off => "未启用",
            UsnWatchState::Watching => "监视中",
            UsnWatchState::Backoff => "退避重试",
            UsnWatchState::Disabled => "已停用",
        }
    }
}

/// 一次轮询/对账交互的结果分类(调用方从 Win32 错误码映射)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsnEvent {
    /// 一次成功轮询(读到数据或空读都算)。
    PollOk,
    /// 暂时性 IO 错误(非 journal 生命周期类):指数退避后重试。
    PollError,
    /// journal 失效/删除中/失活(0x19C/0x19D/0x19E):立即 DirSync 对账。
    JournalGone,
    /// DirSync 对账结束;ok=true 后重新握手(journal 状态 + 新游标)。
    DirSyncDone { ok: bool },
}

/// 状态机给调用方的指令;worker 卷线程负责实际执行(sleep/对账/停用)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsnDirective {
    /// 无事:按正常节奏继续轮询。
    Continue,
    /// 退避:先 sleep 指定毫秒再进入下一轮。
    Sleep(u64),
    /// 立即对该卷跑一遍 DirSync 对账,完成后报 [`UsnEvent::DirSyncDone`]。
    RunDirSync,
    /// 本 session 停用该卷 USN 层(调用方记日志后结束轮询线程)。
    Disable,
}

/// 每卷独立的轮询熔断器。纯状态转移,无 IO/无时钟:
/// 退避时长由[`UsnDirective::Sleep`]带出,何时执行由调用方决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsnCircuit {
    pub state: UsnWatchState,
    /// 连续失败次数(成功一次即清零)。
    pub failures: u32,
    /// 下一次错误时的退避基数(毫秒);成功后复位。
    pub backoff_ms: u64,
}

impl UsnCircuit {
    /// 连续失败上限:达到即本 session Disabled。
    pub const MAX_FAILURES: u32 = 5;
    /// 首次错误退避(规格:1s→2s→4s→…→60s)。
    pub const BACKOFF_START_MS: u64 = 1_000;
    /// 退避上限。
    pub const BACKOFF_MAX_MS: u64 = 60_000;

    pub fn new() -> Self {
        Self {
            state: UsnWatchState::Off,
            failures: 0,
            backoff_ms: Self::BACKOFF_START_MS,
        }
    }

    /// 退避公式:逐次翻倍,封顶 [`UsnCircuit::BACKOFF_MAX_MS`]。
    /// 提成模块级纯函数便于直接单测封顶行为。
    fn next_backoff(backoff_ms: u64) -> u64 {
        (backoff_ms.saturating_mul(2)).min(Self::BACKOFF_MAX_MS)
    }

    /// 处理一次事件,返回调用方应执行的指令。
    /// Disabled 是终态:本 session 内任何事件都不再改变它(自锁,
    /// 不依赖调用方纪律),一律继续返回 Disable。
    pub fn on_event(&mut self, ev: UsnEvent) -> UsnDirective {
        if self.state == UsnWatchState::Disabled {
            return UsnDirective::Disable;
        }
        match ev {
            UsnEvent::PollOk => {
                self.state = UsnWatchState::Watching;
                self.failures = 0;
                self.backoff_ms = Self::BACKOFF_START_MS;
                UsnDirective::Continue
            }
            UsnEvent::PollError | UsnEvent::DirSyncDone { ok: false } => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= Self::MAX_FAILURES {
                    self.state = UsnWatchState::Disabled;
                    return UsnDirective::Disable;
                }
                self.state = UsnWatchState::Backoff;
                let wait = self.backoff_ms;
                self.backoff_ms = Self::next_backoff(self.backoff_ms);
                UsnDirective::Sleep(wait)
            }
            UsnEvent::JournalGone => {
                // journal 生命周期错误不数入「轮询失败」:对账本身是自愈手段,
                // 成败由 DirSyncDone 单独计量(失败同样走 5 次熔断)。
                self.state = UsnWatchState::Backoff;
                UsnDirective::RunDirSync
            }
            UsnEvent::DirSyncDone { ok: true } => {
                // 对账成功 → 重新握手(new journal_id + 新游标),回到 Watching。
                self.state = UsnWatchState::Watching;
                self.failures = 0;
                self.backoff_ms = Self::BACKOFF_START_MS;
                UsnDirective::Continue
            }
        }
    }
}

impl Default for UsnCircuit {
    fn default() -> Self {
        Self::new()
    }
}

/// 重启后的游标可信判定。
///
/// `saved_jid/saved_usn` = 快照里的 (journal_id, next_usn);
/// `live` = 当前的 (journal_id, FirstUsn, NextUsn);None = 卷上没有活动 journal。
/// 规则:journal_id 匹配且 FirstUsn <= saved_usn <= NextUsn 才信任;
/// 否则(不匹配/越界/无 journal)一律回退 dirscan 全量。
pub fn cursor_trusted(saved_jid: u64, saved_usn: i64, live: Option<(u64, i64, i64)>) -> bool {
    let Some((jid, first_usn, next_usn)) = live else {
        return false;
    };
    if saved_jid == 0 || saved_usn <= 0 {
        return false;
    }
    jid == saved_jid && saved_usn >= first_usn && saved_usn <= next_usn
}

/// journal 轮询的 ReasonMask:lifecycle(建/删/改名)+ CLOSE + DATA 写类。
///
/// - lifecycle 与 [`crate::usn::REASON_LIFECYCLE`] 的差别:这里按规格额外带上
///   `RENAME_OLD_NAME` —— FSCTL 层多收一条旧名记录无害(引擎侧 H3 规则会忽略
///   单独出现的 OLD_NAME),但部分卷实现把 OLD|NEW 合并进同一条记录,漏读会丢改名。
/// - DATA_*/CLOSE 记录到达后会被 [`crate::usn::RawRecord::to_engine_op`] 过滤,
///   读进来再丢是有意为之:合并记录(如 DATA_EXTEND|RENAME_NEW)不能在内核层裁掉。
pub const REASON_MASK_WATCH: u32 = crate::usn::USN_REASON_FILE_CREATE
    | crate::usn::USN_REASON_FILE_DELETE
    | crate::usn::USN_REASON_RENAME_OLD_NAME
    | crate::usn::USN_REASON_RENAME_NEW_NAME
    | crate::usn::USN_REASON_CLOSE
    | crate::usn::USN_REASON_DATA_OVERWRITE
    | crate::usn::USN_REASON_DATA_EXTEND
    | crate::usn::USN_REASON_DATA_TRUNCATION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_ok_resets_failures_and_state() {
        let mut cb = UsnCircuit::new();
        assert_eq!(cb.state, UsnWatchState::Off);
        assert_eq!(cb.on_event(UsnEvent::PollError), UsnDirective::Sleep(1_000));
        assert_eq!(cb.state, UsnWatchState::Backoff);
        assert_eq!(cb.on_event(UsnEvent::PollOk), UsnDirective::Continue);
        assert_eq!(cb.state, UsnWatchState::Watching);
        assert_eq!(cb.failures, 0);
        // 退避基数也被成功复位:下次错误从 1s 重新起算
        assert_eq!(cb.on_event(UsnEvent::PollError), UsnDirective::Sleep(1_000));
    }

    #[test]
    fn backoff_doubles_up_to_cap() {
        let mut cb = UsnCircuit::new();
        let waits: Vec<u64> = (0..4)
            .map(|_| match cb.on_event(UsnEvent::PollError) {
                UsnDirective::Sleep(ms) => ms,
                other => panic!("expected Sleep, got {other:?}"),
            })
            .collect();
        // 1s → 2s → 4s → 8s,此后 16s(封顶前)…
        assert_eq!(waits, vec![1_000, 2_000, 4_000, 8_000]);
        assert_eq!(cb.failures, 4);
        // 第 5 次连续失败 → 本 session 停用
        assert_eq!(cb.on_event(UsnEvent::PollError), UsnDirective::Disable);
        assert_eq!(cb.state, UsnWatchState::Disabled);
    }

    #[test]
    fn backoff_formula_caps_at_60s() {
        assert_eq!(UsnCircuit::next_backoff(1_000), 2_000);
        assert_eq!(UsnCircuit::next_backoff(16_000), 32_000);
        assert_eq!(UsnCircuit::next_backoff(32_000), UsnCircuit::BACKOFF_MAX_MS);
        assert_eq!(UsnCircuit::next_backoff(60_000), UsnCircuit::BACKOFF_MAX_MS);
        assert_eq!(UsnCircuit::next_backoff(u64::MAX), UsnCircuit::BACKOFF_MAX_MS);
    }

    #[test]
    fn journal_gone_schedules_dirsync_then_recover() {
        let mut cb = UsnCircuit::new();
        // journal 失效:立即对账,不计入轮询失败
        assert_eq!(cb.on_event(UsnEvent::JournalGone), UsnDirective::RunDirSync);
        assert_eq!(cb.state, UsnWatchState::Backoff);
        assert_eq!(cb.failures, 0);
        // 对账成功 → 重新握手,回到 Watching、计数清零
        assert_eq!(
            cb.on_event(UsnEvent::DirSyncDone { ok: true }),
            UsnDirective::Continue
        );
        assert_eq!(cb.state, UsnWatchState::Watching);
        assert_eq!(cb.failures, 0);
    }

    #[test]
    fn failed_dirsync_counts_toward_disable() {
        let mut cb = UsnCircuit::new();
        cb.on_event(UsnEvent::JournalGone);
        for i in 0..4 {
            let d = cb.on_event(UsnEvent::DirSyncDone { ok: false });
            assert_eq!(d, UsnDirective::Sleep(1_000 << i), "对账失败也按指数退避");
        }
        assert_eq!(
            cb.on_event(UsnEvent::DirSyncDone { ok: false }),
            UsnDirective::Disable,
            "连续 5 次对账失败 → 本 session 停用"
        );
        assert_eq!(cb.state, UsnWatchState::Disabled);
    }

    #[test]
    fn disable_is_latching() {
        let mut cb = UsnCircuit::new();
        for _ in 0..5 {
            cb.on_event(UsnEvent::PollError);
        }
        assert_eq!(cb.state, UsnWatchState::Disabled);
        // 终态自锁:后续任何事件(包括成功)都不再改变指令与状态,
        // 「本 session 停用」不依赖调用方是否记得退出。
        for ev in [
            UsnEvent::PollOk,
            UsnEvent::PollError,
            UsnEvent::JournalGone,
            UsnEvent::DirSyncDone { ok: true },
        ] {
            assert_eq!(cb.on_event(ev), UsnDirective::Disable);
            assert_eq!(cb.state, UsnWatchState::Disabled);
        }
    }

    #[test]
    fn state_wire_roundtrip() {
        for s in [
            UsnWatchState::Off,
            UsnWatchState::Watching,
            UsnWatchState::Backoff,
            UsnWatchState::Disabled,
        ] {
            assert_eq!(UsnWatchState::from_u8(s.to_u8()), s);
        }
        // 未知值回落 Off
        assert_eq!(UsnWatchState::from_u8(99), UsnWatchState::Off);
    }

    #[test]
    fn cursor_requires_matching_journal_and_window() {
        let live = Some((7u64, 100i64, 500i64));
        // 正常:同一 journal、位点在窗口内
        assert!(cursor_trusted(7, 100, live));
        assert!(cursor_trusted(7, 500, live));
        assert!(cursor_trusted(7, 300, live));
        // journal_id 不匹配(journal 已重建)
        assert!(!cursor_trusted(8, 300, live));
        // 越界:早于 FirstUsn / 晚于 NextUsn
        assert!(!cursor_trusted(7, 99, live));
        assert!(!cursor_trusted(7, 501, live));
        // 回退保护:无效位点(jid=0 / usn<=0)一律不信任
        assert!(!cursor_trusted(0, 300, live));
        assert!(!cursor_trusted(7, 0, live));
        assert!(!cursor_trusted(7, -5, live));
        // 卷上没有活动 journal
        assert!(!cursor_trusted(7, 300, None));
    }
}
