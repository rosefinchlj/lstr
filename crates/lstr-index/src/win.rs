//! Win32 辅助:卷枚举、命名管道(服务端 ACL + 对端校验、客户端连接)。
//!
//! 安全(方案 §4.1 必做):提权 worker 建的管道若不设 ACL,同机任意账户
//! 都能连上拿全盘文件清单。措施 = 仅当前用户 SID 的 DACL + 对端进程映像校验。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_PIPE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, NAMED_PIPE_MODE,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, SetThreadPriority, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};

/// UTF-16 (nul 结尾)。
pub fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 固定 NTFS 卷:(盘符, GUID 路径 "\\?\Volume{...}\")。
pub fn fixed_ntfs_volumes() -> Vec<(char, String)> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from_u32('A' as u32 + i).unwrap_or('C');
        let root = format!("{letter}:\\");
        let wroot = to_wide(&root);
        unsafe {
            // DRIVE_FIXED = 3(windows crate 未导出该常量)
            if GetDriveTypeW(PCWSTR(wroot.as_ptr())) != 3 {
                continue;
            }
            let mut fs = [0u16; 32];
            if GetVolumeInformationW(
                PCWSTR(wroot.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs),
            )
            .is_err()
                || from_wide(&fs) != "NTFS"
            {
                continue;
            }
            let mut name = [0u16; 64];
            if GetVolumeNameForVolumeMountPointW(PCWSTR(wroot.as_ptr()), &mut name).is_err() {
                continue;
            }
            out.push((letter, from_wide(&name)));
        }
    }
    out
}

/// "\\?\Volume{GUID}\" → u128(32 位 hex)。
pub fn guid_to_u128(volume_guid_path: &str) -> Option<u128> {
    let inner = volume_guid_path
        .strip_prefix(r"\\?\Volume{")
        .and_then(|s| s.strip_suffix(r"}\"))?;
    let hex: String = inner.chars().filter(|c| *c != '-').collect();
    u128::from_str_radix(&hex, 16).ok()
}

/// 当前进程用户的 SID 字符串(S-1-5-...)。
pub fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut _), len, &mut len).ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut str_sid = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut str_sid).ok()?;
        let mut l = 0usize;
        while *str_sid.0.add(l) != 0 {
            l += 1;
        }
        let s = from_wide(std::slice::from_raw_parts(str_sid.0, l));
        let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(str_sid.0 as _)));
        let _ = CloseHandle(token);
        Some(s)
    }
}

/// 创建仅限当前用户访问的管道服务端(单实例)。
pub fn create_pipe_server(name: &str) -> Result<HANDLE, String> {
    let sid = current_user_sid().ok_or("no user sid")?;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let wsddl = to_wide(&sddl);
    let mut psd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wsddl.as_ptr()),
            1, // SECURITY_DESCRIPTOR_REVISION
            &mut psd,
            None,
        )
        .map_err(|e| format!("SDDL: {e}"))?;
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    let wname = to_wide(name);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wname.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            NAMED_PIPE_MODE(PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0),
            1,
            1 << 20,
            1 << 20,
            0,
            Some(&mut sa),
        )
    };
    if handle.is_invalid() {
        return Err("CreateNamedPipeW failed".into());
    }
    Ok(handle)
}

/// 校验管道对端是本用户会话里的 lstr.exe。
pub fn verify_peer(client: HANDLE) -> bool {
    unsafe {
        let mut pid: u32 = 0;
        if GetNamedPipeClientProcessId(client, &mut pid).is_err() || pid == 0 {
            return false;
        }
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            proc,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(proc);
        if !ok || len as usize > buf.len() {
            return false;
        }
        let path = from_wide(&buf[..len as usize]);
        let exe = path.rsplit(['\\', '/']).next().unwrap_or("");
        exe.eq_ignore_ascii_case("lstr.exe")
    }
}

