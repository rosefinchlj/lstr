use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    File,
    Folder,
    Program,
    App,
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Folder,
    Program,
    File,
    App,
    Command,
}

impl Scope {
    /// 标签栏显示顺序：应用放第一列；默认选中「全部」由启动时 scope 决定。
    pub const ALL: [Scope; 6] = [
        Scope::App,
        Scope::All,
        Scope::Folder,
        Scope::Program,
        Scope::File,
        Scope::Command,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Scope::All => "全部",
            Scope::Folder => "文件夹",
            Scope::Program => "程序",
            Scope::File => "文件",
            Scope::App => "应用",
            Scope::Command => "命令",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn step(self, delta: i32) -> Self {
        let len = Self::ALL.len() as i32;
        let at = self.index() as i32;
        Self::ALL[((at + delta % len + len) % len) as usize]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub path: Option<PathBuf>,
    pub size: Option<u64>,
    pub modified: Option<i64>,
    pub pinyin: Option<String>,
    pub initial: String,
    pub tone: u32,
}

impl Entry {
    pub fn full_path(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.name.clone())
    }
}

#[derive(Clone, Debug)]
pub struct Query {
    pub raw: String,
    pub scope: Scope,
    pub text: String,
    pub ext: Option<String>,
    pub path_filter: Option<String>,
    /// 限定盘符（bit i = 'A'+i）：`c: d: 关键词`。0 = 不限制。
    pub drives_include: u32,
    /// 排除盘符（bit i = 'A'+i）：`-c: 关键词`。0 = 不排除。
    pub drives_exclude: u32,
    /// 通配前缀：`abc*` / `abc.*`（小写，匹配名字开头）。
    pub prefix: Option<String>,
    /// 通配后缀：`*abc`（小写，匹配名字结尾）。
    pub suffix: Option<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.ext.is_none()
            && self.path_filter.is_none()
            && self.drives_include == 0
            && self.drives_exclude == 0
            && self.prefix.is_none()
            && self.suffix.is_none()
    }

    /// 盘符约束的最终卷掩码：None=不限制，Some(mask)=只查这些卷（可能为空）。
    pub fn drive_mask(&self) -> Option<u32> {
        if self.drives_include == 0 && self.drives_exclude == 0 {
            return None;
        }
        let base = if self.drives_include == 0 {
            (1u32 << 26) - 1
        } else {
            self.drives_include
        };
        Some(base & !self.drives_exclude)
    }
}
