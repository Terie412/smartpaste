//! 工具条图标。
//!
//! assets/icons.png 是编译期嵌进来的雪碧图（三格横排，每格 96x96，
//! 源设计是 24x24 网格按 4 倍渲染）。运行时解码、缩到按钮尺寸，
//! 按目标颜色着色后转成 32bpp DIB 交给 AlphaBlend。
//!
//! 源图是纯白线条，所以着色只要把 RGB 换掉、alpha 留着，
//! 抗锯齿的半透明边会自然带上颜色。

use image::imageops::FilterType;
use windows_sys::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};

const SPRITE: &[u8] = include_bytes!("../assets/icons.png");
/// 源雪碧图里每格的边长
const CELL: u32 = 96;
/// 贴到屏幕上的边长（跟随 UI 缩放）
pub const SIZE: i32 = 32;

/// 每格图标的 [默认色, 选中色]。
///
/// 前三个是操作按钮（复制/保存/钉住），选中色无意义，两者相同。
/// 后六个是标注工具，未选中深灰，选中转玫红。
const SPECS: [([u8; 3], [u8; 3]); 9] = [
    ([0x00, 0x8F, 0xB3], [0x00, 0x8F, 0xB3]), // 复制
    ([0xFF, 0x2E, 0x63], [0xFF, 0x2E, 0x63]), // 保存
    ([0xD9, 0x7E, 0x00], [0xD9, 0x7E, 0x00]), // 钉住
    ([0x45, 0x45, 0x4C], [0xFF, 0x2E, 0x63]), // 箭头
    ([0x45, 0x45, 0x4C], [0xFF, 0x2E, 0x63]), // 矩形
    ([0x45, 0x45, 0x4C], [0xFF, 0x2E, 0x63]), // 椭圆
    ([0x45, 0x45, 0x4C], [0xFF, 0x2E, 0x63]), // 画笔
    ([0x45, 0x45, 0x4C], [0xFF, 0x2E, 0x63]), // 文本
    ([0x45, 0x45, 0x4C], [0xFF, 0x2E, 0x63]), // 橡皮
];

pub struct Icon {
    dc: HDC,
    bmp: HBITMAP,
}

impl Icon {
    pub fn draw(&self, dst: HDC, x: i32, y: i32) {
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            AlphaBlend(dst, x, y, SIZE, SIZE, self.dc, 0, 0, SIZE, SIZE, bf);
        }
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        unsafe {
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
            if !self.bmp.is_null() {
                DeleteObject(self.bmp as HGDIOBJ);
            }
        }
    }
}

#[derive(Default)]
pub struct IconSet {
    pub normal: Vec<Icon>,
    pub active: Vec<Icon>,
}

/// 解码失败时返回空集合，调用方画不出图标但不会崩。
pub fn load() -> IconSet {
    let Ok(sprite) = image::load_from_memory(SPRITE) else {
        return IconSet::default();
    };
    let sprite = sprite.to_rgba8();
    if sprite.width() < CELL * 9 {
        return IconSet::default();
    }

    let mut set = IconSet::default();
    for (i, (base, hl)) in SPECS.iter().enumerate() {
        let x = i as u32 * CELL;
        let cell = image::imageops::crop_imm(&sprite, x, 0, CELL, CELL).to_image();
        let scaled = image::imageops::resize(&cell, SIZE as u32, SIZE as u32, FilterType::Lanczos3);
        if let Some(icon) = build(&scaled, *base) {
            set.normal.push(icon);
        }
        if let Some(icon) = build(&scaled, *hl) {
            set.active.push(icon);
        }
    }
    set
}

fn build(rgba: &image::RgbaImage, tint: [u8; 3]) -> Option<Icon> {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        if dc.is_null() {
            return None;
        }

        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = SIZE;
        bi.bmiHeader.biHeight = -SIZE; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bmp.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        SelectObject(dc, bmp as HGDIOBJ);

        let dst = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
        for (i, px) in rgba.as_raw().chunks_exact(4).enumerate() {
            let a = px[3] as u32;
            // AlphaBlend 用 AC_SRC_ALPHA 时源像素必须是预乘过的
            let pr = (tint[0] as u32 * a / 255) & 0xFF;
            let pg = (tint[1] as u32 * a / 255) & 0xFF;
            let pb = (tint[2] as u32 * a / 255) & 0xFF;
            dst[i] = (a << 24) | (pr << 16) | (pg << 8) | pb;
        }

        Some(Icon { dc, bmp })
    }
}
