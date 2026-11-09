//! 卷扫描(ENUM_USN_DATA)与 journal 增量读取(worker 侧)。
//!
//! 实测注意(P0 spike):E: 卷在 1MB 输出 buffer 下出现过内核态挂死,
//! 64KB buffer 正常(~44 万条/秒)—— worker 统一用 64KB。
//!
//! 2026-11-09 本机出现 SYSTEM_SERVICE_EXCEPTION(0x3B, Ntfs.sys) 蓝屏,时段与
//! 四卷并发 USN 扫描重合。用户态只读 FSCTL 不应崩内核,但本机 Ntfs.sys 对大
//! buffer 的 FSCTL 已有已知异常(P0 spike 挂死),因此 journal 读取 buffer 也收
//! 回到 64KB,worker 全量扫描改串行(见 worker::SCAN_GATE),以降低触发面。

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V1,
    USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::usn::{self, Rec};
use crate::win;

pub const ENUM_BUF: usize = 64 << 10;
/// journal 增量读取的输出 buffer。原为 256KB,2026-11-09 蓝屏后与 ENUM 统一到
/// 64KB:本机 Ntfs.sys 对大输出 buffer 的 FSCTL 出现过内核态挂死/异常,小 buffer
/// 只多几次 IO,换来更低的触发面。
pub const JOURNAL_BUF: usize = 64 << 10;

fn open_volume(letter: char) -> Result<HANDLE, String> {
    let path = win::to_wide(&format!(r"\\.\{letter}:"));
    unsafe {
        CreateFileW(
            windows::core::PCWSTR(path.as_ptr()),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| format!("open {letter}: {e}"))
    }
}

pub struct VolumeHandle {
    pub handle: HANDLE,
}

impl VolumeHandle {
    pub fn open(letter: char) -> Result<Self, String> {
        Ok(Self { handle: open_volume(letter)? })
    }

    pub fn ntfs_data(&self) -> Option<(u64, u64)> {
        // (估计记录数 = MftValidDataLength / BytesPerFileRecordSegment, mft 长度)
        let mut vd = NTFS_VOLUME_DATA_BUFFER::default();
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(&mut vd as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                Some(&mut ret),
                None,
            )
        };
        if ok.is_err() {
            return None;
        }
        Some((
            vd.MftValidDataLength as u64 / vd.BytesPerFileRecordSegment as u64,
            vd.MftValidDataLength as u64,
        ))
    }

    pub fn journal_state(&self) -> Option<(u64, i64, i64)> {
        // (journal_id, first_usn, next_usn)
        let mut jd = USN_JOURNAL_DATA_V0::default();
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut jd as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut ret),
                None,
            )
        };
        if ok.is_err() {
            return None;
        }
        Some((jd.UsnJournalID, jd.FirstUsn, jd.NextUsn))
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        win::close(self.handle);
    }
}

/// ENUM 全卷,目录先行回放(存储层 scan 模式依赖父先到)。
/// `resume_frn > 0` 时从断点续扫(其余记录已在主进程快照里)。
/// `high_usn` 必须是**扫描开始前**采样的 journal NextUsn:ENUM 只回放该位点之前
/// 的记录,之后的变更留给 journal 增量,避免「扫描期间新建的文件」永久丢失(H4)。
/// `emit` 逐批回调;`stop` 置位即中止。返回 `Ok(Some(总条数))` 表示扫完,
/// `Ok(None)` 表示被 stop 中止(调用方不得宣告 Ready)。
pub fn scan_volume(
    letter: char,
    resume_frn: u64,
    high_usn: i64,
    stop: &std::sync::atomic::AtomicBool,
    mut progress: impl FnMut(u64, u64, u32),
    mut emit: impl FnMut(Vec<Rec>),
) -> Result<Option<u32>, String> {
    use std::sync::atomic::Ordering;
    let vol = VolumeHandle::open(letter)?;
    let (est, _mft_len) = vol.ntfs_data().unwrap_or((0, 0));
    let mut med = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: resume_frn,
        LowUsn: 0,
        HighUsn: high_usn,
    };
    let mut buf = vec![0u8; ENUM_BUF];
    let mut all: Vec<Rec> = Vec::new();
    let mut count: u32 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                vol.handle,
                FSCTL_ENUM_USN_DATA,
                Some(&med as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                buf.len() as u32,
                Some(&mut ret),
                None,
            )
        };
        if ok.is_err() || ret <= 8 {
            break;
        }
        let resume = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let (recs, _consumed) = usn::scan_records(&buf[8..ret as usize]);
        count += recs.len() as u32;
        all.extend(recs);
        med.StartFileReferenceNumber = resume;
        if all.len() >= 200_000 {
            progress(resume, est, count);
        }
    }
    progress(med.StartFileReferenceNumber, est, count);

    // 目录先行
    let (dirs, files): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|r| r.attrs & usn::FILE_ATTRIBUTE_DIRECTORY != 0);
    for chunk in dirs.chunks(4000) {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        emit(chunk.to_vec());
    }
    for chunk in files.chunks(4000) {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        emit(chunk.to_vec());
    }
    Ok(Some(count))
}

