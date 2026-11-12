//! 结果条目转换的纯函数：作用域判定、类型推导、拼音首字母、稳定色调。
//!
//! 这些函数原先随 Everything 后端放在 `search/everything.rs`；删除 Everything
//! 后仍被内置索引适配层（`search/builtin.rs`）使用，因此独立成模块，避免把
//! 后端依赖带进查询链路。

use crate::model::{Kind, Scope};

pub(crate) fn kept_in_scope(scope: Scope, kind: Kind) -> bool {
    match scope {
        Scope::Folder => kind == Kind::Folder,
        Scope::Program => kind == Kind::Program,
        Scope::File => kind == Kind::File,
        _ => true,
    }
}

pub(crate) fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        Kind::Folder
    } else if name
        .rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        Kind::Program
    } else {
        Kind::File
    }
}

pub(crate) fn initial_of(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

pub(crate) fn tone_for(value: &str) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash & 0x00FF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_uses_first_unicode_char() {
        assert_eq!(initial_of("微信"), "微");
        assert_eq!(initial_of("readme"), "R");
        assert_eq!(initial_of(""), "");
    }

    #[test]
    fn tone_is_stable_and_bounded() {
        assert_eq!(tone_for(r"C:\a.rs"), tone_for(r"C:\a.rs"));
        assert!(tone_for(r"C:\a.rs") <= 0x00FF_FFFF);
    }

    #[test]
    fn kind_infers_program_and_folder() {
        assert_eq!(kind_of("foo", true), Kind::Folder);
        assert_eq!(kind_of("foo.exe", false), Kind::Program);
        assert_eq!(kind_of("FOO.EXE", false), Kind::Program);
        assert_eq!(kind_of("readme", false), Kind::File);
    }

    #[test]
    fn scope_keeps_only_matching_kind() {
        assert!(kept_in_scope(Scope::Folder, Kind::Folder));
        assert!(!kept_in_scope(Scope::Folder, Kind::Program));
        assert!(kept_in_scope(Scope::Program, Kind::Program));
        assert!(!kept_in_scope(Scope::Program, Kind::Folder));
        assert!(!kept_in_scope(Scope::Program, Kind::File));
        assert!(kept_in_scope(Scope::File, Kind::File));
        assert!(!kept_in_scope(Scope::File, Kind::Program));
        assert!(kept_in_scope(Scope::All, Kind::File));
    }
}
