use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use eframe::egui::ColorImage;
use rusqlite::{Connection, OpenFlags};
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
    SIIGBF_ICONONLY, SIIGBF_THUMBNAILONLY,
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

/// Edge 可执行文件的路径：书签结果统一显示 Edge 图标（用户默认用它开网页）。
/// 未安装/装在非常规位置时返回 None，调用方回退到绘制的地球图标。
pub fn edge_exe() -> Option<PathBuf> {
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        let Some(base) = std::env::var_os(var) else {
            continue;
        };
        let candidate = PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 取书签对应网站的图标：读浏览器本地 favicon 缓存（Edge/Chrome 的
/// `Favicons` SQLite、Firefox 的 `favicons.sqlite`），全程不联网。
/// 缓存里没有这个站点时返回 None，调用方回退到 Edge 图标。
pub fn load_favicon(url: &str) -> Option<ColorImage> {
    let bytes = {
        let store = favicon_store().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        store.lookup(url)?
    };
    decode_favicon(&bytes)
}

/// 浏览器 favicon 缓存（只读副本）。首次需要书签图标时才初始化；
/// 浏览器运行时会锁 WAL，所以复制主库 + WAL/SHM 到临时目录再打开。
struct FaviconStore {
    /// Edge 优先（默认浏览器），其次是其它 Chromium，最后 Firefox。
    sources: Vec<FaviconSource>,
    /// 进程级临时目录：连接关闭前不能删除（Windows 不允许删打开的文件）。
    _dir: PathBuf,
}

enum FaviconSource {
    Chromium(Connection),
    Firefox(Connection),
}

static FAVICONS: OnceLock<Mutex<FaviconStore>> = OnceLock::new();

fn favicon_store() -> &'static Mutex<FaviconStore> {
    FAVICONS.get_or_init(|| Mutex::new(FaviconStore::open()))
}

impl FaviconStore {
    fn open() -> Self {
        let dir = std::env::temp_dir().join(format!("lstr-favicons-{}", std::process::id()));
        Self::clean_stale_dirs(&dir);
        let mut sources = Vec::new();
        if fs::create_dir_all(&dir).is_ok() {
            let mut candidates: Vec<(u8, PathBuf, bool)> = Vec::new();
            for root in super::browserdata::chromium_user_data_roots() {
                let is_edge = root
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("microsoft\\edge");
                for profile in super::browserdata::chromium_profile_dirs(&root) {
                    let db = profile.join("Favicons");
                    if db.is_file() {
                        candidates.push((if is_edge { 0 } else { 1 }, db, false));
                    }
                }
            }
            for profile in super::browserdata::firefox_profile_dirs() {
                let db = profile.join("favicons.sqlite");
                if db.is_file() {
                    candidates.push((2, db, true));
                }
            }
            candidates.sort_by_key(|(priority, _, _)| *priority);
            for (index, (_, db, firefox)) in candidates.into_iter().enumerate() {
                let Some(conn) = Self::copy_and_open(&dir, &db, index) else {
                    continue;
                };
                sources.push(if firefox {
                    FaviconSource::Firefox(conn)
                } else {
                    FaviconSource::Chromium(conn)
                });
            }
        }
        Self { sources, _dir: dir }
    }

