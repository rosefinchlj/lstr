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

impl Kind {
    pub fn is_file_like(self) -> bool {
        matches!(self, Kind::File | Kind::Folder | Kind::Program)
    }
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

    pub fn from_key(key: &str) -> Self {
        match key {
            "folder" => Scope::Folder,
            "program" => Scope::Program,
            "file" => Scope::File,
            "app" => Scope::App,
            "command" => Scope::Command,
            _ => Scope::All,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

    pub fn is_executable(&self) -> bool {
        self.kind == Kind::Program
    }
}

#[derive(Clone, Debug)]
pub struct Query {
    pub raw: String,
    pub scope: Scope,
    pub text: String,
    pub ext: Option<String>,
    pub path_filter: Option<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.ext.is_none() && self.path_filter.is_none()
    }
}
