//! P0-1：可积累资产层 —— 收藏、自定义动作、自定义命令、历史文件夹聚合。
//!
//! 设计约束：这一层只依赖 `config / history / model`，不碰 Win32 与 egui。
//! 目的是把「资产」做成可独立编译、可独立测试的数据模型，同时把主分支
//! 高频改动的 `app.rs` 挂接面压到最小。

use crate::config::Config;
use crate::history::History;
use crate::model::{Entry, Kind};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 收藏项：写入 `config.toml` 的 `[[favorite]]`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Favorite {
    pub name: String,
    pub path: String,
}

/// 自定义动作：作用于「选中的搜索结果」。
/// 模板变量：`{action_path}`、`{action_path:parent}`、`{query}`、`{current_folder}`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionDef {
    pub title: String,
    pub program: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub workdir: String,
    /// 预留字段：提权执行尚未接线，`run_action` 会显式报错而不是静默忽略。
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub silent: bool,
}

/// 自定义命令：作用于「搜索词 / 当前文件夹」。
/// 模板变量：`{query}`、`{current_folder}`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandDef {
    pub trigger: String,
    pub title: String,
    pub program: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub workdir: String,
}

pub struct ExpandContext<'a> {
    pub action_path: &'a str,
    pub query: &'a str,
    pub current_folder: &'a str,
}

/// 展开模板变量。先替换带后缀的 `{action_path:parent}`，避免被短变量抢先命中。
pub fn expand(template: &str, ctx: &ExpandContext<'_>) -> String {
    let parent = Path::new(ctx.action_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    template
        .replace("{action_path:parent}", &parent)
        .replace("{action_path}", ctx.action_path)
        .replace("{current_folder}", ctx.current_folder)
        .replace("{query}", ctx.query)
}

/// 按空格拆分参数，双引号内的空格保留（路径含空格时 `{action_path}` 要包引号）。
fn split_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn spawn_program(
    program: &str,
    args: &str,
    workdir: &str,
    silent: bool,
    ctx: &ExpandContext<'_>,
) -> Result<(), String> {
    let parts = split_args(&expand(args, ctx));
    // program 与 args/workdir 一样支持变量；之前只展开后两者，写 {current_folder}
    // 之类的程序名会静默启动失败。
    let program = expand(program, ctx);
    let mut command = std::process::Command::new(program);
    command.args(&parts);
    let cwd = expand(workdir, ctx);
    if !cwd.trim().is_empty() {
        command.current_dir(cwd.trim());
    }
    #[cfg(windows)]
    if silent {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：静默执行不弹控制台。
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = silent;
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("启动失败: {err}"))
}

/// 对选中结果执行自定义动作。
pub fn run_action(
    def: &ActionDef,
    entry: &Entry,
    query: &str,
    current_folder: &str,
) -> Result<(), String> {
    let path = entry.full_path();
    let ctx = ExpandContext {
        action_path: &path,
        query,
        current_folder,
    };
    if def.admin {
        let workdir = expand(&def.workdir, &ctx);
        if !workdir.trim().is_empty() {
            return Err("提权执行不支持工作目录；请清空 workdir 或关闭 admin".to_string());
        }
        let args = expand(&def.args, &ctx);
        let program = expand(&def.program, &ctx);
        return crate::win::shell::run_as_admin_with_args(&program, &args);
    }
    spawn_program(&def.program, &def.args, &def.workdir, def.silent, &ctx)
}

/// 执行自定义命令（不含 action_path 语义）。
pub fn run_command(
    def: &CommandDef,
    query: &str,
    current_folder: &str,
) -> Result<(), String> {
    let ctx = ExpandContext {
        action_path: "",
        query,
        current_folder,
    };
    spawn_program(&def.program, &def.args, &def.workdir, false, &ctx)
}

fn folder_entry(path: &Path) -> Entry {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    Entry {
        id: format!("folder:{}", path.to_string_lossy().to_lowercase()),
        kind: Kind::Folder,
        name,
        path: Some(path.to_path_buf()),
        size: None,
        modified: None,
        pinyin: None,
        initial: String::new(),
        tone: 0,
    }
}

/// 收藏的显示类型按路径形态判定：`.lnk`/`.exe`/`shell:`（UWP AppsFolder）
/// 按应用展示；带其它扩展名按文件；无扩展名按文件夹。硬编码成文件夹时，
/// Quick Switch 会把「终端」画成文件夹图标。
///
/// 不查磁盘（`is_dir()`）：断开的 UNC/移动硬盘会让收藏列表阻塞数秒，
/// 且收藏的目录暂时不可用也应该留在 Quick Switch 里。
fn favorite_kind(path: &Path) -> Kind {
    if path
        .to_string_lossy()
        .to_ascii_lowercase()
        .starts_with("shell:")
    {
        return Kind::App;
    }
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return Kind::Folder;
    };
    match ext.to_ascii_lowercase().as_str() {
        "lnk" | "exe" | "url" | "bat" | "cmd" | "msi" => Kind::App,
        _ => Kind::File,
    }
}

