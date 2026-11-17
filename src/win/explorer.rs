//! Quick Switch 数据源 + Explorer 当前目录上下文（ShellWindows COM）。
//!
//! 两条链路共用同一份「前台 Explorer 窗口」信息：
//! 1. 对话框模式空查询：列出已打开的文件夹，Enter 一键把「打开/保存」对话框切过去；
//! 2. Explorer 内键入即搜：锁定当前目录作为搜索范围，并让文件夹结果在**原窗口**跳转。
//!
//! 目录匹配用 ShellWindows 里 IWebBrowserApp::HWND 与 GetForegroundWindow 精确比对，
//! 不用「窗口标题 ≈ 目录名」猜——同名目录（两个 src）会串错，而 HWND 不会。

use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::{Interface, BSTR, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED, IDispatch,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4,
};
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellFolder, IShellWindows, IWebBrowser2, SHParseDisplayName,
    ShellWindows, SBSP_ABSOLUTE, SID_STopLevelBrowser, SVGIO_BACKGROUND, SVSI_DESELECTOTHERS,
    SVSI_ENSUREVISIBLE, SVSI_SELECT,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowTextW,
};

/// 记住「最近一次前台 Explorer 窗口正在浏览的目录」。
/// 文件对话框/启动器拿到前台后，前台窗口已不是 Explorer，只能靠这份缓存做
/// Ctrl+G 一键跳 / {current_folder} 命令。
static LAST_FOLDER: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static LAST_SEEN: OnceLock<Mutex<(isize, String)>> = OnceLock::new();
/// 同一个窗口的 HWND：结果要跳回「原来那个」资源管理器，而不是新开一个。
static ACTIVE_HWND: OnceLock<Mutex<Option<isize>>> = OnceLock::new();

fn last_folder() -> &'static Mutex<Option<PathBuf>> {
    LAST_FOLDER.get_or_init(|| Mutex::new(None))
}

fn last_seen() -> &'static Mutex<(isize, String)> {
    LAST_SEEN.get_or_init(|| Mutex::new((0, String::new())))
}

fn active_hwnd_slot() -> &'static Mutex<Option<isize>> {
    ACTIVE_HWND.get_or_init(|| Mutex::new(None))
}

fn remember(hwnd: isize, folder: &PathBuf) {
    if let Ok(mut guard) = last_folder().lock() {
        *guard = Some(folder.clone());
    }
    if let Ok(mut guard) = active_hwnd_slot().lock() {
        *guard = Some(hwnd);
    }
}

/// 返回缓存的活动目录（不触发 COM 枚举，可在每帧/热键路径里安全调用）。
pub fn active_explorer_folder() -> Option<PathBuf> {
    last_folder().lock().ok().and_then(|guard| guard.clone())
}

/// 前台 Explorer 的锚点：窗口句柄 + 当前目录（贴角浮层用）。
///
/// 只在「键盘钩子吞下第一个字符」的那一刻调用：此刻前台还是资源管理器，
/// 一旦 lstr 抢到前台，`GetForegroundWindow` 就变成 lstr 自己，锚点窗口
/// 再也找不回来。
///
/// 返回的 HWND 用于：浮层贴右下角的几何锚定、跟随窗口移动，以及收起时
/// 把前台交还给它。目录解析失败（此电脑/回收站/控制面板等虚拟位置）时
/// 返回 None，退化回普通居中启动器。
pub fn foreground_explorer_anchor() -> Option<(isize, PathBuf)> {
    let handle = unsafe { GetForegroundWindow() };
    let hwnd = handle.0 as isize;
    if hwnd == 0 {
        log_explorer("anchor: no foreground window");
        return None;
    }
    let title = window_title(handle);
    let resolved = with_com(|| unsafe { resolve_foreground(hwnd, &title) }).flatten();
    let (matched_hwnd, folder) = resolved?;
    log_explorer(&format!(
        "anchor hwnd={hwnd} matched={matched_hwnd} folder={}",
        folder.display()
    ));
    remember(matched_hwnd, &folder);
    // 用 ShellWindows 匹配到的 HWND 而不是 GetForegroundWindow 的返回值：
    // 少数情况下前台是 Explorer 的子窗口/弹层，只有前者是能贴边的 frame。
    Some((matched_hwnd, folder))
}

