//! worker:`lstr.exe --index-worker` 的进程主体(提权、无窗口、常驻)。
//!
//! 职责:读卷(ENUM/journal),经命名管道把记录流推给主进程。
//! 无状态约定:每卷的回放位点由主进程在握手里下发(journal_id + next_usn),
//! worker 自身不落盘、不持有索引。
//!
//! 安全:管道名固定,启动时主进程用 runas 命令行传 16 字节能力令牌
//! (`--index-token <hex>`),Hello 里回传后在这里校验;没有匹配令牌的进程
//! 连上管道也会被拒。
//!
//! 威胁模型(残余风险,不做「完整修复」):
//! - 令牌走命令行是**能力校验**而非密码学防护:同用户进程只要能读到提权进程
//!   的命令行就能拿到令牌。但主进程本身不提权,它传给提权 worker 的任何通道
//!   (用户目录文件、环境变量、共享内存)对同用户非提权进程都更易读——命令行
//!   反而是最小暴露面,故保持不变;令牌每进程随机、worker 退出即失效。
//! - 令牌不符/帧型不符会关掉会话,累计 [`BAD_HELLO_GRACE`] 后 worker 退出。
//!   退出本身是必需的:旧令牌 worker 必须让位(唯一管道实例 + FIRST_PIPE_INSTANCE),
//!   否则新客户端永远连不上。陌生同用户进程反复发错 Hello 可让 worker 退出重拉,
//!   属信任边界内的噪音级 DoS,不针对数据。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;

use crate::proto::{self, Frame, TOKEN_LEN};
use crate::scan::{self, JournalError};
use crate::win;
use crate::{PROTO_VERSION, PIPE_NAME};

/// 帧通道容量:批帧最大 4000 条,32 帧 ≈ 十余 MB 上限(背压防扫描期积压)。
const FRAME_CHANNEL_CAP: usize = 32;

/// 无客户端连接的空闲上限:超过即退出。兜底清理「主进程崩溃、没发
/// Shutdown」留下的提权 worker;正常关闭走 Shutdown 立即退出,不会等这么久。
const IDLE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(120);

/// 主进程在会话里显式发过 `Frame::Shutdown`(或握手被拒)后置位:
/// 本次会话结束后不再重建管道,worker 直接退出。这样 lstr 正常关闭时不会
/// 留下常驻的提权进程;主进程崩溃时则靠 [`IDLE_ACCEPT_TIMEOUT`] 兜底。
static EXIT_AFTER_SESSION: AtomicBool = AtomicBool::new(false);

/// 全量/续扫的串行闸门:2026-11-09 本机蓝屏(0x3B / Ntfs.sys,时段与四卷并发
/// USN 扫描重合)后的触发面收敛措施。卷线程仍在、journal 轮询不受影响,只是
/// 全量 ENUM 一次只放行一卷;首次构建的墙钟时间变长,但 IO 压力显著降低。
static SCAN_GATE: Mutex<()> = Mutex::new(());

/// 启动时命令行传入的能力令牌;None = 没带 `--index-token`,一律拒绝握手。
static WORKER_TOKEN: OnceLock<[u8; TOKEN_LEN]> = OnceLock::new();

/// 首扫模式。2026-11-09 本机两次 `SYSTEM_SERVICE_EXCEPTION(0x3B, Ntfs.sys)`
/// 蓝屏,时段都落在 USN FSCTL 路径活跃期;Everything 直读 `$MFT` 不走 FSCTL,
/// 长期运行无碍。因此把默认首扫切到目录枚举(`dirscan`),零 USN FSCTL;
/// USN 首扫 + journal 增量降级为显式开关(`--index-scan-mode usn`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMode {
    /// 目录枚举首扫(`NtQueryDirectoryFile`),不触碰任何 USN FSCTL。
    Dir,
    /// USN 首扫 + journal 增量(原路径;蓝屏后仅保留给显式开启)。
    Usn,
}

/// worker 启动参数(命令行 → 静态配置)。
#[derive(Clone, Debug)]
pub struct WorkerOpts {
    pub scan_mode: ScanMode,
    /// dir 模式下首扫完成后是否继续轮询 journal(阶段 2;默认关闭)。
    pub journal: bool,
    /// 只处理这些卷;None = 全部本机固定 NTFS 卷。
    pub volumes: Option<Vec<char>>,
}

impl Default for WorkerOpts {
    fn default() -> Self {
        Self {
            scan_mode: ScanMode::Dir,
            journal: false,
            volumes: None,
        }
    }
}

static OPTS: OnceLock<WorkerOpts> = OnceLock::new();

fn opts() -> &'static WorkerOpts {
    OPTS.get_or_init(WorkerOpts::default)
}

