#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod assets_ui;
mod config;
mod history;
/// 唤起热键的纯逻辑定义（配置串解析 + 双击检测器）。
mod hotkey;
mod model;
mod preview;
mod search;
mod ui;
mod win;

use std::io::Write;
use std::sync::{Arc, Mutex};

/// 把 `SystemTime` 格式化成可读的 UTC 时间 + unix 秒：
/// `2026-11-16T14:47:01.123Z (unix 1794800000)`。
///
/// P1-11：旧实现直接 `{:?}` 写 `SystemTime { intervals: ... }`，用户/支持人员
/// 无法判断崩溃发生在什么时候。这里用 Howard Hinnant 的 civil-from-days
/// 自己换算成公历（不为一个时间戳引入 chrono 依赖）。
pub(crate) fn format_utc(now: std::time::SystemTime) -> String {
    let Ok(duration) = now.duration_since(std::time::UNIX_EPOCH) else {
        return format!("{now:?} (pre-epoch)");
    };
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z (unix {secs})",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// P2-19：本次开机的「boot id」（Unix 秒）。用当前时间减系统 uptime 近似
/// 开机时刻，只用于判断标记文件是否来自上一次开机；精度 ±1s 足够。
pub(crate) fn boot_id_secs() -> Option<u64> {
    let uptime = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() } / 1_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(uptime))
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)。算法见
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 记录 panic 到 %APPDATA%\lstr\crash.log：release 版没有控制台，
/// 崩溃必须留下证据（可读时间 + 线程名 + panic 位置 + 调用栈）。
fn install_panic_logger() {
    std::panic::set_hook(Box::new(|info| {
        let dir = config::attach_dir();
        let _ = std::fs::create_dir_all(&dir);
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let mut text = format!(
            "time: {}\nthread: {thread_name} (pid {})\npanic: {info}\n",
            format_utc(std::time::SystemTime::now()),
            std::process::id()
        );
        text.push_str(&format!(
            "backtrace:\n{}\n",
            std::backtrace::Backtrace::force_capture()
        ));
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            let _ = file.write_all(text.as_bytes());
        }
        eprintln!("lstr panic: {info}");
    }));
}

/// P1-2：取单实例互斥体。
///
/// 普通启动：拿不到就通知已运行的实例唤出窗口并退出（原行为）。
/// `--relaunch`（「重启」按钮拉起的新实例）：旧进程此时可能还在
/// `on_exit` 的 `graceful_shutdown`/旁车落盘里（最长约 6s），互斥体尚未
/// 释放；直接退出会把「重启」变成彻底关闭。因此 15s 内每 250ms 重试，
/// 拿不到才放弃（用户手动再开）。
fn acquire_single_instance() -> Option<win::single_instance::SingleInstance> {
    let relaunch = std::env::args().any(|a| a == "--relaunch");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(guard) = win::single_instance::acquire() {
            return Some(guard);
        }
        if !relaunch || std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn main() -> eframe::Result<()> {
    // panic 钩子必须先装:--index-worker 分支也要留下崩溃证据,
    // 否则提权 worker 在 release 下挂掉时没有任何日志可查。
    install_panic_logger();
    // --index-worker:提权数据泵,不走单实例、不进 UI
    if std::env::args().any(|a| a == "--index-worker") {
        // 提权进程的键盘钩子能看到发往管理员窗口的按键：用它中继唤起热键，
        // 否则管理员终端在前台时主界面的双击 Ctrl 收不到（UIPI 限制）。
        win::hotkey_relay::start_worker_relay();
        std::process::exit(lstr_index::worker::worker_main());
    }
    let single = match acquire_single_instance() {
        Some(guard) => guard,
        None => return Ok(()),
    };
    let activation_event = single.event_handle();

    let cfg = config::Config::load();
    let history = Arc::new(Mutex::new(history::History::load()));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("lstr")
            .with_inner_size([cfg.width, 420.0])
            .with_min_inner_size([420.0, 160.0])
            .with_max_inner_size([1000.0, 800.0])
            .with_position([-30000.0, -30000.0])
            .with_active(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "lstr",
        options,
        Box::new(move |cc| Ok(Box::new(app::LstrApp::new(cc, cfg, history, activation_event)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn utc_format_epoch_and_known_instants() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z (unix 0)");
        assert_eq!(
            format_utc(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20.000Z (unix 1700000000)"
        );
        assert_eq!(
            format_utc(UNIX_EPOCH + Duration::from_millis(1_700_000_000_123)),
            "2023-11-14T22:13:20.123Z (unix 1700000000)"
        );
        // 2026-11-16 14:47:01 UTC = Asia/Shanghai 22:47:01。
        assert_eq!(
            format_utc(UNIX_EPOCH + Duration::from_secs(1_794_840_421)),
            "2026-11-16T14:47:01.000Z (unix 1794840421)"
        );
    }

    #[test]
    fn civil_from_days_handles_epoch_and_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29), "闰日");
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
