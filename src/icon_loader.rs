use anyhow::Result;
use image::imageops::FilterType;
use std::path::{Path, PathBuf};
use std::collections::{HashMap, VecDeque};
use std::cell::RefCell;

const ICON_MEMO_MAX: usize = 64;
const ICON_RASTER_MAX_EDGE: u32 = 48;

pub struct IconLoader {
    cache_dir: PathBuf,
    memo: RefCell<HashMap<String, slint::Image>>,
    memo_lru: RefCell<VecDeque<String>>,
}

impl IconLoader {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            cache_dir: cache_dir.into(),
            memo: RefCell::new(HashMap::new()),
            memo_lru: RefCell::new(VecDeque::new()),
        })
    }

    fn memo_touch(&self, key: &str) {
        let mut lru = self.memo_lru.borrow_mut();
        if let Some(pos) = lru.iter().position(|k| k == key) {
            lru.remove(pos);
        }
        lru.push_back(key.to_string());
    }

    fn memo_get(&self, key: &str) -> Option<slint::Image> {
        let img = self.memo.borrow().get(key).cloned();
        if img.is_some() {
            self.memo_touch(key);
        }
        img
    }

    fn memo_put(&self, key: String, img: slint::Image) {
        self.memo.borrow_mut().insert(key.clone(), img);
        self.memo_touch(&key);
        self.memo_trim();
    }

    fn memo_trim(&self) {
        loop {
            if self.memo.borrow().len() <= ICON_MEMO_MAX {
                break;
            }
            let Some(evict_key) = self.memo_lru.borrow_mut().pop_front() else {
                break;
            };
            self.memo.borrow_mut().remove(&evict_key);
        }
    }

    pub fn trim_memo_to(&self, keep: usize) {
        loop {
            if self.memo.borrow().len() <= keep {
                break;
            }
            let Some(evict_key) = self.memo_lru.borrow_mut().pop_front() else {
                break;
            };
            self.memo.borrow_mut().remove(&evict_key);
        }
    }

    fn normalize_rgba_for_cache(&self, rgba: image::RgbaImage) -> image::RgbaImage {
        let (w, h) = rgba.dimensions();
        if w <= ICON_RASTER_MAX_EDGE && h <= ICON_RASTER_MAX_EDGE {
            return rgba;
        }
        image::imageops::resize(
            &rgba,
            ICON_RASTER_MAX_EDGE,
            ICON_RASTER_MAX_EDGE,
            FilterType::Triangle,
        )
    }

 /// 从文件路径加载真实图标（Windows Shell API），失败时回退到字母图标
    pub fn load_real_icon(&self, path: &Path, fallback_name: &str) -> slint::Image {
        let key = path.to_string_lossy().to_lowercase();
        if let Some(cached) = self.memo_get(&key) {
            return cached;
        }

        let img = win_icon::extract_icon_rgba(path)
            .and_then(|(rgba, w, h)| {
                let rgba = image::RgbaImage::from_raw(w, h, rgba)?;
                let rgba = self.normalize_rgba_for_cache(rgba);
                let (w2, h2) = rgba.dimensions();
                let pb = slint::SharedPixelBuffer::clone_from_slice(rgba.as_raw(), w2, h2);
                Some(slint::Image::from_rgba8(pb))
            })
            .unwrap_or_else(|| self.create_letter_icon(fallback_name));

        self.memo_put(key, img.clone());
        img
    }

 /// 从文件路径加载图标（兼容旧调用）
    pub fn load_from_path(&self, path: &Path) -> Option<slint::Image> {
        let bytes = std::fs::read(path).ok()?;
        self.bytes_to_slint_image(&bytes).ok()
    }

    fn bytes_to_slint_image(&self, bytes: &[u8]) -> Result<slint::Image> {
        let img = image::load_from_memory(bytes)?;
        let rgba = self.normalize_rgba_for_cache(img.to_rgba8());
        let (width, height) = (rgba.width(), rgba.height());
        Ok(slint::Image::from_rgba8(
            slint::SharedPixelBuffer::clone_from_slice(rgba.as_raw(), width, height),
        ))
    }

 /// 字母图标降级方案
    pub fn create_letter_icon(&self, name: &str) -> slint::Image {
        let size = ICON_RASTER_MAX_EDGE;
        let letter = name.chars().next().unwrap_or('?').to_ascii_uppercase();
        let hue = (letter as u32 * 137) % 360;
        let (r, g, b) = hsl_to_rgb(hue as f32, 0.55, 0.45);

        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        let center = size as f32 / 2.0;
        let radius = size as f32 / 2.0 - 4.0;

        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 - center;
                let dy = y as f32 - center;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist <= radius {
                    let g_ = 1.0 - (dist / radius) * 0.3;
                    pixels.push((r * g_) as u8);
                    pixels.push((g * g_) as u8);
                    pixels.push((b * g_) as u8);
                    pixels.push(255);
                } else {
                    pixels.push(0);
                    pixels.push(0);
                    pixels.push(0);
                    pixels.push(0);
                }
            }
        }

        slint::Image::from_rgba8(
            slint::SharedPixelBuffer::clone_from_slice(&pixels, size, size),
        )
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

 /// 站点快捷方式占位图（链.globes）区别于字母占位图
    pub fn create_web_placeholder_icon(&self) -> slint::Image {
        let key = String::from("__web_placeholder__v1");
        if let Some(cached) = self.memo_get(&key) {
            return cached;
        }
        let size = ICON_RASTER_MAX_EDGE;
 // 深蓝渐变背景 + 简化的「链条」环形高光表示网址
        let mut pixels = vec![0u8; (size * size * 4) as usize];
        let cx = size as f32 / 2.0;
        let cy = size as f32 / 2.0;
        let r_outer = size as f32 / 2.0 - 2.0;
        let r_ring = r_outer - 10.0;
        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                let i = ((y * size + x) * 4) as usize;
                if dist <= r_outer {
                    let t = dist / r_outer;
                    let bg_r = (28.0 + t * 18.0) as u8;
                    let bg_g = (72.0 + t * 30.0) as u8;
                    let bg_b = (130.0 + t * 40.0) as u8;
                    let rd = dist - r_ring;
                    if rd.abs() < 4.5 && dist > 8.0 {
 // 环形高光
                        pixels[i] = (bg_r as f32 * 0.85 + 60.0).min(255.0) as u8;
                        pixels[i + 1] = (bg_g as f32 * 0.85 + 90.0).min(255.0) as u8;
                        pixels[i + 2] = (bg_b as f32 * 0.85 + 120.0).min(255.0) as u8;
                        pixels[i + 3] = 255;
                    } else {
                        pixels[i] = bg_r;
                        pixels[i + 1] = bg_g;
                        pixels[i + 2] = bg_b;
                        pixels[i + 3] = 255;
                    }
                }
            }
        }
        let img = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            &pixels, size, size,
        ));
        self.memo_put(key, img.clone());
        img
    }

 /// 使某路径对应图标缓存失效（磁盘文件更新后调用）
    pub fn invalidate_memo_path(&self, path: &Path) {
        let key = path.to_string_lossy().to_lowercase();
        self.memo.borrow_mut().remove(&key);
        if let Some(pos) = self.memo_lru.borrow().iter().position(|k| k == &key) {
            self.memo_lru.borrow_mut().remove(pos);
        }
    }

 /// 根据配置加载图标：自定义 PNG → 站点占位 → Shell 提取 / 字母图标
    pub fn load_app_icon(
        &self,
        app: &crate::models::AppInfo,
        resolved_path: &Path,
        config_base: &Path,
    ) -> slint::Image {
        if let Some(ref ip) = app.icon_path {
            let resolved_icon = crate::path_convert::resolve_path(config_base, ip);
            let key = resolved_icon.to_string_lossy().to_lowercase();
            if let Some(cached) = self.memo_get(&key) {
                return cached;
            }
            if let Some(img) = self.load_from_path(&resolved_icon) {
                self.memo_put(key, img.clone());
                return img;
            }
        }

        let is_url_shortcut = app.web_url.is_some()
            || resolved_path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("url"))
                .unwrap_or(false);

        if is_url_shortcut {
            return self.create_web_placeholder_icon();
        }

        self.load_real_icon(resolved_path, &app.name)
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    ((r1 + m) * 255.0, (g1 + m) * 255.0, (b1 + m) * 255.0)
}

