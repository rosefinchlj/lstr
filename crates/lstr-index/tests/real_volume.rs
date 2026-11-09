//! 真卷端到端(需管理员 + 真实 NTFS 卷),默认忽略。
//!
//! 这是 #5 残余风险「真卷 e2e 未跑」的自动化入口,但打开卷句柄读
//! FSCTL_ENUM_USN_DATA 需要管理员权限,测试框架无法自行提权(UAC 是交互的),
//! 所以标记 `#[ignore]`,由人工从提权终端触发一次验收:
//!
//! ```text
//! :: 从「管理员」终端,工作目录 crates/lstr-index 的上两级
//! set LSTR_E2E_VOLUME=C
//! cargo test -p lstr-index --test real_volume -- --ignored --nocapture
//! ```
//!
//! 覆盖链路:打开卷句柄 → 读 journal 元数据(jid/first_usn/next_usn)→
//! 从 FRN 0 全量 ENUM(中途 stop,验证可中止)→ 记录含非空名字 →
//! 从断点 FRN 续扫(验证 resume 路径不 panic、不越界)。
//! 非提权运行会在 `VolumeHandle::open` 处失败,属预期。

use lstr_index::scan;

#[test]
#[ignore = "需要管理员权限打开卷句柄;从提权终端运行 cargo test -p lstr-index --test real_volume -- --ignored --nocapture"]
fn real_volume_scan_produces_records() {
    let letter = std::env::var("LSTR_E2E_VOLUME")
        .ok()
        .and_then(|s| s.trim().chars().next())
        .unwrap_or('C');

    // 非管理员:CreateFileW 返回 ERROR_ACCESS_DENIED(5) → 这里失败并提示。
    let vol = scan::VolumeHandle::open(letter).expect("打开卷句柄失败(需管理员运行)");
    let (journal_id, first_usn, next_usn) = vol.journal_state().expect("卷应启用 USN journal");
    assert!(journal_id > 0, "journal_id 应非 0");
    assert!(next_usn >= first_usn, "NextUsn 不应小于 FirstUsn");

    // 第一批:从 FRN 0 全量 ENUM,扫到 2000 条就 stop(小卷可能一次扫完)。
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut seen = 0usize;
    let mut sample: Vec<String> = Vec::new();
    let mut last_frn = 0u64;
    let res = scan::scan_volume(letter, 0, next_usn, &stop, |_, _, _| {}, |recs| {
        for r in &recs {
            last_frn = last_frn.max(r.frn);
            if sample.len() < 3 && !r.name.is_empty() {
                sample.push(r.name.clone());
            }
        }
        seen += recs.len();
        if seen >= 2000 {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    assert!(res.is_ok(), "首批扫描失败: {res:?}");
    assert!(seen > 0, "真卷应扫出至少一条记录");
    assert!(!sample.is_empty(), "记录应含非空名字(前 3 条: {sample:?})");

    // 第二批:从断点 FRN 续扫(覆盖 resume 路径),同样限 500 条即停。
    let stop2 = std::sync::atomic::AtomicBool::new(false);
    let mut seen2 = 0usize;
    let res2 = scan::scan_volume(
        letter,
        last_frn.saturating_add(1),
        next_usn,
        &stop2,
        |_, _, _| {},
        |recs| {
            seen2 += recs.len();
            if seen2 >= 500 {
                stop2.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        },
    );
    assert!(res2.is_ok(), "续扫失败: {res2:?}");
}