/// 从 Explorer 窗口标题解析「活动标签名」。
///
/// Win11 标签页窗口的 caption 不是裸目录名，实测（26H2 中文）：
/// - 多标签：`4.2.0.13 和 2 个其他选项卡 - 文件资源管理器`
/// - 单标签：`<名> - 文件资源管理器`
/// - 无后缀（控制面板）：`程序和功能`
/// 旧的「整标题 == 目录名」比对永远失败，只能退回枚举第一个（= 非活动标签）。
fn active_tab_name(title: &str) -> String {
    let t = title.trim();
    // 中文多标签：`<名> 和 N 个其他选项卡 ...`——必须校验后缀形态，
    // 否则目录名本身含「 和 」的窗口（A 和 B）会被截错。
    if let Some(idx) = t.find(" 和 ") {
        let rest = &t[idx + " 和 ".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest.contains("个其他选项卡") {
            return t[..idx].trim().to_string();
        }
    }
    // 英文多标签：`<name> and N other tabs - File Explorer`
    if let Some(idx) = t.find(" and ") {
        let rest = &t[idx + " and ".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest.to_ascii_lowercase().contains("other tab") {
            return t[..idx].trim().to_string();
        }
    }
    // 单标签：去掉 ` - 文件资源管理器` / ` - File Explorer` 后缀。
    if let Some((head, tail)) = t.rsplit_once(" - ") {
        if tail.contains("文件资源管理器") || tail.to_ascii_lowercase().contains("file explorer") {
            return head.trim().to_string();
        }
    }
    t.to_string()
}

/// 一次 ShellWindows 枚举里先试 HWND 精确匹配，再试标题近似匹配。
/// 返回匹配到的 Shell 窗口 HWND（标题兜底时可能不是原前台窗口，但至少是真实窗口）。
///
/// Win11 标签页资源管理器：同一窗口的多个标签是多个 IWebBrowser2，但共享
/// 同一个顶层 HWND。只按 HWND 取第一个会选到**非活动标签**（实测：活动标签
/// 在 C:\...\4.2.0.13，却解析成另一个标签"文档"）。同 HWND 多候选时解析
/// 标题里的活动标签名，与目录名精确匹配来消歧。
unsafe fn resolve_foreground(hwnd: isize, title: &str) -> Option<(isize, PathBuf)> {
    let list = enumerate_with_hwnd();
    let mut exact: Vec<(isize, PathBuf)> = list
        .iter()
        .filter(|(candidate, _)| *candidate == hwnd)
        .cloned()
        .collect();
    let active = active_tab_name(title);
    if exact.len() > 1 && !active.is_empty() {
        if let Some(hit) = exact.iter().find(|(_, path)| {
            path.file_name()
                .map(|name| name.to_string_lossy().eq_ignore_ascii_case(&active))
                .unwrap_or(false)
        }) {
            return Some(hit.clone());
        }
    }
    if !exact.is_empty() {
        // 活动标签名对不上（本地化显示名如「文档」vs Documents）：保持旧行为取第一个。
        return Some(exact.remove(0));
    }
    if active.is_empty() {
        return None;
    }
    list.iter()
        .find(|(_, path)| {
            path.file_name()
                .map(|name| name.to_string_lossy().eq_ignore_ascii_case(&active))
                .unwrap_or(false)
        })
        .cloned()
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let mut buffer = [0u16; 256];
        let length = GetWindowTextW(hwnd, &mut buffer);
        if length <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..length as usize])
            .trim()
            .to_string()
    }
}

/// 与 typing.rs 共用 typing.log：目录解析/窗口跳转失败时用户可直接把日志发来定位。
fn log_explorer(line: &str) {
    let path = crate::config::attach_dir().join("typing.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = std::io::Write::write_fmt(&mut file, format_args!("[explorer] {line}\n"));
    }
}

/// 让「原来那个」资源管理器窗口导航到指定文件夹，不新开窗口。
/// 找不到窗口（已关闭 / 桌面 / 虚拟位置）时返回 Err，由调用方降级为普通打开。
pub fn navigate_active_to(folder: &str) -> Result<(), String> {
    navigate_to(None, folder)
}

