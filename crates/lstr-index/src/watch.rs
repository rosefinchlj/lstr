//! 运行期实时变更:`ReadDirectoryChangesW`(纯 Win32 变更通知,不碰 USN FSCTL)。
//!
//! 为什么不直接用通知里的路径增量:通知只给「相对路径 + 动作码」,没有 FRN;
//! 而 store 以 FRN 为键。这里采用「父目录刷新」策略:某条通知到达 → 枚举它
//! 所在的**父目录一层**(`NtQueryDirectoryFile`,与首扫同一条安全路径),把最新
//! 子项集合作为 [`Frame::DirChildren`] 发给引擎替换该目录的子项。
//!
//! - 文件新增/删除/改名/改属性/写大小,都会触发父目录刷新,一次刷新覆盖该目录
//!   本轮所有变更(去重);
//! - 卷根(`parent_frn=5`)的删除无法与未消解条目区分,只做 upsert,删除等下次
//!   启动对账修正(见 `store::apply_children`);
//! - 若通知缓冲溢出(`ERROR_NOTIFY_ENUM_DIR`),退化为整卷对账(DirSync),
//!   保证不丢事件。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::dirscan::{self, ROOT_FRN};
use crate::proto::Frame;

const NOTIFY_BUF: usize = 64 << 10;
const WAIT_MS: u32 = 500;
/// A12:枚举父目录被拒(权限/暂时不可访问)后的冷却时间。期间同一目录的
/// 后续通知直接跳过,不反复重试、不重复写日志;冷却结束再试一次。
const DENIED_RETRY_COOLDOWN: Duration = Duration::from_secs(60);
/// A12:被拒目录冷却表的容量上限,超过后整体清空(避免长跑无限增长)。
const DENIED_MAP_CAP: usize = 1024;

/// A12:该目录是否仍在「拒绝访问」冷却期内。
fn denied_recently(denied: &HashMap<String, Instant>, path: &str, now: Instant) -> bool {
    denied
        .get(path)
        .is_some_and(|&at| now.saturating_duration_since(at) < DENIED_RETRY_COOLDOWN)
}

fn notify_filter() -> windows::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE {
    FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
}

fn open_root(letter: char) -> Result<windows::Win32::Foundation::HANDLE, String> {
    let path = format!(r"\\?\{letter}:\");
    let wide = crate::win::to_wide(&path);
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0x0001, // FILE_LIST_DIRECTORY
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .map_err(|e| format!("watch open {path}: {e}"))
    }
}

/// 解析 `FILE_NOTIFY_INFORMATION` 链,返回相对卷根的路径(UTF-8,`\` 分隔)。
///
/// 纯函数,便于单测:通知结构 = 头部 12 字节(next u32 / action u32 /
/// name_len u32)+ UTF-16 名字,4 字节对齐。
pub fn parse_notify(buffer: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 12 <= buffer.len() {
        let next = u32::from_le_bytes(buffer[off..off + 4].try_into().unwrap()) as usize;
        let name_len =
            u32::from_le_bytes(buffer[off + 8..off + 12].try_into().unwrap()) as usize;
        let name_start = off + 12;
        if name_len == 0 || name_len % 2 != 0 || name_start + name_len > buffer.len() {
            break;
        }
        let units: Vec<u16> = (0..name_len / 2)
            .map(|i| {
                u16::from_le_bytes(
                    buffer[name_start + i * 2..name_start + i * 2 + 2]
                        .try_into()
                        .unwrap(),
                )
            })
            .collect();
        out.push(String::from_utf16_lossy(&units));
        if next == 0 || next >= buffer.len() {
            break;
        }
        off += next;
    }
    out
}

/// 相对路径 → (父目录相对路径, 是否在卷根)。
fn parent_rel(rel: &str) -> (&str, bool) {
    match rel.rfind('\\') {
        Some(i) => (&rel[..i], false),
        None => ("", true),
    }
}

