use std::os::windows::ffi::OsStrExt;
use windows::core::w;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ,
};

const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "lstr";

fn open_run_key(access: REG_SAM_FLAGS) -> Result<HKEY, String> {
    let mut key = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            None,
            access,
            &mut key,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(format!("打开注册表失败: {}", err.0));
    }
    Ok(key)
}

/// 开机自启是否已开启（HKCU Run 里有 lstr 项）。
pub fn is_enabled() -> bool {
    let Ok(key) = open_run_key(KEY_QUERY_VALUE) else {
        return false;
    };
    let err = unsafe { RegQueryValueExW(key, w!("lstr"), None, None, None, None) };
    unsafe {
        let _ = RegCloseKey(key);
    }
    err == ERROR_SUCCESS
}

/// 开启/关闭开机自启。
pub fn set_enabled(on: bool) -> Result<(), String> {
    let key = open_run_key(KEY_SET_VALUE)?;
    let result = if on {
        let exe = std::env::current_exe().map_err(|e| format!("获取 exe 路径失败: {e}"))?;
        let mut value: Vec<u16> = exe.as_os_str().encode_wide().collect();
        value.push(0);
        unsafe {
            RegSetValueExW(
                key,
                w!("lstr"),
                None,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    value.as_ptr() as *const u8,
                    value.len() * 2,
                )),
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, w!("lstr")) }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if result == ERROR_SUCCESS || (!on && result == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(format!("注册表操作失败: {}", result.0))
    }
}
