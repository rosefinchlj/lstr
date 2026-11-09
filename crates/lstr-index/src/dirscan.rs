//! 安全首扫:目录枚举(`NtQueryDirectoryFile`),不触碰任何 USN FSCTL。
//!
//! 背景:2026-11-09 本机两次 `SYSTEM_SERVICE_EXCEPTION(0x3B, Ntfs.sys)` 蓝屏,
//! 两次都落在内置索引的 USN 路径活跃时段(首扫 ENUM / journal 轮询)。Everything
//! 直接读 `$MFT`、不走 `FSCTL_*`,长期运行无碍;为把触发面降到零,首扫改成普通
//! 目录枚举:不打开卷句柄、不发 `FSCTL_ENUM_USN_DATA / QUERY_USN_JOURNAL /
//! READ_USN_JOURNAL`。
//!
//! 代价(阶段 1 明确接受):大卷首扫更慢、看不到已删除条目、每次启动重扫一次。
//! journal 增量(阶段 2)由 `worker::WorkerOpts` 的 `journal` 开关控制,默认关闭。
//!
//! 目录项的 `FileId` 就是完整 64 位 FRN(含序列号),因此不需要逐文件打开;
//! 根目录 FRN 固定为 [`ROOT_FRN`]。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::usn::{Op, Rec};

/// NTFS 卷根目录固定 FRN(与 `store::ROOT_FRN` 一致)。
pub const ROOT_FRN: u64 = 5;

/// 目录查询输出缓冲:与 USN 路径一样取 64KB,减少 IO 次数。
const QUERY_BUF: usize = 64 << 10;
/// 每批回调的记录数(与 USN 扫描的 chunk 一致)。
const BATCH_RECS: usize = 4000;
/// 递归深度上限:防御异常目录树(正常 NTFS 深度远小于此)。
const MAX_DEPTH: usize = 512;

const FILE_LIST_DIRECTORY: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// `FileIdBothDirectoryInformation`
const FILE_ID_BOTH_DIR_INFORMATION: u32 = 37;
/// `STATUS_NO_MORE_FILES`
const STATUS_NO_MORE_FILES: i32 = 0x8000_0006u32 as i32;

#[repr(C)]
struct IoStatusBlock {
    status: isize,
    information: usize,
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryDirectoryFile(
        file_handle: HANDLE,
        event: *mut core::ffi::c_void,
        apc_routine: *const core::ffi::c_void,
        apc_context: *const core::ffi::c_void,
        io_status_block: *mut IoStatusBlock,
        file_information: *mut core::ffi::c_void,
        length: u32,
        file_information_class: u32,
        return_single_entry: u8,
        file_name: *const UnicodeString,
        restart_scan: u8,
    ) -> i32;
}

pub(crate) fn open_dir(path: &str) -> Result<HANDLE, String> {
    // `\\?\` 前缀支持超过 MAX_PATH 的路径
    let wide = crate::win::to_wide(&format!(r"\\?\{path}"));
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .map_err(|e| format!("open {path}: {e}"))
    }
}

