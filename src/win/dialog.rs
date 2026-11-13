use windows::core::{BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_ACCESS_DENIED, ERROR_SUCCESS, HWND, LPARAM, RECT, TRUE,
    WPARAM,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetActiveWindow, SetFocus, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_CLICK, BringWindowToTop, EnumChildWindows, EnumWindows, FindWindowExW, GetClassNameW,
    GetDlgItem, GetForegroundWindow, WM_KEYDOWN, WM_KEYUP,
    GetParent, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    PostMessageW, SetForegroundWindow, SendMessageW, IDOK, WM_COMMAND, WM_SETTEXT,
};

const DIALOG_CLASS: &str = "#32770";
const DIRECT_UI_CLASS: &str = "DirectUIHWND";
const COMBO_EX_CLASS: &str = "ComboBoxEx32";
const COMBO_CLASS: &str = "ComboBox";
const EDIT_CLASS: &str = "Edit";
const SEARCH_BOX_CLASS: &str = "SearchEditBoxWrapperClass";
/// 文件列表视图：真正的文件对话框必有其一。只凭「#32770 + ComboBoxEx →
/// ComboBox → Edit」会把 mstsc「远程桌面连接」这类普通对话框误认成文件
/// 对话框（它的「计算机」下拉正好是同一条控件链）。
const FILE_VIEW_CLASSES: [&str; 3] = ["SysListView32", "ShellView", "SHELLDLL_DefView"];

#[derive(Clone, Debug)]
pub struct DialogInfo {
    pub hwnd: isize,
    pub edit: isize,
    pub is_save: bool,
    /// Vista+ IFileDialog（现代打开/保存对话框）：确认走按钮点击而非 IDOK。
    pub vista: bool,
}

pub fn active_dialog() -> Option<DialogInfo> {
    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.0.is_null() {
            if let Some(info) = dialog_info(foreground) {
                return Some(info);
            }
        }

        // 启动器可能盖在文件对话框上面：枚举可见的 #32770，按 z-order 取最上层。
        let mut candidates: Vec<HWND> = Vec::new();
        let _ = EnumWindows(
            Some(enum_dialog_candidates),
            LPARAM(&mut candidates as *mut _ as isize),
        );
        candidates.into_iter().find_map(|hwnd| dialog_info(hwnd))
    }
}

/// 自动唤出用：前台是文件对话框，或前台窗口所属进程有可见的文件对话框。
///
/// Win11 记事本（XAML 岛）Ctrl+O 的 `#32770` 显示后，前台仍停在主窗口
/// （`GetForegroundWindow` 返回 `Notepad`），只认「前台 == #32770」会漏掉
/// 这类宿主。此时按 pid 找同进程的可见 `#32770`；不同进程的后台对话框
/// 不会被认领。
pub fn foreground_dialog() -> Option<DialogInfo> {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            return None;
        }
        if let Some(info) = dialog_info(foreground) {
            return Some(info);
        }
        let mut foreground_pid = 0u32;
        GetWindowThreadProcessId(foreground, Some(&mut foreground_pid));
        if foreground_pid == 0 {
            return None;
        }
        let mut candidates: Vec<HWND> = Vec::new();
        let _ = EnumWindows(
            Some(enum_dialog_candidates),
            LPARAM(&mut candidates as *mut _ as isize),
        );
        candidates.into_iter().find_map(|hwnd| {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == foreground_pid {
                dialog_info(hwnd)
            } else {
                None
            }
        })
    }
}

