//! 把结果行拖出到其他应用（微信/邮件/Explorer/IDE）：OLE drag source。
//! 数据用 `CF_HDROP`（DROPFILES + 双零结尾的宽字符路径列表），
//! `IDataObject` 借 shell 的标准实现（SHCreateDataObject），
//! 自己只实现两个方法的 `IDropSource`。

use std::path::PathBuf;
use windows::core::{implement, BOOL, HRESULT};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, S_OK,
};
use windows::Win32::System::Com::{IDataObject, FORMATETC, STGMEDIUM, STGMEDIUM_0};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, RevokeDragDrop, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{DROPFILES, SHCreateDataObject};

const TYMED_HGLOBAL_U32: u32 = 1; // TYMED_HGLOBAL
const DVASPECT_CONTENT_U32: u32 = 1; // DVASPECT_CONTENT

pub enum DragOutcome {
    Dropped,
    Cancelled,
}

/// 注销窗口上的 OLE 拖放接收器。winit（egui 底层）注册的接收器
/// `QueryInterface` 是 `unimplemented!()` 占位——从结果行发起 DoDragDrop 时
/// shell 会查询它，直接 panic → 0xc0000409 闪退。lstr 不接受「往窗口里
/// 拖文件」，这个接收器毫无用处，启动即注销。
pub fn revoke_window_drop_target(hwnd: isize) {
    if let Err(err) = unsafe { RevokeDragDrop(HWND(hwnd as *mut _)) } {
        eprintln!("lstr: RevokeDragDrop 失败（{}）", err.code());
    }
}

/// 把一组文件作为拖放数据发起模态拖拽；返回时拖放已结束（落下或取消）。
/// 调用期间 OLE 自己泵消息，egui 主循环会阻塞——这是模态循环的常态。
pub fn drag_files(paths: Vec<PathBuf>) -> Result<DragOutcome, String> {
    if paths.is_empty() {
        return Err("没有可拖拽的文件".to_string());
    }
    // OLE 必须已在当前线程初始化（winit 通常会初始化过；S_FALSE 也接受）。
    unsafe {
        let _ = OleInitialize(None);
    }

    let bytes = hdrop_bytes(&paths);
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
        .map_err(|e| format!("分配拖放数据失败: {e}"))?;
    unsafe {
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return Err("锁定拖放数据失败".to_string());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(hglobal);
    }

    // shell 的标准 DataObject 省得手写 IDataObject 的 9 个方法。
    let data: IDataObject = unsafe {
        SHCreateDataObject(None, None, Option::<&IDataObject>::None)
    }
    .map_err(|e| format!("创建拖放对象失败: {e}"))?;
    let format = FORMATETC {
        cfFormat: CF_HDROP.0 as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT_U32,
        lindex: -1,
        tymed: TYMED_HGLOBAL_U32,
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL_U32,
        u: STGMEDIUM_0 { hGlobal: hglobal },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    };
    // frelease = TRUE：HGLOBAL 的所有权交给 data object。
    unsafe { data.SetData(&format, &medium, true) }.map_err(|e| format!("写入拖放数据失败: {e}"))?;

    let source: IDropSource = DropSource.into();
    let mut effect = DROPEFFECT_NONE;
    let hr = unsafe {
        DoDragDrop(
            &data,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            &mut effect,
        )
    };
    if hr == DRAGDROP_S_DROP {
        Ok(DragOutcome::Dropped)
    } else if hr == DRAGDROP_S_CANCEL {
        Ok(DragOutcome::Cancelled)
    } else if hr.is_ok() {
        Ok(DragOutcome::Cancelled)
    } else {
        Err(format!("拖拽失败（{}）", hr.0))
    }
}

