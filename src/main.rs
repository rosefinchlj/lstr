#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod config;
mod history;
mod model;
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
    install_panic_logger();
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
