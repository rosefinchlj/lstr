//! Quick Switch 数据源：枚举当前打开的 Explorer 窗口文件夹（ShellWindows COM）。
//! 对话框模式空查询时这些目录置顶展示，Enter 一键把「打开/保存」对话框切过去——
//! 这是 Listary Pro 用户最高频的工作流。

use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::Interface;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    IDispatch,
};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowTextW,
};

/// P0-3：记住「最近一次前台 Explorer 窗口正在浏览的目录」。
/// 文件对话框拿到前台后，前台窗口已不是 Explorer，只能靠这份缓存做
/// Ctrl+G 一键跳 / 自动跟随。
static LAST_FOLDER: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static LAST_SEEN: OnceLock<Mutex<(isize, String)>> = OnceLock::new();

fn last_folder() -> &'static Mutex<Option<PathBuf>> {
    LAST_FOLDER.get_or_init(|| Mutex::new(None))
}

fn last_seen() -> &'static Mutex<(isize, String)> {
    LAST_SEEN.get_or_init(|| Mutex::new((0, String::new())))
}

/// 返回缓存的活动目录（不触发 COM 枚举，可在每帧/热键路径里安全调用）。
pub fn active_explorer_folder() -> Option<PathBuf> {
    last_folder().lock().ok().and_then(|guard| guard.clone())
}

/// 每帧调用一次：只有「前台窗口是 Explorer 且窗口标题变化」时才做一次
/// ShellWindows 枚举，避免把 COM 枚举（几十 ms）放进渲染循环。
/// 匹配方式是窗口标题 ≈ 目录名，属于近似；匹配不到时保留上一次的值。
pub fn note_foreground_explorer() {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return;
        }
        let mut class_buf = [0u16; 64];
        let class_len = GetClassNameW(hwnd, &mut class_buf);
        if class_len <= 0 {
            return;
        }
        let class = String::from_utf16_lossy(&class_buf[..class_len as usize]);
        if !class.eq_ignore_ascii_case("CabinetWClass")
            && !class.eq_ignore_ascii_case("ExploreWClass")
        {
            return;
        }
        let mut title_buf = [0u16; 256];
        let title_len = GetWindowTextW(hwnd, &mut title_buf);
        if title_len <= 0 {
            return;
        }
        let title = String::from_utf16_lossy(&title_buf[..title_len as usize]);
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return;
        }
        let key = hwnd.0 as isize;
        if let Ok(mut seen) = last_seen().lock() {
            if seen.0 == key && seen.1 == trimmed {
                return;
            }
            seen.0 = key;
            seen.1 = trimmed.to_string();
        }
        if let Some(folder) = open_explorer_folders().into_iter().find(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().eq_ignore_ascii_case(trimmed))
                .unwrap_or(false)
        }) {
            if let Ok(mut guard) = last_folder().lock() {
                *guard = Some(folder);
            }
        }
    }
}

/// 返回当前 Explorer 窗口正在显示的文件系统目录（按窗口枚举顺序，去重）。
/// 只收真实存在的路径：回收站/控制面板等虚拟位置会被过滤掉。
pub fn open_explorer_folders() -> Vec<PathBuf> {
    // ShellWindows 走 STA；worker/测试线程现场初始化，RPC_E_CHANGED_MODE 说明
    // 线程已按 MTA 初始化，直接复用现有套间即可。
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let need_uninit = if hr.is_ok() {
        // S_OK 与 S_FALSE 都要配对 CoUninitialize。
        true
    } else if hr == RPC_E_CHANGED_MODE {
        false
    } else {
        return Vec::new();
    };
    let folders = unsafe { enumerate() };
    if need_uninit {
        unsafe { CoUninitialize() };
    }
    folders
}

unsafe fn enumerate() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(shell_windows): Result<IShellWindows, _> =
        CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)
    else {
        return out;
    };
    let count = shell_windows.Count().unwrap_or(0).max(0);
    for index in 0..count.min(64) {
        let Ok(dispatch) = shell_windows.Item(&int_variant(index)) else {
            continue;
        };
        if let Some(path) = window_folder_path(&dispatch) {
            if !out.iter().any(|existing| existing == &path) {
                out.push(path);
            }
        }
    }
    out
}

/// 单个 Shell 窗口 → 文件夹路径；非 Explorer 进程或虚拟位置返回 None。
unsafe fn window_folder_path(dispatch: &IDispatch) -> Option<PathBuf> {
    let browser: IWebBrowser2 = dispatch.cast().ok()?;
    // ShellWindows 里混着 IE/Office 的窗口对象，只收 explorer.exe。
    let full_name = browser.FullName().ok()?.to_string();
    if !full_name.to_lowercase().ends_with("explorer.exe") {
        return None;
    }
    // LocationURL：file:///E:/use/ai；虚拟位置（此电脑/回收站）为空串。
    let url = browser.LocationURL().ok()?.to_string();
    let path = PathBuf::from(folder_path_from_url(&url)?);
    // 过滤掉已被关闭/指向不存在位置的窗口（快速切换窗口的瞬间会出现）。
    if !path.exists() {
        return None;
    }
    Some(path)
}

/// `file:///E:/use/ai` / `file:///E:/%E4%B8%AD%E6%96%87` → `E:\use\ai`。
/// 非 file 协议或空路径（虚拟命名空间）返回 None。
fn folder_path_from_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file:///")?;
    if rest.is_empty() {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(decoded).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(text.replace('/', "\\"))
}

/// ShellWindows::Item 只吃 VARIANT 索引，手工构造 VT_I4。
fn int_variant(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VARENUM(VT_I4.0),
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_does_not_panic() {
        // COM 枚举依赖桌面会话里开着的 Explorer 窗口数量，内容不固定；
        // 这里只保证调用安全且返回合法数据。
        let folders = open_explorer_folders();
        for path in &folders {
            assert!(path.is_absolute(), "枚举结果必须是绝对路径: {path:?}");
            assert!(path.exists(), "枚举结果必须真实存在: {path:?}");
        }
    }

    #[test]
    fn active_folder_cache_read_is_safe() {
        let _ = active_explorer_folder();
    }

    #[test]
    fn file_urls_decode_to_windows_paths() {
        assert_eq!(
            folder_path_from_url("file:///E:/use/ai"),
            Some("E:\\use\\ai".to_string())
        );
        assert_eq!(
            folder_path_from_url("file:///E:/%E4%B8%AD%E6%96%87"),
            Some("E:\\中文".to_string())
        );
        assert_eq!(
            folder_path_from_url("file:///C:/Program%20Files"),
            Some("C:\\Program Files".to_string())
        );
    }

    #[test]
    fn non_file_urls_and_virtual_locations_are_rejected() {
        assert_eq!(folder_path_from_url("http://example.com/"), None);
        assert_eq!(folder_path_from_url(""), None);
        // 此电脑/回收站等 shell 命名空间没有 file:/// 形式。
        assert_eq!(folder_path_from_url("file:///"), None);
    }
}