/// DROPFILES 头 + UTF-16LE 路径列表（各以 0 结尾，最后再加一个 0）。
fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let header_len = std::mem::size_of::<DROPFILES>() as u32;
    let mut wide: Vec<u16> = Vec::new();
    for path in paths {
        let text = path.display().to_string();
        wide.extend(text.encode_utf16());
        wide.push(0);
    }
    wide.push(0); // 列表末尾的双零
    let total = header_len as usize + wide.len() * 2;
    let mut bytes = vec![0u8; total];
    // 手写头部，避免未初始化读取：pFiles 偏移 + fWide = TRUE。
    bytes[0..4].copy_from_slice(&header_len.to_le_bytes());
    let fwide_offset = 4 + 8 + 4; // pFiles + POINT + fNC
    bytes[fwide_offset..fwide_offset + 4].copy_from_slice(&1i32.to_le_bytes());
    for (i, word) in wide.iter().enumerate() {
        let at = header_len as usize + i * 2;
        bytes[at..at + 2].copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    /// Escape 取消；初始按键（左键）松开即落下。
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if grfkeystate.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    /// 光标交给 OLE 默认（目标应用自己显示复制/移动提示）。
    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hdrop(bytes: &[u8]) -> (u32, bool, Vec<String>) {
        let pfiles = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let fwide_offset = 4 + 8 + 4;
        let fwide = i32::from_le_bytes(bytes[fwide_offset..fwide_offset + 4].try_into().unwrap());
        let mut wide = Vec::new();
        let mut at = pfiles as usize;
        while at + 1 < bytes.len() {
            wide.push(u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()));
            at += 2;
        }
        // 以 0 切分，末尾的空段丢掉。
        let mut paths = Vec::new();
        let mut current = Vec::new();
        for word in wide {
            if word == 0 {
                if !current.is_empty() {
                    paths.push(String::from_utf16_lossy(&current));
                    current.clear();
                }
            } else {
                current.push(word);
            }
        }
        (pfiles, fwide != 0, paths)
    }

    #[test]
    fn hdrop_layout_round_trips_paths() {
        let bytes = hdrop_bytes(&[PathBuf::from(r"D:\proj\说明.txt")]);
        let (pfiles, fwide, paths) = decode_hdrop(&bytes);
        assert_eq!(pfiles as usize, std::mem::size_of::<DROPFILES>());
        assert!(fwide, "fWide 必须为 TRUE，中文路径才不会乱码");
        assert_eq!(paths, vec![r"D:\proj\说明.txt"]);
    }

    #[test]
    fn hdrop_supports_multiple_files() {
        let bytes = hdrop_bytes(&[PathBuf::from("C:\\a.txt"), PathBuf::from("C:\\b.txt")]);
        let (_, _, paths) = decode_hdrop(&bytes);
        assert_eq!(paths, vec!["C:\\a.txt", "C:\\b.txt"]);
    }

    #[test]
    fn hdrop_ends_with_double_nul() {
        let bytes = hdrop_bytes(&[PathBuf::from("C:\\a")]);
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
    }

    /// SetData 路径的集成测试（无需 GUI）：构造 shell DataObject 并验证
    /// CF_HDROP 格式被接受。DoDragDrop 本身依赖真实鼠标，不在自动化范围。
    #[test]
    fn data_object_accepts_hdrop_medium() {
        unsafe {
            let _ = OleInitialize(None);
        }
        let bytes = hdrop_bytes(&[PathBuf::from(r"C:\test\文件.txt")]);
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.expect("alloc");
        unsafe {
            let ptr = GlobalLock(hglobal);
            assert!(!ptr.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(hglobal);
        }
        let data: IDataObject = unsafe { SHCreateDataObject(None, None, Option::<&IDataObject>::None) }
            .expect("SHCreateDataObject");
        let format = FORMATETC {
            cfFormat: CF_HDROP.0 as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT_U32,
            lindex: -1,
            tymed: TYMED_HGLOBAL_U32,
        };
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL_U32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        unsafe { data.SetData(&format, &medium, true) }.expect("SetData");
        // 同一个格式能查回来（shell 实现会校验 dwAspect/tymed 匹配）。
        let check = FORMATETC {
            cfFormat: CF_HDROP.0 as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT_U32,
            lindex: -1,
            tymed: TYMED_HGLOBAL_U32,
        };
        unsafe { data.QueryGetData(&check) }
            .ok()
            .expect("QueryGetData 应接受 CF_HDROP");
    }
}