unsafe fn dialog_info(hwnd: HWND) -> Option<DialogInfo> {
    if hwnd.0.is_null()
        || !class_eq(&class_name(hwnd), DIALOG_CLASS)
        || !IsWindowVisible(hwnd).as_bool()
    {
        return None;
    }
    let title = window_text(hwnd);
    let is_save = is_save_title(&title);

    // 两代对话框，Vista 优先判断：
    // - Vista+ IFileDialog：DirectUIHWND 子窗口 + 普通 Edit 文件名框
    //   （按「最宽的可见 Edit」识别，宽于右上角搜索框）。部分 Vista 对话框
    //   还藏有老式 Combo 链，必须先走 Vista 判定，否则会误报成老式。
    // - 老式：文件名是 ComboBoxEx32 → ComboBox → Edit 控件链。
    let (edit, vista) = if has_descendant_class(hwnd, DIRECT_UI_CLASS) {
        let edit = find_vista_filename_edit(hwnd)?;
        (edit, true)
    } else if has_file_view(hwnd) {
        // 老式文件对话框：文件名编辑框 + 文件列表视图。缺列表视图的
        // #32770（mstsc 远程桌面连接、各类带下拉输入框的普通对话框）
        // 一律不认，避免误触发 Quick Switch。
        let edit = find_edit_chain(hwnd).or_else(|| find_edit_fallback(hwnd))?;
        (edit, false)
    } else {
        return None;
    };

    Some(DialogInfo {
        hwnd: hwnd.0 as isize,
        edit: edit.0 as isize,
        is_save,
        vista,
    })
}

unsafe extern "system" fn enum_dialog_candidates(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    if class_eq(&class_name(hwnd), DIALOG_CLASS)
        && unsafe { IsWindowVisible(hwnd).as_bool() }
    {
        out.push(hwnd);
    }
    TRUE
}

pub fn fill_path(dialog: &DialogInfo, path: &str) -> Result<(), String> {
    set_edit_text(dialog.edit, path)
}

/// 把对话框切到某个文件夹：**供用户显式动作**（Quick Switch / Ctrl+G /
/// 选中文件夹结果）使用。填路径后确认：打开/保存对话框进入该目录并保持打开；
/// 文件夹选择器（`FOS_PICKFOLDERS`）则接受该目录并关闭——后者正是
/// 「用户选定这个文件夹」的结果，勿用于任何自动行为。
pub fn navigate_to_folder(dialog: &DialogInfo, path: &str) -> Result<(), String> {
    if dialog.hwnd == 0 {
        return Err("对话框句柄无效".to_string());
    }
    set_edit_text(dialog.edit, path)?;
    if dialog.vista {
        confirm_vista_dialog(dialog);
    } else {
        // WM_COMMAND(IDOK) 会让老式对话框把编辑框里的路径当作目录并跳转。
        unsafe {
            let _ = PostMessageW(
                Some(hwnd(dialog.hwnd)),
                WM_COMMAND,
                WPARAM(IDOK.0 as usize),
                LPARAM(0),
            );
        }
    }
    Ok(())
}

/// Vista IFileDialog 的确认：点击默认按钮（「打开/选择文件夹」，控件 ID=1）；
/// 找不到按钮时退回向编辑框发送回车。填文件夹路径时确认 = 跳转进去
/// （打开对话框保持打开；文件夹选择器则接受并关闭——这是系统原生行为）。
fn confirm_vista_dialog(dialog: &DialogInfo) {
    unsafe {
        let ok_button = GetDlgItem(Some(hwnd(dialog.hwnd)), 1);
        if let Ok(ok_button) = ok_button {
            let _ = PostMessageW(Some(ok_button), BM_CLICK, WPARAM(0), LPARAM(0));
            return;
        }
        let _ = PostMessageW(
            Some(hwnd(dialog.edit)),
            WM_KEYDOWN,
            WPARAM(VK_RETURN.0 as usize),
            LPARAM(0),
        );
        let _ = PostMessageW(
            Some(hwnd(dialog.edit)),
            WM_KEYUP,
            WPARAM(VK_RETURN.0 as usize),
            LPARAM(0),
        );
    }
}

pub fn is_alive(dialog: &DialogInfo) -> bool {
    if dialog.hwnd == 0 {
        return false;
    }
    let target = hwnd(dialog.hwnd);
    unsafe { IsWindow(Some(target)).as_bool() && IsWindowVisible(target).as_bool() }
}