/// 按 `--index-volumes` 过滤卷列表(阶段 1 先在单盘验收)。
fn filter_volumes(list: Vec<(char, String)>) -> Vec<(char, String)> {
    match &opts().volumes {
        Some(allowed) => list
            .into_iter()
            .filter(|(letter, _)| allowed.contains(letter))
            .collect(),
        None => list,
    }
}

/// 首次坏握手的时间点(进程启动后的纳秒数,0 = 尚无)。
static BAD_HELLO_FIRST_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 连续坏握手（令牌/帧型不符）容忍窗口:期内只丢会话、worker 留在原地;
/// 超过则退出让主进程重拉。这样单个陌生包打不掉 worker,旧令牌残留 worker
/// 也最多占管道 [`BAD_HELLO_GRACE`] 就会让位。
const BAD_HELLO_GRACE: Duration = Duration::from_secs(30);

/// 进程启动以来的纳秒数(单调时钟,只用于比较窗口)。
fn now_nanos() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

/// 纯函数:坏握手窗口是否已超时(first=0 表示尚未记录)。
fn bad_hello_expired(first_nanos: u64, now_nanos: u64) -> bool {
    first_nanos != 0
        && now_nanos.saturating_sub(first_nanos) >= BAD_HELLO_GRACE.as_nanos() as u64
}

/// 记录一次坏握手;返回 true 表示应退出 worker。
fn note_bad_hello() -> bool {
    let now = now_nanos();
    match BAD_HELLO_FIRST_NANOS.compare_exchange(
        0,
        now,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => false, // 窗口刚开始,先留在原地
        Err(first) => bad_hello_expired(first, now),
    }
}

/// 握手成功:清空坏握手窗口。
fn note_good_hello() {
    BAD_HELLO_FIRST_NANOS.store(0, Ordering::Relaxed);
}

/// worker 入口(lstr main 检测到 --index-worker 后调用)。不返回。
pub fn worker_main() -> i32 {
    let mut parsed = WorkerOpts::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--index-token" => {
                let Some(raw) = args.next() else {
                    crate::snapshot::debug_log("worker: missing --index-token value");
                    return 2;
                };
                let Some(token) = proto::token_from_hex(&raw) else {
                    crate::snapshot::debug_log("worker: malformed --index-token");
                    return 2;
                };
                let _ = WORKER_TOKEN.set(token);
            }
            "--index-scan-mode" => {
                let Some(raw) = args.next() else {
                    crate::snapshot::debug_log("worker: missing --index-scan-mode value");
                    return 2;
                };
                parsed.scan_mode = match raw.to_ascii_lowercase().as_str() {
                    "dir" => ScanMode::Dir,
                    "usn" => ScanMode::Usn,
                    other => {
                        crate::snapshot::debug_log(&format!(
                            "worker: unknown --index-scan-mode {other}"
                        ));
                        return 2;
                    }
                };
            }
            "--index-journal" => parsed.journal = true,
            "--index-volumes" => {
                let Some(raw) = args.next() else {
                    crate::snapshot::debug_log("worker: missing --index-volumes value");
                    return 2;
                };
                let letters: Vec<char> = raw
                    .split(',')
                    .filter_map(|s| s.trim().chars().next())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if letters.is_empty() {
                    crate::snapshot::debug_log("worker: empty --index-volumes");
                    return 2;
                }
                parsed.volumes = Some(letters);
            }
            _ => {}
        }
    }
    let mode = parsed.scan_mode;
    let journal = parsed.journal;
    let volumes = parsed.volumes.as_ref().map(|v| {
        v.iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    });
    let _ = OPTS.set(parsed);
    crate::snapshot::debug_log(&format!(
        "worker: opts scan_mode={mode:?} journal={journal} volumes={volumes:?}"
    ));
    let Ok(server) = win::create_pipe_server(PIPE_NAME) else {
        // 管道已存在 = 已有 worker 在跑(主进程重拉时可能撞上),静默退出
        return 0;
    };
    let mut worker = Worker { server };
    worker.run()
}

struct Worker {
    server: HANDLE,
}

