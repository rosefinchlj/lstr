//! 浏览器本地数据目录发现。
//!
//! 收藏夹（`Bookmarks` / `places.sqlite`）与图标缓存（`Favicons` /
//! `favicons.sqlite`）需要枚举同一批浏览器 Profile，路径规则集中在这里，
//! 避免搜索与图标两处各写一份（新增浏览器时只改一处）。

use std::fs;
use std::path::{Path, PathBuf};

/// Chromium 系浏览器的 User Data 根目录（Chrome / Edge / Brave / Vivaldi /
/// Chromium），外加 Opera 的配置目录（它把 `Bookmarks` 放在根目录自身）。
pub(crate) fn chromium_user_data_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        for rel in [
            r"Google\Chrome\User Data",
            r"Microsoft\Edge\User Data",
            r"BraveSoftware\Brave-Browser\User Data",
            r"Vivaldi\User Data",
            r"Chromium\User Data",
        ] {
            roots.push(local.join(rel));
        }
    }
    if let Some(roaming) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(roaming).join(r"Opera Software\Opera Stable"));
    }
    roots
}

/// 一个 User Data 根目录下所有可能的 Profile 目录（根自身 + 一层子目录）。
/// 只做目录枚举，不判断具体文件——调用方按需拼 `Bookmarks` / `Favicons`。
pub(crate) fn chromium_profile_dirs(root: &Path) -> Vec<PathBuf> {
    let mut profiles = vec![root.to_path_buf()];
    let Ok(entries) = fs::read_dir(root) else {
        return profiles;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            profiles.push(path);
        }
    }
    profiles
}

/// Firefox 的 Profile 目录（`%APPDATA%\Mozilla\Firefox\Profiles\*`）。
pub(crate) fn firefox_profile_dirs() -> Vec<PathBuf> {
    let Some(base) = std::env::var_os("APPDATA") else {
        return Vec::new();
    };
    let dir = PathBuf::from(base).join(r"Mozilla\Firefox\Profiles");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut profiles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            profiles.push(path);
        }
    }
    profiles
}
