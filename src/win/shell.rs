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

pub fn launch_everything() -> Result<(), String> {
    // 注册表 App Paths 覆盖自定义安装位置；标准路径作为兜底。
    let candidates = [
        registry_everything_path(),
        Some(PathBuf::from(r"C:\Program Files\Everything\Everything.exe")),
        Some(PathBuf::from(
            r"C:\Program Files (x86)\Everything\Everything.exe",
        )),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return Command::new(&candidate)
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("启动 Everything 失败: {e}"));
        }
    }
    // 最后试试 PATH 里的 Everything.exe（便携版常见）。
    Command::new("Everything.exe")
        .spawn()
        .map(|_| ())
        .map_err(|_| "未找到 Everything.exe".to_string())
}

fn registry_everything_path() -> Option<PathBuf> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\App Paths\Everything.exe");
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut size = 0u32;
        let found = unsafe {
            RegGetValueW(
                root,
                PCWSTR(subkey.as_ptr()),
                w!(""),
                flags,
                None,
                None,
                Some(&mut size),
            )
        };
        if found != ERROR_SUCCESS || size < 4 {
            continue;
        }
        let mut buffer = vec![0u16; (size as usize / 2) + 1];
        let mut written = (buffer.len() * 2) as u32;
        let found = unsafe {
            RegGetValueW(
                root,
                PCWSTR(subkey.as_ptr()),
                w!(""),
                flags,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut written),
            )
        };
        if found != ERROR_SUCCESS {
            continue;
        }
        let len = (written as usize / 2).saturating_sub(1);
        let path = PathBuf::from(String::from_utf16_lossy(&buffer[..len]));
        if path.exists() {
            return Some(path);
        }
    }
    None
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
}