impl Worker {
    fn run(&mut self) -> i32 {
        loop {
            // 阻塞的 ConnectNamedPipe 放到独立线程,主线程带超时等待:
            // 无客户端超过 IDLE_ACCEPT_TIMEOUT 就关闭句柄退出(兜底清理残留)。
            let (connected_tx, connected_rx) = mpsc::channel::<bool>();
            let addr = self.server.0 as usize;
            let acceptor = std::thread::Builder::new()
                .name("lstr-idx-accept".to_string())
                .spawn(move || {
                    let server = HANDLE(addr as *mut core::ffi::c_void);
                    let ok = win::wait_for_client(server) && win::verify_peer(server);
                    let _ = connected_tx.send(ok);
                })
                .ok();
            let connected = match connected_rx.recv_timeout(IDLE_ACCEPT_TIMEOUT) {
                Ok(v) => v,
                Err(_) => {
                    // 空闲超时:没有客户端。accept 线程仍阻塞在 ConnectNamedPipe,
                    // 不做关闭/join(同步句柄上的取消行为不可靠),直接返回让进程退出
                    // —— worker 是独立进程,main 返回即回收所有线程。
                    crate::snapshot::debug_log("worker: idle timeout, exiting");
                    return 0;
                }
            };
            if let Some(h) = acceptor {
                let _ = h.join();
            }
            if connected {
                run_session(self.server);
            }
            // 会话结束(主进程断开/握手失败):句柄只在这里关闭一次。
            // run_session 只借用句柄、不关闭,外层不再复用已关闭的句柄(H5)。
            win::close(self.server);
            self.server = HANDLE(std::ptr::null_mut());
            // M4:主进程显式关闭 / 握手被拒 → 不重建管道,直接退出。
            if EXIT_AFTER_SESSION.load(Ordering::Relaxed) {
                return 0;
            }
            match win::create_pipe_server(PIPE_NAME) {
                Ok(s) => self.server = s,
                Err(_) => {
                    // 管道实例可能仍被上一个会话占用:退避后重试一次,仍失败则退出
                    std::thread::sleep(Duration::from_secs(2));
                    match win::create_pipe_server(PIPE_NAME) {
                        Ok(s) => self.server = s,
                        Err(_) => return 1,
                    }
                }
            }
        }
    }
}

