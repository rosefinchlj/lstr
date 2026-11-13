use crate::config::Config;
use crate::model::{Entry, Kind};
use std::os::windows::process::CommandExt; // raw_arg：按原样拼命令行
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{SHOpenWithDialog, ShellExecuteW, OAIF_EXEC, OPENASINFO};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 系统工具（appwiz.cpl / taskmgr / devmgmt.msc / regedit …）在索引里存的是
/// 相对名字。ShellExecute 的搜索顺序并不覆盖 System32 的全部场景，直接传
/// 相对名可能失败——这里先解析成绝对路径再交给 shell。
fn resolved_target(target: &Path) -> PathBuf {
    if target.is_absolute() || target.exists() {
        return target.to_path_buf();
    }
    let text = target.to_string_lossy();
    // URI（ms-settings:、shell:）不是文件，交给 shell 自己处理。
    if text.contains(':') {
        return target.to_path_buf();
    }
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let candidate = PathBuf::from(system_root).join("System32").join(target);
        if candidate.exists() {
            return candidate;
        }
    }
    target.to_path_buf()
}

fn shell_execute(operation: &str, target: &Path) -> Result<(), String> {
    let target = resolved_target(target);
    shell_execute_with(operation, &target.display().to_string(), None)
}

pub fn shell_execute_runas_with_args(file: &str, params: &str) -> Result<(), String> {
    shell_execute_with("runas", file, Some(params))
}

fn shell_execute_with(
    operation: &str,
    file: &str,
    params: Option<&str>,
) -> Result<(), String> {
    let file_display = file.to_string();
    let op = wide(operation);
    let file = wide(file);
    let params = params.map(wide);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            params
                .as_ref()
                .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "无法打开「{}」（ShellExecute 返回 {}）",
            file_display,
            result.0 as isize
        ))
    }
}

pub fn open_entry(entry: &Entry) -> Result<(), String> {
    // 书签的 path 是 URL，不是磁盘路径：交给默认浏览器。
    if entry.kind == Kind::Bookmark {
        return open_url(&entry.full_path());
    }
    let path = entry
        .path
        .as_ref()
        .ok_or_else(|| "该结果没有路径".to_string())?;
    let resolved = resolved_target(path);
    // 控制面板小程序（appwiz.cpl 等）在部分机器上 .cpl 关联损坏，
    // ShellExecute("open", ...) 会返回 31；直接交给 control.exe 最稳。
    if resolved
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cpl"))
    {
        if let Some(name) = resolved.file_name().map(|n| n.to_string_lossy().to_string()) {
            return shell_execute_with("open", "control.exe", Some(&name));
        }
    }
    shell_execute("open", &resolved)
}

pub fn reveal_entry(entry: &Entry) -> Result<(), String> {
    let path = entry
        .path
        .as_ref()
        .ok_or_else(|| "该结果没有路径".to_string())?;
    if entry.kind == Kind::Folder {
        return shell_execute("open", path);
    }
    // raw_arg：命令行原样拼成 `explorer.exe /select,"C:\Program Files\..."`。
    // 不能用 .arg()——Rust 会把内部引号转义成 \"，explorer 的解析器不认，
    // 带空格的路径依旧静默失败（实测：Program Files 下的文件定位不出来）。
    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("定位失败: {e}"))
}

pub fn run_as_admin(entry: &Entry) -> Result<(), String> {
    let path = entry
        .path
        .as_ref()
        .ok_or_else(|| "该结果没有路径".to_string())?;
    // UWP/打包应用：ShellExecute 的 runas 无法解析 shell:AppsFolder。
    // Windows Terminal 是常用打包应用里明确支持提权的，走它的 App Execution
    // Alias（wt.exe）；其余打包应用只能明确拒绝，避免弹出一个注定失败的 UAC。
    if let Some(aumid) = path
        .to_string_lossy()
        .strip_prefix("shell:AppsFolder\\")
    {
        if aumid.starts_with("Microsoft.WindowsTerminal_") {
            if let Some(wt) = wt_alias() {
                return shell_execute("runas", &wt);
            }
        }
        return Err("打包应用（UWP）不支持从启动器提权".to_string());
    }
    shell_execute("runas", path)
}

/// Windows Terminal 的 App Execution Alias（不在用户 PATH 里也能按绝对路径
/// 启动）。未安装时返回 None。
fn wt_alias() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let path = PathBuf::from(local).join(r"Microsoft\WindowsApps\wt.exe");
    path.exists().then_some(path)
}

/// 「在终端打开」的目标目录：文件夹=自身，文件/程序=所在目录。
/// 应用（开始菜单 .lnk / UWP）与命令没有有意义的终端目录。
pub fn terminal_dir(entry: &Entry) -> Option<PathBuf> {
    match entry.kind {
        Kind::Folder => entry.path.clone(),
        Kind::File | Kind::Program => entry
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf),
        _ => None,
    }
}

