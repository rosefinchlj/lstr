//! 管道客户端(主进程侧):连接重试、握手、帧收发。
//! 事件循环线程由 lstr 集成层启动(P3),这里提供原语。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;

use crate::proto::{Frame, TOKEN_LEN};
use crate::win;

/// 本进程会话令牌:每次启动生成一次,worker 由我们用同一令牌经 runas 拉起。
/// 固定管道名 + 只比 exe 名的 `verify_peer` 挡不住同用户的其它进程,令牌让
/// 它们即便连上管道也会被 worker 拒绝。
pub fn session_token() -> [u8; TOKEN_LEN] {
    static TOKEN: OnceLock<[u8; TOKEN_LEN]> = OnceLock::new();
    *TOKEN.get_or_init(random_token)
}

/// `--index-token` 命令行参数形式。
pub fn session_token_hex() -> String {
    crate::proto::token_hex(&session_token())
}

/// 无外部随机源依赖的伪随机:时间/进程/栈地址混合。仅用于能力校验
/// (同用户读命令行即可拿到),不承担密码学强度。
fn random_token() -> [u8; TOKEN_LEN] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let stack = (&nanos as *const u64) as u64;
    let mut x = nanos ^ pid.rotate_left(32) ^ stack.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = [0u8; TOKEN_LEN];
    for chunk in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let v = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
        chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
    }
    out
}

/// 连接 + 握手。重试到 timeout(覆盖 worker 启动 + UAC 同意的延迟)。
/// 返回管道句柄;失败返回 Err(原因)。
pub fn connect_and_handshake(
    handshake: Vec<(u128, u64, i64, u64, bool)>,
    timeout: Duration,
) -> Result<HANDLE, String> {
    connect_and_handshake_at(crate::PIPE_NAME, session_token(), handshake, timeout)
}

/// 指定管道名/令牌的握手(集成测试复用;worker 侧校验令牌)。
pub fn connect_and_handshake_at(
    pipe_name: &str,
    token: [u8; TOKEN_LEN],
    handshake: Vec<(u128, u64, i64, u64, bool)>,
    timeout: Duration,
) -> Result<HANDLE, String> {
    let deadline = Instant::now() + timeout;
    let handshake = &handshake;
    #[allow(unused_assignments)]
    let mut last_err = String::new();
    loop {
        match win::open_pipe_client(pipe_name) {
            Ok(client) => {
                let hello = Frame::Hello {
                    proto: crate::PROTO_VERSION,
                    token,
                    volumes: handshake.clone(),
                };
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

/// 管道是否仍然连通(peek 探测)。
pub fn pipe_alive(client: HANDLE) -> bool {
    use windows::Win32::System::Pipes::PeekNamedPipe;
    unsafe { PeekNamedPipe(client, None, 0, None, None, None).is_ok() }
}

/// 探活:是否有 worker 在监听(不建立会话、不动握手)。
pub fn pipe_available() -> bool {
    crate::win::pipe_available(crate::PIPE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_token_is_stable_and_roundtrips_hex() {
        let a = session_token();
        assert_eq!(a, session_token(), "同进程内令牌必须稳定(重连复用)");
        assert!(a.iter().any(|&b| b != 0), "令牌不应全零");
        let hex = session_token_hex();
        assert_eq!(crate::proto::token_from_hex(&hex).unwrap(), a);
    }
}
