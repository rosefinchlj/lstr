use std::ffi::c_void;
use std::sync::Mutex;

use eframe::egui::ColorImage;
use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, BI_RGB,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, IBindCtx, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON,
    SHGFI_LARGEICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// SHGetFileInfoW 并发调用时会返回成功但 hIcon 为空（shell 图标缓存竞态），
/// 因此把整个图标加载过程串行化（查询、位图转换和句柄释放）。
static SHELL_ICON_CALL: Mutex<()> = Mutex::new(());

/// Extract a Windows shell icon for a file/folder/.lnk path.
///
/// The path is resolved by the shell itself (no `SHGFI_USEFILEATTRIBUTES`), so
/// `.lnk` files yield their target's icon and nonexistent paths return `None`.
/// Icons are returned at the system large (32x32) or small (16x16) size.
pub fn load_icon(path: &str, size: u32) -> Option<ColorImage> {
    shget_icon(path, size, 0, false)
}

/// 用 `SHGFI_USEFILEATTRIBUTES` 取「类型」图标：不访问磁盘，
/// 文件夹（FILE_ATTRIBUTE_DIRECTORY）和按扩展名的文件图标（FILE_ATTRIBUTE_NORMAL）
/// 一次调用只要亚毫秒，适合交给后台线程批量取。
pub fn load_icon_with_attrs(path: &str, size: u32, attrs: u32) -> Option<ColorImage> {
    shget_icon(path, size, attrs, true)
}

fn shget_icon(path: &str, size: u32, attrs: u32, use_attrs: bool) -> Option<ColorImage> {
    if path.is_empty() {
        return None;
    }

    // UWP 应用的 AUMID 只能通过 shell 命名空间解析，SHGetFileInfoW 不认
    // "shell:AppsFolder\..."，需要 IShellItemImageFactory。
    if !use_attrs && path.to_ascii_lowercase().starts_with("shell:") {
        if let Some(image) = shell_path_image(path, size) {
            return Some(image);
        }
    }

    let _guard = SHELL_ICON_CALL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut flags = SHGFI_ICON
        | if size >= 32 {
            SHGFI_LARGEICON
        } else {
            SHGFI_SMALLICON
        };
    if use_attrs {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }

    let mut file_info = SHFILEINFOW::default();
    let got = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(attrs),
            Some(&mut file_info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };

    if got == 0 || file_info.hIcon.is_invalid() {
        // 失败时 shell 一般不会写 hIcon，但句柄有效说明必须释放。
        if !file_info.hIcon.is_invalid() {
            unsafe {
                let _ = DestroyIcon(file_info.hIcon);
            }
        }
        return None;
    }

    let image = icon_to_color_image(file_info.hIcon);
    unsafe {
        let _ = DestroyIcon(file_info.hIcon);
    }
    image
}

/// COM 初始化守卫：线程可能已经由 winit 初始化过（MTA），此时不再重复反初始化。
struct ComGuard(bool);

impl ComGuard {
    fn new() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// 用 shell 的 IShellItemImageFactory 取图标；支持 shell:AppsFolder\{AUMID}。
fn shell_path_image(path: &str, size: u32) -> Option<ColorImage> {
    let _guard = SHELL_ICON_CALL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _com = ComGuard::new();

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let factory: IShellItemImageFactory = unsafe {
        SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)
    }
    .ok()?;
    let bitmap = unsafe {
        factory.GetImage(
            SIZE {
                cx: size as i32,
                cy: size as i32,
            },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .ok()?;

    let bitmaps = IconBitmaps {
        color: bitmap,
        mask: HBITMAP::default(),
    };
    let dc = ScreenDc(unsafe { GetDC(None) });
    if dc.0.is_invalid() {
        return None;
    }
    color_bitmap_image(dc.0, bitmaps.color, bitmaps.mask)
}

/// 确保从 GetIconInfo 得到的位图在所有提前返回路径上都被释放。
struct IconBitmaps {
    color: HBITMAP,
    mask: HBITMAP,
}

impl Drop for IconBitmaps {
    fn drop(&mut self) {
        unsafe {
            for bitmap in [self.color, self.mask] {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
            }
        }
    }
}

/// 确保 ReleaseDC 在提前返回时也会执行。
struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = ReleaseDC(None, self.0);
            }
        }
    }
}