/// 在条目所在目录打开终端：优先 Windows Terminal（`wt -d <dir>`），
/// 不可用时回退 PowerShell（`-NoExit` 保留窗口）。
pub fn open_terminal(entry: &Entry) -> Result<(), String> {
    let dir = terminal_dir(entry).ok_or_else(|| "该结果没有可用的目录".to_string())?;
    let wt = wt_alias();
    let spawned = match &wt {
        Some(wt) => Command::new(wt).arg("-d").arg(&dir).spawn(),
        None => Command::new("wt.exe").arg("-d").arg(&dir).spawn(),
    };
    if spawned.is_ok() {
        return Ok(());
    }
    let dir_text = dir.to_string_lossy().replace('\'', "''");
    Command::new("powershell.exe")
        .arg("-NoExit")
        .arg("-Command")
        .arg(format!("Set-Location -LiteralPath '{dir_text}'"))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开终端失败: {e}"))
}

/// P0-1：以管理员权限运行程序并带参数（自定义动作 admin = true）。
/// ShellExecute 的 runas 路径不支持指定工作目录，调用方需自行处理。
pub fn run_as_admin_with_args(program: &str, params: &str) -> Result<(), String> {
    shell_execute_with("runas", program, Some(params))
}

pub fn open_with(entry: &Entry) -> Result<(), String> {
    let path = entry
        .path
        .as_ref()
        .ok_or_else(|| "该结果没有路径".to_string())?;
    let resolved = resolved_target(path);
    let target = resolved.display().to_string();
    // ShellExecute 的 "openas" 动词在新系统上静默失败；SHOpenWithDialog 是
    // Vista+ 的官方入口。它在对话框关闭前阻塞，放到独立 STA 线程：主界面先
    // 收起、界面线程不冻结（否则对话框一直开着时窗口会被系统标记「未响应」）。
    std::thread::Builder::new()
        .name("lstr-open-with".to_string())
        .spawn(move || {
            let file = wide(&target);
            unsafe {
                let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let info = OPENASINFO {
                    pcszFile: PCWSTR(file.as_ptr()),
                    pcszClass: PCWSTR::null(),
                    oaifInFlags: OAIF_EXEC,
                };
                let _ = SHOpenWithDialog(None, &info);
                if hr.is_ok() {
                    CoUninitialize();
                }
            }
        })
        .map(|_| ())
        .map_err(|e| format!("打开方式失败: {e}"))
}

pub fn open_config() -> Result<(), String> {
    Command::new("notepad.exe")
        .arg(Config::path())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开配置失败: {e}"))
}

/// 删除到回收站后拿到的「撤回票据」：回收站里的 `$I` 元数据 + `$R` 数据
/// 路径 + 原路径。撤销时把 `$R` 移回原位置并清掉 `$I`。
#[derive(Clone, Debug)]
pub struct RecycleTicket {
    pub metadata: PathBuf,
    pub payload: PathBuf,
    pub original: PathBuf,
}

/// 删除到回收站（结果菜单 / Del 键用，UI 已做二次确认）。
///
/// 只允许固定本地盘：网络盘/可移动盘没有可靠的回收站，`FOF_ALLOWUNDO`
/// 失败时可能变成永久删除，宁可拒绝也不冒数据丢失的风险。
/// 返回的票据用于「一键撤回」；拿不到票据时删除仍算成功（只是不可撤回）。
pub fn delete_to_recycle_bin(path: &Path) -> Result<Option<RecycleTicket>, String> {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
        FOF_NOERRORUI, FOF_SILENT,
    };

    if !path.exists() {
        return Err("文件不存在（可能已被删除）".to_string());
    }
    let text = path.display().to_string();
    let Some(letter) = text
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
    else {
        return Err("只支持本地盘符路径".to_string());
    };
    let root = wide(&format!("{letter}:\\"));
    // DRIVE_FIXED = 3（Win32 稳定常量；没有为此引入 WindowsProgramming feature）。
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    if drive_type != 3 {
        return Err("该位置不支持回收站删除".to_string());
    }

    // 删除前先记下回收站里已有的 $I 文件，事后差集找到本次新建的那个。
    let bin = recycle_bin_dir(path);
    let before = bin
        .as_deref()
        .map(recycle_metadata_names)
        .unwrap_or_default();

    // SHFileOperationW 要求 pFrom 以双 NUL 结尾。
    let mut from: Vec<u16> = text.encode_utf16().collect();
    from.push(0);
    from.push(0);
    let flags =
        (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0) as u16;
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: flags,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(format!("删除失败（错误码 {code}）"));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err("删除已取消".to_string());
    }

    let ticket = bin.and_then(|dir| recycle_ticket(&dir, &before, path));
    Ok(ticket)
}