/// 客户端连接。
pub fn open_pipe_client(name: &str) -> Result<HANDLE, String> {
    let wname = to_wide(name);
    unsafe {
        CreateFileW(
            PCWSTR(wname.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| format!("connect pipe: {e}"))
    }
}

/// 管道是否有可用实例(零交互探活,不建立会话)。
/// 不能拿 `open_pipe_client` 当探活:它会真的连上 worker,worker 随即按
/// 空握手开一次全量扫描,且句柄不关闭时会永久占住唯一实例(worker 卡死)。
/// 语义:有实例在监听 → true;实例全忙(worker 活着但会话占用) → 仍算 true;
/// 管道不存在 → false。
pub fn pipe_available(name: &str) -> bool {
    const ERROR_SEM_TIMEOUT: u32 = 121; // 实例全忙(worker 活着)
    const ERROR_PIPE_BUSY: u32 = 231; // 实例全忙
    let wname = to_wide(name);
    let ok = unsafe { WaitNamedPipeW(PCWSTR(wname.as_ptr()), 0) };
    if ok.as_bool() {
        return true;
    }
    let code = unsafe { GetLastError() }.0;
    code == ERROR_SEM_TIMEOUT || code == ERROR_PIPE_BUSY
}

/// 阻塞等待客户端连接;客户端已就位时返回 ERROR_PIPE_CONNECTED 也算成功。
pub fn wait_for_client(server: HANDLE) -> bool {
    unsafe {
        match ConnectNamedPipe(server, None) {
            Ok(()) => true,
            // 0x80070067..: ERROR_PIPE_CONNECTED = 535 (0x217) → HRESULT 0x8007_0217
            Err(e) => e.code() == windows::core::HRESULT(0x8007_0217_u32 as i32),
        }
    }
}

pub fn close(h: HANDLE) {
    unsafe {
        let _ = CloseHandle(h);
    }
}

pub fn is_pipe(h: HANDLE) -> bool {
    unsafe { windows::Win32::Storage::FileSystem::GetFileType(h) == FILE_TYPE_PIPE }
}

/// 对目录做 FlushFileBuffers,让刚完成的 rename 目录项落盘。
///
/// fsync 只保证文件数据持久;rename 改的是父目录项,断电后可能出现
/// 「文件名已改、目录项未持久」的窗口。目录句柄需要 FILE_FLAG_BACKUP_SEMANTICS,
/// 且 FlushFileBuffers 要求 GENERIC_WRITE;FAT32/部分网络盘会拒绝,返回 false,
/// 调用方只记日志(最坏结果仍只是一次全量重扫,不会读到坏数据)。
pub fn flush_dir(dir: &std::path::Path) -> bool {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS,
    };
    let wide = to_wide(&dir.to_string_lossy());
    unsafe {
        let h = CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        );
        match h {
            Ok(h) => {
                let ok = FlushFileBuffers(h).is_ok();
                close(h);
                ok
            }
            Err(_) => false,
        }
    }
}

/// 把当前线程切到 Windows「后台模式」(`THREAD_MODE_BACKGROUND_BEGIN`):
/// 该线程随后发起的 IO 优先级与 CPU 调度都降到最低档,用于整卷扫描/对账
/// 这类重负载段(串行闸门之外的又一道负载整形)。
/// 返回是否成功;失败(平台不支持/已处于后台模式)按原优先级继续,不致命。
pub fn thread_io_background_begin() -> bool {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN).is_ok() }
}

/// 退出后台模式(与 [`thread_io_background_begin`] 配对;调用方用 RAII guard
/// 包住,保证 panic/早退也会恢复)。
pub fn thread_io_background_end() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
    }
}

/// 1601-01-01 → 1970-01-01 之间的 100ns tick 数(369 年 × 86400 × 10^7,
/// 即 116_444_736_000_000_000)。旧实现误写成 20 位
/// (`11_644_473_600_000_000_000`,多两个 0),所有 FILETIME 换算结果因此
/// 恒为巨大负数,`find_meta` 的 mtime 在 `modified > 0` 判定下全部被当成
/// 「未知」。正确性由 2024-01-01 的固定 FILETIME 样本单测锁死。
const FILETIME_UNIX_EPOCH_TICKS: u128 = 116_444_736_000_000_000;

/// 合理上限:9999-12-31T23:59:59Z 的 unix 秒。真实文件的 FILETIME 不可能
/// 超过它;超出只可能是损坏/伪造值,按无效(0)处理。
const MAX_SANE_UNIX_SECS: i128 = 253_402_300_799;