struct VolumeThread {
    stop: std::sync::Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// 一次客户端会话。句柄所有权属于调用方(`Worker::run`):这里只借用、不关闭。
fn run_session(client: HANDLE) {
    // 握手:Hello(带能力令牌)→ Welcome
    let hello = match read_frame_pub(client, Duration::from_secs(30)) {
        Some(Frame::Hello { proto, token, volumes }) if proto == PROTO_VERSION => {
            if !token_ok(token) {
                crate::snapshot::debug_log("worker: hello token mismatch, session dropped");
                // 令牌不符 = 旧客户端/旧 worker 混用或陌生进程。单个包不直接退出,
                // 等 BAD_HELLO_GRACE 累计后再让位(见模块文档的威胁模型)。
                if note_bad_hello() {
                    crate::snapshot::debug_log("worker: bad handshake window expired, exiting");
                    EXIT_AFTER_SESSION.store(true, Ordering::Relaxed);
                }
                return;
            }
            note_good_hello();
            volumes
        }
        Some(other) => {
            // 版本/帧型不符 = 与当前二进制不匹配的残留 worker(或陌生进程):
            // 同样等窗口累计后退出,新客户端重拉同版本 worker;否则旧 worker
            // 会一直占着唯一管道实例,新 worker 永远起不来。
            crate::snapshot::debug_log(&format!("worker: bad hello frame {other:?}"));
            if note_bad_hello() {
                crate::snapshot::debug_log("worker: bad handshake window expired, exiting");
                EXIT_AFTER_SESSION.store(true, Ordering::Relaxed);
            }
            return;
        }
        None => return,
    };
    if !write_frame_pub(client, &Frame::Welcome { proto: PROTO_VERSION }) {
        return;
    }
    let hello_map: HashMap<u128, (u64, i64, u64, bool)> = hello
        .into_iter()
        .map(|(g, j, u, r, c)| (g, (j, u, r, c)))
        .collect();

    // 有界帧通道:writer 落盘慢时 volume_loop 在 send_frame 里背压/让位(H7)
    let (tx, rx) = mpsc::sync_channel::<Frame>(FRAME_CHANNEL_CAP);
    // HANDLE 非 Send:以裸地址跨线程移交(单客户端会话内的受控移交)
    let client_addr = client.0 as usize;
    let writer = std::thread::spawn(move || {
        let client = HANDLE(client_addr as *mut core::ffi::c_void);
        for frame in rx.iter() {
            if !write_frame_pub(client, &frame) {
                break;
            }
        }
    });

    // 卷线程表
    let mut threads: HashMap<u128, VolumeThread> = HashMap::new();
    let volumes = filter_volumes(win::fixed_ntfs_volumes());
    for (letter, guid_path) in &volumes {
        let Some(guid) = win::guid_to_u128(guid_path) else { continue };
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let tx = tx.clone();
        let req = hello_map.get(&guid).copied();
        let stop2 = stop.clone();
        let letter_c = *letter;
        let handle = std::thread::Builder::new()
            .name(format!("lstr-vol-{letter_c}"))
            .spawn(move || {
                volume_loop(letter_c, guid, req, stop2, tx);
            })
            .ok();
        threads.insert(guid, VolumeThread { stop, handle });
    }

    // 会话循环:监视卷热插拔 + 主进程指令
    let mut last_drives = volumes;
    'session: loop {
        std::thread::sleep(Duration::from_millis(500));
        // 指令(非阻塞)
        while let Some(frame) = try_read_frame(client) {
            match frame {
                Frame::Rescan { guid } => {
                    if let Some(vt) = threads.get_mut(&guid) {
                        vt.stop.store(true, Ordering::Relaxed);
                        if let Some(h) = vt.handle.take() {
                            let _ = h.join();
                        }
                        let stop = std::sync::Arc::new(AtomicBool::new(false));
                        let tx = tx.clone();
                        let letter = last_drives
                            .iter()
                            .find(|(_, g)| win::guid_to_u128(g) == Some(guid))
                            .map(|(l, _)| *l)
                            .unwrap_or('C');
                        let stop2 = stop.clone();
                        let handle = std::thread::Builder::new()
                            .name(format!("lstr-vol-{letter}"))
                            .spawn(move || volume_loop(letter, guid, None, stop2, tx))
                            .ok();
                        threads.insert(guid, VolumeThread { stop, handle });
                    }
                }
                Frame::Shutdown => {
                    EXIT_AFTER_SESSION.store(true, Ordering::Relaxed);
                    break 'session;
                }
                _ => {}
            }
        }
        // 卷热插拔(轮询代替 WM_DEVICECHANGE:无窗口 worker 里更稳)
        let now = filter_volumes(win::fixed_ntfs_volumes());
        if now != last_drives {
            for (letter, g) in &now {
                if last_drives.iter().any(|(_, g2)| g2 == g) {
                    continue;
                }
                let Some(guid) = win::guid_to_u128(g) else { continue };
                // 控制帧:通道满时宁可丢弃,也不能阻塞会话循环
                let _ = tx.try_send(Frame::VolumeAppeared { guid, letter: *letter });
                let stop = std::sync::Arc::new(AtomicBool::new(false));
                let tx2 = tx.clone();
                let letter_c = *letter;
                let stop2 = stop.clone();
                if let Ok(h) = std::thread::Builder::new()
                    .name(format!("lstr-vol-{letter_c}"))
                    .spawn(move || volume_loop(letter_c, guid, None, stop2, tx2))
                {
                    threads.insert(guid, VolumeThread { stop, handle: Some(h) });
                }
            }
            for (letter, g) in &last_drives {
                if now.iter().any(|(_, g2)| g2 == g) {
                    continue;
                }
                if let Some(guid) = win::guid_to_u128(g) {
                    if let Some(vt) = threads.remove(&guid) {
                        vt.stop.store(true, Ordering::Relaxed);
                    }
                    let _ = tx.try_send(Frame::VolumeGone { guid });
                    let _ = letter;
                }
            }
            last_drives = now;
        }
        // 管道是否还通(主进程退出 → 断开)
        if !pipe_alive(client) {
            break 'session;
        }
    }
    for (_, vt) in threads.drain() {
        vt.stop.store(true, Ordering::Relaxed);
        if let Some(h) = vt.handle {
            let _ = h.join();
        }
    }
    // 先停生产、再断消费者:所有 tx 克隆随线程结束释放,rx.iter() 收尾后 writer 退出
    drop(tx);
    let _ = writer.join();
    // 句柄由 Worker::run 统一关闭
}

/// Hello 令牌校验。worker 由主进程用 `--index-token` 拉起;没带令牌的
/// worker(手动调试直接起)拒绝一切握手,避免暴露一个无鉴权的扫描入口。
fn token_ok(got: [u8; TOKEN_LEN]) -> bool {
    match WORKER_TOKEN.get() {
        Some(expected) => expected == &got,
        None => false,
    }
}

/// 会话起点决策:握手位点仍可用 → 增量;扫描中断 → 续扫;否则全量。
/// `req` = 握手带来的 (journal_id, next_usn, resume_frn, complete);
/// `state` = 扫描前采样的 (journal_id, first_usn, next_usn);None = 卷上没有活动 journal。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanPlan {
    Incremental { journal_id: u64, from_usn: i64 },
    /// 上次全量扫描中断:从 resume_frn 续扫 ENUM,高水位沿用 next_usn,
    /// 扫完接 journal 时跳过已覆盖的区间,不丢扫描窗口内的新建/改名。
    Resume { journal_id: u64, high_usn: i64, resume_frn: u64 },
    Full { journal_id: u64, high_usn: i64 },
}

