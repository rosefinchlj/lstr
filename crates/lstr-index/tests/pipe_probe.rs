//! `win::pipe_available` 回归:探活必须零交互(不建会话),且随服务端句柄
//! 关闭而转 false。旧实现用 `connect_and_handshake` 探活,会真的连上 worker
//! (空握手 → 全量扫描)并泄漏句柄,把唯一管道实例永久占住。

#[test]
fn pipe_available_tracks_server_lifetime() {
    let name = format!(
        r"\\.\pipe\lstr-probe-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    assert!(
        !lstr_index::win::pipe_available(&name),
        "不存在的管道应探活为 false"
    );
    let server = lstr_index::win::create_pipe_server(&name).expect("创建测试管道");
    assert!(
        lstr_index::win::pipe_available(&name),
        "有实例监听时应为 true"
    );
    lstr_index::win::close(server);
    // 句柄关闭后,最后一个实例被删除,探活应转为 false(留少量重试窗口)。
    let mut gone = false;
    for _ in 0..50 {
        if !lstr_index::win::pipe_available(&name) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(gone, "服务端关闭后探活应立即转 false");
}
