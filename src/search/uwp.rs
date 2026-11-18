use crate::model::{Entry, Kind};
use crate::search::pinyin;
use serde::Deserialize;
use std::collections::HashSet;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_ENTRIES: usize = 200;

// COM enumeration needs Win32_UI_Shell_Common (ITEMIDLIST / IEnumIDList::Next)
// and Win32_UI_Shell_PropertiesSystem, which the crate does not enable yet.
// Get-StartApps returns the same shell:AppsFolder items (verified: 237 == 237).
const POWERSHELL_SCRIPT: &str =
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-StartApps | ConvertTo-Json -Compress";

#[derive(Deserialize)]
struct StartApp {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "AppID", default)]
    app_id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StartAppList {
    Many(Vec<StartApp>),
    One(StartApp),
}

/// Enumerate UWP / Store apps from shell:AppsFolder.
pub fn load() -> Vec<Entry> {
    let Some(json) = query_start_apps() else {
        return Vec::new();
    };
    finalize(entries_from_json(&json))
}

fn query_start_apps() -> Option<String> {
    // B16：PowerShell 冷启动/被安全软件拦截时可能长时间不返回；搜索线程是懒启动
    // UWP 索引的调用方，必须硬超时，否则整条搜索链路会永久「搜索中…」。
    const TIMEOUT: Duration = Duration::from_secs(8);
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-Command", POWERSHELL_SCRIPT])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = reader.join().unwrap_or_default();
                if !status.success() {
                    lstr_index::snapshot::debug_log("uwp: Get-StartApps failed (non-zero exit)");
                    return None;
                }
                return String::from_utf8(bytes).ok();
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                lstr_index::snapshot::debug_log(
                    "uwp: Get-StartApps timed out after 8s; using empty app list",
                );
                return None;
            }
            Err(_) => return None,
        }
    }
}

fn entries_from_json(json: &str) -> Vec<Entry> {
    let json = json.trim_start_matches('\u{feff}').trim();
    if json.is_empty() {
        return Vec::new();
    }
    let apps = match serde_json::from_str::<StartAppList>(json) {
        Ok(StartAppList::Many(apps)) => apps,
        Ok(StartAppList::One(app)) => vec![app],
        Err(_) => return Vec::new(),
    };
    apps.iter()
        .filter_map(|app| entry_from_parts(&app.name, &app.app_id))
        .collect()
}

fn entry_from_parts(name: &str, app_id: &str) -> Option<Entry> {
    let name = name.trim();
    let app_id = app_id.trim();
    if name.is_empty() || !is_launchable_aumid(app_id) || is_noise_name(name) {
        return None;
    }
    Some(Entry {
        id: format!("uwp:{app_id}"),
        kind: Kind::App,
        name: name.to_string(),
        path: Some(aumid_path(app_id)),
        size: None,
        modified: None,
        pinyin: Some(pinyin::keys(name)),
        initial: initial_for(name),
        tone: tone_for(name),
    })
}

fn aumid_path(aumid: &str) -> PathBuf {
    PathBuf::from(format!("shell:AppsFolder\\{aumid}"))
}

fn is_launchable_aumid(app_id: &str) -> bool {
    !app_id.is_empty() && !app_id.contains(['\\', '/', ':'])
}

fn is_noise_name(name: &str) -> bool {
    name.trim().is_empty() || !name.chars().any(|ch| ch.is_alphanumeric())
}

fn finalize(mut entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen_ids = HashSet::new();
    let mut seen_names = HashSet::new();
    entries.retain(|entry| {
        seen_ids.insert(entry.id.to_lowercase()) && seen_names.insert(entry.name.to_lowercase())
    });
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    entries.truncate(MAX_ENTRIES);
    entries
}

fn initial_for(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return "?".to_string();
    };
    if !first.is_ascii_alphanumeric() {
        return first.to_string();
    }
    let mut initial = String::with_capacity(2);
    initial.push(first.to_ascii_uppercase());
    if let Some(second) = chars.next() {
        initial.push(second.to_ascii_uppercase());
    }
    initial
}

