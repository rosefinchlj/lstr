//! 快照:卷内 store 的持久化。
//!
//! 格式(小端):
//! ```text
//! magic "LIDX" | u32 version | u128 guid | u8 letter | u8 flags(complete=1,noise=2)
//! u64 journal_id | i64 next_usn | u64 resume_frn | u64 est_records
//! u64 count
//! names  : u64 len + bytes(\0 结尾打包;name_off 由此重建,不落盘)
//! offs   : u64 len + u32×  (按小写名序)
//! frn    : u64 len + u64×  (按 FRN 序)
//! parent : u64 len + u32×
//! attrs  : u64 len + u16×
//! mtime  : u64 len + u32×  (unix 秒;0=未知,USN 路径为 0)
//! size   : u64 len + u64×  (字节;u64::MAX=未知)
//! unres  : u64 len + (u32 slot, u64 parent_frn)×
//! ```
//! 列必须一致有序。就绪卷由 [`SaveTask`] 构造只读快照视图(不 compact store,
//! 分段写);scan 模式/外部调用走 [`save_report`](先 compact 再整写)。两条
//! 路径都沿 tmp+fsync+rename 原子替换。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::store::{VolumeStore, ATTR_INTERNAL, ATTR_SEEN, NO_PARENT};

pub const MAGIC: &[u8; 4] = b"LIDX";
/// v3:未消解父链接 (slot, parent_frn) 也落盘(v2 没有,重启后这些条目永久退化)。
/// v4:增加 mtime 列(unix 秒),内置时间筛选/结果元数据使用。
/// v5:排除表从布尔开关改为内容哈希(noise_hash),排除规则任意变更即失配重扫。
/// v6:增加 size 列(u64 字节,`u64::MAX`=未知),内置大小筛选使用。
pub const VERSION: u32 = 6;

/// 覆盖日志目录的环境变量(集成测试/诊断隔离用)。
const LOG_DIR_ENV: &str = "LSTR_INDEX_LOG_DIR";

/// engine.log 轮转阈值:超过即把当前日志改名为 .1(覆盖旧 .1),只保留最近
/// 一段历史。旧实现无上限追加,长期运行会无限增长吃磁盘。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 超限轮转(纯函数,可离线单测):当前日志 ≥ 阈值时 rename 为同名 .1。
/// Windows 的 rename 不覆盖已存在目标,故先删旧 .1。失败静默(日志非关键)。
fn rotate_if_oversized(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() >= MAX_LOG_BYTES {
            let bak = path.with_extension("log.1");
            let _ = std::fs::remove_file(&bak);
            let _ = std::fs::rename(path, &bak);
        }
    }
}

/// 引擎调试日志(默认 %APPDATA%\lstr\index\engine.log;[`LOG_DIR_ENV`] 可
/// 覆盖目录)。worker 提权进程与 UI 都写同用户目录;失败静默。
///
/// 测试二进制一律旁路(显式设了 [`LOG_DIR_ENV`] 的用例除外):`cfg(test)`
/// 只覆盖 lstr-index 自己的单测;主 crate 的单测/集成测试把本 crate 当普通
/// 依赖编译,数百个用例的「loaded 0 volumes」会写进真实日志并把 2MB 排障
/// 证据刷到轮转(真实踩坑)。测试二进制在 `target/*/deps/` 下,据此识别。
pub fn debug_log(msg: &str) {
    if std::env::var_os(LOG_DIR_ENV).is_none() && running_under_test_harness() {
        return;
    }
    let Some(dir) = log_dir() else { return };
    let path = dir.join("engine.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_if_oversized(&path);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", chrono_now(), msg);
    }
}

/// 当前进程是否为 cargo 测试二进制(可执行文件位于 `.../deps/` 下)。
fn running_under_test_harness() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| name == "deps")
}

fn log_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(LOG_DIR_ENV) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let base = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(base).join("lstr").join("index"))
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    format!("{}.{:03}", secs, d.subsec_millis())
}

pub const FLAG_COMPLETE: u8 = 0x01;

#[derive(Clone, Debug)]
pub struct SnapshotHeader {
    pub guid: u128,
    pub letter: char,
    pub complete: bool,
    /// 排除规则内容哈希(小写正斜杠归一化后排序哈希)。规则变更即失配重扫。
    pub noise_hash: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub resume_frn: u64,
    pub est_records: u64,
}

pub fn snapshot_path(dir: &Path, guid: u128) -> PathBuf {
    dir.join(format!("vol_{guid:032x}.bin"))
}

/// `save` 的附加结果。
///
/// `compacted` 表示保存期间执行了 compact —— 那会重排/收缩 slot 编号。
/// 调用方只要持有以 slot 为键的缓存(engine 里的拼音候选缓存),就必须在
/// 保存成功后重建缓存;否则缓存里的旧 slot 会越界或指向别的条目
/// (m04259: `PinyinCache::search` 越界 panic,整个搜索线程死掉)。
pub struct SaveReport {
    pub bytes: u64,
    pub compacted: bool,
}

/// 序列化并原子写盘。count==0 且 !complete 的空卷不写。
/// 契约「列一致」由这里保证:有未归并 delta 时先 compact(早退使重复调用廉价)。
///
/// 需要知道保存期间是否发生过 compact 的调用方用 [`save_report`]。
pub fn save(
    dir: &Path,
    header: &SnapshotHeader,
    store: &mut VolumeStore,
) -> std::io::Result<u64> {
    save_report(dir, header, store).map(|report| report.bytes)
}

/// 同 [`save`],但把「保存期间是否 compact 过」一并返回,供缓存维护使用。
pub fn save_report(
    dir: &Path,
    header: &SnapshotHeader,
    store: &mut VolumeStore,
) -> std::io::Result<SaveReport> {
    // 列一致契约:未归并 delta、扫描中、或有就地改名(名字区残留幽灵字节)
    // 都必须先 compact,否则 count 与 names 条目数对不上,load 直接拒绝
    // → 每次启动全量重扫(property 随机流里约 1/4 概率命中改名后落盘)。
    let compacted = if !store.delta_is_empty()
        || store.is_scanning()
        || store.has_pending_rename()
    {
        store.compact();
        true
    } else {
        false
    };
    std::fs::create_dir_all(dir)?;
    let final_path = snapshot_path(dir, header.guid);
    let tmp_path = dir.join(format!("vol_{:032x}.tmp", std::process::id()));
    let f = std::fs::File::create(&tmp_path)?;
    let mut w = std::io::BufWriter::with_capacity(8 << 20, f);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&header.guid.to_le_bytes())?;
    w.write_all(&[header.letter as u32 as u8])?;
    w.write_all(&[flags_of(header.complete)])?;
    w.write_all(&header.journal_id.to_le_bytes())?;
    w.write_all(&header.next_usn.to_le_bytes())?;
    w.write_all(&header.resume_frn.to_le_bytes())?;
    w.write_all(&header.est_records.to_le_bytes())?;
    w.write_all(&header.noise_hash.to_le_bytes())?;
    w.write_all(&(store.name_off.len() as u64).to_le_bytes())?;
    write_blob(&mut w, &store.names)?;
    write_col_u32(&mut w, &store.offs)?;
    write_col_u64(&mut w, &store.frn)?;
    write_col_u32(&mut w, &store.parent)?;
    write_col_u16(&mut w, &store.attrs)?;
    write_col_u32(&mut w, &store.mtime)?;
    write_col_u64(&mut w, &store.size)?;
    // 未消解父链接:compact 会把 slot 重映射到最终编号,此时写入可直接恢复
    let pairs = store.unresolved_pairs();
    w.write_all(&(pairs.len() as u64).to_le_bytes())?;
    for (slot, parent_frn) in &pairs {
        w.write_all(&slot.to_le_bytes())?;
        w.write_all(&parent_frn.to_le_bytes())?;
    }
    w.flush()?;
    // 先 fsync 再 rename:否则崩溃/断电时 rename 可能先于数据落盘,
    // 留下「文件名在、内容空/截断」的快照(load 拒绝 → 全量重扫)。
    let f = w.into_inner().map_err(|e| e.into_error())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path)?;
    // rename 改的是父目录项:再 flush 一次目录,缩小「文件名已改、目录项未持久」
    // 的断电窗口。FAT/网络盘可能不支持,失败只记日志(最坏仍是一次重扫)。
    if let Some(dir) = final_path.parent() {
        if !crate::win::flush_dir(dir) {
            debug_log(&format!(
                "snapshot: flush_dir {} failed, dir entry may be non-durable",
                dir.display()
            ));
        }
    }
    Ok(SaveReport {
        bytes: std::fs::metadata(&final_path)?.len(),
        compacted,
    })
}