fn icon_to_color_image(hicon: HICON) -> Option<ColorImage> {
    let mut icon_info = ICONINFO::default();
    unsafe {
        GetIconInfo(hicon, &mut icon_info).ok()?;
    }
    let bitmaps = IconBitmaps {
        color: icon_info.hbmColor,
        mask: icon_info.hbmMask,
    };
    let dc = ScreenDc(unsafe { GetDC(None) });
    if dc.0.is_invalid() {
        return None;
    }

    if !bitmaps.color.is_invalid() {
        return color_bitmap_image(dc.0, bitmaps.color, bitmaps.mask);
    }

    // 单色图标没有彩色位图：hbmMask 高度为图标的两倍，上半是 AND 掩码，
    // 下半是 XOR 掩码。
    monochrome_mask_image(dc.0, bitmaps.mask)
}

fn color_bitmap_image(dc: HDC, color: HBITMAP, mask: HBITMAP) -> Option<ColorImage> {
    let (width, height) = bitmap_size(color)?;
    let bgra = read_pixels(dc, color, width, height)?;
    let mut rgba = bgra_to_rgba(&bgra);

    if rgba.chunks_exact(4).all(|pixel| pixel[3] == 0) {
        // 位图的 alpha 通道可能丢失（DDB -> DIB 转换），此时用 AND 掩码
        // 恢复透明度：掩码位为 1 表示该像素透明。
        let derived = if mask.is_invalid() {
            None
        } else {
            bitmap_size(mask).and_then(|(mask_width, mask_height)| {
                if mask_width == width && mask_height == height {
                    read_pixels(dc, mask, mask_width, mask_height)
                } else {
                    None
                }
            })
        };
        match derived {
            Some(mask_pixels) => apply_mask_alpha(&mut rgba, &mask_pixels),
            // 没有可用掩码时按完全不透明处理，避免整幅图消失。
            None => set_opaque(&mut rgba),
        }
    }

    Some(ColorImage::from_rgba_unmultiplied(
        [width, height],
        &rgba,
    ))
}

fn monochrome_mask_image(dc: HDC, mask: HBITMAP) -> Option<ColorImage> {
    if mask.is_invalid() {
        return None;
    }
    let (mask_width, mask_height) = bitmap_size(mask)?;
    let mask_pixels = read_pixels(dc, mask, mask_width, mask_height)?;

    if mask_height % 2 == 0 {
        let height = mask_height / 2;
        let mut rgba = vec![0u8; mask_width.checked_mul(height)?.checked_mul(4)?];
        for y in 0..height {
            for x in 0..mask_width {
                let and_at = (y * mask_width + x) * 4;
                let xor_at = ((y + height) * mask_width + x) * 4;
                let and_set = mask_bit_set(&mask_pixels[and_at..and_at + 4]);
                let xor_set = mask_bit_set(&mask_pixels[xor_at..xor_at + 4]);
                let out = (y * mask_width + x) * 4;
                if and_set {
                    // AND=1 表示背景透出；AND=1/XOR=1 的“反色”无法用 RGBA
                    // 表示，也按透明处理。
                    rgba[out..out + 4].copy_from_slice(&[0, 0, 0, 0]);
                } else {
                    let value = if xor_set { 255 } else { 0 };
                    rgba[out] = value;
                    rgba[out + 1] = value;
                    rgba[out + 2] = value;
                    rgba[out + 3] = 255;
                }
            }
        }
        return Some(ColorImage::from_rgba_unmultiplied(
            [mask_width, height],
            &rgba,
        ));
    }

    // 非标准的单高度掩码：按纯 AND 掩码处理，颜色取黑色。
    let mut rgba = vec![0u8; mask_width.checked_mul(mask_height)?.checked_mul(4)?];
    apply_mask_alpha(&mut rgba, &mask_pixels);
    Some(ColorImage::from_rgba_unmultiplied(
        [mask_width, mask_height],
        &rgba,
    ))
}

