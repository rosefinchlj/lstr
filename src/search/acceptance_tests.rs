//! 验收表自动化版（2026-11-10 的 `LstrAcc` 夹具）。
//!
//! 用户按编号逐条手测过一张 30 行的搜索验收表，这组测试把其中
//! **能在无 GUI / 无 Everything / 无权限依赖下复现的行**固化成断言：
//! 输入字符串 → `query::parse` → `build_spec` → 内置索引引擎 → Entry 列表。
//! 夹具是测试自己创建的真实文件（挂在 target-opt 下），索引由真实
//! `Engine` 吃 `EngineMsg::Records` 建立，因此路径拼接、拼音缓存、
//! 扩展名大小写、时间筛选都走生产代码，而不是另写一套测试逻辑。
//!
//! 未覆盖的行（需要真实 GUI / 真机索引状态）：
//! - E21/E22 应用页：`src/search/apps.rs` 的 `search_hits_pinyin_initials`、
//!   `scattered_letters_do_not_match_english_names` 已覆盖；
//! - G26 空态技巧面板、G27 Esc 后再次唤出保留输入/结果、G29 预览面板
//!   的「修改」行：属于 egui 状态与布局，留在手工验收表。
//!
//! 夹具名以 `LstrAcc` 开头的唯一前缀是为了对齐验收表；测试目录带
//! 进程号，重复运行不会互相干扰。

use std::path::{Component, Path, PathBuf};

use lstr_index::{Engine, EngineConfig, EngineMsg, Op, Phase, Rec};

use crate::model::{Entry, Kind, Scope};
use crate::search::builtin::BuiltinProvider;
use crate::search::filters::{SearchFilters, TimeFilter};
use crate::search::{browse, query};

const DIR_ATTR: u32 = 0x10;

/// 每个夹具一个唯一目录名：测试并行运行时共用 `target-opt` 目录，
/// 只用进程号会让多个测试互相删掉对方的文件。
static FIXTURE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 真实文件夹具 + 已喂好的内置索引。
struct Fixture {
    provider: BuiltinProvider,
    /// 夹具根目录（含 4 子目录 + 7 文件 + 根文件），实际路径由测试临时创建。
    root: PathBuf,
    empty_dir: PathBuf,
    snap: PathBuf,
    letter: char,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.snap);
    }
}

