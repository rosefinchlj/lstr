//! 管道客户端(主进程侧):连接重试、握手、帧收发。
//! 事件循环线程由 lstr 集成层启动(P3),这里提供原语。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;

use crate::proto::Frame;
use crate::win;

/// 连接 + 握手。重试到 timeout(覆盖 worker 启动 + UAC 同意的延迟)。
/// 返回管道句柄;失败返回 Err(原因)。
pub fn connect_and_handshake(
    handshake: Vec<(u128, u64, i64)>,
    timeout: Duration,
) -> Result<HANDLE, String> {
    let deadline = Instant::now() + timeout;
    let handshake = &handshake;
    let mut last_err;
    loop {
        last_err = String::from("unknown");
        match win::open_pipe_client(crate::PIPE_NAME) {
            Ok(client) => {
                let hello = Frame::Hello { proto: crate::PROTO_VERSION, volumes: handshake.clone() };
                if !crate::worker::write_frame_pub(client, &hello) {
                    win::close(client);
                    return Err("handshake write failed".into());
                }
                match crate::worker::read_frame_pub(client, Duration::from_secs(15)) {
                    Some(Frame::Welcome { proto }) if proto == crate::PROTO_VERSION => {
                        return Ok(client);
                    }
                    Some(other) => {
                        win::close(client);
                        return Err(format!("unexpected frame in handshake: {other:?}"));
                    }
                    None => {
                        win::close(client);
                        last_err = "no welcome".into();
                    }
                }
            }
            Err(err) => {
                last_err = err;
            }
        }
        if Instant::now() >= deadline {
            return Err(last_err);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// 阻塞读一帧(worker 侧原语复用)。
pub fn read_frame(client: HANDLE, timeout: Duration) -> Option<Frame> {
    crate::worker::read_frame_pub(client, timeout)
}

/// 写一帧。
pub fn write_frame(client: HANDLE, frame: &Frame) -> bool {
    crate::worker::write_frame_pub(client, frame)
}