fn flags_of(complete: bool) -> u8 {
    if complete { FLAG_COMPLETE } else { 0 }
}

fn write_blob<W: Write>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)
}

fn write_col<W: Write, T: Copy>(w: &mut W, col: &[T]) -> std::io::Result<()> {
    let bytes = unsafe {
        std::slice::from_raw_parts(col.as_ptr() as *const u8, col.len() * std::mem::size_of::<T>())
    };
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)
}

fn write_col_u32<W: Write>(w: &mut W, col: &[u32]) -> std::io::Result<()> {
    write_col(w, col)
}
fn write_col_u64<W: Write>(w: &mut W, col: &[u64]) -> std::io::Result<()> {
    write_col(w, col)
}
fn write_col_u16<W: Write>(w: &mut W, col: &[u16]) -> std::io::Result<()> {
    write_col(w, col)
}

// ---------------------------------------------------------------------------
// 分段快照保存(可中断状态机)
// ---------------------------------------------------------------------------
//
// 背景:整卷 compact + 一次写完 50-120MB 会把引擎线程独占 1-3 秒(实测
// `save vol C took 2397ms`),查询只能排队。这里把「为快照准备一致列 + 写盘」
// 拆成可恢复的步骤:
//
// - 保存期间**不 compact store**,而是按「base(已按 FRN 有序)+ delta」构造
//   只读快照视图:活 slot 按 FRN 归并、父指针重映射、名字区重建(就地改名的
//   幽灵字节不落盘)、offs 按当前名归并。store 一个字节都不改,查询可继续
//   (引擎把修改类消息延迟到保存完成,见 Engine::handle)。
// - 每步只做一小块(64K 元素 / 256KB 输出),步间检查查询让路钩子;状态
//   (阶段 + 游标)保存在 SaveTask 里,下一轮 maintain 续写。
// - 沿用 tmp + fsync + rename 纪律:只有全部列写完并 fsync 后才 rename;
//   中途崩溃留下的 `.tmp` 由 load_snapshots 启动清理。
//
// 一次性保存(save_view_sync)与分段保存跑同一个状态机,只是预算不同,
// 因此两条路径的最终字节天然一致(测试对拍)。