/// FILETIME(100ns tick,u64)→ unix 秒。0 = 无效/未知。
///
/// 保留既有语义:0 tick 表示「没有时间」;早于 1970 的时间戳(负数)以及
/// 超出合理范围的异常值同样归 0,避免负 mtime 进入 rank/UI。1970-01-01
/// 整点本身换算为 0,与「未知」共用哨兵值 —— 该边界只影响纪元瞬间。
fn filetime_to_unix_secs(ticks: u64) -> i64 {
    if ticks == 0 {
        return 0;
    }
    let secs = (ticks as i128 - FILETIME_UNIX_EPOCH_TICKS as i128) / 10_000_000;
    if secs <= 0 || secs > MAX_SANE_UNIX_SECS {
        0
    } else {
        secs as i64
    }
}

/// 单条查询(`GetFileAttributesExW`)与目录枚举(`FindFirstFileW`)共用的
/// 字段换算(size/mtime/is_dir)。两条路径必须走同一份换算,否则批量填充
/// 会对不上逐条查询。
fn meta_from_parts(size_high: u32, size_low: u32, write_ticks: u64, attrs: u32) -> FileMeta {
    let size = ((size_high as u64) << 32) | size_low as u64;
    let mtime = filetime_to_unix_secs(write_ticks);
    let is_dir = attrs & 0x10 != 0;
    FileMeta { size, mtime, is_dir }
}