#[cfg(windows)]
mod win_icon {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, SelectObject,
        BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    };
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;

    fn to_wide(s: &Path) -> Vec<u16> {
        s.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

 /// 提取图标并返回 RGBA 像素 + 宽高
    pub fn extract_icon_rgba(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
        unsafe { large_icon(path) }
    }

    unsafe fn large_icon(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
        let wpath = to_wide(path);
        let mut sfi = SHFILEINFOW::default();
        let flags = SHGFI_ICON
            | SHGFI_LARGEICON
            | if !path.exists() { SHGFI_USEFILEATTRIBUTES } else { Default::default() };
        let res = SHGetFileInfoW(
            PCWSTR(wpath.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if res == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let result = hicon_to_rgba(sfi.hIcon);
        let _ = DestroyIcon(sfi.hIcon);
        result
    }

    unsafe fn hicon_to_rgba(hicon: HICON) -> Option<(Vec<u8>, u32, u32)> {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;

        let color_bmp: HBITMAP = info.hbmColor;
        let mask_bmp: HBITMAP = info.hbmMask;

        if color_bmp.is_invalid() {
            if !mask_bmp.is_invalid() { let _ = DeleteObject(mask_bmp); }
            return None;
        }

        let mut bmp = BITMAP::default();
        if GetObjectW(
            color_bmp,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        ) == 0 {
            let _ = DeleteObject(color_bmp);
            if !mask_bmp.is_invalid() { let _ = DeleteObject(mask_bmp); }
            return None;
        }

        let width = bmp.bmWidth;
        let height = bmp.bmHeight;
        if width <= 0 || height <= 0 {
            let _ = DeleteObject(color_bmp);
            if !mask_bmp.is_invalid() { let _ = DeleteObject(mask_bmp); }
            return None;
        }

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // 自顶向下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let pixel_count = (width * height) as usize;
        let mut buf = vec![0u8; pixel_count * 4];

        let hdc: HDC = CreateCompatibleDC(None);
        let prev = SelectObject(hdc, color_bmp);

        let got = GetDIBits(
            hdc,
            color_bmp,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(hdc, prev);
        let _ = DeleteDC(hdc);
        let _ = DeleteObject(color_bmp);
        if !mask_bmp.is_invalid() { let _ = DeleteObject(mask_bmp); }

        if got == 0 {
            return None;
        }

 // BGRA -> RGBA
        let mut all_zero_alpha = true;
        for px in buf.chunks_exact_mut(4) {
            let b = px[0];
            let r = px[2];
            px[0] = r;
            px[2] = b;
            if px[3] != 0 {
                all_zero_alpha = false;
            }
        }

 // 某些 32 位 HICON 的 alpha 为 0（旧格式），此时按非黑像素填充 alpha
        if all_zero_alpha {
            for px in buf.chunks_exact_mut(4) {
                if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                    px[3] = 255;
                }
            }
        }

        Some((buf, width as u32, height as u32))
    }
}

#[cfg(not(windows))]
mod win_icon {
    use std::path::Path;
    pub fn extract_icon_rgba(_path: &Path) -> Option<(Vec<u8>, u32, u32)> { None }
}