fn tone_for(name: &str) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash & 0x00ff_ffff) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use std::path::PathBuf;

    #[test]
    fn aumid_path_points_into_shell_apps_folder() {
        assert_eq!(
            aumid_path("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"),
            PathBuf::from("shell:AppsFolder\\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App")
        );
    }

    #[test]
    fn entry_from_parts_uses_entry_conventions() {
        let entry = entry_from_parts("计算器", "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App")
            .expect("calculator entry");

        assert_eq!(
            entry.id,
            "uwp:Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"
        );
        assert_eq!(entry.kind, Kind::App);
        assert_eq!(entry.name, "计算器");
        assert_eq!(
            entry.path,
            Some(PathBuf::from(
                "shell:AppsFolder\\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"
            ))
        );
        assert!(entry.size.is_none());
        assert!(entry.modified.is_none());
        assert_eq!(entry.pinyin.as_deref(), Some("jsq jisuanqi"));
        assert_eq!(entry.initial, "计");
        assert!(entry.tone <= 0x00ff_ffff);
    }

    #[test]
    fn entries_from_json_handles_array_and_single_object() {
        let array = r#"[
            {"Name":"Calculator","AppID":"pkg!App"},
            {"Name":"Settings","AppID":"pkg!Settings"}
        ]"#;
        let entries = entries_from_json(array);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "uwp:pkg!App");

        let single = r#"{"Name":"Calculator","AppID":"pkg!App"}"#;
        let entries = entries_from_json(single);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Calculator");
    }

    #[test]
    fn entries_from_json_skips_path_ids_empty_names_and_noise() {
        let json = r#"[
            {"Name":"Desktop App","AppID":"D:\\Program Files\\App\\app.exe"},
            {"Name":"---","AppID":"pkg!App"},
            {"Name":"","AppID":"pkg!App"},
            {"Name":"Calculator","AppID":"pkg!App"},
            {"Name":"No Id","AppID":""}
        ]"#;
        let entries = entries_from_json(json);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Calculator");
    }

    #[test]
    fn entries_from_json_ignores_invalid_payload() {
        assert!(entries_from_json("").is_empty());
        assert!(entries_from_json("not json").is_empty());
        assert!(entries_from_json("null").is_empty());
    }

    #[test]
    fn finalize_dedups_sorts_and_caps() {
        let mut entries: Vec<Entry> = (0..205)
            .map(|index| {
                entry_from_parts(&format!("App{index:03}"), &format!("pkg!App{index:03}"))
                    .expect("generated entry")
            })
            .collect();
        entries.push(entry_from_parts("App000", "pkg!Duplicate").expect("duplicate name entry"));

        let entries = finalize(entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().expect("first entry").name, "App000");
        assert_eq!(entries.last().expect("last entry").name, "App199");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name == "App000")
                .count(),
            1
        );
    }

    #[test]
    fn initial_uses_up_to_two_ascii_chars_or_one_display_char() {
        assert_eq!(initial_for("Visual Studio Code"), "VI");
        assert_eq!(initial_for("Notepad"), "NO");
        assert_eq!(initial_for("微信"), "微");
        assert_eq!(initial_for(""), "?");
    }

    #[test]
    fn tone_is_stable_and_rgb_sized() {
        assert_eq!(tone_for("Calculator"), tone_for("Calculator"));
        assert_eq!(tone_for("Calculator"), tone_for("calculator"));
        assert!(tone_for("Calculator") <= 0x00ff_ffff);
    }

    #[test]
    #[ignore]
    fn live_load_prints_count_and_samples() {
        let entries = load();
        println!("uwp entries: {}", entries.len());
        for entry in entries.iter().take(10) {
            println!("  {} -> {}", entry.name, entry.full_path());
        }
        assert!(entries.len() <= MAX_ENTRIES);
        assert!(entries.iter().all(|entry| entry.id.starts_with("uwp:")));
    }
}