/// 让**指定** HWND 的资源管理器窗口就地跳到 `folder`（贴角浮层专用）。
///
/// 贴角模式在触发那一刻就拿到了锚点窗口的 HWND（`foreground_explorer_anchor`），
/// 直接按它匹配，不再依赖「上次记录的活动窗口」——用户中途点过别的资源管理器
/// 也不会跳错窗口。
pub fn navigate_hwnd_to(hwnd: isize, folder: &str) -> Result<(), String> {
    navigate_to(Some(hwnd), folder)
}

fn navigate_to(target_hwnd: Option<isize>, folder: &str) -> Result<(), String> {
    let hwnd = match target_hwnd {
        Some(hwnd) => hwnd,
        None => match active_hwnd_slot().lock().ok().and_then(|guard| *guard) {
            Some(hwnd) => hwnd,
            None => {
                log_explorer("navigate failed: no active Explorer HWND recorded");
                return Err("没有记录的资源管理器窗口".to_string());
            }
        },
    };
    // 记录时的活动目录：标签页窗口里用它挑出「活动标签」那个浏览器
    // （同 HWND 的多个标签共享顶层窗口，只按 HWND 取第一个会跳到非活动标签）。
    let remembered = last_folder().lock().ok().and_then(|guard| guard.clone());
    with_com(|| unsafe {
        let Ok(shell_windows): Result<IShellWindows, _> =
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)
        else {
            return Err("无法连接 ShellWindows".to_string());
        };
        let count = shell_windows.Count().unwrap_or(0).max(0);
        let mut first_hwnd_match: Option<i32> = None;
        let mut active_tab: Option<i32> = None;
        for index in 0..count.min(64) {
            let Ok(dispatch) = shell_windows.Item(&int_variant(index)) else {
                continue;
            };
            let Ok(browser): Result<IWebBrowser2, _> = dispatch.cast() else {
                continue;
            };
            let Ok(window_hwnd) = browser.HWND() else {
                continue;
            };
            if window_hwnd.0 != hwnd {
                continue;
            }
            if first_hwnd_match.is_none() {
                first_hwnd_match = Some(index);
            }
            if let Some(want) = &remembered {
                if let Ok(location) = browser.LocationURL() {
                    if let Some(path) = folder_path_from_url(&location.to_string()) {
                        if PathBuf::from(&path) == *want {
                            active_tab = Some(index);
                            break;
                        }
                    }
                }
            }
        }
        let Some(chosen) = active_tab.or(first_hwnd_match) else {
            log_explorer(&format!(
                "navigate failed: Explorer HWND {hwnd} no longer in ShellWindows"
            ));
            return Err("目标资源管理器窗口已关闭".to_string());
        };
        let Ok(dispatch) = shell_windows.Item(&int_variant(chosen)) else {
            return Err("目标资源管理器窗口已关闭".to_string());
        };
        let Ok(browser): Result<IWebBrowser2, _> = dispatch.cast() else {
            return Err("目标资源管理器窗口已关闭".to_string());
        };
        // 首选 shell 命名空间的 BrowseObject(PIDL)：绕开 URL 编码层。
        // IWebBrowser2::Navigate 的 %XX 转义在 explorer 里按 ANSI 代码页解码，
        // 中文目录会变成乱码并弹出「无法找到 file:///…」（实测
        // D:\Program Files (x86)\方直金太阳教师版…），结果落到新开窗口兜底。
        // PIDL 是 shell 原生标识，任意 Unicode 都原样命中，也不会新开窗口。
        let result = match browse_object_to(&browser, Path::new(folder)) {
            Ok(()) => Ok(()),
            Err(browse_err) => {
                log_explorer(&format!(
                    "navigate browse failed: {browse_err}; fallback to raw url"
                ));
                navigate_raw_url(&browser, folder)
            }
        };
        log_explorer(&format!(
            "navigate hwnd={hwnd} tab={chosen} folder={folder} result={result:?}"
        ));
        result
    })
    .unwrap_or_else(|| Err("COM 初始化失败".to_string()))
}