fn plan_volume_session(
    req: Option<(u64, i64, u64, bool)>,
    state: Option<(u64, i64, i64)>,
) -> ScanPlan {
    if let Some((jid, usn, resume_frn, complete)) = req {
        if complete {
            // H2:jid/usn 都必须有效才能谈增量;jid 来自快照里的真实 journal_id
            if usn > 0 && jid > 0 {
                if let Some((jid_now, first_usn, next_usn)) = state {
                    if jid_now == jid && usn >= first_usn && usn <= next_usn {
                        return ScanPlan::Incremental { journal_id: jid, from_usn: usn };
                    }
                }
            }
        } else if resume_frn > 0 && jid > 0 && usn > 0 {
            // M9:续扫只在 journal 还是同一个且高水位仍在窗口内时成立;
            // 否则回退全量(journal 已重置,旧位点不可信)。
            if let Some((jid_now, first_usn, next_usn)) = state {
                if jid_now == jid && usn >= first_usn && usn <= next_usn {
                    return ScanPlan::Resume { journal_id: jid, high_usn: usn, resume_frn };
                }
            }
        }
    }
    match state {
        Some((jid, _, next_usn)) => ScanPlan::Full { journal_id: jid, high_usn: next_usn },
        None => ScanPlan::Full { journal_id: 0, high_usn: i64::MAX },
    }
}

/// 全量/续扫共用:发 VolumeMeta(带扫描起点高水位)+ ScanBegin,ENUM 期间发 Batch,
/// 完成后发 VolumeReady 并返回 (journal_id, ready_usn) 供 journal 轮询接入。
/// `resume_frn=0` 即从头全量;>0 表示从中断处续扫(M9)。
fn scan_full_or_resume(
    tx: &mpsc::SyncSender<Frame>,
    guid: u128,
    letter: char,
    journal_id: u64,
    high_usn: i64,
    resume_frn: u64,
    est: u64,
    stop: &AtomicBool,
) -> Option<(u64, i64)> {
    // next_usn = 本次扫描的起点高水位:中断后写进快照,重连时用它续扫
    if !send_frame(
        tx,
        Frame::VolumeMeta { guid, letter, journal_id, next_usn: high_usn, est_records: est },
        stop,
    ) {
        return None;
    }
    if !send_frame(tx, Frame::ScanBegin { guid, resume_frn }, stop) {
        return None;
    }
    // 串行闸门:见 SCAN_GATE 注释。guard 持有到本函数返回(即整卷 ENUM 结束)。
    let _scan_slot = SCAN_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut last_report = std::time::Instant::now();
    let result = scan::scan_volume(
        letter,
        resume_frn,
        high_usn,
        stop,
        |frn, est_now, count| {
            if last_report.elapsed() >= Duration::from_millis(500) {
                last_report = std::time::Instant::now();
                let _ = send_frame(tx, Frame::ScanProgress { guid, frn, est: est_now, count }, stop);
            }
        },
        |recs| {
            let _ = send_frame(tx, Frame::Batch { guid, next_usn: 0, recs }, stop);
        },
    );
    match result {
        Ok(Some(count)) => {
            // journal_id==0(卷无活动 journal)时位点无意义 → 保持 0,下次重扫
            let ready_usn = if journal_id == 0 { 0 } else { high_usn };
            if !send_frame(
                tx,
                Frame::VolumeReady { guid, journal_id, next_usn: ready_usn, count },
                stop,
            ) {
                return None;
            }
            Some((journal_id, ready_usn))
        }
        Ok(None) => None, // stop 中止:不宣告 Ready,主进程保持 Scanning
        Err(err) => {
            crate::snapshot::debug_log(&format!("scan failed {letter}: {err}"));
            None // 卷打不开(权限/被拔):主进程端状态停在 Scanning
        }
    }
}

