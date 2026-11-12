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

/// 记录 panic 到 %APPDATA%\lstr\crash.log：release 版没有控制台，
/// 崩溃必须留下证据（调用栈 + panic 位置）。
fn install_panic_logger() {
    std::panic::set_hook(Box::new(|info| {
        let dir = config::attach_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut text = format!("time: {:?}\npanic: {info}\n", std::time::SystemTime::now());
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
    let single = match win::single_instance::acquire() {
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
