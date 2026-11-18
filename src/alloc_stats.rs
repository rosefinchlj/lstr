//! P1-8 内存归因：`alloc-stats` feature 下的计数分配器。
//!
//! 目的：在不引入第三方 profiler 的前提下，按大小档统计 Rust 侧的常驻分配，
//! 配合 `VirtualQueryEx` 区域采样，把 `private` 拆成
//! 「引擎数据 / UI 纹理 / 分配器」三块，给结构性内存优化提供依据。
//!
//! 只在 `--features alloc-stats` 构建时编译，默认发布版零开销；
//! 运行时需设置 `LSTR_ALLOC_STATS=1` 才周期写日志（engine.log）。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

const BUCKETS: usize = 7;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);
/// P1-8 归因辅助：GUI 帧计数。隐藏空闲时若帧率仍高，说明有代码在持续
/// `request_repaint`/动画，会带来每帧临时分配——需要和 allocs 速率对照。
static FRAMES: AtomicUsize = AtomicUsize::new(0);

/// 每帧调用一次（仅 alloc-stats 构建），累计帧数供周期报告对比分配速率。
pub fn count_frame() {
    FRAMES.fetch_add(1, Ordering::Relaxed);
}
static BUCKET_LIVE: [AtomicUsize; BUCKETS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

fn bucket_of(size: usize) -> usize {
    match size {
        0..=64 => 0,
        65..=256 => 1,
        257..=1024 => 2,
        1025..=4096 => 3,
        4097..=65_536 => 4,
        65_537..=1_048_576 => 5,
        _ => 6,
    }
}

fn record_alloc(size: usize) {
    let bucket = bucket_of(size);
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    let _ = PEAK.fetch_max(live, Ordering::Relaxed);
    BUCKET_LIVE[bucket].fetch_add(size, Ordering::Relaxed);
    ALLOCS.fetch_add(1, Ordering::Relaxed);
}

fn record_free(size: usize) {
    let bucket = bucket_of(size);
    LIVE.fetch_sub(size, Ordering::Relaxed);
    BUCKET_LIVE[bucket].fetch_sub(size, Ordering::Relaxed);
    FREES.fetch_add(1, Ordering::Relaxed);
}

/// realloc 成功：同一块分配从 old 大小迁移到 new 大小。
///
/// 早前版本只把「差额」记进差额所在档，dealloc 时却按 new_size 扣档，
/// 导致小档桶下溢成 `usize::MAX`（2026-11-17 测量中出现的
/// `17592186044415.3MB`），这里按档迁移修正。
fn record_realloc(old: usize, new: usize) {
    BUCKET_LIVE[bucket_of(old)].fetch_sub(old, Ordering::Relaxed);
    BUCKET_LIVE[bucket_of(new)].fetch_add(new, Ordering::Relaxed);
    if new >= old {
        let grow = new - old;
        let live = LIVE.fetch_add(grow, Ordering::Relaxed) + grow;
        let _ = PEAK.fetch_max(live, Ordering::Relaxed);
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    } else {
        LIVE.fetch_sub(old - new, Ordering::Relaxed);
    }
}

/// 透传系统分配器并累计统计；只由 `#[global_allocator]` 安装。
pub struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_free(layout.size());
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            record_realloc(layout.size(), new_size);
        }
        new_ptr
    }
}

/// 一次快照（字节）。桶按分配大小分组，便于区分「小对象碎片」与「大块缓冲」。
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub live: usize,
    pub peak: usize,
    pub allocs: usize,
    pub frees: usize,
    pub buckets: [usize; BUCKETS],
}

pub fn snapshot() -> Stats {
    let mut buckets = [0usize; BUCKETS];
    for (i, slot) in buckets.iter_mut().enumerate() {
        *slot = BUCKET_LIVE[i].load(Ordering::Relaxed);
    }
    Stats {
        live: LIVE.load(Ordering::Relaxed),
        peak: PEAK.load(Ordering::Relaxed),
        allocs: ALLOCS.load(Ordering::Relaxed),
        frees: FREES.load(Ordering::Relaxed),
        buckets,
    }
}

/// 单行报告，写 engine.log 用（debug_log 自带时间戳/轮转）。
pub fn report() -> String {
    let s = snapshot();
    let pid = std::process::id();
    format!(
        "alloc[{pid}]: live={:.1}MB peak={:.1}MB allocs={} frees={} frames={} \
         buckets(<=64/<=256/<=1K/<=4K/<=64K/<=1M/>1M)={:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1}MB",
        s.live as f64 / 1_048_576.0,
        s.peak as f64 / 1_048_576.0,
        s.allocs,
        s.frees,
        FRAMES.load(Ordering::Relaxed),
        s.buckets[0] as f64 / 1_048_576.0,
        s.buckets[1] as f64 / 1_048_576.0,
        s.buckets[2] as f64 / 1_048_576.0,
        s.buckets[3] as f64 / 1_048_576.0,
        s.buckets[4] as f64 / 1_048_576.0,
        s.buckets[5] as f64 / 1_048_576.0,
        s.buckets[6] as f64 / 1_048_576.0,
    )
}

/// 在关键阶段点写一行分配快照，便于和 VirtualQueryEx 采样对齐，
/// 判断某块内存是「引擎载入」还是「UI/框架」带来的。
pub fn log_phase(label: &str) {
    if std::env::var_os("LSTR_ALLOC_STATS").is_none() {
        return;
    }
    lstr_index::snapshot::debug_log(&format!("alloc-phase[{label}]: {}", report()));
}

/// 周期写日志：`LSTR_ALLOC_STATS=1` 时每 10s 一行，最多 3 分钟（足够覆盖
/// 启动 → 快照载入 → 搜索 → 预览 → 闲置各阶段）。
pub fn start_periodic_logging() {
    if std::env::var_os("LSTR_ALLOC_STATS").is_none() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("lstr-alloc-stats".into())
        .spawn(|| {
            for _ in 0..18 {
                std::thread::sleep(std::time::Duration::from_secs(10));
                lstr_index::snapshot::debug_log(&report());
            }
        });
}

#[cfg(all(test, feature = "alloc-stats"))]
mod tests {
    #[test]
    fn counting_allocator_tracks_live_bytes() {
        let before = super::snapshot();
        let v = vec![0u8; 1 << 20];
        let during = super::snapshot();
        assert!(during.live >= before.live + (1 << 20));
        assert!(during.buckets[5] >= (1 << 20));
        drop(v);
        let after = super::snapshot();
        assert!(after.live + (1 << 20) <= during.live);
    }
}
