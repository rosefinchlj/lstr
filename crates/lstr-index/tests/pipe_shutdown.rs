//! Shutdown 帧往返 + worker 收帧退出语义(管道集成,参照 pipe_auth 风格)。
//!
//! 覆盖链路:客户端真实写 `Frame::Shutdown` → 服务端(worker 侧
//! `read_frame_pub` 原语,模拟 run_session 的会话循环)收帧后结束会话并关闭
//! 管道 → 客户端 `shutdown_and_wait`/`wait_worker_exit` 应观察到「worker 已
//! 退出」(唯一管道实例消失)。这正是 GUI 优雅退出收提权 worker 的线上语义:
//! GUI 退出路径(eframe on_exit → SearchService::graceful_shutdown → client
//! 线程 shutdown_and_wait)对 worker 的全部要求。
//!
//! worker 进程级的退出与 clean-exit 标记由 worker_main/Worker::run 承担;
//! 测试不运行真实 worker(不枚举真实卷/不发 FSCTL),服务端只在握手后等
//! Shutdown 帧 —— 与 run_session 的控制帧处理一一对应。

use std::time::Duration;

use lstr_index::proto::{Frame, TOKEN_LEN};
use lstr_index::{client, win, worker, PROTO_VERSION};

fn pipe_name(tag: &str) -> String {
    format!(
        r"\\.\pipe\lstr-shutdown-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// 收到 Shutdown 后结束会话(关句柄)→ 客户端的等待应返回 true。
#[test]
fn shutdown_frame_roundtrip_and_worker_exit_wait() {
    let name = pipe_name("ok");
    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let server = win::create_pipe_server(&server_name).expect("create server");
        let mut ok = win::wait_for_client(server);
        if ok {
            // 握手:Hello(带令牌)→ Welcome —— 与 run_session 同款
            ok = match worker::read_frame_pub(server, Duration::from_secs(5)) {
                Some(Frame::Hello { proto, token, volumes }) => {
                    proto == PROTO_VERSION
                        && token == [0x5a; TOKEN_LEN]
                        && worker::write_frame_pub(server, &Frame::Welcome { proto: PROTO_VERSION })
                        && volumes.is_empty()
                }
                _ => false,
            };
        }
        if ok {
            // 会话循环:控制帧处理 —— Shutdown 即 break 'session(worker 侧
            // 置 EXIT_AFTER_SESSION,不再重建管道,进程退出)
            ok = matches!(
                worker::read_frame_pub(server, Duration::from_secs(5)),
                Some(Frame::Shutdown)
            );
        }
        // 收帧退出:关闭唯一实例 = 进程退出后的管道消失
        win::close(server);
        ok
    });

    let cl = client::connect_and_handshake_at(
        &name,
        [0x5a; TOKEN_LEN],
        Vec::new(),
        Duration::from_secs(5),
    )
    .expect("handshake");
    // GUI 退出路径的核心调用:发 Shutdown 并等 worker 退出(有超时)
    assert!(
        client::shutdown_and_wait(cl, &name, Duration::from_secs(5)),
        "worker 收到 Shutdown 后应在超时内退出(管道实例消失)"
    );
    win::close(cl);
    assert!(server.join().unwrap(), "服务端应完成握手并收到 Shutdown");
}

/// worker 存活但无人在收 Shutdown:等待必须有上限,到点返回 false(半开容忍)。
#[test]
fn wait_worker_exit_is_bounded_while_instance_alive() {
    let name = pipe_name("timeout");
    let _server = win::create_pipe_server(&name).expect("create server");
    let started = std::time::Instant::now();
    assert!(
        !client::wait_worker_exit(&name, Duration::from_millis(300)),
        "实例存活 → 等待超时返回 false"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "等待应持续到超时窗口结束"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "等待必须有界,不得无限阻塞 GUI 退出"
    );
}

/// worker 不存在:wait_worker_exit 立即返回 true(不空等,退出不被拖慢)。
#[test]
fn wait_worker_exit_returns_immediately_when_absent() {
    let name = pipe_name("absent");
    let started = std::time::Instant::now();
    assert!(client::wait_worker_exit(&name, Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_millis(500));
}