/// journal 增量读取。返回 Ok(Some((记录, 新 next_usn))) 或
/// Ok(None)(暂无数据)/ Err(需要重扫)。
pub fn read_journal(
    letter: char,
    journal_id: u64,
    start_usn: i64,
) -> Result<Option<(Vec<Rec>, i64)>, JournalError> {
    let vol = VolumeHandle::open(letter).map_err(|_| JournalError::VolumeGone)?;
    read_journal_handle(&vol, journal_id, start_usn)
}

/// journal 增量读取(复用已开句柄;长轮询场景省开/关句柄开销)。
/// Ok(None) = 暂无数据;返回空记录 = 位点已到最新,调用方需休眠。
pub fn read_journal_handle(
    vol: &VolumeHandle,
    journal_id: u64,
    start_usn: i64,
) -> Result<Option<(Vec<Rec>, i64)>, JournalError> {
    let Some((jid, first_usn, _next_usn)) = vol.journal_state() else {
        return Err(JournalError::Reset);
    };
    if jid != journal_id {
        return Err(JournalError::Reset);
    }
    if start_usn < first_usn {
        return Err(JournalError::Reset);
    }
    let rujd = READ_USN_JOURNAL_DATA_V1 {
        StartUsn: start_usn,
        ReasonMask: u32::MAX,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: jid,
        MinMajorVersion: 2,
        MaxMajorVersion: 2,
    };
    let mut buf = vec![0u8; JOURNAL_BUF];
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            vol.handle,
            FSCTL_READ_USN_JOURNAL,
            Some(&rujd as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            buf.len() as u32,
            Some(&mut ret),
            None,
        )
    };
    if ok.is_err() {
        let code = unsafe { windows::Win32::Foundation::GetLastError() };
        if code == windows::Win32::Foundation::WIN32_ERROR(0x19D)
            || code == windows::Win32::Foundation::WIN32_ERROR(0x19E)
            || code == windows::Win32::Foundation::WIN32_ERROR(0x19C)
        {
            return Err(JournalError::Reset);
        }
        return Err(JournalError::Io);
    }
    if ret < 8 {
        return Ok(None);
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
    let (raw, _used, _skipped) = usn::parse_records(&buf[8..ret as usize]);
    let recs: Vec<Rec> = raw.iter().filter_map(|r| r.to_engine_op()).collect();
    Ok(Some((recs, next)))
}

pub enum JournalError {
    /// journal 回绕/删除/失活 → 该卷全量重扫。
    Reset,
    /// 卷消失。
    VolumeGone,
    /// 其他 IO 错误(下一轮重试)。
    Io,
}

/// 打开卷句柄供外部持有(journal 长轮询可复用同一句柄)。
pub fn keep_volume_open(letter: char) -> Option<VolumeHandle> {
    VolumeHandle::open(letter).ok()
}

#[allow(dead_code)]
pub fn close_handle(h: HANDLE) {
    win::close(h);
}