/// 用 PIDL 让指定资源管理器窗口就地跳转（贴角浮层 / Ctrl+G 共用）。
unsafe fn browse_object_to(browser: &IWebBrowser2, folder: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = folder
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None)
        .map_err(|err| format!("SHParseDisplayName 失败: {err}"))?;
    if pidl.is_null() {
        return Err("SHParseDisplayName 返回空 PIDL".to_string());
    }
    let outcome = (|| {
        let provider: IServiceProvider = browser
            .cast()
            .map_err(|err| format!("IServiceProvider 失败: {err}"))?;
        let shell_browser: IShellBrowser = provider
            .QueryService(&SID_STopLevelBrowser)
            .map_err(|err| format!("QueryService 失败: {err}"))?;
        shell_browser
            .BrowseObject(pidl, SBSP_ABSOLUTE)
            .map_err(|err| format!("BrowseObject 失败: {err}"))
    })();
    CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
    outcome
}

/// 兜底：用**不转义**的 file:// URL 调 Navigate。
///
/// 不能用 `path_to_file_url` 的 %XX 形式：explorer 的 Navigate 会按 ANSI 代码页
/// 解码转义，中文目录变成乱码后弹「无法找到」。BSTR 本身是 UTF-16，未转义的
/// Unicode 路径能原样送达（#/?/% 等少见字符可能出问题，所以只作兜底）。
fn navigate_raw_url(browser: &IWebBrowser2, folder: &str) -> Result<(), String> {
    let url = raw_file_url(folder);
    // 四个 VARIANT* 必须传真实 VARIANT：IWebBrowser2 跨进程（explorer.exe）
    // 代理会把 NULL 指针判成坏参数，返回 RPC_X_BAD_STUB_DATA(0x800706F4)，
    // 跳转就静默失败、结果落到 shell::open_entry 又开出一个新窗口。
    // 传 VT_EMPTY 的 VARIANT 与「省略参数」语义等价，实测可用。
    let target = BSTR::from(url.as_str());
    let empty = VARIANT::default();
    unsafe {
        browser
            .Navigate(
                &target,
                Some(&empty as *const VARIANT),
                Some(&empty as *const VARIANT),
                Some(&empty as *const VARIANT),
                Some(&empty as *const VARIANT),
            )
            .map_err(|err| format!("资源管理器跳转失败: {err}"))
    }
}

/// 让资源管理器活动标签的选中项跟着贴角浮层走（Listary 的联动行为）。
///
/// 只同步「当前目录的直接子项」：深层结果在资源管理器里不可见，强行同步只会
/// 把选中项跳到祖先目录上，反而打断用户。任何一步失败都静默返回 false——
/// 这是增强项，不能影响输入、打开等主流程。
pub fn select_in_explorer(hwnd: isize, path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let name = name.to_string_lossy().to_string();
    let result = with_com(|| unsafe { select_in_explorer_inner(hwnd, parent, &name) })
        .unwrap_or(Err("COM 初始化失败"));
    match result {
        Ok(()) => {
            log_explorer(&format!(
                "select hwnd={hwnd} path={} result=ok",
                path.display()
            ));
            true
        }
        Err(reason) => {
            // 失败路径也记一行：联动是静默降级的，没有日志就没法排查
            // 「为什么资源管理器没跟着选中」。
            log_explorer(&format!(
                "select hwnd={hwnd} path={} result={reason}",
                path.display()
            ));
            false
        }
    }
}