pub fn restore_focus(dialog: &DialogInfo) {
    if dialog.hwnd == 0 {
        return;
    }
    let target = hwnd(dialog.hwnd);
    unsafe {
        let _ = BringWindowToTop(target);
        if SetForegroundWindow(target).as_bool() {
            return;
        }
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() || foreground.0 == target.0 {
            return;
        }
        let current_thread = GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        if foreground_thread == 0 || foreground_thread == current_thread {
            return;
        }
        let _ = AttachThreadInput(current_thread, foreground_thread, true);
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
        let _ = SetActiveWindow(target);
        let _ = SetFocus(Some(target));
        let _ = AttachThreadInput(current_thread, foreground_thread, false);
    }
}

/// 标准文件对话框的编辑框链：ComboBoxEx32 -> ComboBox -> Edit。
fn find_edit_chain(dialog: HWND) -> Option<HWND> {
    let combo_ex = find_child_by_class(dialog, COMBO_EX_CLASS)?;
    let combo = find_child_by_class(combo_ex, COMBO_CLASS)?;
    find_child_by_class(combo, EDIT_CLASS)
}

/// 链不存在时的兜底：枚举全部后代，取 ComboBoxEx32 下可见的 Edit，
/// 并排除地址栏搜索框（SearchEditBoxWrapperClass）。
fn find_edit_fallback(dialog: HWND) -> Option<HWND> {
    let mut children: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog),
            Some(collect_child),
            LPARAM((&mut children as *mut Vec<HWND>) as isize),
        );
    }
    children.into_iter().find(|child| {
        !child.0.is_null()
            && unsafe { IsWindowVisible(*child).as_bool() }
            && class_eq(&class_name(*child), EDIT_CLASS)
            && !has_ancestor_class(*child, dialog, SEARCH_BOX_CLASS)
            && has_ancestor_class(*child, dialog, COMBO_EX_CLASS)
    })
}

unsafe extern "system" fn collect_child(child: HWND, lparam: LPARAM) -> BOOL {
    let children = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    children.push(child);
    BOOL(1)
}

fn find_child_by_class(parent: HWND, class: &str) -> Option<HWND> {
    let class = HSTRING::from(class);
    unsafe { FindWindowExW(Some(parent), None, &class, PCWSTR::null()).ok() }
}

/// 在全部后代里找类名（EnumChildWindows 本身就遍历整棵子树）。
/// Vista IFileDialog 的 DirectUIHWND 是 DUIViewWndClassName 的子级、
/// #32770 的孙级——用 FindWindowExW 查直接子级会漏掉。
fn has_descendant_class(parent: HWND, class: &str) -> bool {
    let mut children: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(parent),
            Some(collect_child),
            LPARAM((&mut children as *mut Vec<HWND>) as isize),
        );
    }
    children
        .iter()
        .any(|child| class_eq(&class_name(*child), class))
}

/// 文件对话框的列表视图（老式对话框的结构性判据，与语言无关）。
fn has_file_view(parent: HWND) -> bool {
    let mut children: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(parent),
            Some(collect_child),
            LPARAM((&mut children as *mut Vec<HWND>) as isize),
        );
    }
    children
        .iter()
        .any(|child| is_file_view_class(&class_name(*child)))
}

fn is_file_view_class(class: &str) -> bool {
    FILE_VIEW_CLASSES
        .iter()
        .any(|candidate| class_eq(class, candidate))
}

/// Vista IFileDialog 的文件名编辑框：可见的 Edit 里取最宽的
/// （底部「文件名/文件夹」框横贯对话框，远宽于右上角搜索框）。
fn find_vista_filename_edit(dialog: HWND) -> Option<HWND> {
    let mut children: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog),
            Some(collect_child),
            LPARAM((&mut children as *mut Vec<HWND>) as isize),
        );
    }
    let mut best: Option<(i32, HWND)> = None;
    for child in children {
        if child.0.is_null()
            || !unsafe { IsWindowVisible(child) }.as_bool()
            || !class_eq(&class_name(child), EDIT_CLASS)
        {
            continue;
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(child, &mut rect) }.is_err() {
            continue;
        }
        let width = rect.right - rect.left;
        if width >= 200 && best.map_or(true, |(best_w, _)| width > best_w) {
            best = Some((width, child));
        }
    }
    best.map(|(_, hwnd)| hwnd)
}

