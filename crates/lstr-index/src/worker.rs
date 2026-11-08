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
            if !win::wait_for_client(self.server) {
                // 管道损坏/被关闭:重建后再等
                win::close(self.server);
                let Ok(rebuilt) = win::create_pipe_server(PIPE_NAME) else {
                    std::thread::sleep(Duration::from_secs(2));
                    if let Ok(s) = win::create_pipe_server(PIPE_NAME) {
                        self.server = s;
                    }
                    continue;
                };
                self.server = rebuilt;
                continue;
            }
            if !win::verify_peer(self.server) {
                win::close(self.server);
                let Ok(s) = win::create_pipe_server(PIPE_NAME) else { return 1 };
                self.server = s;
                continue;
            }
            run_session(self.server);
            // 主进程断开(退出/重启):回到 accept。journal 继续在系统里滚,
            // 重连后的握手位点决定增量还是重扫。
        }
    }
}

struct VolumeThread {
    stop: std::sync::Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn run_session(client: HANDLE) {
    // 握手:Hello → Welcome
    let hello = match read_frame_pub(client, Duration::from_secs(30)) {
        Some(Frame::Hello { proto, volumes }) if proto == PROTO_VERSION => volumes,
        _ => {
            win::close(client);
            return;
        }
    };
    if !write_frame_pub(client, &Frame::Welcome { proto: PROTO_VERSION }) {
        win::close(client);
        return;
    }
    let hello_map: HashMap<u128, (u64, i64)> =
        hello.into_iter().map(|(g, j, u)| (g, (j, u))).collect();

    let (tx, rx) = mpsc::channel::<Frame>();
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
    loop {
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
                Frame::Shutdown => break,
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
                let _ = tx.send(Frame::VolumeAppeared { guid, letter: *letter });
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
                    let _ = tx.send(Frame::VolumeGone { guid });
                    let _ = letter;
                }
            }
            last_drives = now;
        }
        // 管道是否还通(主进程退出 → 断开)
        if !pipe_alive(client) {
            break;
        }
    }
    for (_, vt) in threads.drain() {
        vt.stop.store(true, Ordering::Relaxed);
        if let Some(h) = vt.handle {
            let _ = h.join();
        }
    }
    win::close(client);
    // writer 线程随管道关闭自然退出
    drop(writer);
}

/// 单卷循环:决定全量扫描或增量续传,随后进入 journal 轮询。
fn volume_loop(
    letter: char,
    guid: u128,
    req: Option<(u64, i64)>,
    stop: std::sync::Arc<AtomicBool>,
    tx: mpsc::Sender<Frame>,
) {
    let _ = tx.send(Frame::VolumeMeta {
        guid,
        letter,
        journal_id: 0,
        next_usn: 0,
        est_records: 0,
    });
    // 握手位点判定:主进程快照的 next_usn 仍在 journal 范围内 → 增量
    let mut incremental_from: Option<(u64, i64)> = None;
    if let Some((jid, usn)) = req {
        if usn > 0 && jid > 0 {
            if let Ok(vol) = scan::VolumeHandle::open(letter) {
                if let Some((jid_now, first_usn, next_usn)) = vol.journal_state() {
                    if jid_now == jid && usn >= first_usn && usn <= next_usn {
                        incremental_from = Some((jid, usn));
                        let _ = tx.send(Frame::VolumeMeta {
                            guid,
                            letter,
                            journal_id: jid,
                            next_usn: usn,
                            est_records: 0,
                        });
                    }
                }
            }
        }
    }
    let (journal_id, mut next_usn) = match incremental_from {
        Some((jid, usn)) => (jid, usn),
        None => {
            // 全量扫描
            let _ = tx.send(Frame::ScanBegin { guid, resume_frn: 0 });
            let tx2 = tx.clone();
            let mut last_report = std::time::Instant::now();
            let result = scan::scan_volume(
                letter,
                0,
                &stop,
                |frn, est, count| {
                    if last_report.elapsed() >= Duration::from_millis(500) {
                        last_report = std::time::Instant::now();
                        let _ = tx2.send(Frame::ScanProgress { guid, frn, est, count });
                    }
                },
                |recs| {
                    let _ = tx2.send(Frame::Batch { guid, next_usn: 0, recs: recs.to_vec() });
                },
            );
            match result {
                Ok((count, usn)) => {
                    let _ = tx.send(Frame::VolumeReady { guid, journal_id: 0, next_usn: usn, count });
                    (0u64, usn)
                }
                Err(_) => return, // 卷打不开(权限/被拔):主进程端状态停在 Scanning
            }
        }
    };

    // journal 轮询(非阻塞读 + 50ms 间隔;事件延迟 ≤100ms)
    let mut journal_id = journal_id;
    if journal_id == 0 {
        if let Ok(vol) = scan::VolumeHandle::open(letter) {
            if let Some((jid, _, _)) = vol.journal_state() {
                journal_id = jid;
            }
        }
    }
    let mut idle = 0u32;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if journal_id == 0 {
            std::thread::sleep(Duration::from_millis(500));
            if let Ok(vol) = scan::VolumeHandle::open(letter) {
                if let Some((jid, _, _)) = vol.journal_state() {
                    journal_id = jid;
                }
            }
            continue;
        }
        match scan::read_journal(letter, journal_id, next_usn) {
            Ok(Some((recs, next))) => {
                next_usn = next;
                if !recs.is_empty() {
                    for chunk in recs.chunks(4000) {
                        let _ = tx.send(Frame::Batch { guid, next_usn, recs: chunk.to_vec() });
                    }
                }
                idle = 0;
            }
            Ok(None) => {
                idle += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(JournalError::Reset) => {
                let _ = tx.send(Frame::JournalReset { guid });
                return; // 主进程清掉该卷后,WorkerLost/重连会触发重扫;
                        // 简化:直接重新全量扫描(同一会话内自愈)
            }
            Err(JournalError::VolumeGone) => {
                let _ = tx.send(Frame::VolumeGone { guid });
                return;
            }
            Err(JournalError::Io) => {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let _ = idle;
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