unsafe fn select_in_explorer_inner(
    hwnd: isize,
    parent: &Path,
    name: &str,
) -> Result<(), &'static str> {
    let Ok(shell_windows): Result<IShellWindows, _> =
        CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)
    else {
        return Err("no-shellwindows");
    };
    // 多标签窗口：同一个 HWND 下每个标签各有一个 IWebBrowser2，只有「正显示
    // 我们搜索目录」的那个才是活动标签。目录对不上就不动手（用户可能切走了）。
    let count = shell_windows.Count().unwrap_or(0).max(0);
    let mut active_browser: Option<IWebBrowser2> = None;
    for index in 0..count.min(64) {
        let Ok(dispatch) = shell_windows.Item(&int_variant(index)) else {
            continue;
        };
        let Ok(browser): Result<IWebBrowser2, _> = dispatch.cast() else {
            continue;
        };
        let Ok(window_hwnd) = browser.HWND() else {
            continue;
        };
        if window_hwnd.0 != hwnd {
            continue;
        }
        let Some(folder) = browser
            .LocationURL()
            .ok()
            .and_then(|url| folder_path_from_url(&url.to_string()))
        else {
            continue;
        };
        if same_folder(&folder, parent) {
            active_browser = Some(browser);
            break;
        }
    }
    let Some(browser) = active_browser else {
        return Err("folder-mismatch");
    };
    let Ok(provider) = browser.cast::<IServiceProvider>() else {
        return Err("no-service-provider");
    };
    let Ok(shell_browser) = provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else {
        return Err("no-shell-browser");
    };
    let Ok(view) = shell_browser.QueryActiveShellView() else {
        return Err("no-shell-view");
    };
    let Some(folder_obj) = shell_folder_of(&view) else {
        return Err("no-shell-folder");
    };
    let wide = HSTRING::from(name);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    if folder_obj
        .ParseDisplayName(
            HWND::default(),
            None,
            &wide,
            None,
            &mut pidl,
            std::ptr::null_mut(),
        )
        .is_err()
    {
        return Err("no-pidl");
    }
    if pidl.is_null() {
        return Err("null-pidl");
    }
    // 不带 SVSI_FOCUSED：那会把前台从贴角浮层抢回资源管理器，触发我们自己的
    // 失焦收起。只改选中项 + 滚入视野，键盘焦点仍留在浮层输入框。
    let flags = (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0 | SVSI_ENSUREVISIBLE.0) as u32;
    let ok = view.SelectItem(pidl, flags).is_ok();
    CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
    if ok {
        Ok(())
    } else {
        Err("selectitem-failed")
    }
}

/// 从活动视图拿当前文件夹的 IShellFolder。
///
/// 正常路径是 `IShellView::GetItemObject(SVGIO_BACKGROUND, IID_IShellFolder)`；
/// 少数视图（缩略图/预览等非 DefView 包装）上 GetItemObject 会失败，退回
/// `IFolderView::GetFolder`——两条路拿到的是同一个文件夹对象。
unsafe fn shell_folder_of(view: &windows::Win32::UI::Shell::IShellView) -> Option<IShellFolder> {
    if let Ok(folder) = view.GetItemObject::<IShellFolder>(SVGIO_BACKGROUND) {
        return Some(folder);
    }
    let folder_view = view.cast::<IFolderView>().ok()?;
    folder_view.GetFolder::<IShellFolder>().ok()
}

/// 目录比较：忽略大小写与结尾分隔符（`F:\C下载` vs `F:\c下载\`）。
fn same_folder(folder: &str, expected: &Path) -> bool {
    let normalized = |value: &str| {
        value
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    let expected = expected.to_string_lossy();
    normalized(folder) == normalized(&expected)
}

/// 每帧调用一次：只有「前台窗口是 Explorer 且窗口标题变化」时才做一次
/// ShellWindows 枚举，避免把 COM 枚举（几十 ms）放进渲染循环。
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
        let title = window_title(hwnd);
        let key = hwnd.0 as isize;
        if let Ok(mut seen) = last_seen().lock() {
            if seen.0 == key && seen.1 == title {
                return;
            }
            seen.0 = key;
            seen.1 = title.clone();
        }
        if let Some((matched, folder)) =
            with_com(|| resolve_foreground(key, &title)).flatten()
        {
            remember(matched, &folder);
        }
    }
}

/// 统一处理 COM 套间初始化：调用线程可能是 egui 主线程（STA 已初始化，返回
/// S_FALSE），也可能是 worker（MTA，RPC_E_CHANGED_MODE 时复用现有套间）。
fn with_com<T>(work: impl FnOnce() -> T) -> Option<T> {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let need_uninit = hr.is_ok();
    if !need_uninit && hr != RPC_E_CHANGED_MODE {
        return None;
    }
    let result = work();
    if need_uninit {
        unsafe { CoUninitialize() };
    }
    Some(result)
}

/// 返回当前 Explorer 窗口正在显示的文件系统目录（按窗口枚举顺序，去重）。
/// 只收真实存在的路径：回收站/控制面板等虚拟位置会被过滤掉。
pub fn open_explorer_folders() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for (_, path) in with_com(|| unsafe { enumerate_with_hwnd() }).unwrap_or_default() {
        if !out.iter().any(|existing| existing == &path) {
            out.push(path);
        }
    }
    out
}