/// dir 模式单卷循环。
///
/// - `sync=false`:首次构建 —— 目录枚举首扫 → VolumeReady → 空闲。
/// - `sync=true`:引擎已有该卷快照(握手 complete=true)—— 后台对账,
///   逐批 patch 现有 store,结束时删除本轮没见到的条目(离线期间被删/移走)。
///
/// 全程不打开卷句柄、不发任何 USN FSCTL —— 这是 2026-11-09 蓝屏后的安全路径。
fn scan_dir_volume(
    letter: char,
    guid: u128,
    sync: bool,
    tx: &mpsc::SyncSender<Frame>,
    stop: &AtomicBool,
) {
    if sync {
        if !send_frame(tx, Frame::DirSyncBegin { guid }, stop) {
            return;
        }
    } else {
        if !send_frame(
            tx,
            Frame::VolumeMeta {
                guid,
                letter,
                journal_id: 0,
                next_usn: 0,
                est_records: 0,
            },
            stop,
        ) {
            return;
        }
        if !send_frame(tx, Frame::ScanBegin { guid, resume_frn: 0 }, stop) {
            return;
        }
    }
    // 与 USN 全量扫描共用串行闸门:多卷首次构建逐卷来,降低 IO/内核压力。
    // 闸门只包住枚举本身:扫描完成后本卷会进入空闲等待,若把 guard 带到
    // 空闲循环里,下一卷会永远等不到锁(实测卡在「构建中(1/4)」)。
    let mut count: u64 = 0;
    let mut last_report = std::time::Instant::now();
    let result = {
        let _scan_slot = SCAN_GATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::dirscan::enumerate_volume(letter, stop, &mut |recs| {
            count += recs.len() as u64;
            let _ = send_frame(tx, Frame::Batch { guid, next_usn: 0, recs }, stop);
            if !sync && last_report.elapsed() >= Duration::from_millis(500) {
                last_report = std::time::Instant::now();
                let _ = send_frame(
                    tx,
                    Frame::ScanProgress {
                        guid,
                        frn: 0,
                        est: 0,
                        count: count.min(u32::MAX as u64) as u32,
                    },
                    stop,
                );
            }
        })
    };
    match result {
        Ok(total) if !stop.load(Ordering::Relaxed) => {
            if sync {
                let _ = send_frame(tx, Frame::DirSyncEnd { guid, ok: true }, stop);
                crate::snapshot::debug_log(&format!(
                    "dirsync {letter}: done count={count} (enumerated={total})"
                ));
            } else if send_frame(
                tx,
                Frame::VolumeReady {
                    guid,
                    journal_id: 0,
                    next_usn: 0,
                    count: count.min(u32::MAX as u64) as u32,
                },
                stop,
            ) {
                crate::snapshot::debug_log(&format!(
                    "dirscan {letter}: ready count={count} (enumerated={total})"
                ));
            }
            // 首扫/对账完成后进入实时 watcher:ReadDirectoryChangesW 通知
            // 父目录变更时重枚举该目录,经 DirChildren 帧 patch 内存索引。
            // 不碰 USN FSCTL(蓝屏后收敛的安全路径)。
            crate::watch::watch_volume(letter, guid, tx, stop);
        }
        Ok(_) => {
            if sync {
                // 被抓停:只清引擎里的 SEEN 临时位,保留快照原样。
                let _ = send_frame(tx, Frame::DirSyncEnd { guid, ok: false }, stop);
            }
        }
        Err(err) => {
            crate::snapshot::debug_log(&format!("dirscan {letter} failed: {err}"));
            if sync {
                let _ = send_frame(tx, Frame::DirSyncEnd { guid, ok: false }, stop);
            }
        }
    }
}