    /// 清理上次进程遗留的复制目录（单实例程序，别的 pid 目录都是垃圾）。
    fn clean_stale_dirs(current: &Path) {
        let Some(parent) = current.parent() else {
            return;
        };
        let Ok(entries) = fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == current {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("lstr-favicons-") {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }

    fn copy_and_open(dir: &Path, db: &Path, index: usize) -> Option<Connection> {
        let name = db.file_name()?.to_string_lossy().to_string();
        let copy = dir.join(format!("{index}-{name}"));
        fs::copy(db, &copy).ok()?;
        for suffix in ["-wal", "-shm"] {
            let mut from = db.as_os_str().to_os_string();
            from.push(suffix);
            let from = PathBuf::from(from);
            if !from.is_file() {
                continue;
            }
            let mut to = copy.as_os_str().to_os_string();
            to.push(suffix);
            let _ = fs::copy(from, PathBuf::from(to));
        }
        Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    }

    fn lookup(&self, url: &str) -> Option<Vec<u8>> {
        let candidates = url_candidates(url);
        let host_pattern = host_like_pattern(url);
        for source in &self.sources {
            let bytes = match source {
                FaviconSource::Chromium(conn) => {
                    chromium_favicon(conn, &candidates, host_pattern.as_deref())
                }
                FaviconSource::Firefox(conn) => {
                    firefox_favicon(conn, &candidates, host_pattern.as_deref())
                }
            };
            if let Some(bytes) = bytes {
                if !bytes.is_empty() {
                    return Some(bytes);
                }
            }
        }
        None
    }
}

/// 书签 URL 的可能写法：原文、去 fragment、带/不带尾斜杠、站点根。
fn url_candidates(url: &str) -> Vec<String> {
    fn push(out: &mut Vec<String>, value: String) {
        if !value.is_empty() && !out.iter().any(|existing| *existing == value) {
            out.push(value);
        }
    }
    let base = url.trim();
    let no_fragment = base.split('#').next().unwrap_or(base);
    let mut out = Vec::new();
    for candidate in [base, no_fragment] {
        push(&mut out, candidate.to_string());
        if let Some(stripped) = candidate.strip_suffix('/') {
            push(&mut out, stripped.to_string());
        } else {
            push(&mut out, format!("{candidate}/"));
        }
    }
    if let Some((scheme, rest)) = base.split_once("://") {
        if let Some(host) = rest.split(['/', '?', '#']).next() {
            if !host.is_empty() {
                push(&mut out, format!("{scheme}://{host}/"));
                push(&mut out, format!("{scheme}://{host}"));
            }
        }
    }
    out
}

/// 同一站点任意页面的兜底模式（`https://host/%`），LIKE 通配符已转义。
fn host_like_pattern(url: &str) -> Option<String> {
    let base = url.trim();
    let (scheme, rest) = base.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    let escaped = host
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    Some(format!("{scheme}://{escaped}/%"))
}

fn chromium_favicon(
    conn: &Connection,
    candidates: &[String],
    host_pattern: Option<&str>,
) -> Option<Vec<u8>> {
    const EXACT: &str = "SELECT b.image_data FROM icon_mapping m \
         JOIN favicon_bitmaps b ON b.icon_id = m.icon_id \
         WHERE m.page_url = ?1 AND b.image_data IS NOT NULL \
         ORDER BY b.width DESC LIMIT 1";
    const HOST: &str = "SELECT b.image_data FROM icon_mapping m \
         JOIN favicon_bitmaps b ON b.icon_id = m.icon_id \
         WHERE m.page_url LIKE ?1 ESCAPE '\\' AND b.image_data IS NOT NULL \
         ORDER BY b.width DESC LIMIT 1";
    if let Ok(mut stmt) = conn.prepare_cached(EXACT) {
        for url in candidates {
            if let Ok(bytes) = stmt.query_row([url.as_str()], |row| row.get::<_, Vec<u8>>(0)) {
                return Some(bytes);
            }
        }
    }
    if let (Some(pattern), Ok(mut stmt)) = (host_pattern, conn.prepare_cached(HOST)) {
        if let Ok(bytes) = stmt.query_row([pattern], |row| row.get::<_, Vec<u8>>(0)) {
            return Some(bytes);
        }
    }
    None
}

fn firefox_favicon(
    conn: &Connection,
    candidates: &[String],
    host_pattern: Option<&str>,
) -> Option<Vec<u8>> {
    const EXACT: &str = "SELECT i.data FROM moz_icons_to_pages ip \
         JOIN moz_pages_w_icons p ON p.id = ip.page_id \
         JOIN moz_icons i ON i.id = ip.icon_id \
         WHERE p.page_url = ?1 AND i.data IS NOT NULL \
         ORDER BY i.width DESC LIMIT 1";
    const HOST: &str = "SELECT i.data FROM moz_icons_to_pages ip \
         JOIN moz_pages_w_icons p ON p.id = ip.page_id \
         JOIN moz_icons i ON i.id = ip.icon_id \
         WHERE p.page_url LIKE ?1 ESCAPE '\\' AND i.data IS NOT NULL \
         ORDER BY i.width DESC LIMIT 1";
    if let Ok(mut stmt) = conn.prepare_cached(EXACT) {
        for url in candidates {
            if let Ok(bytes) = stmt.query_row([url.as_str()], |row| row.get::<_, Vec<u8>>(0)) {
                return Some(bytes);
            }
        }
    }
    if let (Some(pattern), Ok(mut stmt)) = (host_pattern, conn.prepare_cached(HOST)) {
        if let Ok(bytes) = stmt.query_row([pattern], |row| row.get::<_, Vec<u8>>(0)) {
            return Some(bytes);
        }
    }
    None
}

/// PNG/ICO/JPEG blob → 32×32 透明底方块（非正方形图标居中）。
fn decode_favicon(bytes: &[u8]) -> Option<ColorImage> {
    let image = image::load_from_memory(bytes).ok()?;
    let thumb = image.thumbnail(32, 32);
    let rgba = thumb.to_rgba8();
    let width = (rgba.width() as usize).min(32);
    let height = (rgba.height() as usize).min(32);
    if width == 0 || height == 0 {
        return None;
    }
    let mut pixels = vec![0u8; 32 * 32 * 4];
    let offset_x = (32 - width) / 2;
    let offset_y = (32 - height) / 2;
    for y in 0..height {
        for x in 0..width {
            let pixel = rgba.get_pixel(x as u32, y as u32);
            let at = ((offset_y + y) * 32 + offset_x + x) * 4;
            pixels[at..at + 4].copy_from_slice(&[pixel[0], pixel[1], pixel[2], pixel[3]]);
        }
    }
    Some(ColorImage::from_rgba_unmultiplied([32, 32], &pixels))
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

/// 取文件缩略图(图片预览用):SIIGBF_THUMBNAILONLY 让 shell 返回缩略图而非
/// 图标,走系统缩略图缓存(支持 png/jpg/gif/bmp/webp 等,无需自解码)。非图片
/// 文件会回退图标,调用方按扩展名先筛。与图标共用串行锁(shell 竞态)。
pub fn load_thumbnail(path: &str, size: u32) -> Option<ColorImage> {
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
            SIZE { cx: size as i32, cy: size as i32 },
            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .ok()?;
    let bitmaps = IconBitmaps { color: bitmap, mask: HBITMAP::default() };
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

    /// 书签行图标来自 Edge 的 exe；未安装 Edge 的机器跳过（调用方回退地球图标）。
    #[test]
    fn loads_edge_icon_when_installed() {
        let Some(exe) = edge_exe() else {
            return;
        };
        let image = load_icon(&exe.to_string_lossy(), 32).expect("edge icon");
        assert_eq!(image.size, [32, 32]);
        assert!(
            image.pixels.iter().any(|pixel| pixel.a() > 0),
            "Edge 图标不应是整张透明图"
        );
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

    fn png_bytes_with(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    fn png_bytes(size: u32) -> Vec<u8> {
        png_bytes_with(size, size)
    }

    fn temp_db_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lstr-favicon-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn url_candidates_cover_slash_and_fragment_variants() {
        let candidates = url_candidates("https://Example.com/a/#frag");
        assert!(candidates.iter().any(|url| url == "https://Example.com/a/#frag"));
        assert!(candidates.iter().any(|url| url == "https://Example.com/a/"));
        assert!(candidates.iter().any(|url| url == "https://Example.com/a"));
        assert!(candidates.iter().any(|url| url == "https://Example.com/"));
    }

    #[test]
    fn chromium_favicon_lookup_picks_largest_bitmap() {
        let dir = temp_db_dir("chromium");
        let db = dir.join("Favicons");
        let small = png_bytes(16);
        let large = png_bytes(32);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE favicon_bitmaps (id INTEGER PRIMARY KEY, icon_id INTEGER,
                     image_data BLOB, width INTEGER DEFAULT 0, height INTEGER DEFAULT 0);
                 CREATE TABLE icon_mapping (id INTEGER PRIMARY KEY, page_url LONGVARCHAR,
                     icon_id INTEGER);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO icon_mapping (page_url, icon_id) VALUES ('https://example.com/a', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO favicon_bitmaps (icon_id, image_data, width) VALUES (1, ?1, 16)",
                rusqlite::params![small],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO favicon_bitmaps (icon_id, image_data, width) VALUES (1, ?1, 32)",
                rusqlite::params![large.clone()],
            )
            .unwrap();
        }
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let candidates = url_candidates("https://example.com/a");
        let bytes = chromium_favicon(&conn, &candidates, None).expect("favicon blob");
        assert_eq!(bytes, large, "应取 width 最大的位图");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn firefox_favicon_lookup_matches_page_url() {
        let dir = temp_db_dir("firefox");
        let db = dir.join("favicons.sqlite");
        let icon = png_bytes(16);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE moz_icons (id INTEGER PRIMARY KEY, data BLOB, width INTEGER DEFAULT 0);
                 CREATE TABLE moz_pages_w_icons (id INTEGER PRIMARY KEY, page_url TEXT NOT NULL);
                 CREATE TABLE moz_icons_to_pages (page_id INTEGER, icon_id INTEGER);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO moz_pages_w_icons (id, page_url) VALUES (1, 'https://mozilla.org/')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO moz_icons (id, data, width) VALUES (1, ?1, 16)",
                rusqlite::params![icon.clone()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO moz_icons_to_pages (page_id, icon_id) VALUES (1, 1)",
                [],
            )
            .unwrap();
        }
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let candidates = url_candidates("https://mozilla.org/");
        let bytes = firefox_favicon(&conn, &candidates, None).expect("favicon blob");
        assert_eq!(bytes, icon);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_favicon_fits_into_32_square_with_transparent_letterbox() {
        let square = decode_favicon(&png_bytes(16)).expect("png");
        assert_eq!(square.size, [32, 32]);
        assert!(square.pixels[16 * 32 + 16].a() > 0, "中心应有图标像素");

        // 非正方形图标等比缩放后居中，上下留透明边。
        let wide = decode_favicon(&png_bytes_with(16, 8)).expect("png");
        assert_eq!(wide.size, [32, 32]);
        assert_eq!(wide.pixels[0].a(), 0, "顶部留白透明");
        assert!(wide.pixels[16 * 32 + 16].a() > 0, "中间行应有图标像素");
    }
}
