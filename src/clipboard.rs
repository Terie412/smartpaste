//! 剪贴板：同时写 CF_DIB 和 PNG 两种格式。
//!
//! 只写 CF_DIB 的话，粘到浏览器/部分应用里会丢；只写 PNG 的话，老程序不认。
//! 两个都写，兼容性最好。

use image::ImageEncoder;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

use crate::win::to_wide;

/// CF_DIB = 8（Win32::System::Ole 里也有，但为它开一个 feature 不值）
const CF_DIB: u32 = 8;

/// 把 RGBA 图写进剪贴板。
pub fn copy_image(img: &image::RgbaImage) -> Result<(), String> {
    unsafe {
        // 剪贴板经常被其他程序短暂占用，重试几次比直接失败实用
        let mut opened = false;
        for _ in 0..4 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        if !opened {
            let e = windows_sys::Win32::Foundation::GetLastError();
            crate::win::trace(&format!("clipboard: OpenClipboard failed err={e}"));
            return Err(format!("OpenClipboard failed ({e})"));
        }
        EmptyClipboard();

        let mut ok = false;

        // --- CF_DIB ---
        let dib = build_dib(img);
        match alloc_global(&dib) {
            Some(hmem) => {
                if SetClipboardData(CF_DIB, hmem).is_null() {
                    let e = windows_sys::Win32::Foundation::GetLastError();
                    crate::win::trace(&format!("clipboard: SetClipboardData(CF_DIB) err={e}"));
                } else {
                    ok = true;
                }
            }
            None => crate::win::trace("clipboard: GlobalAlloc failed"),
        }

        // --- PNG ---
        if let Ok(png) = encode_png(img) {
            let fmt = RegisterClipboardFormatW(to_wide("PNG").as_ptr());
            if fmt != 0 {
                if let Some(hmem) = alloc_global(&png) {
                    SetClipboardData(fmt, hmem);
                }
            }
        }

        CloseClipboard();
        if ok {
            Ok(())
        } else {
            Err("写入剪贴板失败".into())
        }
    }
}

unsafe fn alloc_global(data: &[u8]) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    let h = GlobalAlloc(GMEM_MOVEABLE, data.len());
    if h.is_null() {
        return None;
    }
    let p = GlobalLock(h);
    if p.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
    GlobalUnlock(h);
    Some(h)
}

/// 构造 CF_DIB 数据：BITMAPINFOHEADER(40) + bottom-up BGRA。
fn build_dib(img: &image::RgbaImage) -> Vec<u8> {
    let w = img.width() as i32;
    let h = img.height() as i32;
    let raw = img.as_raw();
    let mut out = Vec::with_capacity(40 + (w * h * 4) as usize);

    out.extend_from_slice(&40i32.to_le_bytes()); // biSize
    out.extend_from_slice(&w.to_le_bytes()); // biWidth
    out.extend_from_slice(&h.to_le_bytes()); // biHeight（正数 = bottom-up）
    out.extend_from_slice(&1i16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32i16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0i32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(w * h * 4).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&2835i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&2835i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0i32.to_le_bytes()); // biClrImportant

    for row in (0..h).rev() {
        let start = (row * w * 4) as usize;
        for px in raw[start..start + (w * 4) as usize].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    out
}

fn encode_png(img: &image::RgbaImage) -> Result<Vec<u8>, image::ImageError> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf).write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(buf)
}
