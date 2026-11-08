//! Win32 辅助:卷枚举、命名管道(服务端 ACL + 对端校验、客户端连接)。
//!
//! 安全(方案 §4.1 必做):提权 worker 建的管道若不设 ACL,同机任意账户
//! 都能连上拿全盘文件清单。措施 = 仅当前用户 SID 的 DACL + 对端进程映像校验。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
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
    FILE_TYPE_PIPE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, NAMED_PIPE_MODE,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW, OpenProcessToken,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
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

/// 懒填元数据(方案 §3.2 路线 A):FindFirstFileW 拿 size/mtime/精确名。
/// 目标不存在(USN 延迟)返回 None。
pub fn find_meta(path: &str) -> Option<(u64, i64, bool)> {
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, WIN32_FIND_DATAW,
    };
    let wpath = to_wide(path);
    let mut ffd = WIN32_FIND_DATAW::default();
    unsafe {
        let handle = FindFirstFileW(PCWSTR(wpath.as_ptr()), &mut ffd).ok()?;
        let _ = FindClose(handle);
    }
    let size = ((ffd.nFileSizeHigh as u64) << 32) | ffd.nFileSizeLow as u64;
    let mtime = ((ffd.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | ffd.ftLastWriteTime.dwLowDateTime as u64;
    // FILETIME → unix 秒(0 = 无效)
    let mtime = if mtime == 0 {
        None
    } else {
        Some(((mtime as i128 - 11_644_473_600_000_000_000u128 as i128) / 10_000_000) as i64)
    };
    let is_dir = ffd.dwFileAttributes & 0x10 != 0;
    Some((size, mtime.unwrap_or(0), is_dir))
}
