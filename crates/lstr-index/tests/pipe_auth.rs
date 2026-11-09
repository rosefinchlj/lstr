//! 能力令牌握手回归:真实命名管道上,Hello 必须携带 worker 启动时拿到的
//! 令牌;错误令牌不返回 Welcome(客户端握手失败),正确令牌才拿到 Welcome。
//!
//! 这里用 worker 侧同样的 `read_frame_pub`/`write_frame_pub` 原语模拟服务端,
//! 覆盖「令牌经命令行下发 → Hello 回传 → worker 校验」这条链路里除
//! `worker_main` 参数解析外的部分(参数解析由 worker 单测覆盖)。

use std::time::Duration;

use lstr_index::proto::{Frame, TOKEN_LEN};
use lstr_index::{client, win, worker};

fn pipe_name(tag: &str) -> String {
    format!(
        r"\\.\pipe\lstr-auth-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

#[test]
fn handshake_requires_matching_token() {
    let name = pipe_name("ok");
    let expected: [u8; TOKEN_LEN] = [0x42; TOKEN_LEN];

    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let server = win::create_pipe_server(&server_name).expect("create server");
        let connected = win::wait_for_client(server);
        let ok = if connected {
            match worker::read_frame_pub(server, Duration::from_secs(5)) {
                Some(Frame::Hello { proto, token, volumes }) => {
                    proto == lstr_index::PROTO_VERSION && token == expected && volumes == vec![(1, 2, 3, 4, true)]
                }
                _ => false,
            }
        } else {
            false
        };
        if ok {
            let _ = worker::write_frame_pub(
                server,
                &Frame::Welcome { proto: lstr_index::PROTO_VERSION },
            );
        }
        win::close(server);
        ok
    });

    let res = client::connect_and_handshake_at(&name, expected, vec![(1, 2, 3, 4, true)], Duration::from_secs(5));
    match res {
        Ok(client_handle) => win::close(client_handle),
        Err(err) => panic!("正确令牌应握手成功: {err}"),
    }
    assert!(server.join().unwrap(), "服务端应收到带正确令牌的 Hello");
}

#[test]
fn handshake_rejects_wrong_token() {
    let name = pipe_name("bad");
    let expected: [u8; TOKEN_LEN] = [0x11; TOKEN_LEN];

    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let server = win::create_pipe_server(&server_name).expect("create server");
        let connected = win::wait_for_client(server);
        let saw_mismatch = if connected {
            match worker::read_frame_pub(server, Duration::from_secs(5)) {
                Some(Frame::Hello { token, .. }) => token != expected,
                _ => false,
            }
        } else {
            false
        };
        // 令牌不符:不写 Welcome,直接断开(worker 侧还会置 EXIT 退出)
        win::close(server);
        saw_mismatch
    });

    let res = client::connect_and_handshake_at(
        &name,
        [0x22; TOKEN_LEN],
        Vec::new(),
        Duration::from_millis(800),
    );
    assert!(res.is_err(), "错误令牌不应拿到 Welcome");
    assert!(server.join().unwrap(), "服务端应看到令牌与期望不同");
}