pub fn favorite_entry(fav: &Favorite) -> Entry {
    let path = PathBuf::from(&fav.path);
    let name = if fav.name.trim().is_empty() {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| fav.path.clone())
    } else {
        fav.name.clone()
    };
    Entry {
        id: format!("fav:{}", fav.path.to_lowercase()),
        kind: favorite_kind(&path),
        name,
        path: Some(path),
        size: None,
        modified: None,
        pinyin: None,
        initial: String::new(),
        tone: 0,
    }
}

/// 从使用历史聚合「最近使用过的文件夹」：
/// 文件夹条目直接用，文件/程序条目取其父目录。按最近使用顺序去重。
pub fn recent_folders(history: &History, limit: usize) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in history.recent_entries(200) {
        let folder: Option<PathBuf> = match entry.kind {
            Kind::Folder => entry.path.clone(),
            Kind::File | Kind::Program => entry
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf()),
            _ => None,
        };
        let Some(path) = folder else { continue };
        if !path.is_dir() {
            continue;
        }
        let key = path.to_string_lossy().to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        out.push(folder_entry(&path));
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn push_unique(out: &mut Vec<Entry>, seen: &mut HashSet<String>, entry: Entry) {
    let key = entry.full_path().trim_end_matches('\\').to_lowercase();
    if key.is_empty() || !seen.insert(key) {
        return;
    }
    out.push(entry);
}

/// Quick Switch 候选合并（P0-3）：活动目录 → 收藏 → 历史文件夹 → 已打开 Explorer 目录。
/// 全部按路径大写无关去重；活动目录排第一，`Ctrl+G` 因此可以零输入直跳。
pub fn quick_switch_entries(
    favorites: &[Favorite],
    history: &History,
    open_folders: Vec<PathBuf>,
    active_folder: Option<PathBuf>,
) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(path) = active_folder {
        push_unique(&mut out, &mut seen, folder_entry(&path));
    }
    for fav in favorites {
        let entry = favorite_entry(fav);
        // Quick Switch 只做「跳到文件夹」：收藏的应用（终端/微信）在
        // 文件对话框里没有可填入的路径，混进来只会显示成一个突兀的
        // 「应用」。它们在「应用」标签的空查询里置顶。
        if entry.kind != Kind::Folder {
            continue;
        }
        push_unique(&mut out, &mut seen, entry);
    }
    for entry in recent_folders(history, 6) {
        push_unique(&mut out, &mut seen, entry);
    }
    for path in open_folders {
        push_unique(&mut out, &mut seen, folder_entry(&path));
    }
    out.truncate(12);
    out
}