fn win32_meta(ffd: &windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW) -> FileMeta {
    let ticks = ((ffd.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | ffd.ftLastWriteTime.dwLowDateTime as u64;
    meta_from_parts(
        ffd.nFileSizeHigh,
        ffd.nFileSizeLow,
        ticks,
        ffd.dwFileAttributes,
    )
}

fn win32_attribute_meta(
    data: &windows::Win32::Storage::FileSystem::WIN32_FILE_ATTRIBUTE_DATA,
) -> FileMeta {
    let ticks = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | data.ftLastWriteTime.dwLowDateTime as u64;
    meta_from_parts(
        data.nFileSizeHigh,
        data.nFileSizeLow,
        ticks,
        data.dwFileAttributes,
    )
}

/// 懒填元数据(方案 §3.2 路线 A):GetFileAttributesExW 拿 size/mtime/attrs。
/// 目标不存在(USN 延迟)返回 None。
///
/// 用 GetFileAttributesExW 而不是 FindFirstFileW:后者每条都要开/关 find
/// handle 并做通配符匹配,实测约 20–40µs/条;前者是单次属性查询,快 2–3 倍。
/// 两者取的字段完全一致(同一套 `meta_from_parts` 换算),所以批量枚举
/// (`list_dir_meta`)与逐条查询的结果仍逐字段相等。
pub fn find_meta(path: &str) -> Option<(u64, i64, bool)> {
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA,
    };
    let wpath = to_wide(path);
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    unsafe {
        GetFileAttributesExW(
            PCWSTR(wpath.as_ptr()),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut std::ffi::c_void,
        )
        .ok()?;
    }
    let meta = win32_attribute_meta(&data);
    Some((meta.size, meta.mtime, meta.is_dir))
}

/// 一条元数据,字段与 [`find_meta`] 的返回值一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

/// 一次目录枚举的结果:按名字(ASCII 大小写折叠,与索引 `name_lc` 同规则)
/// 索引到元数据。同一目录里折叠后重名(只能在大小写敏感目录出现)的条目
/// 会被标记为歧义,`get` 返回 None,调用方回退逐条 `find_meta` —— 绝不猜
/// 哪一条才是候选路径对应的文件。
#[derive(Debug, Default)]
pub struct DirMeta {
    map: std::collections::HashMap<Vec<u8>, FileMeta>,
    ambiguous: std::collections::HashSet<Vec<u8>>,
}

impl DirMeta {
    /// 按名字查元数据。None = 目录枚举不可用 / 名字歧义 / 名字不在目录内,
    /// 调用方一律回退逐条查询(与旧路径语义完全一致)。
    pub fn get(&self, name: &str) -> Option<FileMeta> {
        let folded = crate::store::fold_ascii(name.as_bytes());
        if self.ambiguous.contains(folded.as_slice()) {
            return None;
        }
        self.map.get(folded.as_slice()).copied()
    }

    /// 枚举到的唯一条目数(诊断用)。
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// 一次 `FindFirstFileExW` 拿回整个目录的 attrs/mtime/size,按名匹配候选,
/// 取代「每个候选一次 FindFirstFileW」。失败(权限/目录不存在/路径过长)返回
/// None,调用方逐条回退。语义与逐条查询一致:同一 API 的同一组字段,
/// 且不做任何缓存/TTL —— 每次查询自己枚举一次,不引入陈旧元数据。
pub fn list_dir_meta(dir: &str) -> Option<DirMeta> {
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        FIND_FIRST_EX_FLAGS, WIN32_FIND_DATAW,
    };
    if dir.is_empty() {
        return None;
    }
    let pattern = if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}*")
    } else {
        format!("{dir}\\*")
    };
    let wpattern = to_wide(&pattern);
    let mut ffd = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(wpattern.as_ptr()),
            FindExInfoBasic,
            &mut ffd as *mut _ as *mut std::ffi::c_void,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_FLAGS(0),
        )
    }
    .ok()?;
    let mut out = DirMeta::default();
    loop {
        let name = from_wide(&ffd.cFileName);
        // "." / ".." 正常不会由通配匹配返回;防御性跳过,绝不进入映射。
        if !name.is_empty() && name != "." && name != ".." {
            let meta = win32_meta(&ffd);
            let folded = crate::store::fold_ascii(name.as_bytes());
            if out.map.contains_key(folded.as_slice()) {
                out.ambiguous.insert(folded);
            } else {
                out.map.insert(folded, meta);
            }
        }
        if unsafe { FindNextFileW(handle, &mut ffd) }.is_err() {
            break;
        }
    }
    unsafe {
        let _ = FindClose(handle);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定样本的 FILETIME → unix 秒换算:2024-01-01T00:00:00Z 的 FILETIME
    /// 是 133_485_408_000_000_000(已知常量),对应 unix 1_704_067_200。
    /// 同时锁死「0/负数/异常值归 0」的防御语义。
    #[test]
    fn filetime_to_unix_secs_matches_known_samples() {
        assert_eq!(filetime_to_unix_secs(133_485_408_000_000_000), 1_704_067_200);
        // 2000-01-01T00:00:00Z = unix 946_684_800。
        assert_eq!(filetime_to_unix_secs(125_911_584_000_000_000), 946_684_800);

        // 0 tick = 没有时间;1970 纪元之前(负数秒)与异常巨大值一律归 0。
        assert_eq!(filetime_to_unix_secs(0), 0);
        assert_eq!(filetime_to_unix_secs(1), 0);
        assert_eq!(filetime_to_unix_secs(FILETIME_UNIX_EPOCH_TICKS as u64), 0);
        assert_eq!(filetime_to_unix_secs(FILETIME_UNIX_EPOCH_TICKS as u64 - 1), 0);
        assert_eq!(filetime_to_unix_secs(u64::MAX), 0);
    }

    /// win32_meta 必须把 FILETIME 拆出的 ticks 交给同一个换算,且 size/is_dir
    /// 不受影响(回归:旧常数多两个 0 时这里是巨大负数)。
    #[test]
    fn win32_meta_maps_filetime_to_positive_unix_seconds() {
        let ticks: u64 = 133_485_408_000_000_000;
        let mut ffd = windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW::default();
        ffd.ftLastWriteTime.dwLowDateTime = ticks as u32;
        ffd.ftLastWriteTime.dwHighDateTime = (ticks >> 32) as u32;
        ffd.nFileSizeLow = 123;
        ffd.dwFileAttributes = 0x10;
        assert_eq!(
            win32_meta(&ffd),
            FileMeta { size: 123, mtime: 1_704_067_200, is_dir: true }
        );
    }

    /// GetFileAttributesExW 路径与 FindFirstFileW 路径必须换算成同一结果
    /// （size 高低位、FILETIME、is_dir 都不能有差）。
    #[test]
    fn attribute_meta_matches_find_data_meta() {
        use windows::Win32::Storage::FileSystem::{
            WIN32_FILE_ATTRIBUTE_DATA, WIN32_FIND_DATAW,
        };
        let ticks: u64 = 133_485_408_000_000_000;
        let mut ffd = WIN32_FIND_DATAW::default();
        ffd.ftLastWriteTime.dwLowDateTime = ticks as u32;
        ffd.ftLastWriteTime.dwHighDateTime = (ticks >> 32) as u32;
        ffd.nFileSizeHigh = 1;
        ffd.nFileSizeLow = 123;
        ffd.dwFileAttributes = 0x10;

        let mut attr = WIN32_FILE_ATTRIBUTE_DATA::default();
        attr.ftLastWriteTime.dwLowDateTime = ticks as u32;
        attr.ftLastWriteTime.dwHighDateTime = (ticks >> 32) as u32;
        attr.nFileSizeHigh = 1;
        attr.nFileSizeLow = 123;
        attr.dwFileAttributes = 0x10;

        assert_eq!(win32_meta(&ffd), win32_attribute_meta(&attr));
        assert_eq!(
            win32_attribute_meta(&attr),
            FileMeta {
                size: (1u64 << 32) | 123,
                mtime: 1_704_067_200,
                is_dir: true
            }
        );
    }

    /// 对拍:目录枚举(按名查)与逐条 GetFileAttributesExW 在真实文件系统上
    /// 逐字段一致。覆盖 500 条(大小写混写/子目录/中文名)、大小写不同名查询、
    /// 磁盘上缺失的候选,以及目录不存在时的回退信号。
    #[test]
    fn dir_meta_matches_find_meta_on_real_entries() {
        let dir = std::env::temp_dir().join(format!(
            "lstr-dirmeta-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut names: Vec<(String, bool)> = Vec::new();
        for i in 0..480 {
            let name = if i % 2 == 0 {
                format!("Case_Mix_{i:04}.TXT")
            } else {
                format!("case_mix_{i:04}.txt")
            };
            std::fs::write(dir.join(&name), vec![0u8; (i % 31) as usize]).unwrap();
            names.push((name, false));
        }
        for i in 0..20 {
            let name = format!("Sub_Dir_{i:02}");
            std::fs::create_dir_all(dir.join(&name)).unwrap();
            // 子目录里放一个文件,证明枚举不只是在根上碰巧一致。
            std::fs::write(dir.join(&name).join("inner.txt"), b"x").unwrap();
            names.push((name, true));
        }
        for (name, is_dir) in [
            ("AI_CAPS.TXT", false),
            ("报告_AI.txt", false),
            ("MiXeD_报告_AI.LOG", false),
        ] {
            std::fs::write(dir.join(name), b"meta").unwrap();
            names.push((name.to_string(), is_dir));
        }

        let dir_str = dir.to_string_lossy().into_owned();
        let meta = list_dir_meta(&dir_str).expect("目录枚举必须成功");
        assert_eq!(meta.len(), names.len(), "枚举条目数");

        for (name, _) in &names {
            let path = dir.join(name);
            let path_str = path.to_string_lossy().into_owned();
            let expect = find_meta(&path_str);
            let got = meta.get(name).map(|m| (m.size, m.mtime, m.is_dir));
            assert_eq!(got, expect, "元数据不一致: {name}");
            // Windows 路径大小写不敏感;索引名折叠只覆盖 ASCII 大小写,
            // 批量匹配必须和 find_meta(传入大小写不同的路径)一致。
            let upper = name.to_ascii_uppercase();
            let upper_path = dir.join(&upper).to_string_lossy().into_owned();
            let expect_upper = find_meta(&upper_path);
            assert_eq!(
                meta.get(&upper).map(|m| (m.size, m.mtime, m.is_dir)),
                expect_upper,
                "大小写不同名查询不一致: {name}"
            );
        }

        // 索引有、磁盘无:两条路径都必须返回 None。
        let missing_path = dir.join("missing.txt").to_string_lossy().into_owned();
        assert_eq!(meta.get("missing.txt"), None);
        assert_eq!(find_meta(&missing_path), None);

        // 目录不存在/不可枚举 → None,调用方据此回退逐条。
        let gone = dir.join("gone-dir").to_string_lossy().into_owned();
        assert!(list_dir_meta(&gone).is_none());
        assert!(list_dir_meta("").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