/// 把一批相对路径规约成需要刷新的父目录集合(去重,保持确定顺序)。
pub fn parents_to_refresh(paths: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for rel in paths {
        let (parent, at_root) = parent_rel(rel);
        let key = if at_root { String::new() } else { parent.to_string() };
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

/// 刷新一个父目录:枚举一层子项并发送 `DirChildren`。
fn refresh_parent(
    letter: char,
    guid: u128,
    parent_rel: &str,
    tx: &SyncSender<Frame>,
    stop: &AtomicBool,
    denied: &mut HashMap<String, Instant>,
) {
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let (dir_path, parent_frn) = if parent_rel.is_empty() {
        (format!("{letter}:\\"), ROOT_FRN)
    } else {
        let path = format!("{letter}:\\{parent_rel}");
        let Some(frn) = dirscan::file_id_of(&path) else {
            return; // 父目录已被删除/改名:交给后续通知或下次对账
        };
        (path, frn)
    };
    // A12:该目录刚被拒绝过,冷却期内直接跳过,避免每个通知都重试+刷日志。
    if denied_recently(denied, &dir_path, Instant::now()) {
        return;
    }
    match dirscan::enumerate_children(&dir_path, parent_frn, stop) {
        Ok(recs) => {
            denied.remove(&dir_path);
            let _ = tx.send(Frame::DirChildren { guid, parent_frn, recs });
        }
        Err(err) => {
            if denied.len() >= DENIED_MAP_CAP {
                denied.clear();
            }
            denied.insert(dir_path.clone(), Instant::now());
            crate::snapshot::debug_log(&format!("watch refresh {dir_path}: {err}"));
        }
    }
}

/// 通知缓冲溢出后的兜底:整卷对账(不删未扫到的,除非枚举完整)。
fn full_resync(letter: char, guid: u128, tx: &SyncSender<Frame>, stop: &AtomicBool) {
    if stop.load(Ordering::Relaxed) {
        return;
    }
    if tx.send(Frame::DirSyncBegin { guid }).is_err() {
        return;
    }
    let mut count = 0u64;
    let result = dirscan::enumerate_volume(letter, stop, &mut |recs| {
        count += recs.len() as u64;
        let _ = tx.send(Frame::Batch { guid, next_usn: 0, recs });
    });
    let ok = result.is_ok() && !stop.load(Ordering::Relaxed);
    let _ = tx.send(Frame::DirSyncEnd { guid, ok });
    crate::snapshot::debug_log(&format!(
        "watch {letter}: overflow resync done count={count} ok={ok}"
    ));
}

/// 单卷 watcher 主循环:直到 `stop` 置位。
pub fn watch_volume(letter: char, guid: u128, tx: &SyncSender<Frame>, stop: &AtomicBool) {
    let dir = match open_root(letter) {
        Ok(h) => h,
        Err(err) => {
            crate::snapshot::debug_log(&format!("watch {letter}: {err}"));
            return;
        }
    };
    let event = match unsafe { CreateEventW(None, true, false, None) } {
        Ok(e) => e,
        Err(err) => {
            crate::snapshot::debug_log(&format!("watch {letter}: CreateEventW {err}"));
            crate::win::close(dir);
            return;
        }
    };
    let mut buf = vec![0u8; NOTIFY_BUF];
    // A12:被拒目录的冷却表(只记录最近一次拒绝时间)。
    let mut denied: HashMap<String, Instant> = HashMap::new();
    crate::snapshot::debug_log(&format!("watch {letter}: started"));
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event;
        let started = unsafe {
            ReadDirectoryChangesW(
                dir,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                true,
                notify_filter(),
                None,
                Some(&mut overlapped),
                None,
            )
        };
        if started.is_err() {
            crate::snapshot::debug_log(&format!("watch {letter}: ReadDirectoryChangesW failed"));
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        // 等通知,每 500ms 检查一次 stop(不能无限等,否则退出要卡 120s)。
        let mut aborted = false;
        loop {
            if stop.load(Ordering::Relaxed) {
                unsafe {
                    let _ = CancelIoEx(dir, Some(&overlapped));
                    let mut got = 0u32;
                    let _ = GetOverlappedResult(dir, &overlapped, &mut got, true);
                }
                aborted = true;
                break;
            }
            let wait = unsafe { WaitForSingleObject(event, WAIT_MS) };
            if wait == windows::Win32::Foundation::WAIT_TIMEOUT {
                continue;
            }
            break;
        }
        if aborted {
            break;
        }
        let mut got = 0u32;
        let res = unsafe { GetOverlappedResult(dir, &overlapped, &mut got, false) };
        if let Err(err) = res {
            let code = err.code().0 as u32;
            if code == ERROR_NOTIFY_ENUM_DIR.0 {
                crate::snapshot::debug_log(&format!("watch {letter}: notify buffer overflow"));
                full_resync(letter, guid, tx, stop);
                continue;
            }
            if code == ERROR_OPERATION_ABORTED.0 {
                break;
            }
            crate::snapshot::debug_log(&format!("watch {letter}: overlapped err {code:#x}"));
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        let paths = parse_notify(&buf[..got as usize]);
        for parent in parents_to_refresh(&paths) {
            refresh_parent(letter, guid, &parent, tx, stop, &mut denied);
        }
    }
    let _ = unsafe { CloseHandle(event) };
    crate::win::close(dir);
    crate::snapshot::debug_log(&format!("watch {letter}: stopped"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify_entry(action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes()); // next
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_single_notification() {
        let buf = notify_entry(1, "a\\b.txt");
        assert_eq!(parse_notify(&buf), vec!["a\\b.txt".to_string()]);
    }

    /// A12:被拒目录在冷却期内应跳过,冷却结束后再重试。
    #[test]
    fn denied_directories_are_skipped_during_cooldown() {
        let mut denied: HashMap<String, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(!denied_recently(&denied, "C:\\a", now));
        denied.insert("C:\\a".to_string(), now);
        assert!(denied_recently(&denied, "C:\\a", now));
        assert!(!denied_recently(&denied, "C:\\b", now));
        let later = now + DENIED_RETRY_COOLDOWN;
        assert!(!denied_recently(&denied, "C:\\a", later));
    }

    #[test]
    fn parses_chained_notifications() {
        let mut buf = notify_entry(1, "x.txt");
        let second = notify_entry(2, "dir\\y.log");
        let first_len = buf.len();
        // 第一条的 next 指向第二条
        buf[..4].copy_from_slice(&(first_len as u32).to_le_bytes());
        buf.extend_from_slice(&second);
        assert_eq!(
            parse_notify(&buf),
            vec!["x.txt".to_string(), "dir\\y.log".to_string()]
        );
    }

    #[test]
    fn groups_paths_by_parent() {
        let paths = vec![
            "a\\1.txt".to_string(),
            "a\\2.txt".to_string(),
            "b\\c\\3.txt".to_string(),
            "rootfile.txt".to_string(),
        ];
        let parents = parents_to_refresh(&paths);
        assert_eq!(parents, vec!["a".to_string(), "b\\c".to_string(), String::new()]);
    }

    #[test]
    fn ignores_truncated_buffer() {
        let buf = vec![0u8; 12]; // name_len = 0
        assert!(parse_notify(&buf).is_empty());
    }
}