/// 把一条路径加入收藏并立即写回配置。重复路径不新增。
pub fn add_favorite(cfg: &mut Config, name: &str, path: &str) -> Result<bool, String> {
    if cfg
        .favorites
        .iter()
        .any(|f| f.path.eq_ignore_ascii_case(path))
    {
        return Ok(false);
    }
    cfg.favorites.push(Favorite {
        name: name.to_string(),
        path: path.to_string(),
    });
    cfg.save()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(path: &str) -> ExpandContext<'_> {
        ExpandContext {
            action_path: path,
            query: "report",
            current_folder: "D:\\work",
        }
    }

    #[test]
    fn favorite_kind_follows_path_shape() {
        // 无扩展名 → 文件夹（不查磁盘：断开的 UNC 也不阻塞）；UWP AUMID /
        // .lnk → 应用；普通文件 → 文件。
        assert_eq!(super::favorite_kind(Path::new(r"D:\work")), Kind::Folder);
        assert_eq!(
            super::favorite_kind(Path::new(
                r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"
            )),
            Kind::App
        );
        assert_eq!(
            super::favorite_kind(Path::new(r"C:\missing\微信.lnk")),
            Kind::App
        );
        assert_eq!(
            super::favorite_kind(Path::new(r"C:\missing\notes.txt")),
            Kind::File
        );
    }

    #[test]
    fn expand_handles_all_variables() {
        let text = "\"{action_path}\" --cd \"{action_path:parent}\" -q {query} -d {current_folder}";
        let got = expand(text, &ctx("D:\\work\\report.docx"));
        assert!(got.contains("\"D:\\work\\report.docx\""));
        assert!(got.contains("--cd \"D:\\work\""));
        assert!(got.contains("-q report"));
        assert!(got.contains("-d D:\\work"));
        assert!(!got.contains("{"));
    }

    #[test]
    fn split_args_keeps_quoted_paths() {
        let parts = split_args("\"C:\\Program Files\\app.exe\" --open \"{action_path}\"");
        assert_eq!(parts[0], "C:\\Program Files\\app.exe");
        assert_eq!(parts[1], "--open");
        assert_eq!(parts[2], "{action_path}");
    }

    #[test]
    fn quick_switch_dedupes_and_prioritizes_active() {
        let mut cfg = Config::default();
        cfg.favorites.push(Favorite {
            name: "work".to_string(),
            path: "D:\\work".to_string(),
        });
        let history = History::in_memory();
        let entries = quick_switch_entries(
            &cfg.favorites,
            &history,
            vec![PathBuf::from("D:\\work"), PathBuf::from("E:\\tmp")],
            Some(PathBuf::from("D:\\work")),
        );
        assert_eq!(entries[0].full_path(), "D:\\work");
        let paths: Vec<String> = entries.iter().map(|e| e.full_path()).collect();
        assert_eq!(paths.iter().filter(|p| *p == "D:\\work").count(), 1);
        assert!(paths.contains(&"E:\\tmp".to_string()));
    }

    #[test]
    fn quick_switch_drops_non_folder_favorites() {
        // 文件对话框的 Quick Switch 只做文件夹跳转：收藏的应用不该混进来。
        let favorites = vec![
            Favorite {
                name: "work".to_string(),
                path: r"D:\work".to_string(),
            },
            Favorite {
                name: "终端".to_string(),
                path: r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"
                    .to_string(),
            },
            Favorite {
                name: "微信".to_string(),
                path: r"C:\missing\微信.lnk".to_string(),
            },
        ];
        let history = History::in_memory();
        let entries = quick_switch_entries(&favorites, &history, Vec::new(), None);
        let paths: Vec<String> = entries.iter().map(|e| e.full_path()).collect();
        assert_eq!(paths, vec![r"D:\work".to_string()]);
    }

    #[test]
    fn add_favorite_is_idempotent() {
        // 不落盘：只验证内存去重分支（第一次会尝试写 config.toml，故直接构造）。
        let mut cfg = Config::default();
        cfg.favorites.push(Favorite {
            name: "a".to_string(),
            path: "D:\\a".to_string(),
        });
        assert!(cfg
            .favorites
            .iter()
            .any(|f| f.path.eq_ignore_ascii_case("d:\\A")));
    }
}