/// 单个保存步的元素/输出上限(约 1-4ms 工作量)。
const SAVE_CHUNK_ELEMS: usize = 64 * 1024;
/// 输出缓冲上限;超过即写盘,避免一次 write 过大。
const SAVE_WRITE_BUF: usize = 256 * 1024;
/// 引擎空闲轮里单个 maintain 的保存预算(超过就让出,查询优先)。
pub const SAVE_STEP_BUDGET: Duration = Duration::from_millis(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SavePhase {
    Start,
    ScanBase,
    ScanDelta,
    SortDelta,
    Merge,
    NamesLen,
    Header,
    Names,
    OffsPrepare,
    OffsBase,
    OffsExtras,
    OffsSort,
    OffsHeader,
    Offs,
    Frn,
    Parent,
    Attrs,
    Mtime,
    Size,
    UnresolvedWrite,
    Finish,
    SyncWait,
}

pub(crate) enum SaveStep {
    Pending,
    Done(SaveReport),
    Failed(std::io::Error),
}

/// 可中断的整卷快照写入器。一次只服务一个卷(engine 持有 `Option<SaveTask>`)。
pub(crate) struct SaveTask {
    guid: u128,
    header: SnapshotHeader,
    dir: PathBuf,
    tmp_path: PathBuf,
    final_path: PathBuf,
    file: Option<std::fs::File>,
    phase: SavePhase,
    started: Instant,
    /// base 已归并 slot 数 / 当前 store 总 slot 数。
    base_len: u32,
    slot_count: u32,
    /// 最终 slot 序(旧 slot id,按 FRN 升序)与 old→new 映射。
    order: Vec<u32>,
    delta: Vec<u32>,
    merged: Vec<u32>,
    remap: Vec<u32>,
    merge_b: usize,
    merge_d: usize,
    /// 当前阶段的元素游标。
    cursor: usize,
    /// 名字区总字节数(头里先写,再流式追加名字)。
    names_len: u64,
    /// 写缓冲。
    out: Vec<u8>,
    /// offs 视图:base 中仍有效的部分(新 id,按名有序)+ 其余活槽(新 id)。
    renamed: std::collections::HashSet<u32>,
    covered: Vec<bool>,
    base_kept: Vec<u32>,
    extras: Vec<u32>,
    bi: usize,
    ei: usize,
    offs_written: usize,
    /// 落盘的未消解父链接(新 slot id)。
    unresolved: Vec<(u32, u64)>,
    /// 保存视图里可解析的未消解父链:(新 slot id, 新父 slot)。
    /// 旧 compact 会顺手解析;视图不改 store,但可以在父列写正确链接。
    parent_override: Vec<(u32, u32)>,
    /// fsync 放到独立线程:慢盘上 sync_all 可能数百 ms,不能占着引擎线程
    /// (查询还在等)。等待期间引擎继续服务查询,收尾只做 rename。
    sync_thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    finished: bool,
}

impl SaveTask {
    pub(crate) fn new(dir: &Path, header: &SnapshotHeader) -> Self {
        let guid = header.guid;
        Self {
            guid,
            header: header.clone(),
            dir: dir.to_path_buf(),
            tmp_path: dir.join(format!("vol_{:032x}.tmp", std::process::id())),
            final_path: snapshot_path(dir, guid),
            file: None,
            phase: SavePhase::Start,
            started: Instant::now(),
            base_len: 0,
            slot_count: 0,
            order: Vec::new(),
            delta: Vec::new(),
            merged: Vec::new(),
            remap: Vec::new(),
            merge_b: 0,
            merge_d: 0,
            cursor: 0,
            names_len: 0,
            out: Vec::with_capacity(SAVE_WRITE_BUF),
            renamed: std::collections::HashSet::new(),
            covered: Vec::new(),
            base_kept: Vec::new(),
            extras: Vec::new(),
            bi: 0,
            ei: 0,
            offs_written: 0,
            unresolved: Vec::new(),
            parent_override: Vec::new(),
            sync_thread: None,
            finished: false,
        }
    }

    pub(crate) fn guid(&self) -> u128 {
        self.guid
    }

    /// 从开始(含可能的让路等待)到当前的总耗时。
    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn file_mut(&mut self) -> std::io::Result<&mut std::fs::File> {
        self.file.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "snapshot save: file not opened")
        })
    }

    fn flush_out(&mut self) -> std::io::Result<()> {
        if !self.out.is_empty() {
            let out = std::mem::take(&mut self.out);
            self.file_mut()?.write_all(&out)?;
            self.out = out;
            self.out.clear();
        }
        Ok(())
    }

    fn pause(
        &mut self,
        started: Instant,
        budget: Duration,
        yield_now: &mut dyn FnMut() -> bool,
    ) -> bool {
        yield_now() || started.elapsed() >= budget
    }

    pub(crate) fn advance(
        &mut self,
        store: &VolumeStore,
        budget: Duration,
        yield_now: &mut dyn FnMut() -> bool,
    ) -> SaveStep {
        match self.advance_inner(store, budget, yield_now) {
            Ok(Some(report)) => SaveStep::Done(report),
            Ok(None) => SaveStep::Pending,
            Err(err) => SaveStep::Failed(err),
        }
    }

    fn advance_inner(
        &mut self,
        store: &VolumeStore,
        budget: Duration,
        yield_now: &mut dyn FnMut() -> bool,
    ) -> std::io::Result<Option<SaveReport>> {
        if self.finished {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "snapshot save: advanced after finish",
            ));
        }
        let step_started = Instant::now();
        loop {
            if yield_now() {
                return Ok(None);
            }
            match self.phase {
                SavePhase::Start => {
                    std::fs::create_dir_all(&self.dir)?;
                    self.file = Some(std::fs::File::create(&self.tmp_path)?);
                    self.base_len = store.base_len();
                    self.slot_count = store.len() as u32;
                    self.remap = vec![u32::MAX; self.slot_count as usize];
                    self.order = Vec::with_capacity(self.slot_count as usize);
                    self.phase = SavePhase::ScanBase;
                    self.cursor = 0;
                }
                SavePhase::ScanBase => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.base_len as usize);
                    for s in self.cursor..end {
                        if store.visible(s as u32) {
                            self.order.push(s as u32);
                        }
                    }
                    self.cursor = end;
                    if self.cursor >= self.base_len as usize {
                        self.phase = SavePhase::ScanDelta;
                        self.cursor = self.base_len as usize;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::ScanDelta => {
                    let end =
                        (self.cursor + SAVE_CHUNK_ELEMS).min(self.slot_count as usize);
                    for s in self.cursor..end {
                        if store.visible(s as u32) {
                            self.delta.push(s as u32);
                        }
                    }
                    self.cursor = end;
                    if self.cursor >= self.slot_count as usize {
                        self.phase = SavePhase::SortDelta;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::SortDelta => {
                    self.delta
                        .sort_unstable_by_key(|&s| store.frn[s as usize]);
                    self.merged = Vec::with_capacity(self.order.len() + self.delta.len());
                    self.merge_b = 0;
                    self.merge_d = 0;
                    self.phase = SavePhase::Merge;
                }
                SavePhase::Merge => {
                    // 两段都已按 FRN 升序:稳定地取较小者(base 先,FRN 唯一)。
                    let mut n = 0usize;
                    while self.merge_b < self.order.len() || self.merge_d < self.delta.len() {
                        let take_base = if self.merge_d >= self.delta.len() {
                            true
                        } else if self.merge_b >= self.order.len() {
                            false
                        } else {
                            store.frn[self.order[self.merge_b] as usize]
                                <= store.frn[self.delta[self.merge_d] as usize]
                        };
                        let old = if take_base {
                            let old = self.order[self.merge_b];
                            self.merge_b += 1;
                            old
                        } else {
                            let old = self.delta[self.merge_d];
                            self.merge_d += 1;
                            old
                        };
                        let new_id = self.merged.len() as u32;
                        self.merged.push(old);
                        self.remap[old as usize] = new_id;
                        n += 1;
                        if n >= SAVE_CHUNK_ELEMS {
                            break;
                        }
                    }
                    if self.merge_b >= self.order.len() && self.merge_d >= self.delta.len() {
                        self.order = std::mem::take(&mut self.merged);
                        self.delta.clear();
                        self.unresolved.clear();
                        self.parent_override.clear();
                        for (old, frn) in store.unresolved_pairs() {
                            let new_id = *self.remap.get(old as usize).unwrap_or(&u32::MAX);
                            if new_id == u32::MAX {
                                continue;
                            }
                            // 父项已在 store 里(只是子项先到):视图直接写父链接,
                            // 不必等下次 compact;父被排除时写 NO_PARENT。
                            if let Some(p) = store.find_slot(frn) {
                                let np =
                                    *self.remap.get(p as usize).unwrap_or(&NO_PARENT);
                                self.parent_override.push((new_id, np));
                            } else {
                                self.unresolved.push((new_id, frn));
                            }
                        }
                        self.parent_override.sort_unstable_by_key(|&(id, _)| id);
                        self.phase = SavePhase::NamesLen;
                        self.cursor = 0;
                        self.names_len = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::NamesLen => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    for k in self.cursor..end {
                        let name = store.name(self.order[k]);
                        self.names_len += name.len() as u64 + 1;
                    }
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::Header;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Header => {
                    let mut out = Vec::with_capacity(64);
                    out.extend_from_slice(MAGIC);
                    out.extend_from_slice(&VERSION.to_le_bytes());
                    out.extend_from_slice(&self.header.guid.to_le_bytes());
                    out.push(self.header.letter as u32 as u8);
                    out.push(flags_of(self.header.complete));
                    out.extend_from_slice(&self.header.journal_id.to_le_bytes());
                    out.extend_from_slice(&self.header.next_usn.to_le_bytes());
                    out.extend_from_slice(&self.header.resume_frn.to_le_bytes());
                    out.extend_from_slice(&self.header.est_records.to_le_bytes());
                    out.extend_from_slice(&self.header.noise_hash.to_le_bytes());
                    out.extend_from_slice(&(self.order.len() as u64).to_le_bytes());
                    out.extend_from_slice(&self.names_len.to_le_bytes());
                    self.file_mut()?.write_all(&out)?;
                    self.phase = SavePhase::Names;
                    self.cursor = 0;
                }
                SavePhase::Names => {
                    let mut n = 0usize;
                    while self.cursor < self.order.len() && n < SAVE_CHUNK_ELEMS {
                        let name = store.name(self.order[self.cursor]);
                        self.out.extend_from_slice(name);
                        self.out.push(0);
                        self.cursor += 1;
                        n += 1;
                        if self.out.len() >= SAVE_WRITE_BUF {
                            self.flush_out()?;
                        }
                    }
                    if self.cursor >= self.order.len() {
                        self.flush_out()?;
                        self.phase = SavePhase::OffsPrepare;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::OffsPrepare => {
                    self.renamed = store.renamed_base_slots().into_iter().collect();
                    self.covered = vec![false; self.order.len()];
                    self.base_kept.clear();
                    self.base_kept.reserve(store.offs.len());
                    self.phase = SavePhase::OffsBase;
                    self.cursor = 0;
                }
                SavePhase::OffsBase => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(store.offs.len());
                    for k in self.cursor..end {
                        let old = store.offs[k];
                        let new_id = *self.remap.get(old as usize).unwrap_or(&u32::MAX);
                        if new_id != u32::MAX && !self.renamed.contains(&old) {
                            self.covered[new_id as usize] = true;
                            self.base_kept.push(new_id);
                        }
                    }
                    self.cursor = end;
                    if self.cursor >= store.offs.len() {
                        self.phase = SavePhase::OffsExtras;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::OffsExtras => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    for k in self.cursor..end {
                        if !self.covered[k] {
                            self.extras.push(k as u32);
                        }
                    }
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::OffsSort;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::OffsSort => {
                    let order = &self.order;
                    self.extras.sort_unstable_by(|&a, &b| {
                        store
                            .name_lc(order[a as usize])
                            .cmp(store.name_lc(order[b as usize]))
                    });
                    self.phase = SavePhase::OffsHeader;
                    self.bi = 0;
                    self.ei = 0;
                    self.offs_written = 0;
                }
                SavePhase::OffsHeader => {
                    let n = self.order.len() as u64 * 4;
                    self.file_mut()?.write_all(&n.to_le_bytes())?;
                    self.phase = SavePhase::Offs;
                }
                SavePhase::Offs => {
                    self.out.clear();
                    let before = self.offs_written;
                    let mut n = 0usize;
                    while (self.bi < self.base_kept.len() || self.ei < self.extras.len())
                        && n < SAVE_CHUNK_ELEMS
                    {
                        let take_base = if self.ei >= self.extras.len() {
                            true
                        } else if self.bi >= self.base_kept.len() {
                            false
                        } else {
                            let a = self.order[self.base_kept[self.bi] as usize];
                            let b = self.order[self.extras[self.ei] as usize];
                            store.name_lc(a) <= store.name_lc(b)
                        };
                        let id = if take_base {
                            let id = self.base_kept[self.bi];
                            self.bi += 1;
                            id
                        } else {
                            let id = self.extras[self.ei];
                            self.ei += 1;
                            id
                        };
                        self.out.extend_from_slice(&id.to_le_bytes());
                        self.offs_written += 1;
                        n += 1;
                    }
                    self.flush_out()?;
                    if self.offs_written >= self.order.len() {
                        self.phase = SavePhase::Frn;
                        self.cursor = 0;
                    } else if self.offs_written == before {
                        // 防御:base_kept ∪ extras 必须恰好覆盖全部活槽。
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "snapshot offs incomplete: {}/{}",
                                self.offs_written,
                                self.order.len()
                            ),
                        ));
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Frn => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    if self.cursor == 0 {
                        let n = self.order.len() as u64 * 8;
                        self.file_mut()?.write_all(&n.to_le_bytes())?;
                    }
                    self.out.clear();
                    for k in self.cursor..end {
                        let old = self.order[k];
                        self.out.extend_from_slice(&store.frn[old as usize].to_le_bytes());
                    }
                    self.flush_out()?;
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::Parent;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Parent => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    if self.cursor == 0 {
                        let n = self.order.len() as u64 * 4;
                        self.file_mut()?.write_all(&n.to_le_bytes())?;
                    }
                    self.out.clear();
                    for k in self.cursor..end {
                        let old = self.order[k];
                        let p = store.parent[old as usize];
                        let mut new_p = if p == NO_PARENT {
                            NO_PARENT
                        } else {
                            *self.remap.get(p as usize).unwrap_or(&NO_PARENT)
                        };
                        if let Ok(j) = self
                            .parent_override
                            .binary_search_by_key(&(k as u32), |&(id, _)| id)
                        {
                            new_p = self.parent_override[j].1;
                        }
                        self.out.extend_from_slice(&new_p.to_le_bytes());
                    }
                    self.flush_out()?;
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::Attrs;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Attrs => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    if self.cursor == 0 {
                        let n = self.order.len() as u64 * 2;
                        self.file_mut()?.write_all(&n.to_le_bytes())?;
                    }
                    self.out.clear();
                    for k in self.cursor..end {
                        let old = self.order[k];
                        // DEAD/NOISE/SEEN 都是内存态:可见 slot 本就不带 DEAD/NOISE,
                        // SEEN 只在对账窗口存在(保存被禁止),这里统一剥掉兜底。
                        let a = store.attrs[old as usize] & !(ATTR_INTERNAL | ATTR_SEEN);
                        self.out.extend_from_slice(&a.to_le_bytes());
                    }
                    self.flush_out()?;
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::Mtime;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Mtime => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    if self.cursor == 0 {
                        let n = self.order.len() as u64 * 4;
                        self.file_mut()?.write_all(&n.to_le_bytes())?;
                    }
                    self.out.clear();
                    for k in self.cursor..end {
                        let old = self.order[k];
                        self.out
                            .extend_from_slice(&store.mtime[old as usize].to_le_bytes());
                    }
                    self.flush_out()?;
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::Size;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Size => {
                    let end = (self.cursor + SAVE_CHUNK_ELEMS).min(self.order.len());
                    if self.cursor == 0 {
                        let n = self.order.len() as u64 * 8;
                        self.file_mut()?.write_all(&n.to_le_bytes())?;
                    }
                    self.out.clear();
                    for k in self.cursor..end {
                        let old = self.order[k];
                        self.out
                            .extend_from_slice(&store.size[old as usize].to_le_bytes());
                    }
                    self.flush_out()?;
                    self.cursor = end;
                    if self.cursor >= self.order.len() {
                        self.phase = SavePhase::UnresolvedWrite;
                        self.cursor = 0;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::UnresolvedWrite => {
                    if self.cursor == 0 {
                        let n = self.unresolved.len() as u64;
                        self.file_mut()?.write_all(&n.to_le_bytes())?;
                    }
                    let end =
                        (self.cursor + SAVE_CHUNK_ELEMS).min(self.unresolved.len());
                    self.out.clear();
                    for k in self.cursor..end {
                        let (slot, parent_frn) = self.unresolved[k];
                        self.out.extend_from_slice(&slot.to_le_bytes());
                        self.out.extend_from_slice(&parent_frn.to_le_bytes());
                    }
                    self.flush_out()?;
                    self.cursor = end;
                    if self.cursor >= self.unresolved.len() {
                        self.phase = SavePhase::Finish;
                    }
                    if self.pause(step_started, budget, yield_now) {
                        return Ok(None);
                    }
                }
                SavePhase::Finish => {
                    self.flush_out()?;
                    let f = self.file.take().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "snapshot save: missing file at finish",
                        )
                    })?;
                    // 慢盘 fsync 可能数百 ms:移到独立线程,引擎继续服务查询。
                    self.sync_thread = Some(
                        std::thread::Builder::new()
                            .name("lstr-snapshot-fsync".to_string())
                            .spawn(move || f.sync_all())
                            .map_err(|e| {
                                std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    format!("spawn fsync thread: {e}"),
                                )
                            })?,
                    );
                    self.phase = SavePhase::SyncWait;
                }
                SavePhase::SyncWait => {
                    let Some(handle) = self.sync_thread.as_ref() else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "snapshot save: missing fsync thread",
                        ));
                    };
                    if !handle.is_finished() {
                        // 不忙等:1ms 粒度让出,宿主每轮回来检查;查询优先。
                        std::thread::sleep(Duration::from_millis(1));
                        return Ok(None);
                    }
                    let handle = self.sync_thread.take().expect("fsync thread");
                    match handle.join() {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => return Err(err),
                        Err(_) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                "snapshot fsync thread panicked",
                            ))
                        }
                    }
                    std::fs::rename(&self.tmp_path, &self.final_path)?;
                    if let Some(parent) = self.final_path.parent() {
                        if !crate::win::flush_dir(parent) {
                            debug_log(&format!(
                                "snapshot: flush_dir {} failed, dir entry may be non-durable",
                                parent.display()
                            ));
                        }
                    }
                    self.finished = true;
                    let bytes = std::fs::metadata(&self.final_path)?.len();
                    return Ok(Some(SaveReport { bytes, compacted: false }));
                }
            }
        }
    }
}

