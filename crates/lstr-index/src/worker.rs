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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// 常驻模式(`--index-keep-alive`)下无客户端等待上限:客户端正常退出时
/// 发 `Detach` 而不是 `Shutdown`,worker 保持索引热态等下一次连接——
/// 下次启动免 UAC、免 1~3 分钟重建。30 分钟窗口覆盖「关掉再打开」的日常
/// 使用;超时后仍优雅退出并写 clean-exit 标记(用户注销/关机同样会结束它)。
const KEEP_ALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 退出看门狗宽限:stop 发出后给卷线程这么久退出;到点仍未退则强制结束
/// 进程,保证正常退出几秒内 worker 必退(不锁 exe 挡住重编译)。与主进程
/// WORKER_EXIT_WAIT 同量级;卷线程大多 500ms 内响应 stop,看门狗只是兜底。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// 主进程在会话里显式发过 `Frame::Shutdown` 后置位(含 `try_read_frame`
/// 在管道断开时合成的 Shutdown,以及握手被拒):本次会话结束后不再重建
/// 管道,worker 直接退出。这样 lstr 正常关闭时不会留下常驻的提权进程。
/// 若主进程崩溃且断开恰好落在读帧间隙(合成 Shutdown 未发出),会话循环
/// 由 `pipe_alive` 兜底退出、不置位本标志 —— 只有这种窗口会走管道重建,
/// 等新客户端,最终由 [`IDLE_ACCEPT_TIMEOUT`] 兜底退出。
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
    /// dir 模式下首扫完成后是否继续轮询 journal(backend="usn" 时由
    /// worker_launch_args 下发 `--index-journal`;等价 builtin 的旧手动开关)。
    pub journal: bool,
    /// 只处理这些卷;None = 全部本机固定 NTFS 卷。
    pub volumes: Option<Vec<char>>,
    /// dirscan 批次间隔毫秒(0 = 不限;>0 时每个批次后 sleep,负载整形)。
    pub scan_throttle_ms: u64,
    /// journal 轮询间隔毫秒(usn 增量层;worker 侧二次兜底最小 100)。
    pub journal_poll_ms: u64,
    /// 会话结束后是否继续常驻（客户端 Detach/退出时保留索引热态）。
    /// 常驻时无客户端等待期从 120s 延长到 [`KEEP_ALIVE_IDLE_TIMEOUT`]。
    pub keep_alive: bool,
}

impl Default for WorkerOpts {
    fn default() -> Self {
        Self {
            scan_mode: ScanMode::Dir,
            journal: false,
            volumes: None,
            scan_throttle_ms: 0,
            journal_poll_ms: 500,
            keep_alive: false,
        }
    }
}

static OPTS: OnceLock<WorkerOpts> = OnceLock::new();

fn opts() -> &'static WorkerOpts {
    OPTS.get_or_init(WorkerOpts::default)
}

/// 运行期卷掩码(设置页勾选 → C→W `Frame::SetVolumes`)。
/// 0 = 未设置,此时沿用启动参数 `--index-volumes`(见 [`filter_volumes`])。
/// bit i = 盘符 `'A' + i`,位运算语义统一由 [`crate::volmask`] 定义。
///
/// 语义是**会话级**的:每个新会话开始时复位为 0(见 [`reset_session_volume_mask`]),
/// 客户端连上后再用 `SetVolumes` 覆盖;不跨会话继承上一个主进程的勾选。
static RUNTIME_VOLUME_MASK: AtomicU32 = AtomicU32::new(0);

/// 会话级掩码复位:新会话先回到启动参数 `--index-volumes` 的行为,
/// 避免上一个会话残留的掩码让「本次启动参数里本来不索引的卷」先起
/// volume_loop 扫一段、再收 `VolumeGone`(无谓 churn,engine 还会删快照)。
fn reset_session_volume_mask() {
    RUNTIME_VOLUME_MASK.store(0, Ordering::Relaxed);
}

/// 纯函数:按运行期掩码过滤固定 NTFS 卷列表。
/// mask==0 表示「未设置」,原样返回(是否回落到 `--index-volumes` 由
/// [`filter_volumes`] 决定);不读全局状态,便于单测且不会污染 `opts()`。
fn apply_volume_mask(list: Vec<(char, String)>, mask: u32) -> Vec<(char, String)> {
    if mask == 0 {
        return list;
    }
    list.into_iter()
        .filter(|(letter, _)| crate::volmask::has_letter(mask, *letter))
        .collect()
}