/// 把回收站里的条目移回原位置（一键撤回）。
pub fn restore_from_recycle_bin(ticket: &RecycleTicket) -> Result<(), String> {
    if !ticket.payload.exists() {
        return Err("回收站里找不到原文件（可能已被清空）".to_string());
    }
    if ticket.original.exists() {
        return Err("原位置已有同名文件，无法撤回".to_string());
    }
    if let Some(parent) = ticket.original.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err("原目录已不存在，无法撤回".to_string());
        }
    }
    // $R 与目标在同一卷，rename 只是改目录项，很快。
    std::fs::rename(&ticket.payload, &ticket.original)
        .map_err(|err| format!("撤回失败：{err}"))?;
    // 元数据清理失败不影响文件已还原。
    let _ = std::fs::remove_file(&ticket.metadata);
    Ok(())
}

/// 当前用户 SID 对应的回收站目录：`<盘符>:\$Recycle.Bin\<SID>`。
fn recycle_bin_dir(path: &Path) -> Option<PathBuf> {
    let letter = path
        .display()
        .to_string()
        .chars()
        .next()?
        .to_ascii_uppercase();
    let sid = current_sid_string()?;
    let dir = PathBuf::from(format!("{letter}:\\$Recycle.Bin\\{sid}"));
    dir.is_dir().then_some(dir)
}

fn recycle_metadata_names(dir: &Path) -> std::collections::HashSet<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Default::default();
    };
    read.filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| name.to_string())
        })
        .filter(|name| name.starts_with("$I"))
        .collect()
}

/// 用删除前后的 `$I` 文件差集定位本次删除，解析出原路径并拼出 `$R` 路径。
fn recycle_ticket(
    dir: &Path,
    before: &std::collections::HashSet<String>,
    deleted: &Path,
) -> Option<RecycleTicket> {
    let after = recycle_metadata_names(dir);
    let name = after.difference(before).next()?.clone();
    let metadata = dir.join(&name);
    let original = std::fs::read(&metadata)
        .ok()
        .and_then(|bytes| parse_recycle_metadata(&bytes))
        .filter(|path| {
            path.to_string_lossy()
                .eq_ignore_ascii_case(&deleted.to_string_lossy())
        })
        .unwrap_or_else(|| deleted.to_path_buf());
    let suffix = name.strip_prefix("$I").unwrap_or(&name);
    let payload = dir.join(format!("$R{suffix}"));
    payload.exists().then_some(RecycleTicket {
        metadata,
        payload,
        original,
    })
}

/// 解析 `$I` 回收站元数据里的原路径。
/// V2：version(u64)/size(u64)/time(u64)/len(u32)/UTF-16LE 路径；
/// V1（Vista）：version/size/time + 固定 260 个 WCHAR。
fn parse_recycle_metadata(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.len() < 24 {
        return None;
    }
    let version = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let (offset, units) = if version >= 2 {
        if bytes.len() < 28 {
            return None;
        }
        let units = u32::from_le_bytes(bytes[24..28].try_into().ok()?) as usize;
        (28usize, units)
    } else if version == 1 {
        (24usize, 260usize)
    } else {
        return None;
    };
    // 路径长度通常不含结尾 NUL；多读一个单元也无妨，下面统一按 NUL 截断。
    let end = offset.checked_add(units.saturating_mul(2))?;
    if bytes.len() < end {
        return None;
    }
    let mut utf16: Vec<u16> = bytes[offset..end]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if let Some(pos) = utf16.iter().position(|&unit| unit == 0) {
        utf16.truncate(pos);
    }
    if utf16.is_empty() {
        return None;
    }
    String::from_utf16(&utf16).ok().map(PathBuf::from)
}

/// 当前进程用户 SID 的字符串形式（`S-1-5-21-...`）。
fn current_sid_string() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buffer = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return None;
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        sid_to_string(user.User.Sid.0)
    }
}

fn sid_to_string(sid_ptr: *mut core::ffi::c_void) -> Option<String> {
    use windows::Win32::Security::SID;
    if sid_ptr.is_null() {
        return None;
    }
    unsafe {
        let sid = &*(sid_ptr as *const SID);
        let count = sid.SubAuthorityCount as usize;
        let authority = sid
            .IdentifierAuthority
            .Value
            .iter()
            .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
        let subs = std::slice::from_raw_parts(sid.SubAuthority.as_ptr(), count);
        let mut text = format!("S-{}-{}", sid.Revision, authority);
        for sub in subs {
            text.push_str(&format!("-{sub}"));
        }
        Some(text)
    }
}