/// 查询一次目录。返回写入缓冲的字节数(0 表示结束)。
fn query_dir(handle: HANDLE, buf: &mut [u8], restart: bool) -> Result<usize, String> {
    let mut iosb = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtQueryDirectoryFile(
            handle,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            &mut iosb,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            FILE_ID_BOTH_DIR_INFORMATION,
            0,
            core::ptr::null(),
            u8::from(restart),
        )
    };
    if status == STATUS_NO_MORE_FILES {
        return Ok(0);
    }
    if status < 0 {
        return Err(format!("NtQueryDirectoryFile: 0x{:08x}", status as u32));
    }
    Ok(iosb.information.min(buf.len()))
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn u64_at(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..off + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

fn i64_at(buf: &[u8], off: usize) -> Option<i64> {
    buf.get(off..off + 8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
}

/// FILETIME(1601 起的 100ns 计数)→ unix 秒;越界/0 返回 0(未知)。
fn unix_from_filetime(ticks: i64) -> i64 {
    if ticks <= 0 {
        return 0;
    }
    let secs = ticks / 10_000_000 - 11_644_473_600;
    if secs > 0 && secs <= i64::from(u32::MAX) {
        secs
    } else {
        0
    }
}

/// `FILE_ID_BOTH_DIR_INFORMATION` 的固定头长度:`FileName` 从这里开始。
const ENTRY_HEAD: usize = 104;

/// 遍历 `root_path` 下的所有条目,按 ~4000 条一批回调。
///
/// `root_frn` 是 `root_path` 自身在卷内的 FRN(卷根为 5;单测可用任意目录)。
/// 返回条目总数;`stop` 置位时提前返回已扫到的数量。
pub fn enumerate_dir(
    root_path: &str,
    root_frn: u64,
    stop: &AtomicBool,
    batch: &mut dyn FnMut(Vec<Rec>),
) -> Result<u64, String> {
    let mut stack: Vec<(String, u64, usize)> = vec![(root_path.to_string(), root_frn, 0)];
    let mut pending: Vec<Rec> = Vec::with_capacity(BATCH_RECS);
    let mut total: u64 = 0;
    let mut buf = vec![0u8; QUERY_BUF];

    while let Some((dir_path, dir_frn, depth)) = stack.pop() {
        if stop.load(Ordering::Relaxed) {
            return Ok(total);
        }
        let handle = match open_dir(&dir_path) {
            Ok(h) => h,
            Err(err) => {
                // 权限不足的目录(如受保护系统目录)跳过并记录,不中断整卷
                crate::snapshot::debug_log(&format!("dirscan: {err}"));
                continue;
            }
        };
        let mut restart = true;
        loop {
            if stop.load(Ordering::Relaxed) {
                crate::win::close(handle);
                return Ok(total);
            }
            let used = match query_dir(handle, &mut buf, restart) {
                Ok(n) => n,
                Err(err) => {
                    crate::snapshot::debug_log(&format!("dirscan {dir_path}: {err}"));
                    break;
                }
            };
            restart = false;
            if used == 0 {
                break;
            }
            let mut off = 0usize;
            while off + ENTRY_HEAD <= used {
                let next = u32_at(&buf, off).unwrap_or(0) as usize;
                let attrs = u32_at(&buf, off + 56).unwrap_or(0);
                let name_len = u32_at(&buf, off + 60).unwrap_or(0) as usize;
                let file_id = u64_at(&buf, off + 96).unwrap_or(0);
                let name_start = off + ENTRY_HEAD;
                if name_len == 0 || name_len % 2 != 0 || name_start + name_len > used {
                    break;
                }
                let units: Vec<u16> = (0..name_len / 2)
                    .map(|i| {
                        u16::from_le_bytes(
                            buf[name_start + i * 2..name_start + i * 2 + 2]
                                .try_into()
                                .unwrap(),
                        )
                    })
                    .collect();
                let name = String::from_utf16_lossy(&units);
                if name != "." && name != ".." {
                    let mfrn = file_id & 0x0000_FFFF_FFFF_FFFF;
                    let is_meta = mfrn < 0x100 && name.starts_with('$');
                    if !is_meta {
                        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                        let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                        let mtime = unix_from_filetime(i64_at(&buf, off + 24).unwrap_or(0));
                        pending.push(Rec {
                            frn: file_id,
                            parent_frn: dir_frn,
                            attrs,
                            op: Op::Upsert,
                            name: name.clone(),
                            mtime,
                        });
                        total += 1;
                        if pending.len() >= BATCH_RECS {
                            batch(std::mem::take(&mut pending));
                        }
                        // reparse point 不递归(防目录环/跨卷链接)
                        if is_dir && !is_reparse && depth < MAX_DEPTH {
                            let child = if dir_path.ends_with('\\') {
                                format!("{dir_path}{name}")
                            } else {
                                format!("{dir_path}\\{name}")
                            };
                            stack.push((child, file_id, depth + 1));
                        }
                    }
                }
                if next == 0 || next >= used {
                    break;
                }
                off += next;
            }
        }
        crate::win::close(handle);
    }

    if !pending.is_empty() {
        batch(pending);
    }
    Ok(total)
}

/// 枚举整卷(根 FRN = 5)。见 [`enumerate_dir`]。
pub fn enumerate_volume(
    letter: char,
    stop: &AtomicBool,
    batch: &mut dyn FnMut(Vec<Rec>),
) -> Result<u64, String> {
    enumerate_dir(&format!("{letter}:\\"), ROOT_FRN, stop, batch)
}

/// 只枚举一层目录(不递归):watcher 收到变更后用它刷新单个父目录。
pub fn enumerate_children(
    dir_path: &str,
    dir_frn: u64,
    stop: &AtomicBool,
) -> Result<Vec<Rec>, String> {
    let handle = open_dir(dir_path)?;
    let mut buf = vec![0u8; QUERY_BUF];
    let mut out: Vec<Rec> = Vec::new();
    let mut restart = true;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let used = match query_dir(handle, &mut buf, restart) {
            Ok(n) => n,
            Err(err) => {
                crate::win::close(handle);
                return Err(err);
            }
        };
        restart = false;
        if used == 0 {
            break;
        }
        let mut off = 0usize;
        while off + ENTRY_HEAD <= used {
            let next = u32_at(&buf, off).unwrap_or(0) as usize;
            let attrs = u32_at(&buf, off + 56).unwrap_or(0);
            let name_len = u32_at(&buf, off + 60).unwrap_or(0) as usize;
            let file_id = u64_at(&buf, off + 96).unwrap_or(0);
            let name_start = off + ENTRY_HEAD;
            if name_len == 0 || name_len % 2 != 0 || name_start + name_len > used {
                break;
            }
            let units: Vec<u16> = (0..name_len / 2)
                .map(|i| {
                    u16::from_le_bytes(
                        buf[name_start + i * 2..name_start + i * 2 + 2]
                            .try_into()
                            .unwrap(),
                    )
                })
                .collect();
            let name = String::from_utf16_lossy(&units);
            if name != "." && name != ".." {
                let mfrn = file_id & 0x0000_FFFF_FFFF_FFFF;
                let is_meta = mfrn < 0x100 && name.starts_with('$');
                if !is_meta {
                    let mtime = unix_from_filetime(i64_at(&buf, off + 24).unwrap_or(0));
                    out.push(Rec {
                        frn: file_id,
                        parent_frn: dir_frn,
                        attrs,
                        op: Op::Upsert,
                        name,
                        mtime,
                    });
                }
            }
            if next == 0 || next >= used {
                break;
            }
            off += next;
        }
    }
    crate::win::close(handle);
    Ok(out)
}

/// `FILE_ID_INFO`(windows crate 的类常量在部分 feature 下不可见,自行声明)。
#[repr(C)]
#[derive(Default)]
struct FileIdInfoRaw {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

/// 指定路径(文件或目录)的 64 位 FRN;打开失败(例如条目已被删除)返回 None。
pub fn file_id_of(path: &str) -> Option<u64> {
    // FileIdInfo = 18
    const FILE_ID_INFO_CLASS: i32 = 18;
    let wide = crate::win::to_wide(&format!(r"\\?\{path}"));
    unsafe {
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .ok()?;
        let mut info = FileIdInfoRaw::default();
        let ok = GetFileInformationByHandleEx(
            handle,
            windows::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS(FILE_ID_INFO_CLASS),
            &mut info as *mut FileIdInfoRaw as *mut core::ffi::c_void,
            std::mem::size_of::<FileIdInfoRaw>() as u32,
        );
        crate::win::close(handle);
        ok.ok()?;
        Some(u64::from_le_bytes(info.file_id[..8].try_into().ok()?))
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lstr-dirscan-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempTree(dir)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn enumerates_nested_directories_and_batches() {
        let t = TempTree::new("nested");
        std::fs::create_dir_all(t.0.join("sub/deep")).unwrap();
        std::fs::write(t.0.join("a.txt"), b"a").unwrap();
        std::fs::write(t.0.join("sub/b.txt"), b"b").unwrap();
        std::fs::write(t.0.join("sub/deep/c.txt"), b"c").unwrap();

        let stop = AtomicBool::new(false);
        let mut batches = 0usize;
        let mut recs: Vec<Rec> = Vec::new();
        let total = enumerate_dir(&t.0.display().to_string(), ROOT_FRN, &stop, &mut |b| {
            batches += 1;
            recs.extend(b);
        })
        .unwrap();

        assert!(batches >= 1);
        assert_eq!(total as usize, recs.len());
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        for want in ["a.txt", "sub", "b.txt", "deep", "c.txt"] {
            assert!(names.contains(&want), "missing {want}");
        }
        // 目录的 parent_frn 应等于其父目录的 frn
        let sub = recs.iter().find(|r| r.name == "sub").unwrap();
        let deep = recs.iter().find(|r| r.name == "deep").unwrap();
        assert_eq!(deep.parent_frn, sub.frn);
        assert_eq!(sub.parent_frn, ROOT_FRN);
        // 顶层文件 parent 是根 FRN
        let a = recs.iter().find(|r| r.name == "a.txt").unwrap();
        assert_eq!(a.parent_frn, ROOT_FRN);
        // B 步:mtime 来自 FILE_ID_BOTH_DIR_INFORMATION(刚创建的文件必须非 0)。
        assert!(a.mtime > 0, "mtime 应为最近时间,实际 {}", a.mtime);
    }

    #[test]
    fn respects_stop_flag() {
        let t = TempTree::new("stop");
        std::fs::write(t.0.join("x.txt"), b"x").unwrap();
        let stop = AtomicBool::new(true);
        let mut calls = 0usize;
        let total = enumerate_dir(&t.0.display().to_string(), ROOT_FRN, &stop, &mut |_| {
            calls += 1
        })
        .unwrap();
        assert_eq!(total, 0);
        assert_eq!(calls, 0);
    }

    #[test]
    fn reports_error_for_missing_root() {
        let stop = AtomicBool::new(false);
        let missing = std::env::temp_dir().join("lstr-dirscan-does-not-exist-xyz");
        let mut called = false;
        enumerate_dir(&missing.display().to_string(), ROOT_FRN, &stop, &mut |_| called = true)
            .unwrap();
        assert!(!called);
    }
}