fn has_ancestor_class(mut child: HWND, stop: HWND, class: &str) -> bool {
    loop {
        let parent = match unsafe { GetParent(child) } {
            Ok(parent) => parent,
            Err(_) => return false,
        };
        if parent.0.is_null() || parent.0 == stop.0 {
            return false;
        }
        if class_eq(&class_name(parent), class) {
            return true;
        }
        child = parent;
    }
}

fn set_edit_text(edit: isize, text: &str) -> Result<(), String> {
    if edit == 0 {
        return Err("文件名输入框句柄无效".to_string());
    }
    let edit = hwnd(edit);
    if !unsafe { IsWindow(Some(edit)).as_bool() } {
        return Err("文件名输入框已失效".to_string());
    }
    // SendMessageW 同步返回，宽字符串在调用期间保持存活。
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let result = unsafe {
        SetLastError(ERROR_SUCCESS);
        SendMessageW(
            edit,
            WM_SETTEXT,
            None,
            Some(LPARAM(wide.as_ptr() as isize)),
        )
    };
    if result.0 != 0 {
        return Ok(());
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_ACCESS_DENIED {
        Err("写入文件名失败：消息被 UIPI 拦截（目标对话框权限更高）".to_string())
    } else {
        Err(format!(
            "写入文件名失败（WM_SETTEXT 返回 FALSE，GetLastError={}）",
            error.0
        ))
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    wide_to_string(&buffer, length)
}

fn window_text(hwnd: HWND) -> String {
    let mut buffer = [0u16; 512];
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    wide_to_string(&buffer, length)
}

fn is_save_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    lower.contains("保存") || lower.contains("另存") || lower.contains("save")
}

fn class_eq(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

fn wide_to_string(buffer: &[u16], length: i32) -> String {
    if length <= 0 {
        return String::new();
    }
    let end = (length as usize).min(buffer.len());
    let mut slice = &buffer[..end];
    while let [rest @ .., 0] = slice {
        slice = rest;
    }
    String::from_utf16_lossy(slice)
}

fn hwnd(value: isize) -> HWND {
    HWND(value as *mut _)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_save_titles() {
        assert!(is_save_title("Save As"));
        assert!(is_save_title("save as - 记事本"));
        assert!(is_save_title("SAVE"));
        assert!(is_save_title("保存"));
        assert!(is_save_title("另存为"));
        assert!(is_save_title("保存文件 - 记事本"));
        assert!(!is_save_title("打开"));
        assert!(!is_save_title("Open File"));
        assert!(!is_save_title(""));
    }

    #[test]
    fn matches_class_names_ignoring_ascii_case() {
        assert!(class_eq("#32770", "#32770"));
        assert!(class_eq("Edit", "edit"));
        assert!(class_eq("ComboBoxEx32", "comboboxex32"));
        assert!(!class_eq("Edit", "EditBox"));
        assert!(!class_eq("SearchEditBoxWrapperClass", "Edit"));
        assert!(!class_eq("ComboBox", "ComboBoxEx32"));
    }

    #[test]
    fn file_dialog_requires_a_file_view() {
        // 老式文件对话框的结构性判据：文件列表视图。
        assert!(is_file_view_class("SysListView32"));
        assert!(is_file_view_class("shellview"));
        assert!(is_file_view_class("SHELLDLL_DefView"));
        // mstsc 远程桌面连接只有「计算机」下拉的 ComboBoxEx→ComboBox→Edit，
        // 没有列表视图——曾因此被误判成文件对话框、误触发 Quick Switch。
        assert!(!is_file_view_class("ComboBoxEx32"));
        assert!(!is_file_view_class("ComboBox"));
        assert!(!is_file_view_class("Edit"));
        assert!(!is_file_view_class("ToolbarWindow32"));
    }

    #[test]
    fn converts_wide_buffers_to_strings() {
        let buffer = [0x4f60u16, 0x597d, 0x4e16, 0x754c, 0];
        assert_eq!(wide_to_string(&buffer, 4), "你好世界");
        assert_eq!(wide_to_string(&buffer, 0), "");
        assert_eq!(wide_to_string(&buffer, -1), "");
        // 报告长度超出缓冲区时按缓冲区截断，并去掉尾部 NUL。
        assert_eq!(wide_to_string(&buffer, 100), "你好世界");
        // 非法 UTF-16 代理项使用替换字符。
        assert_eq!(wide_to_string(&[0xd800], 1), "\u{fffd}");
    }
}

#[cfg(test)]
mod vista_dialog_tests {
    use super::*;
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::time::{Duration, Instant};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYEVENTF_KEYUP, KEYBD_EVENT_FLAGS,
};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, WM_GETTEXT, WM_GETTEXTLENGTH,
    };

    static FOUND_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PID: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn find_by_pid_cb(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == TARGET_PID.load(Ordering::SeqCst)
            && unsafe { IsWindowVisible(hwnd) }.as_bool()
        {
            FOUND_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
            return BOOL(0);
        }
        BOOL(1)
    }

    /// 记事本 Ctrl+O 弹出真正的 Vista IFileDialog（与 Electron 应用同类）。
    /// 仪器化：打印两条探测路径的结果。
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn vista_dialog_detect_fill_navigate() {
        let mut child = std::process::Command::new("notepad.exe")
            .spawn()
            .expect("spawn notepad");
        std::thread::sleep(Duration::from_millis(1000));
        TARGET_PID.store(child.id(), Ordering::SeqCst);
        unsafe {
            let _ = EnumWindows(Some(find_by_pid_cb), LPARAM(0));
        }
        let notepad = FOUND_HWND.swap(0, Ordering::SeqCst);
        assert!(notepad != 0, "应找到记事本窗口");
        unsafe {
            let fg = GetForegroundWindow();
            let ft = GetWindowThreadProcessId(fg, None);
            let ct = GetCurrentThreadId();
            let _ = AttachThreadInput(ft, ct, true);
            let _ = SetForegroundWindow(HWND(notepad as *mut _));
            let _ = AttachThreadInput(ft, ct, false);
        }
        std::thread::sleep(Duration::from_millis(300));
        unsafe {
            let fg = GetForegroundWindow();
            println!("foreground is notepad={}", fg.0 as isize == notepad);
        }
        // Ctrl+O
        unsafe {
            keybd_event(0x11, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(0x4F, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(0x4F, 0, KEYEVENTF_KEYUP, 0);
            keybd_event(0x11, 0, KEYEVENTF_KEYUP, 0);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut info = None;
        while Instant::now() < deadline {
            if let Some(found) = active_dialog() {
                println!(
                    "poll: vista={} edit={}",
                    found.vista, found.edit
                );
                if found.vista {
                    info = Some(found);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        // 先探测失败也可能有对话框——打印子控件清单用于诊断
        if info.is_none() {
            if let Some(found) = active_dialog() {
                println!("fallback detection: vista={} edit={}", found.vista, found.edit);
                let mut children: Vec<HWND> = Vec::new();
                unsafe {
                    let _ = EnumChildWindows(
                        Some(hwnd(found.hwnd)),
                        Some(collect_child),
                        LPARAM((&mut children as *mut Vec<HWND>) as isize),
                    );
                }
                for (i, child) in children.iter().enumerate().take(40) {
                    let (visible, w, h, text) = unsafe {
                        let mut rect = RECT::default();
                        let _ = GetWindowRect(*child, &mut rect);
                        (
                            IsWindowVisible(*child).as_bool(),
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            window_text(*child),
                        )
                    };
                    println!(
                        "  child[{i}] class={} visible={visible} size={w}x{h} text={text:?}",
                        class_name(*child)
                    );
                }
            }
        }
        let info = info.expect("应探测到 Vista 文件对话框（vista=true）");
        println!(
            "detected: vista={} edit={}",
            info.vista, info.edit
        );

        fill_path(&info, r"C:\Windows").expect("填入路径");
        std::thread::sleep(Duration::from_millis(300));
        // 跨进程取 Edit 内容必须用 WM_GETTEXT（GetWindowTextW 跨进程拿不到控件文本）。
        let len = unsafe {
            SendMessageW(hwnd(info.edit), WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize
        };
        let mut buf = vec![0u16; len + 2];
        unsafe {
            SendMessageW(
                hwnd(info.edit),
                WM_GETTEXT,
                Some(WPARAM(buf.len())),
                Some(LPARAM(buf.as_mut_ptr() as isize)),
            );
        }
        let readback = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
        println!("edit text after fill: {readback}");
        assert!(readback.contains(r"C:\Windows"), "编辑框应含填入的路径");

        navigate_to_folder(&info, r"C:\Windows").expect("确认跳转");
        std::thread::sleep(Duration::from_millis(800));
        println!("dialog alive after navigate={}", is_alive(&info));
        let _ = child.kill();
    }
}


#[cfg(test)]
mod navigation_tests {
    //! `navigate_to_folder` 的真实对话框回归：显式跳转（Quick Switch / Ctrl+G /
    //! 选中文件夹结果）依赖「填路径 + 确认」的两种原生语义：
    //! - 打开/保存对话框：进入目录并**保持打开**；
    //! - 文件夹选择器（`FOS_PICKFOLDERS`，VSCode/Electron 的「打开文件夹」）：
    //!   **选中并关闭**，即「用户选定了这个目录」——只用于显式动作，不得自动触发。

    use super::*;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};
    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, IBindCtx,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS,
        SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, FindWindowW, GetDlgCtrlID, PostMessageW, WM_GETTEXT, WM_GETTEXTLENGTH,
    };

    #[derive(Clone, Copy)]
    enum Kind {
        Open,
        PickFolder,
        Save,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0u16]).collect()
    }

    /// 在后台线程弹一个真正的 IFileOpenDialog/IFileSaveDialog，直到被取消
    /// 或确认；返回「用户选中的路径」（取消/失败为 None）。
    fn spawn_dialog(
        title: &'static str,
        kind: Kind,
    ) -> (std::thread::JoinHandle<()>, Receiver<Option<String>>) {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let dialog: IFileDialog = match kind {
                Kind::Save => CoCreateInstance::<_, IFileSaveDialog>(
                    &FileSaveDialog,
                    None,
                    CLSCTX_INPROC_SERVER,
                )
                .expect("创建 IFileSaveDialog")
                .cast()
                .expect("IFileSaveDialog -> IFileDialog"),
                _ => CoCreateInstance::<_, IFileOpenDialog>(
                    &FileOpenDialog,
                    None,
                    CLSCTX_INPROC_SERVER,
                )
                .expect("创建 IFileOpenDialog")
                .cast()
                .expect("IFileOpenDialog -> IFileDialog"),
            };
            let options = match kind {
                Kind::Open => FOS_FILEMUSTEXIST,
                Kind::PickFolder => FOS_PICKFOLDERS,
                Kind::Save => FOS_OVERWRITEPROMPT,
            } | FOS_FORCEFILESYSTEM;
            dialog.SetOptions(options).expect("SetOptions");
            let initial = wide(r"C:\Windows");
            if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                PCWSTR(initial.as_ptr()),
                None::<&IBindCtx>,
            ) {
                let _ = dialog.SetFolder(&folder);
            }
            let _ = dialog.SetTitle(&HSTRING::from(title));
            let shown = dialog.Show(None);
            let chosen = if shown.is_ok() {
                dialog
                    .GetResult()
                    .ok()
                    .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH).ok())
                    .and_then(|path| path.to_string().ok())
            } else {
                None
            };
            let _ = tx.send(chosen);
            CoUninitialize();
        });
        (handle, rx)
    }

    fn wait_for_dialog(title: &str) -> Option<HWND> {
        let wide_title = wide(title);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(hwnd) = unsafe { FindWindowW(None, PCWSTR(wide_title.as_ptr())) } {
                if unsafe { IsWindowVisible(hwnd).as_bool() } {
                    return Some(hwnd);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// 子控件是逐步构建的：拿到对话框后还要等文件名编辑框出现。
    fn wait_for_info(hwnd: HWND) -> Option<DialogInfo> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(info) = unsafe { dialog_info(hwnd) } {
                return Some(info);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn cancel_dialog(hwnd: HWND) {
        unsafe {
            if let Ok(cancel) = GetDlgItem(Some(hwnd), 2) {
                let _ = PostMessageW(Some(cancel), BM_CLICK, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn test_target(name: &str) -> (std::path::PathBuf, String) {
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("创建测试目录");
        let text = path.to_string_lossy().to_string();
        (path, text)
    }

    /// 地址栏面包屑的窗口文本（形如 `地址: C:\Windows`）；读不到返回空串。
    fn breadcrumb_text(dialog: HWND) -> String {
        let mut children: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumChildWindows(
                Some(dialog),
                Some(collect_child),
                LPARAM((&mut children as *mut Vec<HWND>) as isize),
            );
        }
        for child in children {
            if class_eq(&class_name(child), "ToolbarWindow32")
                && unsafe { GetDlgCtrlID(child) } == 1001
            {
                let length = unsafe {
                    SendMessageW(child, WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))).0
                } as usize;
                if length == 0 {
                    return String::new();
                }
                let mut buffer = vec![0u16; length + 2];
                unsafe {
                    SendMessageW(
                        child,
                        WM_GETTEXT,
                        Some(WPARAM(buffer.len())),
                        Some(LPARAM(buffer.as_mut_ptr() as isize)),
                    );
                }
                return String::from_utf16_lossy(&buffer[..length]);
            }
        }
        String::new()
    }

    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn navigate_open_and_save_dialogs_stays_open_at_target() {
        for (title, kind, wanted) in [
            ("lstr-test-open", Kind::Open, "lstr-nav-test-open"),
            ("lstr-test-save", Kind::Save, "lstr-nav-test-save"),
        ] {
            let (target, target_text) = test_target(wanted);
            let (thread, _rx) = spawn_dialog(title, kind);
            let hwnd = wait_for_dialog(title).expect("对话框应在 10s 内出现");
            let info = wait_for_info(hwnd).expect("应识别到文件对话框");
            navigate_to_folder(&info, &target_text).expect("填路径并确认");
            std::thread::sleep(Duration::from_millis(900));
            assert!(is_alive(&info), "{title}: 跳转后应保持打开");
            let crumb = breadcrumb_text(hwnd);
            println!("{title}: crumb={crumb:?}");
            assert!(
                crumb.contains(wanted),
                "{title}: 应已跳到目标目录，实际 {crumb:?}"
            );
            cancel_dialog(hwnd);
            let _ = thread.join();
            let _ = std::fs::remove_dir_all(&target);
        }
    }

    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn navigate_folder_picker_selects_and_closes() {
        let (target, target_text) = test_target("lstr-nav-test-picker");
        let (thread, rx) = spawn_dialog("lstr-test-picker", Kind::PickFolder);
        let hwnd = wait_for_dialog("lstr-test-picker").expect("选择器应在 10s 内出现");
        let info = wait_for_info(hwnd).expect("应识别到选择器");
        navigate_to_folder(&info, &target_text).expect("填路径并确认");
        let chosen = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("选择器应被确认并关闭");
        let _ = thread.join();
        println!("picker chosen={chosen:?}");
        let chosen = chosen.expect("应拿到选中的目录");
        assert!(
            chosen.eq_ignore_ascii_case(&target_text),
            "选择器应返回目标目录，实际 {chosen:?}"
        );
        let _ = std::fs::remove_dir_all(&target);
    }
}