impl Drop for SaveTask {
    fn drop(&mut self) {
        // 未完成(失败/被控制消息中止/进程正常退出):删掉半成品 tmp。
        // 进程被杀不会走到这里,由 load_snapshots 的 `.tmp` 清理兜底。
        if !self.finished {
            self.file.take();
            if let Some(handle) = self.sync_thread.take() {
                let _ = handle.join();
            }
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

/// 一次性(同步)跑完分段保存状态机:同一套列视图/写盘代码,不做步间让路。
/// 用于 flush_all 与测试对拍(分段 vs 一次性的字节必须一致)。
pub(crate) fn save_view_sync(
    dir: &Path,
    header: &SnapshotHeader,
    store: &VolumeStore,
) -> std::io::Result<SaveReport> {
    let mut task = SaveTask::new(dir, header);
    loop {
        match task.advance(store, Duration::MAX, &mut || false) {
            SaveStep::Pending => continue,
            SaveStep::Done(report) => return Ok(report),
            SaveStep::Failed(err) => return Err(err),
        }
    }
}

pub struct LoadedSnapshot {
    pub header: SnapshotHeader,
    pub store: VolumeStore,
}

/// 只读内存映射文件(快照冷启动加载用)。相比 read_to_end 的整卷私有缓冲,
/// 映射走页缓存、按需调入,峰值私有内存约减半且更快;失败时调用方回落整读。
/// 映射对象持有文件引用,File 句柄可先关;Drop 里 unmap + 关映射句柄。
struct MappedFile {
    view: windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    mapping: windows::Win32::Foundation::HANDLE,
    len: usize,
}

impl MappedFile {
    fn open(path: &Path) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY,
        };
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        if len == 0 {
            return None;
        }
        let h = HANDLE(file.as_raw_handle() as *mut _);
        unsafe {
            let mapping =
                CreateFileMappingW(h, None, PAGE_READONLY, 0, 0, windows::core::PCWSTR::null())
                    .ok()?;
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(mapping);
                return None;
            }
            Some(Self { view, mapping, len })
        }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Memory::UnmapViewOfFile(self.view);
            let _ = windows::Win32::Foundation::CloseHandle(self.mapping);
        }
    }
}

/// 快照字节来源:优先内存映射,回落 owned 整读。解析只读 &[u8],列拷贝为 owned。
enum FileBytes {
    Mapped(MappedFile),
    Owned(Vec<u8>),
}

impl FileBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            FileBytes::Mapped(m) => m.as_slice(),
            FileBytes::Owned(v) => v,
        }
    }
}

