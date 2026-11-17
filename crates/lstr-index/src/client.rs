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

/// 跨进程共享的 worker 令牌（P1-1）。
///
/// 旧实现每个 GUI 进程随机生成令牌：`keep_worker_alive` 常驻模式下，退出后
/// 旧 worker 还活着，下次启动是新进程 → 新令牌 ≠ worker 令牌 → 握手被拒，
/// 白等约 60s 再 runas 新 worker（又一次 UAC）。令牌因此必须按用户持久化，
/// 让「退出保留索引服务 → 下次启动」复用同一个 worker。
///
/// 令牌文件位于状态目录（%APPDATA%\lstr\index\worker.token，随用户 profile
/// ACL 隔离）；首次生成后跨进程复用，文件损坏/不可写时回退到进程内随机值。
pub fn worker_token() -> [u8; TOKEN_LEN] {
    static TOKEN: OnceLock<[u8; TOKEN_LEN]> = OnceLock::new();
    *TOKEN.get_or_init(|| {
        token_state_dir()
            .map(|dir| load_or_create_token(&dir))
            .unwrap_or_else(random_token)
    })
}

/// `--index-token` 命令行参数形式（与 [`worker_token`] 一致，跨进程稳定）。
pub fn worker_token_hex() -> String {
    crate::proto::token_hex(&worker_token())
}

/// 令牌所在状态目录（与快照/engine.log 相同）。
fn token_state_dir() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|base| std::path::PathBuf::from(base).join("lstr").join("index"))
}

/// 读取或创建令牌文件。写入用 tmp+rename，避免两个进程首次同时生成时
/// 读到半个文件；写完再读回，竞态时以磁盘内容为准。
fn load_or_create_token(dir: &std::path::Path) -> [u8; TOKEN_LEN] {
    let path = dir.join("worker.token");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(token) = crate::proto::token_from_hex(text.trim()) {
            return token;
        }
    }
    let token = random_token();
    let _ = std::fs::create_dir_all(dir);
    let tmp = dir.join("worker.token.tmp");
    if std::fs::write(&tmp, crate::proto::token_hex(&token)).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| crate::proto::token_from_hex(text.trim()))
        .unwrap_or(token)
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
    connect_and_handshake_at(crate::PIPE_NAME, worker_token(), handshake, timeout)
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
    loop {
        // 每轮只保留「本轮失败原因」；到期时返回最后一次，避免跨轮可变状态。
        let last_err = match win::open_pipe_client(pipe_name) {
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
                        "no welcome".to_string()
                    }
                }
            }
            Err(err) => err,
        };
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

/// 等待 worker 进程退出:轮询管道探活,实例消失(或超时)即返回。
/// worker 收到 `Frame::Shutdown` 后会退出并写 clean-exit 标记;它也可能被
/// 强杀/卡在收尾 —— 超时兜底返回 false,调用方不得无限等(协议容忍半开,
/// 残留 worker 靠自身 idle 超时与 clean-exit 降级机制自愈)。
///
/// 探活语义与 [`pipe_available`] 一致:实例存在(哪怕会话占用)→ worker
/// 还没退;管道名消失 → worker 已退出并释放唯一管道实例。
pub fn wait_worker_exit(pipe_name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !win::pipe_available(pipe_name) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !win::pipe_available(pipe_name)
}

/// 向 worker 发 `Frame::Shutdown` 并等待其退出(有超时)。
/// 返回 true = worker 已确认退出(管道实例消失);false = 超时/写帧失败
/// (半开状态,交由 worker 自身的 idle 超时兜底)。
pub fn shutdown_and_wait(client: HANDLE, pipe_name: &str, timeout: Duration) -> bool {
    let written = write_frame(client, &Frame::Shutdown);
    if !written {
        // 写失败多半是管道已断(worker 已死/半开):探活确认一次
        return !win::pipe_available(pipe_name);
    }
    wait_worker_exit(pipe_name, timeout)
}

/// 向 worker 发 `Frame::Detach`:结束本次会话但让 worker 继续常驻
/// （保持索引热态，下次启动免 UAC 秒连）。不等待进程退出。
/// 返回 false = 写帧失败（管道已断，进程可能已退出）。
pub fn detach(client: HANDLE) -> bool {
    write_frame(client, &Frame::Detach)
}

/// 控制指令 → 线上帧。主进程发指令的**唯一**映射点:新增指令只改这里,
/// 避免各处手搓 `Frame` 与协议枚举漂移。
pub fn command_frame(cmd: &crate::ClientCommand) -> Frame {
    match cmd {
        crate::ClientCommand::RescanVolumes(mask) => Frame::Rescan { mask: *mask },
        crate::ClientCommand::Shutdown => Frame::Shutdown,
        crate::ClientCommand::SetVolumes(mask) => Frame::SetVolumes { mask: *mask },
    }
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
    fn command_frame_maps_every_command() {
        assert_eq!(
            command_frame(&crate::ClientCommand::RescanVolumes(0b100)),
            Frame::Rescan { mask: 0b100 }
        );
        assert_eq!(command_frame(&crate::ClientCommand::Shutdown), Frame::Shutdown);
        // 掩码原样透传,不做语义解释(0 也照样传,worker 侧再决定回落)
        assert_eq!(
            command_frame(&crate::ClientCommand::SetVolumes(0b100)),
            Frame::SetVolumes { mask: 0b100 }
        );
        assert_eq!(
            command_frame(&crate::ClientCommand::SetVolumes(0)),
            Frame::SetVolumes { mask: 0 }
        );
    }

    #[test]
    fn session_token_is_stable_and_roundtrips_hex() {
        let a = session_token();
        assert_eq!(a, session_token(), "同进程内令牌必须稳定(重连复用)");
        assert!(a.iter().any(|&b| b != 0), "令牌不应全零");
        let hex = crate::proto::token_hex(&session_token());
        assert_eq!(crate::proto::token_from_hex(&hex).unwrap(), a);
    }

    #[test]
    fn persisted_worker_token_is_shared_across_processes() {
        let dir = std::env::temp_dir().join(format!(
            "lstr-token-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let a = load_or_create_token(&dir);
        assert!(dir.join("worker.token").exists(), "首次必须落盘令牌文件");
        // 模拟另一个 GUI 进程：同一状态目录、不经过本进程 OnceLock 缓存。
        let b = load_or_create_token(&dir);
        assert_eq!(a, b, "跨进程必须复用同一令牌，否则 keep-alive 重开握手失败");
        assert!(a.iter().any(|&v| v != 0), "令牌不应全零");
        // 文件损坏（半写/被改写）→ 重新生成，且之后保持稳定。
        std::fs::write(dir.join("worker.token"), b"zz").unwrap();
        let c = load_or_create_token(&dir);
        assert_eq!(c, load_or_create_token(&dir));
        assert_ne!(c, [0u8; TOKEN_LEN]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