fn bitmap_size(bitmap: HBITMAP) -> Option<(usize, usize)> {
    let mut info = BITMAP::default();
    let written = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut info as *mut BITMAP).cast::<c_void>()),
        )
    };
    if written == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
        return None;
    }
    Some((info.bmWidth as usize, info.bmHeight as usize))
}

/// 用 GetDIBits 把位图读成自上而下的 32bpp BGRA。
fn read_pixels(dc: HDC, bitmap: HBITMAP, width: usize, height: usize) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let width_i32 = i32::try_from(width).ok()?;
    let height_i32 = i32::try_from(height).ok()?;
    let length = width.checked_mul(height)?.checked_mul(4)?;

    // 负的 biHeight 表示自上而下的 DIB，省去手工翻转。
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width_i32,
            biHeight: -height_i32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut buffer = vec![0u8; length];
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            u32::try_from(height).ok()?,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 || lines as usize != height {
        return None;
    }
    Some(buffer)
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; bgra.len()];
    for (source, target) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        target[0] = source[2];
        target[1] = source[1];
        target[2] = source[0];
        target[3] = source[3];
    }
    rgba
}

/// 掩码位为 1（GetDIBits 转换后为白色）表示透明。
fn apply_mask_alpha(rgba: &mut [u8], mask: &[u8]) {
    for (pixel, mask_pixel) in rgba.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
        pixel[3] = if mask_bit_set(mask_pixel) { 0 } else { 255 };
    }
}

fn mask_bit_set(mask_pixel: &[u8]) -> bool {
    mask_pixel[0] != 0 || mask_pixel[1] != 0 || mask_pixel[2] != 0
}

fn set_opaque(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_icon_of_existing_file() {
        let image = load_icon("C:\\Windows\\notepad.exe", 32).expect("notepad icon");
        assert_eq!(image.size, [32, 32]);
        assert_eq!(image.pixels.len(), 32 * 32);
    }

    #[test]
    fn returns_none_for_missing_path() {
        assert!(load_icon("C:\\lstr\\definitely-missing-file-12345.exe", 32).is_none());
    }

    #[test]
    fn loads_small_icon() {
        let image = load_icon("C:\\Windows\\notepad.exe", 16).expect("small notepad icon");
        assert_eq!(image.size, [16, 16]);
    }

    #[test]
    fn loads_uwp_icon_from_apps_folder() {
        let image = load_icon(
            r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            32,
        )
        .expect("calculator icon");
        assert_eq!(image.size, [32, 32]);
        // 位图必须真的画出内容，而不是一整张透明图。
        assert!(image.pixels.iter().any(|pixel| pixel.a() > 0));
    }

    #[test]
    fn converts_bgra_to_rgba() {
        let bgra = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(bgra_to_rgba(&bgra), [3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn derives_alpha_from_mask() {
        // 颜色位图 alpha 全为 0；掩码第一像素为白（透明），第二像素为黑（不透明）。
        let mut rgba = [10u8, 20, 30, 0, 40, 50, 60, 0];
        let mask = [255u8, 255, 255, 0, 0, 0, 0, 0];
        apply_mask_alpha(&mut rgba, &mask);
        assert_eq!(rgba, [10, 20, 30, 0, 40, 50, 60, 255]);
    }

    #[test]
    fn treats_missing_mask_as_opaque() {
        let mut rgba = [1u8, 2, 3, 0, 4, 5, 6, 0];
        set_opaque(&mut rgba);
        assert_eq!(rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
