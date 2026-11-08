//! worker:`lstr.exe --index-worker` 的进程主体(提权、无窗口、常驻)。
//!
//! 职责:读卷(ENUM/journal),经命名管道把记录流推给主进程。
//! 无状态约定:每卷的回放位点由主进程在握手里下发(journal_id + next_usn),
//! worker 自身不落盘、不持有索引。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;

use crate::proto::Frame;
use crate::scan::{self, JournalError};
use crate::win;
use crate::{PROTO_VERSION, PIPE_NAME};

/// 帧通道容量:批帧最大 4000 条,32 帧 ≈ 十余 MB 上限(背压防扫描期积压)。
const FRAME_CHANNEL_CAP: usize = 32;

/// 主进程在会话里显式发过 `Frame::Shutdown` 后置位:本次会话结束后
/// 不再重建管道,worker 直接退出。这样 lstr 正常关闭时不会留下常驻的
/// 提权进程;若主进程崩溃(没来得及发 Shutdown),worker 仍会留在管道上
/// 等下一次连接——这是有意的兜底,代价是极少数情况下残留一个空闲进程。
static EXIT_AFTER_SESSION: AtomicBool = AtomicBool::new(false);

/// worker 入口(lstr main 检测到 --index-worker 后调用)。不返回。
pub fn worker_main() -> i32 {
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
            if win::wait_for_client(self.server) && win::verify_peer(self.server) {
                run_session(self.server);
            }
            // 会话结束(主进程断开/握手失败):句柄只在这里关闭一次。
            // run_session 只借用句柄、不关闭,外层不再复用已关闭的句柄(H5)。
            win::close(self.server);
            self.server = HANDLE(std::ptr::null_mut());
            // M4:主进程显式关闭 → 不重建管道,直接退出。
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
    // 握手:Hello → Welcome
    let hello = match read_frame_pub(client, Duration::from_secs(30)) {
        Some(Frame::Hello { proto, volumes }) if proto == PROTO_VERSION => volumes,
        _ => return,
    };
    if !write_frame_pub(client, &Frame::Welcome { proto: PROTO_VERSION }) {
        return;
    }
    let hello_map: HashMap<u128, (u64, i64)> =
        hello.into_iter().map(|(g, j, u)| (g, (j, u))).collect();

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
    let volumes = win::fixed_ntfs_volumes();
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
        let now = win::fixed_ntfs_volumes();
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

/// 会话起点决策:握手位点仍可用 → 增量;否则全量。
/// `state` = 扫描前采样的 (journal_id, first_usn, next_usn);None = 卷上没有活动 journal。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanPlan {
    Incremental { journal_id: u64, from_usn: i64 },
    Full { journal_id: u64, high_usn: i64 },
}

fn plan_volume_session(req: Option<(u64, i64)>, state: Option<(u64, i64, i64)>) -> ScanPlan {
    if let Some((jid, usn)) = req {
        // H2:jid/usn 都必须有效才能谈增量;jid 来自快照里的真实 journal_id
        if usn > 0 && jid > 0 {
            if let Some((jid_now, first_usn, next_usn)) = state {
                if jid_now == jid && usn >= first_usn && usn <= next_usn {
                    return ScanPlan::Incremental { journal_id: jid, from_usn: usn };
                }
            }
        }
    }
    match state {
        Some((jid, _, next_usn)) => ScanPlan::Full { journal_id: jid, high_usn: next_usn },
        None => ScanPlan::Full { journal_id: 0, high_usn: i64::MAX },
    }
}

/// 单卷循环:决定全量扫描或增量续传,随后进入 journal 轮询。
fn volume_loop(
    letter: char,
    guid: u128,
    req: Option<(u64, i64)>,
    stop: std::sync::Arc<AtomicBool>,
    tx: mpsc::SyncSender<Frame>,
) {
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
            if !send_frame(
                &tx,
                Frame::VolumeMeta { guid, letter, journal_id, next_usn: 0, est_records: est },
                &stop,
            ) {
                return;
            }
            if !send_frame(&tx, Frame::ScanBegin { guid, resume_frn: 0 }, &stop) {
                return;
            }
            let mut last_report = std::time::Instant::now();
            let result = scan::scan_volume(
                letter,
                0,
                high_usn,
                &stop,
                |frn, est_now, count| {
                    if last_report.elapsed() >= Duration::from_millis(500) {
                        last_report = std::time::Instant::now();
                        let _ = send_frame(
                            &tx,
                            Frame::ScanProgress { guid, frn, est: est_now, count },
                            &stop,
                        );
                    }
                },
                |recs| {
                    let _ = send_frame(&tx, Frame::Batch { guid, next_usn: 0, recs }, &stop);
                },
            );
            match result {
                Ok(Some(count)) => {
                    // journal_id==0(卷无活动 journal)时位点无意义 → 保持 0,下次重扫
                    let ready_usn = if journal_id == 0 { 0 } else { high_usn };
                    if !send_frame(
                        &tx,
                        Frame::VolumeReady { guid, journal_id, next_usn: ready_usn, count },
                        &stop,
                    ) {
                        return;
                    }
                    (journal_id, ready_usn)
                }
                Ok(None) => return, // stop 中止:不宣告 Ready,主进程保持 Scanning
                Err(err) => {
                    crate::snapshot::debug_log(&format!("scan failed {letter}: {err}"));
                    return; // 卷打不开(权限/被拔):主进程端状态停在 Scanning
                }
            }
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
            plan_volume_session(Some((0, 0)), Some((7, 100, 500))),
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
            plan_volume_session(Some((7, 300)), Some((7, 100, 500))),
            ScanPlan::Incremental { journal_id: 7, from_usn: 300 }
        );
        // journal 已重建(jid 变了)→ 全量
        assert_eq!(
            plan_volume_session(Some((8, 300)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
        // 位点早于 first_usn / 晚于 next_usn → 全量
        assert_eq!(
            plan_volume_session(Some((7, 99)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
        );
        assert_eq!(
            plan_volume_session(Some((7, 501)), Some((7, 100, 500))),
            ScanPlan::Full { journal_id: 7, high_usn: 500 }
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
}