/// 按运行期掩码过滤;掩码未设置(0)时保持 `--index-volumes` 启动参数行为
/// 不变(阶段 1 先在单盘验收)。设置页切换后下一轮 500ms 差异逻辑即生效。
fn filter_volumes(list: Vec<(char, String)>) -> Vec<(char, String)> {
    let mask = RUNTIME_VOLUME_MASK.load(Ordering::Relaxed);
    if mask != 0 {
        return apply_volume_mask(list, mask);
    }
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

/// journal 轮询间隔下限(毫秒):worker 侧二次兜底,与配置归一化一致。
const MIN_POLL_MS: u64 = 100;

/// clean-exit 标记文件名(位于 %APPDATA%\lstr\index\)。
///
/// 崩溃自保护:worker 优雅退出时写,启动时「检查即消费」。启动时缺失 =
/// 上次非正常退出(崩溃/被杀/断电)→ 本次 session 强制纯 dirscan(等价
/// builtin,零 USN FSCTL),日志说明原因;正常退出后自动恢复。
/// 防误伤:worker 被杀(用户关机/升级)属常见 —— 只降级一次 session,
/// 不永久锁死(降级 session 正常退出即重写标记)。
const CLEAN_EXIT_FLAG: &str = "worker_clean_exit";

/// clean-exit 标记所在目录(与快照/engine.log 同一状态目录)。
fn clean_exit_flag_dir() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|base| std::path::PathBuf::from(base).join("lstr").join("index"))
}