/// 单卷循环:决定全量扫描或增量续传,随后进入 journal 轮询。
fn volume_loop(
    letter: char,
    guid: u128,
    req: Option<(u64, i64, u64, bool)>,
    stop: std::sync::Arc<AtomicBool>,
    tx: mpsc::SyncSender<Frame>,
) {
    // 阶段 1:默认目录枚举首扫,完全不碰卷句柄/USN FSCTL(蓝屏后的安全路径)。
    // 握手 complete=true 表示引擎已加载该卷快照 → 不再整卷重扫,后台对账即可。
    if opts().scan_mode == ScanMode::Dir {
        let sync = matches!(req, Some((_, _, _, true)));
        scan_dir_volume(letter, guid, sync, &tx, &stop);
        return;
    }
    // H2/H4:扫描开始前先取 journal 状态(journal_id / NextUsn),它是本次全量
    // 扫描的高水位;VolumeMeta / VolumeReady 都回传真实 journal_id。
    // 卷句柄全程持有(避免每 50ms 开/关句柄 + 查询的三连内核调用)。
    let vol = match scan::VolumeHandle::open(letter) {
        Ok(v) => v,
        Err(err) => {
            crate::snapshot::debug_log(&format!("volume open failed {letter}: {err}"));
            return;
        }
    };
    let jstate = vol.journal_state();
    let est = vol.ntfs_data().map(|(est, _)| est).unwrap_or(0);
    let (mut journal_id, mut next_usn) = match plan_volume_session(req, jstate) {
        ScanPlan::Incremental { journal_id, from_usn } => {
            if !send_frame(
                &tx,
                Frame::VolumeMeta { guid, letter, journal_id, next_usn: from_usn, est_records: est },
                &stop,
            ) {
                return;
            }
            (journal_id, from_usn)
        }
        ScanPlan::Full { journal_id, high_usn } => {
            let Some(done) =
                scan_full_or_resume(&tx, guid, letter, journal_id, high_usn, 0, est, &stop)
            else {
                return;
            };
            done
        }
        ScanPlan::Resume { journal_id, high_usn, resume_frn } => {
            let Some(done) = scan_full_or_resume(
                &tx, guid, letter, journal_id, high_usn, resume_frn, est, &stop,
            ) else {
                return;
            };
            done
        }
    };

    // journal 轮询(非阻塞读 + 50ms 间隔;事件延迟 ≤100ms)。
    // 空读(含 0 记录返回)必须休眠,否则卷线程忙等烧 CPU(实测抓出)。
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if journal_id == 0 {
            std::thread::sleep(Duration::from_millis(500));
            if let Some((jid, _, _)) = vol.journal_state() {
                journal_id = jid;
            }
            continue;
        }
        match scan::read_journal_handle(&vol, journal_id, next_usn) {
            Ok(Some((recs, next))) => {
                next_usn = next;
                if recs.is_empty() {
                    std::thread::sleep(Duration::from_millis(50));
                } else {
                    for chunk in recs.chunks(4000) {
                        if !send_frame(
                            &tx,
                            Frame::Batch { guid, next_usn, recs: chunk.to_vec() },
                            &stop,
                        ) {
                            return;
                        }
                    }
                }
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(JournalError::Reset) => {
                let _ = send_frame(&tx, Frame::JournalReset { guid }, &stop);
                return; // 主进程清掉该卷;重连握手(位点 0)会触发重扫自愈
            }
            Err(JournalError::VolumeGone) => {
                let _ = send_frame(&tx, Frame::VolumeGone { guid }, &stop);
                return;
            }
            Err(JournalError::Io) => {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// 有界通道发送:满时 5ms 轮询让位;`stop` 置位(停卷/断连)或对端断开则放弃(H7)。
fn send_frame(tx: &mpsc::SyncSender<Frame>, frame: Frame, stop: &AtomicBool) -> bool {
    let mut frame = frame;
    loop {
        match tx.try_send(frame) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(f)) => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                frame = f;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

// ---------- 帧读写(阻塞句柄上的定长前缀协议) ----------

pub fn write_frame_pub(client: HANDLE, frame: &Frame) -> bool {
    let bytes = frame.encode();
    write_all(client, &bytes)
}

fn write_all(h: HANDLE, mut buf: &[u8]) -> bool {
    use windows::Win32::Storage::FileSystem::WriteFile;
    unsafe {
        while !buf.is_empty() {
            let mut written: u32 = 0;
            let ok = WriteFile(
                h,
                Some(buf),
                Some(&mut written),
                None,
            );
            if ok.is_err() || written == 0 {
                return false;
            }
            buf = &buf[written as usize..];
        }
    }
    true
}

pub fn read_frame_pub(client: HANDLE, timeout: Duration) -> Option<Frame> {
    let deadline = std::time::Instant::now() + timeout;
    let mut len_buf = [0u8; 4];
    if !read_exact(client, &mut len_buf, deadline) {
        return None;
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > (8 << 20) {
        return None;
    }
    let mut payload = vec![0u8; len];
    if !read_exact(client, &mut payload, deadline) {
        return None;
    }
    Frame::decode(&payload)
}

fn try_read_frame(client: HANDLE) -> Option<Frame> {
    // 非阻塞试探:peek 命名管道字节量
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Pipes::PeekNamedPipe;
    unsafe {
        let mut avail: u32 = 0;
        if PeekNamedPipe(client, None, 0, None, Some(&mut avail), None).is_err() {
            return Some(Frame::Shutdown); // 管道断开 → 上层结束
        }
        if avail < 4 {
            return None;
        }
        let mut len_buf = [0u8; 4];
        let mut got: u32 = 0;
        if ReadFile(client, Some(&mut len_buf), Some(&mut got), None).is_err() || got < 4 {
            return Some(Frame::Shutdown);
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > (8 << 20) {
            return Some(Frame::Shutdown);
        }
        let mut payload = vec![0u8; len];
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        if !read_exact(client, &mut payload, deadline) {
            return Some(Frame::Shutdown);
        }
        Frame::decode(&payload)
    }
}

fn read_exact(h: HANDLE, mut buf: &mut [u8], deadline: std::time::Instant) -> bool {
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Pipes::PeekNamedPipe;
    unsafe {
        while !buf.is_empty() {
            let mut avail: u32 = 0;
            if PeekNamedPipe(h, None, 0, None, Some(&mut avail), None).is_err() {
                return false;
            }
            if avail == 0 {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let mut got: u32 = 0;
            if ReadFile(h, Some(buf), Some(&mut got), None).is_err() || got == 0 {
                return false;
            }
            buf = &mut buf[got as usize..];
        }
    }
    true
}

fn pipe_alive(client: HANDLE) -> bool {
    use windows::Win32::System::Pipes::PeekNamedPipe;
    unsafe { PeekNamedPipe(client, None, 0, None, None, None).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn plan_uses_real_journal_id_without_snapshot_position() {
        // H2 回归:旧实现回传 journal_id=0,握手 jid>0 永不成立;
        // 现在全量计划直接带真实 jid,VolumeMeta/VolumeReady 也有值。
        assert_eq!(
            plan_volume_session(None, Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
        assert_eq!(
            plan_volume_session(Some((0, 0, 0, false)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
    }

    #[test]
    fn plan_full_when_no_journal() {
        assert_eq!(
            plan_volume_session(None, None),
            ScanPlan::Full { journal_id: 0, high_usn: i64::MAX }
        );
    }

    #[test]
    fn plan_incremental_only_inside_journal_window() {
        assert_eq!(
            plan_volume_session(Some((7, 300, 0, true)), Some((7, 100, 500))),
            ScanPlan::Incremental { journal_id: 7, from_usn: 300 }
        );
        // journal 已重建(jid 变了)→ 全量
        assert_eq!(
            plan_volume_session(Some((8, 300, 0, true)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
        // 位点早于 first_usn / 晚于 next_usn → 全量
        assert_eq!(
            plan_volume_session(Some((7, 99, 0, true)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
        assert_eq!(
            plan_volume_session(Some((7, 501, 0, true)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
    }

    /// M9:扫描中断(complete=false 且 resume_frn>0)且 journal 未重置 → 从断点续扫;
    /// journal 变了或高水位出窗口则退回全量,不能拿失效位点硬续。
    #[test]
    fn plan_resumes_only_with_same_journal_window() {
        assert_eq!(
            plan_volume_session(Some((7, 500, 1200, false)), Some((7, 100, 800))),
            ScanPlan::Resume { journal_id: 7, high_usn: 500, resume_frn: 1200 }
        );
        assert_eq!(
            plan_volume_session(Some((8, 500, 1200, false)), Some((7, 100, 800))),
            ScanPlan::Full { journal_id: 7, high_usn: 800 }
        );
        assert_eq!(
            plan_volume_session(Some((7, 500, 1200, false)), Some((7, 600, 800))),
            ScanPlan::Full { journal_id: 7, high_usn: 800 }
        );
        // resume_frn==0:还没有任何扫描进度,继续按全量处理
        assert_eq!(
            plan_volume_session(Some((7, 500, 0, false)), Some((7, 100, 800))),
            ScanPlan::Full { journal_id: 7, high_usn: 800 }
        );
    }

    #[test]
    fn send_frame_backpressures_then_stops() {
        let (tx, rx) = mpsc::sync_channel::<Frame>(1);
        tx.try_send(Frame::ScanBegin { guid: 0, resume_frn: 0 }).unwrap();
        let stop = AtomicBool::new(true);
        let started = Instant::now();
        assert!(!send_frame(&tx, Frame::ScanBegin { guid: 1, resume_frn: 0 }, &stop));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(rx);
        assert!(!send_frame(
            &tx,
            Frame::ScanBegin { guid: 2, resume_frn: 0 },
            &AtomicBool::new(false)
        ));
    }

    #[test]
    fn send_frame_succeeds_when_room() {
        let (tx, _rx) = mpsc::sync_channel::<Frame>(1);
        assert!(send_frame(
            &tx,
            Frame::ScanBegin { guid: 3, resume_frn: 0 },
            &AtomicBool::new(false)
        ));
    }

    #[test]
    fn token_must_be_present_and_match() {
        // 本测试进程未设置 WORKER_TOKEN(worker_main 才设置):拒绝一切握手
        assert!(!token_ok([0u8; TOKEN_LEN]));
        assert!(!token_ok([0xff; TOKEN_LEN]));
        // 设置后只接受完全相等
        let expected = [0x5a; TOKEN_LEN];
        let _ = WORKER_TOKEN.set(expected);
        assert!(token_ok(expected));
        let mut other = expected;
        other[0] ^= 0xff;
        assert!(!token_ok(other));
    }

    #[test]
    fn bad_hello_expiry_keeps_single_packet_alive() {
        let grace = BAD_HELLO_GRACE.as_nanos() as u64;
        // 尚未记录 / 窗口内:都不退出(单个陌生包打不掉 worker)
        assert!(!bad_hello_expired(0, 0));
        assert!(!bad_hello_expired(1_000, 1_000));
        assert!(!bad_hello_expired(1_000, 1_000 + grace - 1));
        // 超过窗口:退出让位(旧令牌残留 worker 不会永久占着管道)
        assert!(bad_hello_expired(1_000, 1_000 + grace));
        assert!(bad_hello_expired(1_000, 1_000 + grace * 3));
    }
}