/// 读快照;任何不一致(魔数/版本/长度)返回 None,由调用方触发全量重扫。
pub fn load(dir: &Path, guid: u128, noise_hash: u64, exclude: &crate::noise::ExcludeRules) -> Option<LoadedSnapshot> {
    let path = snapshot_path(dir, guid);
    // 优先 mmap(降低冷启动峰值私有内存并加速);映射失败回落整读。
    let bytes = match MappedFile::open(&path) {
        Some(m) => FileBytes::Mapped(m),
        None => match std::fs::read(&path) {
            Ok(v) => FileBytes::Owned(v),
            Err(err) => {
                debug_log(&format!("load {guid:032x}: read failed: {err}"));
                return None;
            }
        },
    };
    let mut cur = bytes.as_slice();

    if cur.len() < 4 + 4 || &cur[0..4] != MAGIC {
        debug_log(&format!("load {guid:032x}: bad magic/short ({} bytes)", cur.len()));
        return None;
    }
    cur = &cur[4..];
    let Some(version) = u32::from_le_array(&mut cur) else {
        debug_log(&format!("load {guid:032x}: truncated at version"));
        return None;
    };
    if version != VERSION {
        debug_log(&format!(
            "load {guid:032x}: version {version} != {VERSION}, stale snapshot removed (full rescan)"
        ));
        // 旧格式不做迁移:删掉它,避免每次启动都尝试加载;新扫描会重写。
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let Some(guid_h) = u128::from_le_array(&mut cur) else {
        debug_log(&format!("load {guid:032x}: truncated at guid"));
        return None;
    };
    if guid_h != guid {
        debug_log(&format!("load {guid:032x}: guid mismatch header={guid_h:032x}"));
        return None;
    }
    let letter = char::from_u32(*cur.first()? as u32)?;
    cur = &cur[1..];
    let flags = *cur.first()?;
    cur = &cur[1..];
    let journal_id = u64::from_le_array(&mut cur)?;
    let next_usn = i64::from_le_array(&mut cur)?;
    let resume_frn = u64::from_le_array(&mut cur)?;
    let est_records = u64::from_le_array(&mut cur)?;
    let stored_noise_hash = u64::from_le_array(&mut cur)?;
    if stored_noise_hash != noise_hash {
        debug_log(&format!(
            "load {guid:032x}: noise hash snapshot={:016x} cfg={:016x}, stale (full rescan)",
            stored_noise_hash, noise_hash
        ));
        return None;
    }
    let count = u64::from_le_array(&mut cur)? as usize;

    let names = read_blob(&mut cur)?;
    let offs = read_col_u32(&mut cur)?;
    let frn = read_col_u64(&mut cur)?;
    let parent = read_col_u32(&mut cur)?;
    let attrs = read_col_u16(&mut cur)?;
    let mtime = read_col_u32(&mut cur)?;
    let size = read_col_u64(&mut cur)?;
    let n_unresolved = u64::from_le_array(&mut cur)? as usize;
    let mut unresolved: Vec<(u32, u64)> = Vec::with_capacity(n_unresolved.min(1 << 20));
    for _ in 0..n_unresolved {
        let slot = u32::from_le_array(&mut cur)?;
        let parent_frn = u64::from_le_array(&mut cur)?;
        unresolved.push((slot, parent_frn));
    }
    if !cur.is_empty() {
        return None;
    }
    if offs.len() != count
        || frn.len() != count
        || parent.len() != count
        || attrs.len() != count
        || mtime.len() != count
        || size.len() != count
    {
        debug_log(&format!(
            "load {guid:032x}: column len mismatch offs={} frn={} parent={} attrs={} mtime={} size={} count={count}",
            offs.len(), frn.len(), parent.len(), attrs.len(), mtime.len(), size.len()
        ));
        return None;
    }
    if n_unresolved > count
        || unresolved.iter().any(|&(slot, p)| slot as usize >= count || p == 0)
    {
        debug_log(&format!(
            "load {guid:032x}: invalid unresolved table (n={n_unresolved} count={count})"
        ));
        return None;
    }
    // 索引列必须落在 name 区/槽位范围内,否则后续 name(slot)/parent 追踪会越界。
    // (offs 的元素还应是名字起点;由 save 先 compact 保证,这里只做廉价边界校验。)
    if offs.iter().any(|&o| o as usize >= names.len()) {
        debug_log(&format!("load {guid:032x}: offs out of range (names={})", names.len()));
        return None;
    }
    if parent.iter().any(|&p| p != NO_PARENT && p as usize >= count) {
        debug_log(&format!("load {guid:032x}: parent out of range (count={count})"));
        return None;
    }
    // name_off 由 names 重建(\0 结尾 → 偏移即条目起点)
    let mut name_off: Vec<u32> = Vec::with_capacity(count);
    let mut off = 0usize;
    while off < names.len() {
        name_off.push(off as u32);
        match names[off..].iter().position(|&b| b == 0) {
            Some(p) => off += p + 1,
            None => {
                debug_log(&format!(
                    "load {guid:032x}: unterminated name at offset {off} (names={} count={count})",
                    names.len()
                ));
                return None;
            }
        }
    }
    if name_off.len() != count {
        debug_log(&format!(
            "load {guid:032x}: name_off {} != count {count}",
            name_off.len()
        ));
        return None;
    }
    let store = VolumeStore::from_parts(
        names,
        offs,
        frn,
        parent,
        attrs,
        mtime,
        size,
        name_off,
        unresolved,
        exclude.clone(),
        letter,
    );
    Some(LoadedSnapshot {
        header: SnapshotHeader {
            guid,
            letter,
            complete: flags & FLAG_COMPLETE != 0,
            noise_hash: stored_noise_hash,
            journal_id,
            next_usn,
            resume_frn,
            est_records,
        },
        store,
    })
}

trait LeRead: Sized {
    fn from_le_array(cur: &mut &[u8]) -> Option<Self>;
}
macro_rules! le_read {
    ($($t:ty),*) => {
        $(impl LeRead for $t {
            fn from_le_array(cur: &mut &[u8]) -> Option<Self> {
                let n = std::mem::size_of::<$t>();
                if cur.len() < n {
                    return None;
                }
                let v = <$t>::from_le_bytes(cur[..n].try_into().unwrap());
                *cur = &cur[n..];
                Some(v)
            }
        })*
    };
}
le_read!(u16, u32, u64, i64, u128);

fn read_blob(cur: &mut &[u8]) -> Option<Vec<u8>> {
    let len = u64::from_le_array(cur)? as usize;
    if cur.len() < len {
        return None;
    }
    let v = cur[..len].to_vec();
    *cur = &cur[len..];
    Some(v)
}

fn read_col_u32(cur: &mut &[u8]) -> Option<Vec<u32>> {
    let bytes = read_blob(cur)?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

fn read_col_u64(cur: &mut &[u8]) -> Option<Vec<u64>> {
    let bytes = read_blob(cur)?;
    if bytes.len() % 8 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

fn read_col_u16(cur: &mut &[u8]) -> Option<Vec<u16>> {
    let bytes = read_blob(cur)?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QuerySpec;
    use crate::store::VolumeStore;

    /// 单测旁路:未设 LSTR_INDEX_LOG_DIR 时 debug_log 不得写真实
    /// %APPDATA%\lstr\index\engine.log(引擎/snapshot 单测高频经过,
    /// 每次 cargo test 都污染真实日志会淹没线上排障依据)。
    #[test]
    fn debug_log_is_bypassed_in_tests() {
        let marker = format!("debug_log-bypass-marker-{}", std::process::id());
        debug_log(&marker);
        // 无论覆盖变量是否恰好被并行用例设置,真实日志都不得出现 marker:
        // 覆盖激活时 marker 落在覆盖目录,未激活时整体旁路。
        let Ok(base) = std::env::var("APPDATA") else { return };
        let real = PathBuf::from(base).join("lstr").join("index").join("engine.log");
        if let Ok(content) = std::fs::read_to_string(real) {
            assert!(!content.contains(&marker), "单元测试不得写真实 engine.log");
        }
    }

    /// 覆盖变量生效:设了 LSTR_INDEX_LOG_DIR 时写到覆盖目录(集成测试隔离用)。
    #[test]
    fn debug_log_honors_override_dir() {
        let dir = std::env::temp_dir().join(format!("lstr-log-override-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var(LOG_DIR_ENV, &dir);
        debug_log("hello-override");
        let content = std::fs::read_to_string(dir.join("engine.log")).unwrap();
        assert!(content.contains("hello-override"));
        // 先撤变量再清目录:并行的其他测试线程在变量撤掉后即回旁路,
        // 不会往已删除目录里重建文件。
        std::env::remove_var(LOG_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_rotates_when_oversized() {
        // 直接测轮转纯函数,不依赖全局 LOG_DIR_ENV(避免并行测试竞态)。
        let dir = std::env::temp_dir().join(format!("lstr-log-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.log");
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        rotate_if_oversized(&path);
        assert!(dir.join("engine.log.1").exists(), "超限日志应轮转为 .1");
        assert!(!path.exists(), "原日志已轮转走");
        // 小日志不触发轮转
        std::fs::write(&path, b"small").unwrap();
        rotate_if_oversized(&path);
        assert_eq!(std::fs::read(&path).unwrap(), b"small");
        // 再次超限:旧 .1 被覆盖(Windows rename 不覆盖,需先删)
        std::fs::write(&path, vec![b'y'; MAX_LOG_BYTES as usize]).unwrap();
        rotate_if_oversized(&path);
        assert!(dir.join("engine.log.1").exists());
        assert_eq!(std::fs::metadata(dir.join("engine.log.1")).unwrap().len(), MAX_LOG_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut s = VolumeStore::new(
        crate::noise::ExcludeRules::build(&crate::noise::builtin_patterns()),
        'C',
    );
        s.insert(5, 0, 0x10, b"root");
        s.insert_meta(10, 5, 0, b"alpha.txt", 1_700_000_123, 12_345);
        s.insert(11, 5, 0, "报告.docx".as_bytes());
        s.insert(12, 5, 0x10, b"node_modules");
        s.insert(13, 12, 0, b"dep.txt");
        s.compact();
        let header = SnapshotHeader {
            guid: 0x1234_5678_9abc_def0_1122_3344,
            letter: 'C',
            complete: true,
            noise_hash: 0xA11CE,
            journal_id: 0xfeed,
            next_usn: 0x42,
            resume_frn: 0,
            est_records: 999,
        };
        let dir = std::env::temp_dir().join(format!("lstr-idx-test-{}", std::process::id()));
        let size = save(&dir, &header, &mut s).unwrap();
        assert!(size > 0);
        let loaded = load(&dir, header.guid, 0xA11CE, &Default::default()).unwrap();
        // v5:排除表内容哈希与快照生成时不符 → 拒绝加载(触发全量重扫),
        // 否则改排除规则后旧快照里的 NOISE 位会让结果与设置页承诺不一致。
        assert!(load(&dir, header.guid, 0xB0B, &Default::default()).is_none());
        assert_eq!(loaded.header.journal_id, 0xfeed);
        assert_eq!(loaded.header.next_usn, 0x42);
        assert!(loaded.header.complete);
        let ls = &loaded.store;
        assert_eq!(ls.name_off.len(), 4); // A-2:node_modules 子孙被排除,目录自身保留
        assert!(ls.find_slot(10).is_some());
        let slot = ls.find_slot(10).unwrap();
        assert_eq!(ls.name(slot), b"alpha.txt");
        // v4:mtime 列落盘/读回(0=未知)。v6:size 列落盘/读回(MAX=未知)。
        assert_eq!(ls.mtime[slot as usize], 1_700_000_123);
        assert_eq!(ls.size[slot as usize], 12_345);
        let unknown = ls.find_slot(11).unwrap();
        assert_eq!(ls.mtime[unknown as usize], 0);
        assert_eq!(ls.size[unknown as usize], u64::MAX);
        // 加载(from_parts)必须按 mtime/size 列重建待回填队列,否则重启后
        // USN 增量条目仍永久过不了时间/大小筛选。
        assert!(ls.mtime_pending.contains(&unknown), "mtime=0 条目应待回填");
        assert!(!ls.mtime_pending.contains(&slot), "已知 mtime 不登记");
        assert!(ls.size_pending.contains(&unknown), "size=MAX 条目应待回填");
        assert!(!ls.size_pending.contains(&slot), "已知 size 不登记");
        assert_eq!(ls.char_mask.len(), ls.name_off.len());
        assert_eq!(ls.offs.len(), ls.name_off.len());
        // 有序性保持
        let names: Vec<&[u8]> = ls.offs.iter().map(|&x| ls.name_lc(x)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v3:未消解父链接落盘。旧实现 load 后 unresolved 为空,父项永远等不到、
    /// 子项路径永久退化为 NO_PARENT,只能靠一次全量重扫碰运气。
    #[test]
    fn unresolved_parents_survive_roundtrip() {
        let dir = tmpdir("unres");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        // 子项先到、父 FRN 99 未到 → 进 unresolved
        s.insert(20, 99, 0, b"orphan.txt");
        s.compact();
        assert_eq!(s.unresolved_pairs().len(), 1);
        let h = header(0x1234, false);
        save(&dir, &h, &mut s).unwrap();
        let mut ls = load(&dir, h.guid, 0, &Default::default()).expect("load").store;
        assert_eq!(ls.unresolved_pairs().len(), 1);
        // 父项在后续 journal 事件里补到 → compact 消解,不需要全量重扫
        ls.insert(99, 5, 0x10, b"late-dir");
        ls.compact();
        assert!(ls.unresolved_pairs().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lstr-idx-{tag}-{}", std::process::id()))
    }

    fn header(guid: u128, complete: bool) -> SnapshotHeader {
        SnapshotHeader {
            guid,
            letter: 'C',
            complete,
            // 这些用例的 store 都是空规则表,load 也传同值哈希;
            // 指纹不一致会按设计拒绝(v5 起排除表内容入指纹)。
            noise_hash: 0,
            journal_id: 7,
            next_usn: 900,
            resume_frn: 0,
            est_records: 0,
        }
    }

    /// H1 回归:compact → insert×N(含就地改名)→ save → load,
    /// 计数/FRN 定位/名字查询/继续插入/再次 compact 全部自洽。
    ///
    /// 旧实现 save 按 `name_off.len()` 写 count 却落未 compact 的 offs,
    /// 增量 insert 后 load 因 `offs.len() != count` 永久拒绝。
    #[test]
    fn incremental_state_survives_roundtrip() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.compact();
        for i in 0..5u64 {
            s.insert(100 + i, 5, 0, format!("delta{i}.txt").as_bytes());
        }
        s.insert(10, 5, 0, b"alpha-renamed.txt"); // 就地改名:base 名序失效
        let n = s.len();

        let dir = tmpdir("h1-incr");
        let _ = std::fs::remove_dir_all(&dir);
        let h = header(0xabc, true);
        save(&dir, &h, &mut s).unwrap();
        let loaded = load(&dir, h.guid, 0, &Default::default()).unwrap();
        let mut ls = loaded.store;
        assert!(loaded.header.complete);
        assert_eq!(ls.len(), n);
        // save 接管列一致契约:delta/改名已归并,offs 覆盖全部 slot
        assert_eq!(ls.offs.len(), ls.len());
        assert_eq!(
            ls.find_slot(10).map(|x| ls.name(x).to_vec()),
            Some(b"alpha-renamed.txt".to_vec())
        );
        assert_eq!(
            ls.find_slot(103).map(|x| ls.name(x).to_vec()),
            Some(b"delta3.txt".to_vec())
        );

        // 名字查询:子串与精确都必须命中
        let spec = QuerySpec { tokens: vec!["delta3".into()], ..Default::default() };
        let (slots, total, _) = crate::query::query_volume(&ls, &spec, 100);
        assert_eq!(total, 1);
        assert_eq!(ls.name(slots[0]), b"delta3.txt");
        let spec = QuerySpec {
            tokens: vec!["delta4.txt".into()],
            exact: Some("delta4.txt".into()),
            ..Default::default()
        };
        let (_, total, _) = crate::query::query_volume(&ls, &spec, 100);
        assert_eq!(total, 1);

        // 再插入不 panic,FRN 定位可用;compact 后恢复全覆盖 offs
        assert!(ls.insert(200, 5, 0, b"after-load.txt"));
        assert!(ls.find_slot(200).is_some());
        ls.insert(10, 5, 0, b"alpha.txt");
        ls.compact();
        assert_eq!(ls.offs.len(), ls.len());
        let names: Vec<&[u8]> = ls.offs.iter().map(|&x| ls.name_lc(x)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H1 回归(改名无 delta):compact 后只做一次就地改名,delta 为空。
    /// 旧实现 save 的 compact 条件只看 delta/scanning → 不 compact 就落盘,
    /// names 里的幽灵旧名使 name_off 条目数 != count,load 拒绝 → 启动全量重扫
    /// (property 随机流约 1/4 概率踩中,表现为偶发 load=None)。
    #[test]
    fn rename_without_delta_survives_roundtrip() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.compact();
        assert!(s.delta_is_empty());
        assert!(s.insert(10, 5, 0, b"alpha-renamed.txt"));
        assert!(s.delta_is_empty(), "就地改名不进 delta");
        assert!(s.has_pending_rename());
        let dir = tmpdir("h1-rename-no-delta");
        let _ = std::fs::remove_dir_all(&dir);
        let h = header(0xfeed, true);
        save(&dir, &h, &mut s).unwrap();
        let loaded = load(&dir, h.guid, 0, &Default::default()).expect("改名后落盘的快照必须可 load");
        let ls = loaded.store;
        assert_eq!(ls.len(), 2);
        assert_eq!(
            ls.find_slot(10).map(|x| ls.name(x).to_vec()),
            Some(b"alpha-renamed.txt".to_vec())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H1 回归:扫描中断的部分快照必须可落盘/可 load。
    ///
    /// ZCode 方案的语义:save 时先归并(compact),partial 快照被归一化成
    /// ready 列布局;header.complete 仍为 false,重启握手拿不到增量位点,
    /// worker 走全量重扫,已有 entry 按 FRN 去重。因此不需要还原
    /// `is_scanning` 本身,但 count/名字/父链必须完整可用,后续 append 不坏。
    #[test]
    fn partial_scan_snapshot_roundtrip() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0x10, b"users");
        s.insert(11, 10, 0, b"a.txt");
        assert!(s.is_scanning());
        let dir = tmpdir("h1-partial");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = header(0xdef, false);
        h.resume_frn = 1234;
        save(&dir, &h, &mut s).unwrap();
        let loaded = load(&dir, h.guid, 0, &Default::default()).unwrap();
        assert!(!loaded.header.complete);
        assert_eq!(loaded.header.resume_frn, 1234);
        let mut ls = loaded.store;
        // save-time 归一秒变 ready 布局(旧实现这里 offs 为空 → load 拒绝)
        assert!(!ls.is_scanning());
        assert_eq!(ls.len(), 3);
        assert_eq!(ls.offs.len(), ls.len());
        assert_eq!(ls.name(ls.find_slot(10).unwrap()), b"users");
        // 继续 append:目录索引仍在,子项父链可解析
        assert!(ls.insert(12, 10, 0, b"b.txt"));
        assert_eq!(ls.len(), 4);
        let b = ls.len() - 1;
        assert_eq!(ls.parent[b as usize], ls.find_slot(10).unwrap());
        ls.compact();
        assert_eq!(ls.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// save → load → insert → save → load:两轮往返后状态仍一致。
    #[test]
    fn snapshot_roundtrip_twice() {
        let dir = tmpdir("h1-twice");
        let _ = std::fs::remove_dir_all(&dir);
        let guid = 0x5150;
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"one.txt");
        s.compact();
        save(&dir, &header(guid, true), &mut s).unwrap();
        let mut s = load(&dir, guid, 0, &Default::default()).unwrap().store;
        s.insert(11, 5, 0, b"two.txt");
        save(&dir, &header(guid, true), &mut s).unwrap();
        let ls = load(&dir, guid, 0, &Default::default()).unwrap().store;
        assert_eq!(ls.len(), 3);
        assert_eq!(
            ls.find_slot(11).map(|x| ls.name(x).to_vec()),
            Some(b"two.txt".to_vec())
        );
        assert_eq!(
            ls.find_slot(10).map(|x| ls.name(x).to_vec()),
            Some(b"one.txt".to_vec())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1 等旧格式不迁移:load 拒绝并删除文件,避免每次启动重复尝试。
    #[test]
    fn stale_version_snapshot_is_removed() {
        let dir = tmpdir("stale-ver");
        let _ = std::fs::remove_dir_all(&dir);
        let guid = 0x7777;
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"a.txt");
        s.compact();
        let h = header(guid, true);
        save(&dir, &h, &mut s).unwrap();
        let p = snapshot_path(&dir, guid);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes()); // 伪造成 v1
        std::fs::write(&p, &bytes).unwrap();
        assert!(load(&dir, guid, 0, &Default::default()).is_none());
        assert!(!p.exists(), "旧版本快照加载失败后应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_garbage() {        let dir = std::env::temp_dir().join(format!("lstr-idx-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let guid = 42u128;
        let p = snapshot_path(&dir, guid);
        std::fs::write(&p, b"garbage-data-here").unwrap();
        assert!(load(&dir, guid, 0, &Default::default()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分段保存的列视图测试数据:base compact 后叠加 delta、就地改名、
    /// 删除、未消解父链接 —— 覆盖保存视图需要重建的全部状态。
    fn view_store() -> VolumeStore {
        let mut s = VolumeStore::new(
        crate::noise::ExcludeRules::build(&crate::noise::builtin_patterns()),
        'C',
    );
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0x10, b"docs");
        s.insert(11, 10, 0, b"alpha.txt");
        s.insert(12, 10, 0, b"beta.txt");
        s.insert(13, 5, 0x10, b"media");
        s.compact();
        // 就地改名(base slot,offs 里的旧位置失效)
        s.insert(11, 10, 0, b"alpha-renamed.txt");
        // 删除(base slot → DEAD,不得进快照)
        s.remove(12);
        // delta 新 slot
        s.insert(100, 10, 0, b"gamma.txt");
        s.insert(101, 13, 0, b"movie.mkv");
        // 父项尚未到达 → unresolved 必须原样落盘
        s.insert(200, 999, 0, b"orphan.txt");
        // 子项先到、父项随后到达:仍是 unresolved 记录,但保存视图可在父列
        // 直接写正确链接(与 compact 的解析结果一致)。
        s.insert(301, 300, 0, b"late-child.txt");
        s.insert(300, 5, 0x10, b"late-parent");
        s
    }

    fn view_header(guid: u128) -> SnapshotHeader {
        SnapshotHeader {
            guid,
            letter: 'V',
            complete: true,
            noise_hash: 0,
            journal_id: 0xabc,
            next_usn: 12345,
            resume_frn: 0,
            est_records: 7,
        }
    }

    /// 分段保存与一次性保存必须逐字节一致(同一 store、同一列视图)。
    #[test]
    fn segmented_save_is_byte_identical_to_one_shot() {
        let store = view_store();
        let h = view_header(0x5e6);
        let dir_one = tmpdir("seg-one");
        let dir_seg = tmpdir("seg-chunked");
        let _ = std::fs::remove_dir_all(&dir_one);
        let _ = std::fs::remove_dir_all(&dir_seg);
        save_view_sync(&dir_one, &h, &store).unwrap();
        let one = std::fs::read(snapshot_path(&dir_one, h.guid)).unwrap();

        let mut task = SaveTask::new(&dir_seg, &h);
        let mut calls = 0usize;
        loop {
            // 每 3 次检查让路一次(模拟查询到达),步间恢复续写。
            let mut yield_now = || {
                calls += 1;
                calls % 3 == 0
            };
            match task.advance(&store, Duration::from_micros(50), &mut yield_now) {
                SaveStep::Pending => continue,
                SaveStep::Done(_) => break,
                SaveStep::Failed(err) => panic!("segmented save failed: {err}"),
            }
        }
        let seg = std::fs::read(snapshot_path(&dir_seg, h.guid)).unwrap();
        assert_eq!(one, seg, "分段保存文件必须与一次性保存字节级一致");
        assert!(!task.tmp_path.exists(), "完成后 tmp 必须已 rename");

        // 视图语义:改名生效、删除项不落盘、delta/unresolved 可恢复。
        let loaded = load(&dir_one, h.guid, 0, &Default::default()).unwrap();
        let ls = &loaded.store;
        assert_eq!(ls.len(), 9, "9 个活项(删除项不落盘)");
        assert_eq!(ls.find_slot(11).map(|s| ls.name(s).to_vec()), Some(b"alpha-renamed.txt".to_vec()));
        assert!(ls.find_slot(12).is_none(), "DEAD 条目不得进快照");
        assert!(ls.find_slot(100).is_some());
        let child = ls.find_slot(301).unwrap();
        let parent = ls.find_slot(300).unwrap();
        assert_eq!(ls.parent[child as usize], parent, "已到达的父项应在视图里解析");
        assert_eq!(ls.unresolved_pairs().len(), 1, "仍未消解的父链接必须落盘");
        let names: Vec<&[u8]> = ls.offs.iter().map(|&s| ls.name_lc(s)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "offs 必须按小写名有序");
        let _ = std::fs::remove_dir_all(&dir_one);
        let _ = std::fs::remove_dir_all(&dir_seg);
    }

    /// 噪声排除(A-2):噪声目录自身保留进快照(可搜),但其子孙 NOISE slot
    /// 不得进快照;compact 后迟到的噪声子树子项(靠锚点立即判噪)同样必须被视图过滤。
    #[test]
    fn save_view_respects_noise_bits() {
        let mut s = VolumeStore::new(
        crate::noise::ExcludeRules::build(&crate::noise::builtin_patterns()),
        'C',
    );
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0x10, b"node_modules");
        s.insert(11, 10, 0, b"dep.js");
        s.insert(12, 5, 0, b"keep.txt");
        s.compact();
        // compact 已剔除噪声子孙,目录自身与锚点保留;迟到子项立即判噪。
        s.insert(13, 10, 0, b"late.js");
        assert!(!s.visible(s.find_slot(13).unwrap()), "迟到子项应判噪");
        let h = view_header(0x0e5);
        let dir = tmpdir("seg-noise");
        let _ = std::fs::remove_dir_all(&dir);
        save_view_sync(&dir, &h, &s).unwrap();
        let loaded = load(&dir, h.guid, 0, &Default::default()).expect("view snapshot loads");
        assert_eq!(loaded.store.find_slot(12).map(|x| loaded.store.name(x).to_vec()), Some(b"keep.txt".to_vec()));
        assert!(loaded.store.find_slot(10).is_some(), "噪声目录自身保留进快照");
        assert!(loaded.store.find_slot(11).is_none());
        assert!(loaded.store.find_slot(13).is_none(), "迟到的噪声子项不得进快照");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 随机状态(新增/改名/删除/未消解父链/中途 compact)下,保存视图与
    /// compact 后的整存语义一致:可见集合、FRN→名字、offs 有序、父列边界。
    #[test]
    fn save_view_matches_compacted_state_on_random_ops() {
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                self.0
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() >> 33) as usize % n
            }
        }
        let mut rng = Lcg(0x5eed_1234_abcd);
        let mut store = VolumeStore::new(Default::default(), 'C');
        store.insert(5, 0, 0x10, b"root");
        let mut frns: Vec<u64> = vec![5];
        let mut next_frn = 10u64;
        let mut next_parent = 90_000u64; // 只用于制造未消解父链
        for step in 0..3000usize {
            match rng.below(10) {
                0 => {
                    let frn = next_frn;
                    next_frn += 1;
                    let parent = if rng.below(8) == 0 {
                        next_parent += 1;
                        next_parent
                    } else {
                        frns[rng.below(frns.len())]
                    };
                    let name = if rng.below(20) == 0 {
                        format!("文件{step}.txt")
                    } else {
                        format!("file{step}.bin")
                    };
                    store.insert(frn, parent, 0, name.as_bytes());
                    frns.push(frn);
                }
                1 => {
                    // 改名/更新已有条目
                    let frn = frns[rng.below(frns.len())];
                    store.insert(frn, frns[rng.below(frns.len())], 0, format!("renamed{step}.txt").as_bytes());
                }
                2 => {
                    let frn = frns[rng.below(frns.len())];
                    store.remove(frn);
                }
                3 => {
                    let frn = next_frn;
                    next_frn += 1;
                    store.insert(frn, 5, 0x10, format!("dir{step}").as_bytes());
                    frns.push(frn);
                }
                _ => {
                    // 高频短串名,制造就地改名与名字区幽灵
                    let frn = frns[rng.below(frns.len())];
                    store.insert(frn, 5, 0, format!("a{}", step % 37).as_bytes());
                }
            }
            if step % 250 == 249 {
                store.compact();
            }
        }
        // 末尾至少留 delta/改名的视图态
        store.insert(next_frn, 5, 0, b"tail-delta.txt");
        let h = view_header(0x7a7);
        let dir = tmpdir("seg-random");
        let _ = std::fs::remove_dir_all(&dir);
        save_view_sync(&dir, &h, &store).unwrap();
        let loaded = load(&dir, h.guid, 0, &Default::default()).expect("view snapshot loads");
        let ls = &loaded.store;
        let live: Vec<u64> = (0..store.len() as u32)
            .filter(|&s| store.visible(s))
            .map(|s| store.frn[s as usize])
            .collect();
        assert_eq!(ls.len(), live.len(), "可见集合条数一致");
        for frn in &live {
            let a = store.find_slot(*frn).unwrap();
            let b = ls.find_slot(*frn).expect("visible FRN must load");
            assert_eq!(ls.name(b), store.name(a), "FRN={frn} 名字一致");
        }
        let names: Vec<&[u8]> = ls.offs.iter().map(|&s| ls.name_lc(s)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "offs 必须按小写名有序");
        assert!(ls
            .parent
            .iter()
            .all(|&p| p == NO_PARENT || (p as usize) < ls.len()));
        assert!(ls.offs.iter().all(|&s| (s as usize) < ls.len()));

        // 再把同一状态 compact 后整存:可见集合/名字必须与视图完全一致。
        store.compact();
        let dir2 = tmpdir("seg-random-compact");
        let _ = std::fs::remove_dir_all(&dir2);
        save_view_sync(&dir2, &h, &store).unwrap();
        let old = load(&dir2, h.guid, 0, &Default::default()).expect("compacted snapshot loads");
        assert_eq!(old.store.len(), live.len());
        for frn in &live {
            let a = store.find_slot(*frn).unwrap();
            let b = old.store.find_slot(*frn).expect("visible FRN must load");
            assert_eq!(old.store.name(b), store.name(a));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// 写完若干块后中断(只留半成品 tmp):最终文件不得出现;残留 tmp 由
    /// 下一次完整保存覆盖,快照可正常 load。
    #[test]
    fn segmented_save_interrupted_then_resumed() {
        let store = view_store();
        let h = view_header(0x9a9);
        let dir = tmpdir("seg-interrupt");
        let _ = std::fs::remove_dir_all(&dir);

        let mut task = SaveTask::new(&dir, &h);
        let tmp = task.tmp_path.clone();
        let mut steps = 0usize;
        loop {
            match task.advance(&store, Duration::from_micros(20), &mut || false) {
                SaveStep::Pending => {
                    steps += 1;
                    if steps >= 5 {
                        break;
                    }
                }
                SaveStep::Done(_) => panic!("save should not finish in 5 tiny steps"),
                SaveStep::Failed(err) => panic!("save failed: {err}"),
            }
        }
        assert!(tmp.exists(), "中断时必须留下半成品 tmp");
        let partial = std::fs::read(&tmp).unwrap();
        assert!(!partial.is_empty());
        assert!(!snapshot_path(&dir, h.guid).exists(), "中断不得生成正式快照");
        // 模拟进程被杀:不经过 Drop 清理,半成品 tmp 留在盘上。
        std::mem::forget(task);
        assert!(tmp.exists());

        // 恢复:新任务从零重写,覆盖残留 tmp,完成后可 load。
        let report = save_view_sync(&dir, &h, &store).unwrap();
        assert!(report.bytes > 0);
        assert!(!tmp.exists(), "完成后残留 tmp 必须消失");
        let loaded = load(&dir, h.guid, 0, &Default::default()).expect("resumed snapshot must load");
        assert_eq!(loaded.store.len(), 9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