fn disk_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 建与验收表同构的夹具并建立真实索引。
fn fixture() -> Option<Fixture> {
    let base = std::env::current_dir().ok()?;
    let Some(Component::Prefix(prefix)) = base.components().next() else {
        return None; // UNC 路径不做盘符相关的验收
    };
    let letter = prefix
        .as_os_str()
        .to_string_lossy()
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())?
        .to_ascii_uppercase();
    let rel: Vec<String> = base
        .components()
        .skip(1)
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    let tag = format!(
        "lstr-acc-{}-{}",
        std::process::id(),
        FIXTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let root = base.join("target-opt").join(&tag);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).ok()?;

    let dirs = vec![
        "LstrAcc-文档".to_string(),
        "LstrAcc-微信资料".to_string(),
        "LstrAcc-金太阳".to_string(),
        "LstrAcc-空目录".to_string(),
    ];
    for dir in &dirs {
        std::fs::create_dir_all(root.join(dir)).ok()?;
    }

    // (父目录, 文件名)
    let files = vec![
        ("LstrAcc-文档", "LstrAcc-全匹配.md"),
        ("LstrAcc-文档", "LstrAcc-扩展名测试.RS"),
        ("LstrAcc-文档", "LstrAcc-拖尾-2026.txt"),
        ("LstrAcc-文档", "LstrAcc-图片.png"),
        ("LstrAcc-微信资料", "LstrAcc-微信聊天-备份.txt"),
        ("LstrAcc-金太阳", "LstrAcc-配置.json"),
        (".", "LstrAcc-根文件.log"),
    ];
    for (parent, name) in &files {
        let path = if *parent == "." {
            root.join(name)
        } else {
            root.join(parent).join(name)
        };
        std::fs::write(&path, b"x").ok()?;
    }

    let snap = std::env::temp_dir().join(format!("lstr-acc-snap-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&snap);
    let mut engine = Engine::new(EngineConfig {
        index_dir: snap.clone(),
        exclude_noise: false,
        candidate_cap: 2000,
        volumes: Vec::new(),
    });
    let guid = 0xACC5u128;
    // 父链：盘符根 → 当前 crate 路径 → target-opt → 夹具根。路径拼接
    // 与 path: 前缀过滤都依赖这条链，必须和真实目录一一对应。
    let mut chain_names: Vec<String> = rel.clone();
    chain_names.push("target-opt".to_string());
    engine.handle(EngineMsg::VolumeMeta {
        guid,
        letter,
        journal_id: 7,
        next_usn: 100,
        est_records: 0,
    });
    engine.handle(EngineMsg::ScanBegin {
        guid,
        resume_frn: 0,
    });
    let mut recs = Vec::new();
    let mut parent = 0u64;
    let mut chain_disk = PathBuf::from(format!("{letter}:\\"));
    for (j, name) in chain_names.iter().enumerate() {
        if j < rel.len() {
            chain_disk.push(name);
        } else {
            chain_disk = base.join("target-opt");
        }
        let frn = 100 + j as u64;
        recs.push(Rec {
            frn,
            parent_frn: parent,
            attrs: DIR_ATTR,
            op: Op::Upsert,
            name: name.clone(),
            mtime: disk_mtime(&chain_disk),
        });
        parent = frn;
    }
    let root_frn = 100 + chain_names.len() as u64;
    recs.push(Rec {
        frn: root_frn,
        parent_frn: parent,
        attrs: DIR_ATTR,
        op: Op::Upsert,
        name: tag.clone(),
        mtime: disk_mtime(&root),
    });
    for (i, dir) in dirs.iter().enumerate() {
        recs.push(Rec {
            frn: 1001 + i as u64,
            parent_frn: root_frn,
            attrs: DIR_ATTR,
            op: Op::Upsert,
            name: dir.clone(),
            mtime: disk_mtime(&root.join(dir)),
        });
    }
    for (i, (parent_name, name)) in files.iter().enumerate() {
        let parent_frn = if *parent_name == "." {
            root_frn
        } else {
            1001 + dirs.iter().position(|d| d == parent_name).unwrap() as u64
        };
        recs.push(Rec {
            frn: 2001 + i as u64,
            parent_frn,
            attrs: 0,
            op: Op::Upsert,
            name: name.to_string(),
            mtime: disk_mtime(&root.join(parent_name).join(name)),
        });
    }
    let count = recs.len() as u32;
    engine.handle(EngineMsg::Records {
        guid,
        recs,
        next_usn: 100,
    });
    engine.handle(EngineMsg::VolumeReady {
        guid,
        journal_id: 7,
        next_usn: 100,
        count,
    });
    assert_eq!(engine.status().phase, Phase::Ready, "夹具索引未就绪");

    Some(Fixture {
        provider: BuiltinProvider::new(engine),
        root: root.clone(),
        empty_dir: root.join("LstrAcc-空目录"),
        snap,
        letter,
    })
}

fn search(fx: &Fixture, input: &str, scope: Scope, filters: &SearchFilters) -> (Vec<Entry>, usize) {
    let q = query::parse(input, scope);
    fx.provider
        .search(&q, filters, 50)
        .unwrap_or_else(|err| panic!("搜索 {input:?} 失败: {err}"))
}

fn result(fx: &Fixture, input: &str) -> (Vec<String>, usize) {
    let (entries, total) = search(fx, input, Scope::All, &SearchFilters::default());
    (entries.into_iter().map(|e| e.name).collect(), total)
}

// ---------------------------------------------------------------------------
// A. 名称匹配（「全部」页）
// -------------------------------------------------------------------------

#[test]
fn acceptance_a_name_matching() {
    let Some(fx) = fixture() else { return };

    let (entries, total) = search(&fx, "LstrAcc", Scope::All, &SearchFilters::default());
    assert_eq!(total, 11, "A1 输入 LstrAcc 应为 11 条（4 目录 + 7 文件），实际 {total}");
    assert_eq!(entries.len(), 11, "A1 返回条数应与 total 一致");
    assert!(
        entries.iter().all(|e| e.name.starts_with("LstrAcc")),
        "A1 混入非 LstrAcc 条目: {:?}",
        entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    let root_lc = fx.root.to_string_lossy().to_lowercase();
    assert!(
        entries
            .iter()
            .all(|e| e.full_path().to_lowercase().starts_with(&root_lc)),
        "A1 条目路径超出夹具目录: {:?}",
        entries.iter().map(|e| e.full_path()).collect::<Vec<_>>()
    );

    let (_, total) = result(&fx, "lstracc");
    assert_eq!(total, 11, "A2 大小写不敏感：lstracc 应同样命中 11 条");

    let (names, total) = result(&fx, "LstrAcc-全匹配");
    assert_eq!(total, 1, "A3 全匹配名应唯一命中");
    assert_eq!(names[0], "LstrAcc-全匹配.md", "A3 首条应为 LstrAcc-全匹配.md");

    let (names, total) = result(&fx, "LstrAcc 文档");
    assert_eq!(total, 1, "A4 多词 AND 应只命中 LstrAcc-文档");
    assert_eq!(names[0], "LstrAcc-文档", "A4 应命中 LstrAcc-文档");

    let (_, total) = result(&fx, "LstrCc");
    assert_eq!(total, 0, "A5 文件不做模糊匹配：LstrCc 不应命中");

    let (_, total) = result(&fx, "LstrAcc-不存在");
    assert_eq!(total, 0, "A6 不存在的名字不应命中");
}

// ---------------------------------------------------------------------------
// B. 拼音（「全部」页）
// -------------------------------------------------------------------------

#[test]
fn acceptance_b_pinyin() {
    let Some(fx) = fixture() else { return };

    let (names, total) = result(&fx, "wxzl");
    assert_eq!(total, 1, "B7 拼音首字母 wxzl 应命中 LstrAcc-微信资料");
    assert_eq!(names[0], "LstrAcc-微信资料");

    let (names, total) = result(&fx, "weixin");
    assert!(total >= 1, "B8 全拼 weixin 应有命中");
    assert!(
        names.iter().any(|n| n == "LstrAcc-微信资料"),
        "B8 应含 LstrAcc-微信资料，实际: {names:?}"
    );

    let (names, total) = result(&fx, "jty");
    assert_eq!(total, 1, "B9 jty 应命中 LstrAcc-金太阳");
    assert_eq!(names[0], "LstrAcc-金太阳");

    let (names, total) = result(&fx, "微信");
    assert_eq!(total, 2, "B10 微信应命中目录与聊天备份文件");
    for expected in ["LstrAcc-微信资料", "LstrAcc-微信聊天-备份.txt"] {
        assert!(names.iter().any(|n| n == expected), "B10 缺少 {expected}: {names:?}");
    }
}

// ---------------------------------------------------------------------------
// C. 筛选语法
// -------------------------------------------------------------------------

#[test]
fn acceptance_c_ext_and_path_filters() {
    let Some(fx) = fixture() else { return };

    let (names, total) = result(&fx, "ext:rs LstrAcc");
    assert_eq!(total, 1, "C11 ext:rs 应恰命中 1 条（扩展名大小写不敏感）");
    assert_eq!(names[0], "LstrAcc-扩展名测试.RS");

    let (names, total) = result(&fx, "ext:txt LstrAcc");
    assert_eq!(total, 2, "C12 ext:txt 应命中 2 条");
    for expected in ["LstrAcc-拖尾-2026.txt", "LstrAcc-微信聊天-备份.txt"] {
        assert!(names.iter().any(|n| n == expected), "C12 缺少 {expected}: {names:?}");
    }

    let doc = fx.root.join("LstrAcc-文档");
    let input = format!("path:{} LstrAcc", doc.display());
    let (names, total) = result(&fx, &input);
    assert_eq!(total, 4, "C13 path: 应限制到文档目录下的 4 个文件，实际: {names:?}");
    assert!(
        names.iter().all(|n| n != "LstrAcc-文档"),
        "C13 路径前缀不应把目录自身算作命中"
    );

    let input = format!("path:{} ext:txt LstrAcc", doc.display());
    let (names, total) = result(&fx, &input);
    assert_eq!(total, 1, "C14 path + ext 组合应只剩拖尾 txt");
    assert_eq!(names[0], "LstrAcc-拖尾-2026.txt");
}

// ---------------------------------------------------------------------------
// D. 盘符过滤
// -------------------------------------------------------------------------

#[test]
fn acceptance_d_drive_filters() {
    let Some(fx) = fixture() else { return };
    let letter = fx.letter;
    let other = if letter == 'D' { 'E' } else { 'D' };

    let input = format!("{letter}: LstrAcc");
    let (_, total) = result(&fx, &input);
    assert_eq!(total, 11, "D15 {input:?} 应保留夹具的 11 条");

    let input = format!("{other}: LstrAcc");
    let (_, total) = result(&fx, &input);
    assert_eq!(total, 0, "D16 {input:?} 不应命中夹具（夹具只在 {letter} 盘）");

    let input = format!("-{letter}: LstrAcc");
    let (_, total) = result(&fx, &input);
    assert_eq!(total, 0, "D17 {input:?} 排除唯一有夹具的盘后应为 0");

    let input = format!("{letter}: {other}: LstrAcc");
    let (_, total) = result(&fx, &input);
    assert_eq!(total, 11, "D18 多盘并集应保留 11 条");
}

// ---------------------------------------------------------------------------
// E. 作用域（分类标签页）
// -------------------------------------------------------------------------

#[test]
fn acceptance_e_scopes() {
    let Some(fx) = fixture() else { return };

    let (entries, total) = search(&fx, "f LstrAcc", Scope::All, &SearchFilters::default());
    assert_eq!(total, 7, "E19 文件页应恰为 7 个文件");
    assert!(
        entries.iter().all(|e| e.kind == Kind::File),
        "E19 文件页混入非文件: {:?}",
        entries.iter().map(|e| (&e.name, e.kind)).collect::<Vec<_>>()
    );

    let (entries, total) = search(&fx, "LstrAcc", Scope::Folder, &SearchFilters::default());
    assert_eq!(total, 4, "E20 文件夹页应恰为 4 个目录");
    assert!(
        entries.iter().all(|e| e.kind == Kind::Folder),
        "E20 文件夹页混入非目录: {:?}",
        entries.iter().map(|e| (&e.name, e.kind)).collect::<Vec<_>>()
    );

    // E21/E22 应用页（vsc / rust）由 src/search/apps.rs 的单测覆盖：
    // `search_hits_pinyin_initials` 与 `scattered_letters_do_not_match_english_names`。
}

// ---------------------------------------------------------------------------
// F. 路径浏览（老行为）
// -------------------------------------------------------------------------

#[test]
fn acceptance_f_path_browse() {
    let Some(fx) = fixture() else { return };

    let raw = fx.root.display().to_string();
    let pq = browse::resolve_exact(browse::parse(&raw).expect("F23 绝对路径应进入浏览模式"));
    let children = browse::list_children(&pq.dir);
    assert_eq!(children.len(), 5, "F23 应列出 4 个目录 + 根文件，实际: {:?}", children.iter().map(|e| &e.name).collect::<Vec<_>>());
    assert!(
        children[..4].iter().all(|e| e.kind == Kind::Folder),
        "F23 目录应排在文件前"
    );
    assert_eq!(children[4].name, "LstrAcc-根文件.log");

    let empty = fx.empty_dir.display().to_string();
    let pq = browse::resolve_exact(browse::parse(&empty).expect("F24 空目录也是合法路径"));
    assert!(
        browse::list_children(&pq.dir).is_empty(),
        "F24 空目录应返回空列表（UI 据此显示「目录为空」）"
    );

    // F25 冒号后没有空白 → 路径浏览（盘符根 + 过滤词），不是盘符过滤。
    let pq = browse::parse("e:lstracc").expect("F25 e:lstracc 应进入浏览模式");
    assert_eq!(pq.dir, r"e:\");
    assert_eq!(pq.filter, "lstracc");
    let q = query::parse("e:lstracc", Scope::All);
    assert_eq!(q.drive_mask(), None, "F25 无空白时不能当成盘符过滤");
    assert_eq!(q.text, "e:lstracc", "F25 整串应留给浏览解析");
}

// ---------------------------------------------------------------------------
// G. 时间筛选 + 时间格式
// -------------------------------------------------------------------------

#[test]
fn acceptance_g_time_filter_and_relative_time() {
    let Some(fx) = fixture() else { return };

    // G30 筛选条「最近一天」：夹具刚创建，mtime 为当前时间，11 条应全部保留。
    let filters = SearchFilters {
        location: None,
        time: Some(TimeFilter::Today),
        group: None,
    };
    let (_, total) = search(&fx, "LstrAcc", Scope::All, &filters);
    assert_eq!(total, 11, "G30 时间=今天不应过滤掉刚创建的夹具");

    // G28 列表右侧相对时间：刚创建的文件显示「刚刚」。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    assert_eq!(crate::ui::format_relative_time(now), "刚刚", "G28 刚创建应显示「刚刚」");

    // G26 空态技巧面板、G27 Esc 后再次唤出保留输入/结果、G29 预览面板
    // 「修改 刚刚」属于 egui 状态与布局，保留在手工验收表。
}