/// 检查并清除标记(检查即消费:本次会话结束无论好坏都要重写)。
/// 返回 true = 标记在(上次优雅退出);false = 缺失/无法删除(按缺失处理)。
fn take_clean_exit_flag(dir: &std::path::Path) -> bool {
    let path = dir.join(CLEAN_EXIT_FLAG);
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

/// 写 clean-exit 标记(优雅退出路径)。内容带 unix 毫秒时间戳便于排查。
fn write_clean_exit_flag(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = std::fs::write(dir.join(CLEAN_EXIT_FLAG), format!("clean {now}\n"));
}

/// 低 IO 优先级 RAII:进入即把当前线程切到 Windows 后台模式
/// (`THREAD_MODE_BACKGROUND_BEGIN`,IO 与 CPU 都降到最低档),drop 时恢复。
/// 包住整卷扫描/对账等重负载段;begin 失败(平台不支持)按原优先级继续。
struct IoBackgroundGuard;

impl IoBackgroundGuard {
    fn enter() -> Self {
        let ok = win::thread_io_background_begin();
        if !ok {
            crate::snapshot::debug_log("worker: thread background mode unavailable");
        }
        Self
    }
}

impl Drop for IoBackgroundGuard {
    fn drop(&mut self) {
        win::thread_io_background_end();
    }
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
            "--index-keep-alive" => parsed.keep_alive = true,
            "--index-poll-ms" => {
                let Some(raw) = args.next() else {
                    crate::snapshot::debug_log("worker: missing --index-poll-ms value");
                    return 2;
                };
                match raw.parse::<u64>() {
                    Ok(ms) => parsed.journal_poll_ms = ms.max(MIN_POLL_MS),
                    Err(_) => {
                        crate::snapshot::debug_log(&format!("worker: bad --index-poll-ms {raw}"));
                        return 2;
                    }
                }
            }
            "--index-throttle-ms" => {
                let Some(raw) = args.next() else {
                    crate::snapshot::debug_log("worker: missing --index-throttle-ms value");
                    return 2;
                };
                match raw.parse::<u64>() {
                    Ok(ms) => parsed.scan_throttle_ms = ms,
                    Err(_) => {
                        crate::snapshot::debug_log(&format!("worker: bad --index-throttle-ms {raw}"));
                        return 2;
                    }
                }
            }
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
    // clean-exit 自保护(见模块下方 CLEAN_EXIT_FLAG 文档):上次非正常退出
    // → 本次 session 强制纯 dirscan(等价 builtin,零 USN FSCTL),只降级一次。
    if parsed.journal {
        let unclean = clean_exit_flag_dir()
            .map(|dir| !take_clean_exit_flag(&dir))
            .unwrap_or(false); // 无法定位状态目录时不降级(防环境问题误伤)
        if unclean {
            crate::snapshot::debug_log(
                "worker: clean-exit flag missing (last exit unclean) \
                 → journal disabled this session (pure dirscan)",
            );
            parsed.journal = false;
        }
    }
    let journal = parsed.journal;
    let volumes = parsed.volumes.as_ref().map(|v| {
        v.iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    });
    let _ = OPTS.set(parsed);
    crate::snapshot::debug_log(&format!(
        "worker: opts scan_mode={mode:?} journal={journal} volumes={volumes:?} \
         poll_ms={} throttle_ms={} keep_alive={}",
        opts().journal_poll_ms,
        opts().scan_throttle_ms,
        opts().keep_alive,
    ));
    let Ok(server) = win::create_pipe_server(PIPE_NAME) else {
        // 管道已存在 = 已有 worker 在跑(主进程重拉时可能撞上),静默退出
        // (标记已被消费:残留 worker 若被杀,下次启动仍会正确降级一次)
        return 0;
    };
    let mut worker = Worker { server };
    let code = worker.run();
    // 优雅退出(Shutdown/idle 超时/管道重建失败):写回 clean-exit 标记,
    // 下次启动恢复信任 USN 层。被杀/断电走不到这里 → 下次降级一次 session。
    if let Some(dir) = clean_exit_flag_dir() {
        write_clean_exit_flag(&dir);
    }
    code
}

struct Worker {
    server: HANDLE,
}

impl Worker {
    fn run(&mut self) -> i32 {
        loop {
            // 阻塞的 ConnectNamedPipe 放到独立线程,主线程带超时等待:
            // 无客户端超过空闲上限就关闭句柄退出(兜底清理残留)。
            // 常驻模式用 KEEP_ALIVE_IDLE_TIMEOUT,普通模式 120s。
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
            let idle_timeout = if opts().keep_alive {
                KEEP_ALIVE_IDLE_TIMEOUT
            } else {
                IDLE_ACCEPT_TIMEOUT
            };
            let connected = match connected_rx.recv_timeout(idle_timeout) {
                Ok(v) => v,
                Err(_) => {
                    // 空闲超时:没有客户端。accept 线程仍阻塞在 ConnectNamedPipe,
                    // 不做关闭/join(同步句柄上的取消行为不可靠),直接返回让进程退出
                    // —— worker 是独立进程,main 返回即回收所有线程。
                    crate::snapshot::debug_log(&format!(
                        "worker: idle timeout (keep_alive={}), exiting",
                        opts().keep_alive
                    ));
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
            // M4:主进程显式关闭(含 try_read_frame 在管道断开时合成的
            // Shutdown)/ 握手被拒 → 不重建管道,直接退出。
            if EXIT_AFTER_SESSION.load(Ordering::Relaxed) {
                return 0;
            }
            // 触发窗口:仅在「会话结束但未置位 EXIT_AFTER_SESSION」时到达 ——
            // 即主进程崩溃/被杀,管道断开恰落在 try_read_frame 读帧间隙、由
            // 会话循环末尾的 pipe_alive 兜底检出;或者客户端显式 Detach 常驻。
            // 重建后等新主进程重连;无客户端则空闲上限兜底退出。
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
    //
    // 掩码是会话级的:每次会话回到启动参数 `--index-volumes`,随后由客户端
    // 的 `Frame::SetVolumes` 覆盖。必须放在起初始 volume_loop 之前——否则如果
    // 复用上一个会话残留的掩码,已取消的卷会先起线程扫几百毫秒再收 VolumeGone。
    reset_session_volume_mask();
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
                Frame::Rescan { mask } => {
                    // mask=0 = 重扫全部卷;否则按盘符位掩码选卷(噪声排除规则
                    // 变更后的「立即重建」:盘符锚定的规则只重扫对应盘)。
                    let targets: Vec<(u128, char)> = threads
                        .keys()
                        .copied()
                        .filter_map(|g| {
                            let letter = last_drives
                                .iter()
                                .find(|(_, g2)| win::guid_to_u128(g2) == Some(g))
                                .map(|(l, _)| *l)?;
                            if mask == 0
                                || (letter.is_ascii_alphabetic()
                                    && mask & (1 << (letter.to_ascii_uppercase() as u8 - b'A'))
                                        != 0)
                            {
                                Some((g, letter))
                            } else {
                                None
                            }
                        })
                        .collect();
                    crate::snapshot::debug_log(&format!(
                        "worker: rescan requested mask=0x{mask:08x}, targets={:?}",
                        targets.iter().map(|(_, l)| l).collect::<Vec<_>>()
                    ));
                    for (guid, letter) in targets {
                        if let Some(vt) = threads.get_mut(&guid) {
                            vt.stop.store(true, Ordering::Relaxed);
                            if let Some(h) = vt.handle.take() {
                                let _ = h.join();
                            }
                            let stop = std::sync::Arc::new(AtomicBool::new(false));
                            let tx = tx.clone();
                            let stop2 = stop.clone();
                            let handle = std::thread::Builder::new()
                                .name(format!("lstr-vol-{letter}"))
                                .spawn(move || volume_loop(letter, guid, None, stop2, tx))
                                .ok();
                            threads.insert(guid, VolumeThread { stop, handle });
                            crate::snapshot::debug_log(&format!(
                                "worker: rescan restart vol {letter}"
                            ));
                        }
                    }
                }
                Frame::SetVolumes { mask } => {
                    // 只更新掩码,不在这里起/停线程:下面的 500ms 热插拔差异
                    // 逻辑读 filter_volumes(),同一轮就会按新掩码补发
                    // VolumeAppeared/VolumeGone 并起/停对应 volume_loop。
                    RUNTIME_VOLUME_MASK.store(mask, Ordering::Relaxed);
                    crate::snapshot::debug_log(&format!(
                        "worker: runtime volume mask -> {} (0x{mask:08x})",
                        crate::volmask::describe(mask)
                    ));
                }
                Frame::Shutdown => {
                    EXIT_AFTER_SESSION.store(true, Ordering::Relaxed);
                    break 'session;
                }
                Frame::Detach => {
                    // 客户端正常退出、但要求保留索引服务:结束本会话,
                    // 不置 EXIT_AFTER_SESSION → 外层重建管道等待下一次连接。
                    crate::snapshot::debug_log(
                        "worker: client detached, staying alive for next session",
                    );
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
    // 先给所有卷线程并行发 stop,再 join。旧实现逐个「设 stop→join」,慢退的
    // 线程会拖延后续线程收到 stop,把退出延迟放大到几十秒(实测残留 worker
    // 锁住 exe 挡住重编译)。
    let mut joins: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for (_, vt) in threads.drain() {
        vt.stop.store(true, Ordering::Relaxed);
        if let Some(h) = vt.handle {
            joins.push(h);
        }
    }
    // 仅「真要退出」(EXIT_AFTER_SESSION)时武装看门狗:个别卷线程卡在不可中断
    // IO 不能拖死退出。到点强制结束进程(worker 本就要退,OS 回收线程;clean-exit
    // 标记先写,下次启动不降级)。管道重建场景(主进程崩溃、EXIT_AFTER_SESSION
    // 未置位)不武装,以免误杀下一会话。
    if EXIT_AFTER_SESSION.load(Ordering::Relaxed) {
        std::thread::spawn(|| {
            std::thread::sleep(SHUTDOWN_GRACE);
            if let Some(dir) = clean_exit_flag_dir() {
                write_clean_exit_flag(&dir);
            }
            crate::snapshot::debug_log("worker: shutdown grace expired, force exit");
            std::process::exit(0);
        });
    }
    for h in joins {
        let _ = h.join();
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
    // 低 IO 优先级(RAII):与 dir 路径同规格的负载整形。
    let _scan_slot = SCAN_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _io_bg = IoBackgroundGuard::enter();
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
/// 全程不打开卷句柄、不发任何 USN FSCTL —— 这是 2026-11-09 蓝屏后的安全路径
/// (backend=auto/builtin 恒走这里;backend=usn 走 [`journal_incremental_volume`],
/// 首扫同样复用 [`dir_scan_pass`])。
fn scan_dir_volume(
    letter: char,
    guid: u128,
    sync: bool,
    tx: &mpsc::SyncSender<Frame>,
    stop: &AtomicBool,
) {
    let completed = dir_scan_pass(letter, guid, sync, None, tx, stop);
    // 扫描完整完成后进入实时 watcher:ReadDirectoryChangesW 通知父目录变更
    // 时重枚举该目录,经 DirChildren 帧 patch 内存索引。不碰 USN FSCTL。
    if completed {
        crate::watch::watch_volume(letter, guid, tx, stop);
    }
}

/// dir 枚举一遍(首扫或 DirSync 对账),发对应帧;**不接 watcher** ——
/// 调用方决定扫描完成后的实时层(watch.rs 目录通知 或 USN journal 轮询)。
///
/// - `sync=false`:首扫 —— VolumeMeta → ScanBegin → Batch… → VolumeReady。
/// - `sync=true`:对账 —— DirSyncBegin → Batch… → DirSyncEnd(ok)。
///
/// `usn_cursor`:usn 增量层在**扫描前**采样的 (journal_id, 预扫描 NextUsn)
/// (H4 同理:扫描窗口内的变更由 journal 回放补齐,不丢新建/改名)。
/// 扫描完整完成后:
/// - 首扫:VolumeReady 直接带该游标(引擎持久化,下次启动校验后可免扫直读);
/// - 对账:DirSyncEnd 后补发一条 VolumeReady 重新握手(游标写回引擎);
/// - `None` = 纯 dir 模式,VolumeReady 带 0(既有行为不变)。
///
/// 返回 true = 枚举完整结束(未被 stop 打断且未出错)。
fn dir_scan_pass(
    letter: char,
    guid: u128,
    sync: bool,
    usn_cursor: Option<(u64, i64)>,
    tx: &mpsc::SyncSender<Frame>,
    stop: &AtomicBool,
) -> bool {
    if sync {
        if !send_frame(tx, Frame::DirSyncBegin { guid }, stop) {
            return false;
        }
    } else {
        if !send_frame(
            tx,
            Frame::VolumeMeta {
                guid,
                letter,
                journal_id: usn_cursor.map(|(j, _)| j).unwrap_or(0),
                next_usn: usn_cursor.map(|(_, u)| u).unwrap_or(0),
                est_records: 0,
            },
            stop,
        ) {
            return false;
        }
        if !send_frame(tx, Frame::ScanBegin { guid, resume_frn: 0 }, stop) {
            return false;
        }
    }
    // 与 USN 全量扫描共用串行闸门:多卷首次构建逐卷来,降低 IO/内核压力。
    // 闸门只包住枚举本身:扫描完成后本卷会进入空闲等待,若把 guard 带到
    // 空闲循环里,下一卷会永远等不到锁(实测卡在「构建中(1/4)」)。
    // 低 IO 优先级(RAII):扫描线程的元数据 IO/CPU 降到后台档,与
    // scan_throttle_ms 一起构成负载整形(BSOD 取证的风险模型是高并发负载)。
    let mut count: u64 = 0;
    let mut last_report = std::time::Instant::now();
    let throttle_ms = opts().scan_throttle_ms;
    let result = {
        let _scan_slot = SCAN_GATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _io_bg = IoBackgroundGuard::enter();
        crate::dirscan::enumerate_volume(letter, stop, &mut |recs| {
            count += recs.len() as u64;
            let _ = send_frame(tx, Frame::Batch { guid, next_usn: 0, recs }, stop);
            // 负载整形:批次间限流(0 = 不限;最后一次批次的空 sleep 无害)。
            if throttle_ms > 0 {
                std::thread::sleep(Duration::from_millis(throttle_ms));
            }
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
                // 对账成功后重新握手 USN 游标(journal 生命周期错误自愈路径)。
                if let Some((jid, usn)) = usn_cursor {
                    let _ = send_frame(
                        tx,
                        Frame::VolumeReady { guid, journal_id: jid, next_usn: usn, count: 0 },
                        stop,
                    );
                }
            } else if send_frame(
                tx,
                Frame::VolumeReady {
                    guid,
                    journal_id: usn_cursor.map(|(j, _)| j).unwrap_or(0),
                    next_usn: usn_cursor.map(|(_, u)| u).unwrap_or(0),
                    count: count.min(u32::MAX as u64) as u32,
                },
                stop,
            ) {
                crate::snapshot::debug_log(&format!(
                    "dirscan {letter}: ready count={count} (enumerated={total})"
                ));
            }
            true
        }
        Ok(_) => {
            if sync {
                // 被抓停:只清引擎里的 SEEN 临时位,保留快照原样。
                let _ = send_frame(tx, Frame::DirSyncEnd { guid, ok: false }, stop);
            }
            false
        }
        Err(err) => {
            crate::snapshot::debug_log(&format!("dirscan {letter} failed: {err}"));
            if sync {
                let _ = send_frame(tx, Frame::DirSyncEnd { guid, ok: false }, stop);
            }
            false
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
        if opts().journal {
            // backend="usn" 增量层:dirscan 首扫 → USN journal 轮询
            // (熔断器/游标校验/clean-exit 护栏见 usnwatch 模块与 worker_main)。
            journal_incremental_volume(letter, guid, req, &tx, &stop);
            return;
        }
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

// ---------- USN 实时增量层(backend="usn";dirscan 首扫 → journal 轮询) ----------

/// dir+journal 会话计划:快照 complete 且游标可信 → 免扫直读;否则先扫一遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirUsnPlan {
    /// 游标可信:直接从 from_usn 进入 journal 轮询(离线窗口由 journal 覆盖)。
    Watch { journal_id: u64, from_usn: i64 },
    /// 先跑一遍 dirscan:sync=true = 引擎已有快照,走 DirSync 对账(删离线消失项);
    /// sync=false = 全新首扫。
    FullScan { sync: bool },
}

/// 纯函数:决定 dir+usn 会话从哪开始。
/// 校验规则与 [`plan_volume_session`] 的 H2 一致:journal_id 匹配且
/// FirstUsn <= saved_usn <= NextUsn 才信任游标(否则回退全量)。
fn plan_dir_usn_session(
    req: Option<(u64, i64, u64, bool)>,
    live: Option<(u64, i64, i64)>,
) -> DirUsnPlan {
    if let Some((jid, usn, _resume_frn, true)) = req {
        if crate::usnwatch::cursor_trusted(jid, usn, live) {
            return DirUsnPlan::Watch { journal_id: jid, from_usn: usn };
        }
    }
    DirUsnPlan::FullScan { sync: matches!(req, Some((_, _, _, true))) }
}

/// backend="usn" 的单卷增量层:dirscan 首扫 → journal 轮询(带熔断)。
///
/// 流程:
/// 1. 开卷句柄(journal FSCTL 用);打不开 → 退回纯 dirscan 安全路径(不丢索引);
/// 2. 快照 complete 且游标可信 → 免扫,直接从游标进入轮询;
/// 3. 否则扫描前采样 (journal_id, NextUsn) 作为高水位 → dirscan(首扫或对账)
///    → VolumeReady 宣告游标(引擎持久化进快照,下次启动校验后免扫直读)
///    → 进入轮询。
fn journal_incremental_volume(
    letter: char,
    guid: u128,
    req: Option<(u64, i64, u64, bool)>,
    tx: &mpsc::SyncSender<Frame>,
    stop: &AtomicBool,
) {
    let poll_ms = opts().journal_poll_ms.max(MIN_POLL_MS);
    let vol = match scan::VolumeHandle::open(letter) {
        Ok(v) => v,
        Err(err) => {
            // 卷打不开(权限/被拔):退回纯 dirscan + 目录 watcher,索引能力不丢
            crate::snapshot::debug_log(&format!("usn {letter}: volume open failed ({err}), fallback to dirscan"));
            let sync = matches!(req, Some((_, _, _, true)));
            scan_dir_volume(letter, guid, sync, tx, stop);
            return;
        }
    };
    let live = vol.journal_state();
    match plan_dir_usn_session(req, live) {
        DirUsnPlan::Watch { journal_id, from_usn } => {
            crate::snapshot::debug_log(&format!(
                "usn {letter}: cursor trusted (jid={journal_id} usn={from_usn}), skip scan"
            ));
            usn_watch_loop(&vol, letter, guid, journal_id, from_usn, tx, stop, poll_ms);
        }
        DirUsnPlan::FullScan { sync } => {
            // 预扫描采样 journal 高水位:扫描窗口内的新建/改名由 journal 回放补齐(H4)
            let cursor = live.map(|(jid, _first, next)| (jid, next));
            crate::snapshot::debug_log(&format!(
                "usn {letter}: {} scan then journal poll (cursor={cursor:?})",
                if sync { "dirsync" } else { "full dirscan" },
            ));
            dir_scan_pass(letter, guid, sync, cursor, tx, stop);
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let (jid, usn) = cursor.unwrap_or((0, 0));
            usn_watch_loop(&vol, letter, guid, jid, usn, tx, stop, poll_ms);
        }
    }
}

/// 可中断的退避休眠:按 ≤100ms 切片轮询 stop,置位即提前返回。
/// 熔断退避封顶 60s([`crate::usnwatch::UsnCircuit::BACKOFF_MAX_MS`]),
/// 不可中断的 sleep 会把 Shutdown 卡住最坏 60s(卷线程 join 等待)。
fn sleep_interruptible(ms: u64, stop: &AtomicBool) {
    let mut remaining = ms;
    while remaining > 0 {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let slice = remaining.min(100);
        std::thread::sleep(Duration::from_millis(slice));
        remaining -= slice;
    }
}

/// journal 实时增量轮询(每卷独立熔断;空读必须休眠 —— 历史踩坑:忙等烧 CPU)。
///
/// - `journal_id == 0`:卷上暂无活动 journal,循环内轻量 QUERY 等待;
///   激活后宣告新游标(此时目录态刚扫完,无回放缺口)再开始读。
/// - `JournalError::Reset`(0x19C/19D/19E):立即 DirSync 对账,成功后取新
///   journal 状态重新握手(新游标),失败按熔断退避;连续失败达上限 → 本
///   session 停用该卷(记日志后结束线程,不重试风暴)。
/// - 其他 IO 错误:按 1s→2s→4s→…→60s 指数退避重试。
fn usn_watch_loop(
    vol: &scan::VolumeHandle,
    letter: char,
    guid: u128,
    mut journal_id: u64,
    mut next_usn: i64,
    tx: &mpsc::SyncSender<Frame>,
    stop: &AtomicBool,
    poll_ms: u64,
) {
    let mut cb = crate::usnwatch::UsnCircuit::new();
    let mut reported = Option::<crate::usnwatch::UsnWatchState>::None;
    crate::snapshot::debug_log(&format!(
        "usn {letter}: watch loop enter jid={journal_id} usn={next_usn} poll={poll_ms}ms"
    ));
    loop {
        if stop.load(Ordering::Relaxed) {
            crate::snapshot::debug_log(&format!("usn {letter}: watch stopped ({:?})", cb.state));
            return;
        }
        if journal_id == 0 {
            // journal 未激活:仅 QUERY(轻量 FSCTL),激活后宣告游标再开始读
            match vol.journal_state() {
                Some((jid, _first, next)) => {
                    journal_id = jid;
                    next_usn = next;
                    let _ = send_frame(
                        tx,
                        Frame::VolumeReady { guid, journal_id: jid, next_usn: next, count: 0 },
                        stop,
                    );
                    cb.on_event(crate::usnwatch::UsnEvent::PollOk);
                    report_usn_state(tx, guid, cb.state, stop, &mut reported);
                    crate::snapshot::debug_log(&format!(
                        "usn {letter}: journal now active jid={jid} usn={next}"
                    ));
                }
                None => std::thread::sleep(Duration::from_millis(poll_ms)),
            }
            continue;
        }
        match scan::read_journal_masked(vol, journal_id, next_usn, crate::usnwatch::REASON_MASK_WATCH) {
            Ok(Some((recs, next))) => {
                cb.on_event(crate::usnwatch::UsnEvent::PollOk);
                report_usn_state(tx, guid, cb.state, stop, &mut reported);
                next_usn = next;
                if recs.is_empty() {
                    // 位点已到最新:空读必须休眠,否则卷线程忙等烧 CPU(实测踩坑)
                    std::thread::sleep(Duration::from_millis(poll_ms));
                } else {
                    for chunk in recs.chunks(4000) {
                        if !send_frame(
                            tx,
                            Frame::Batch { guid, next_usn, recs: chunk.to_vec() },
                            stop,
                        ) {
                            return;
                        }
                    }
                }
            }
            Ok(None) => {
                cb.on_event(crate::usnwatch::UsnEvent::PollOk);
                report_usn_state(tx, guid, cb.state, stop, &mut reported);
                std::thread::sleep(Duration::from_millis(poll_ms));
            }
            Err(scan::JournalError::Reset) => {
                crate::snapshot::debug_log(&format!(
                    "usn {letter}: journal reset (entry deleted / not active), scheduling dirsync"
                ));
                match cb.on_event(crate::usnwatch::UsnEvent::JournalGone) {
                    crate::usnwatch::UsnDirective::RunDirSync => {
                        report_usn_state(tx, guid, cb.state, stop, &mut reported);
                        // 复用既有 dirscan 对账路径(串行闸门 + 低 IO 优先级内建)
                        let ok = dir_scan_pass(letter, guid, true, None, tx, stop);
                        let directive = cb.on_event(crate::usnwatch::UsnEvent::DirSyncDone { ok });
                        if ok {
                            // 重新握手:取新 journal 状态,宣告新游标后继续轮询
                            match vol.journal_state() {
                                Some((jid, _first, next)) => {
                                    journal_id = jid;
                                    next_usn = next;
                                    let _ = send_frame(
                                        tx,
                                        Frame::VolumeReady {
                                            guid,
                                            journal_id: jid,
                                            next_usn: next,
                                            count: 0,
                                        },
                                        stop,
                                    );
                                    crate::snapshot::debug_log(&format!(
                                        "usn {letter}: re-handshaked jid={jid} usn={next}"
                                    ));
                                }
                                None => {
                                    // journal 又没了:回到「等待激活」分支(游标清零)
                                    journal_id = 0;
                                    next_usn = 0;
                                }
                            }
                        }
                        match directive {
                            crate::usnwatch::UsnDirective::Sleep(ms) => {
                                sleep_interruptible(ms, stop);
                            }
                            crate::usnwatch::UsnDirective::Disable => {
                                report_usn_state(tx, guid, cb.state, stop, &mut reported);
                                crate::snapshot::debug_log(&format!(
                                    "usn {letter}: disabled for this session (repeated dirsync failures)"
                                ));
                                return;
                            }
                            _ => {}
                        }
                    }
                    // 熔断器自锁:Disabled 后任何事件仍返回 Disable;理论上
                    // JournalGone 不会走到这里,兜底防重试风暴。
                    crate::usnwatch::UsnDirective::Disable => {
                        report_usn_state(tx, guid, cb.state, stop, &mut reported);
                        crate::snapshot::debug_log(&format!("usn {letter}: disabled for this session"));
                        return;
                    }
                    other => {
                        crate::snapshot::debug_log(&format!("usn {letter}: unexpected directive {other:?}"));
                        return;
                    }
                }
            }
            Err(scan::JournalError::VolumeGone) => {
                // 卷消失:通知引擎清掉该卷,线程退出(热插拔差异逻辑兜底)
                let _ = tx.try_send(Frame::VolumeGone { guid });
                return;
            }
            Err(scan::JournalError::Io) => {
                crate::snapshot::debug_log(&format!(
                    "usn {letter}: io error (failures={})",
                    cb.failures + 1
                ));
                match cb.on_event(crate::usnwatch::UsnEvent::PollError) {
                    crate::usnwatch::UsnDirective::Sleep(ms) => {
                        report_usn_state(tx, guid, cb.state, stop, &mut reported);
                        sleep_interruptible(ms, stop);
                    }
                    crate::usnwatch::UsnDirective::Disable => {
                        report_usn_state(tx, guid, cb.state, stop, &mut reported);
                        crate::snapshot::debug_log(&format!(
                            "usn {letter}: disabled for this session (repeated io failures)"
                        ));
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 状态变化才上报(`UsnWatch` 帧);同一状态不重复发,避免通道噪音。
fn report_usn_state(
    tx: &mpsc::SyncSender<Frame>,
    guid: u128,
    state: crate::usnwatch::UsnWatchState,
    stop: &AtomicBool,
    last: &mut Option<crate::usnwatch::UsnWatchState>,
) {
    if *last != Some(state) {
        *last = Some(state);
        let _ = send_frame(tx, Frame::UsnWatch { guid, state }, stop);
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

/// 非阻塞试探读一帧(peek 命名管道字节量)。返回 None = 暂无数据;
/// 管道断开/帧异常时返回合成帧 `Some(Frame::Shutdown)` —— 会话循环据此
/// 置位 EXIT_AFTER_SESSION 并退出(与主进程显式发 Shutdown 同路径,
/// 回到 Worker::run 后不再重建管道)。注意主进程崩溃若恰落在两次
/// try_read_frame 之间,也可能由会话循环末尾的 pipe_alive 先检出
/// (那条路径不置位 EXIT_AFTER_SESSION,是管道重建分支的触发窗口)。
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

    /// 熔断退避(封顶 60s)必须可中断:stop 置位后秒级内返回,
    /// 否则 Shutdown 会被卷线程的退避 sleep 卡住最坏 60s。
    #[test]
    fn backoff_sleep_is_interruptible_by_stop() {
        // 已置位:60s 退避立即返回
        let stop = AtomicBool::new(true);
        let t0 = Instant::now();
        sleep_interruptible(60_000, &stop);
        assert!(t0.elapsed() < Duration::from_millis(500), "stop 已置位必须立即返回");

        // 中途置位:切片粒度 100ms,60s 退避应在数百 ms 内返回
        // (断言上限放宽到 2s 防测试机调度抖动;实际 ~150ms,关键是绝不接近 60s)
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let setter = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                stop.store(true, Ordering::Relaxed);
            })
        };
        let t0 = Instant::now();
        sleep_interruptible(60_000, &stop);
        setter.join().unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "中途置位应在切片粒度内返回,实测 {:?}",
            t0.elapsed()
        );

        // 未置位:完整睡满(不少睡 —— 退避节奏是熔断语义的一部分)
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        sleep_interruptible(30, &stop);
        assert!(t0.elapsed() >= Duration::from_millis(25), "未置位不得提前返回");
    }

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

    /// 运行期掩码过滤(纯函数,不碰全局 `opts()`/`RUNTIME_VOLUME_MASK`)。
    /// mask 只留 C 时 D 被过滤;mask==0 原样返回 = 回落到 `--index-volumes`
    /// 的既有行为(默认未设置时即不过滤)。
    #[test]
    fn apply_volume_mask_filters_unselected_letters() {
        let list = vec![
            ('C', r"\\?\Volume{cccc}\".to_string()),
            ('D', r"\\?\Volume{dddd}\".to_string()),
        ];
        // 只选 C:D 被过滤
        assert_eq!(
            apply_volume_mask(list.clone(), crate::volmask::mask_from_letters(['C'])),
            vec![list[0].clone()]
        );
        // 只选 D:反向验证不是「按序号保留」
        assert_eq!(
            apply_volume_mask(list.clone(), crate::volmask::mask_from_letters(['d'])),
            vec![list[1].clone()]
        );
        // 全选:都保留
        assert_eq!(
            apply_volume_mask(list.clone(), crate::volmask::MASK_ALL),
            list
        );
        // 0 = 未设置:原样返回,由 filter_volumes 回落到 opts().volumes
        assert_eq!(apply_volume_mask(list.clone(), 0), list);
        // 掩码里的不存在的盘符不影响已有卷(空结果也是合法过滤结果)
        let only_z = crate::volmask::mask_from_letters(['Z']);
        assert!(apply_volume_mask(list, only_z).is_empty());
    }

    /// 会话级复位:先锁定掩码(只剩 C),复位后 `filter_volumes` 回到启动参数路径。
    /// 本测试进程从不调用 `worker_main`,故 `opts().volumes` 固定为默认 `None`
    /// (不过滤),断言「复位后 D 回到列表」即可证明掩码已不再是过滤依据;
    /// 测试只动 `RUNTIME_VOLUME_MASK` 这一个静态,不写 `OPTS`(OnceLock 不可逆)。
    #[test]
    fn session_reset_restores_startup_volume_behavior() {
        let list = vec![
            ('C', r"\\?\Volume{cccc}\".to_string()),
            ('D', r"\\?\Volume{dddd}\".to_string()),
        ];
        // 模拟上个会话中途勾掉 D:掩码只剩 C,D 被过滤
        RUNTIME_VOLUME_MASK.store(crate::volmask::mask_from_letters(['C']), Ordering::Relaxed);
        assert_eq!(filter_volumes(list.clone()), vec![list[0].clone()]);
        // 新会话入口复位 → 掩码未设置,回落到启动参数(= 本次 opts().volumes)
        reset_session_volume_mask();
        assert_eq!(RUNTIME_VOLUME_MASK.load(Ordering::Relaxed), 0);
        assert_eq!(filter_volumes(list.clone()), list);
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

    /// dir+journal 会话计划:游标可信 → 免扫直读;journal_id 不匹配/越界/
    /// 回退(无活动 journal)→ 先扫一遍(有快照走对账,无快照全新首扫)。
    #[test]
    fn dir_usn_plan_trusts_only_valid_cursor() {
        let live = Some((7u64, 100i64, 500i64));
        assert_eq!(
            plan_dir_usn_session(Some((7, 300, 0, true)), live),
            DirUsnPlan::Watch { journal_id: 7, from_usn: 300 }
        );
        // journal 已重建(jid 不匹配)→ 引擎有快照,走对账而非清空重扫
        assert_eq!(
            plan_dir_usn_session(Some((8, 300, 0, true)), live),
            DirUsnPlan::FullScan { sync: true }
        );
        // 越界:早于 FirstUsn / 晚于 NextUsn
        assert_eq!(
            plan_dir_usn_session(Some((7, 99, 0, true)), live),
            DirUsnPlan::FullScan { sync: true }
        );
        assert_eq!(
            plan_dir_usn_session(Some((7, 501, 0, true)), live),
            DirUsnPlan::FullScan { sync: true }
        );
        // 回退:卷上无活动 journal → 全量
        assert_eq!(
            plan_dir_usn_session(Some((7, 300, 0, true)), None),
            DirUsnPlan::FullScan { sync: true }
        );
        // 无快照(complete=false)→ 全新首扫(即使位点碰巧有效也不跳过)
        assert_eq!(
            plan_dir_usn_session(Some((7, 300, 0, false)), live),
            DirUsnPlan::FullScan { sync: false }
        );
        assert_eq!(plan_dir_usn_session(None, live), DirUsnPlan::FullScan { sync: false });
    }

    /// clean-exit 标记语义:缺失=上次非正常退出(降级);写→存在;检查即消费。
    #[test]
    fn clean_exit_flag_take_once() {
        let dir = std::env::temp_dir().join(format!("lstr-worker-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!take_clean_exit_flag(&dir), "缺失 → 按上次非正常退出处理");
        write_clean_exit_flag(&dir);
        assert!(take_clean_exit_flag(&dir), "优雅退出写回 → 本 session 信任 USN");
        assert!(!take_clean_exit_flag(&dir), "消费后必须消失:worker 被杀 → 下次降级一次");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