unsafe fn enumerate_with_hwnd() -> Vec<(isize, PathBuf)> {
    let mut out: Vec<(isize, PathBuf)> = Vec::new();
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
        if let Some(info) = window_info(&dispatch) {
            out.push(info);
        }
    }
    out
}

/// 单个 Shell 窗口 → (HWND, 文件夹路径)；非 Explorer 进程或虚拟位置返回 None。
unsafe fn window_info(dispatch: &IDispatch) -> Option<(isize, PathBuf)> {
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
    let hwnd = browser.HWND().map(|value| value.0).unwrap_or(0);
    Some((hwnd, path))
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

/// 本地路径 → **不转义**的 file:// URL（Navigate 兜底用）。
///
/// 故意不做 %XX 百分号编码：explorer 的 `Navigate` 会按 ANSI 代码页解码转义，
/// 中文目录会变成乱码并弹「无法找到」；BSTR 本身是 UTF-16，未转义的 Unicode
/// 路径能原样送达。UNC `\\server\share` 走 `file://server/share` 分支。
fn raw_file_url(folder: &str) -> String {
    let normalized = folder.replace('\\', "/");
    if let Some(share) = normalized.strip_prefix("//") {
        format!("file://{share}")
    } else {
        format!("file:///{}", normalized.trim_start_matches('/'))
    }
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

    #[test]
    fn file_urls_keep_unicode_unescaped() {
        // Navigate 走的是「不转义」URL：转义形式会被 explorer 按 ANSI 代码页解码。
        assert_eq!(raw_file_url("E:\\use\\ai"), "file:///E:/use/ai");
        assert_eq!(raw_file_url("C:\\Program Files"), "file:///C:/Program Files");
        assert_eq!(raw_file_url("D:\\中文"), "file:///D:/中文");
        assert_eq!(
            raw_file_url("\\\\server\\share\\dir"),
            "file://server/share/dir"
        );
    }

    #[test]
    fn parses_active_tab_from_window_caption() {
        // 中文 Win11 多标签（实测 26H2）
        assert_eq!(
            active_tab_name("4.2.0.13 和 2 个其他选项卡 - 文件资源管理器"),
            "4.2.0.13"
        );
        assert_eq!(
            active_tab_name("金太阳教育软件客户端 和 3 个其他选项卡 - 文件资源管理器"),
            "金太阳教育软件客户端"
        );
        // 英文多标签
        assert_eq!(
            active_tab_name("4.2.0.13 and 2 other tabs - File Explorer"),
            "4.2.0.13"
        );
        // 单标签
        assert_eq!(active_tab_name("文档 - 文件资源管理器"), "文档");
        assert_eq!(active_tab_name("Downloads - File Explorer"), "Downloads");
        // 无后缀（控制面板）
        assert_eq!(active_tab_name("程序和功能"), "程序和功能");
        // 目录名本身含「 和 」但非多标签格式：原样保留
        assert_eq!(active_tab_name("A 和 B"), "A 和 B");
    }

    #[test]
    fn same_folder_ignores_case_and_trailing_separator() {
        let expected = std::path::Path::new("F:\\C下载");
        assert!(same_folder("F:\\C下载", expected));
        assert!(same_folder("f:\\c下载", expected));
        assert!(same_folder("F:\\C下载\\", expected));
        assert!(!same_folder("F:\\C下载\\子目录", expected));
        assert!(!same_folder("F:\\C下载2", expected));
        let drive = std::path::Path::new("H:\\");
        assert!(same_folder("H:\\", drive));
        assert!(!same_folder("H:\\子目录", drive));
        // 大小写不同的目录名在 Windows 上是同一个目录，比较必须同规则。
        let expected = std::path::Path::new("C:\\Users\\Jun.Liu\\Pictures");
        assert!(same_folder("C:\\Users\\jun.liu\\Pictures", expected));
    }

    #[test]
    fn select_in_explorer_rejects_relative_paths() {
        // 只有文件名、没有父目录的条目（例如命令/应用结果）不参与资源管理器联动。
        assert!(!select_in_explorer(0, std::path::Path::new("chrome.exe")));
    }
}