/// 打开 http(s) 链接（关键字网络搜索），交给系统默认浏览器。
pub fn open_url(url: &str) -> Result<(), String> {
    let op = wide("open");
    let file = wide(url);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("打开链接失败（{}）", result.0 as isize))
    }
}

/// 关键字 exec 模板：首词为程序，其余为参数（{query} 已替换）。
pub fn open_command_line(template: &str) -> Result<(), String> {
    let mut parts = template.split_whitespace();
    let Some(program) = parts.next() else {
        return Err("空命令".to_string());
    };
    Command::new(program)
        .args(parts)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_system_tools_to_absolute_paths() {
        // 控制面板小程序在索引里是相对名，必须能解析到 System32 的真实文件。
        let resolved = resolved_target(Path::new("appwiz.cpl"));
        assert!(resolved.is_absolute(), "应解析为绝对路径: {}", resolved.display());
        assert!(resolved.exists(), "目标文件应存在: {}", resolved.display());
    }

    #[test]
    fn keeps_uri_and_missing_paths_untouched() {
        let uri = resolved_target(Path::new("ms-settings:appsfeatures"));
        assert_eq!(uri.to_string_lossy(), "ms-settings:appsfeatures");
        let missing = resolved_target(Path::new("no-such-file-xyz.cpl"));
        assert_eq!(missing.to_string_lossy(), "no-such-file-xyz.cpl");
    }

    fn entry(kind: Kind, path: Option<&str>) -> Entry {
        Entry {
            id: "test".to_string(),
            kind,
            name: "test".to_string(),
            path: path.map(PathBuf::from),
            size: None,
            modified: None,
            pinyin: None,
            initial: String::new(),
            tone: 0,
        }
    }

    #[test]
    fn terminal_dir_is_folder_itself_or_file_parent() {
        let folder = entry(Kind::Folder, Some(r"C:\work\project"));
        assert_eq!(
            terminal_dir(&folder).as_deref(),
            Some(Path::new(r"C:\work\project"))
        );

        let file = entry(Kind::File, Some(r"C:\work\project\notes.txt"));
        assert_eq!(
            terminal_dir(&file).as_deref(),
            Some(Path::new(r"C:\work\project")),
            "文件应在所在目录打开终端"
        );

        let program = entry(Kind::Program, Some(r"C:\Tools\app.exe"));
        assert_eq!(
            terminal_dir(&program).as_deref(),
            Some(Path::new(r"C:\Tools"))
        );
    }

    #[test]
    fn terminal_dir_rejects_apps_commands_and_missing_paths() {
        assert_eq!(terminal_dir(&entry(Kind::App, Some(r"C:\a\app.lnk"))), None);
        assert_eq!(terminal_dir(&entry(Kind::Command, None)), None);
        assert_eq!(terminal_dir(&entry(Kind::File, None)), None);
    }

    #[test]
    fn open_with_requires_a_path() {
        // 无路径的命令/结果不进 shell，直接给出错误（不弹系统对话框）。
        assert!(open_with(&entry(Kind::File, None)).is_err());
    }

    /// V2 回收站元数据：version/size/time/len + UTF-16LE 原路径。
    #[test]
    fn parses_recycle_metadata_v2() {
        let original = r"E:\use\ai\deepseek\Rust\lstr\test.log";
        let utf16: Vec<u16> = original.encode_utf16().collect();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&123u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&(utf16.len() as u32).to_le_bytes());
        for unit in &utf16 {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes()); // 结尾 NUL（len 不含它）
        assert_eq!(
            parse_recycle_metadata(&bytes).as_deref(),
            Some(Path::new(original))
        );
    }

    /// V1 元数据：固定 260 个 WCHAR 路径缓冲区。
    #[test]
    fn parses_recycle_metadata_v1() {
        let original = r"C:\Temp\old.txt";
        let utf16: Vec<u16> = original.encode_utf16().collect();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        let mut buffer = [0u16; 260];
        buffer[..utf16.len()].copy_from_slice(&utf16);
        for unit in buffer {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            parse_recycle_metadata(&bytes).as_deref(),
            Some(Path::new(original))
        );
    }

    #[test]
    fn rejects_malformed_recycle_metadata() {
        assert!(parse_recycle_metadata(&[]).is_none());
        assert!(parse_recycle_metadata(&[0u8; 23]).is_none());
        // 未知版本。
        let mut bytes = vec![3u8; 64];
        bytes[0..8].copy_from_slice(&3u64.to_le_bytes());
        assert!(parse_recycle_metadata(&bytes).is_none());
        // V2 声明长度超过实际数据。
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes());
        assert!(parse_recycle_metadata(&bytes).is_none());
    }
}
