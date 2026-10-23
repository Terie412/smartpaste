//! 屏幕抓取（GDI），直接持有 DIB Section。
//!
//! DIB 的好处：既是 HBITMAP 能直接 BitBlt 到窗口，又能拿像素指针做裁剪，
//! 全程零拷贝、不经过 GPU。

use std::ffi::c_void;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetWindowDC,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

pub struct Screen {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub bitmap: HBITMAP,
    /// 兼容 DC，里面已选入 bitmap，可直接作为 BitBlt 源
    pub hdc: HDC,
    pub bits: *mut u32,
}

impl Drop for Screen {
    fn drop(&mut self) {
        unsafe {
            if !self.hdc.is_null() {
                DeleteDC(self.hdc);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap as HGDIOBJ);
            }
        }
    }
}

impl Screen {
    /// 裁一块出来转 RGBA（用于保存 PNG / 写剪贴板）。
    pub fn crop_rgba(&self, x: i32, y: i32, w: i32, h: i32) -> Option<image::RgbaImage> {
        if w <= 0 || h <= 0 || x < 0 || y < 0 || x + w > self.width || y + h > self.height {
            return None;
        }
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h {
            let row_ptr = unsafe { self.bits.add((row * self.width) as usize) };
            let line = unsafe { std::slice::from_raw_parts(row_ptr, self.width as usize) };
            for px in &line[x as usize..(x + w) as usize] {
                let b = (px & 0xFF) as u8;
                let g = ((px >> 8) & 0xFF) as u8;
                let r = ((px >> 16) & 0xFF) as u8;
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
        image::RgbaImage::from_raw(w as u32, h as u32, out)
    }
}

/// 虚拟桌面在物理像素下的边界：(x, y, width, height)。
pub fn desktop_bounds() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// 用某个 DC 的指定区域做一张 Screen。
unsafe fn grab(src: HDC, src_x: i32, src_y: i32, w: i32, h: i32) -> Option<Screen> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let mem_dc = CreateCompatibleDC(src);
    if mem_dc.is_null() {
        return None;
    }

    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h; // top-down
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(
        mem_dc,
        &bi,
        DIB_RGB_COLORS,
        &mut bits,
        std::ptr::null_mut(),
        0,
    );
    if bitmap.is_null() || bits.is_null() {
        DeleteDC(mem_dc);
        if !bitmap.is_null() {
            DeleteObject(bitmap as HGDIOBJ);
        }
        return None;
    }
    SelectObject(mem_dc, bitmap as HGDIOBJ);

    if BitBlt(mem_dc, 0, 0, w, h, src, src_x, src_y, SRCCOPY) == 0 {
        DeleteDC(mem_dc);
        DeleteObject(bitmap as HGDIOBJ);
        return None;
    }

    Some(Screen {
        x: 0,
        y: 0,
        width: w,
        height: h,
        bitmap,
        hdc: mem_dc,
        bits: bits as *mut u32,
    })
}

/// 抓整个虚拟桌面。调用前必须已设置 per-monitor DPI awareness。
pub fn capture_desktop() -> Option<Screen> {
    let (vx, vy, vw, vh) = desktop_bounds();
    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return None;
        }
        let mut shot = grab(screen_dc, vx, vy, vw, vh);
        ReleaseDC(std::ptr::null_mut(), screen_dc);
        if let Some(s) = &mut shot {
            s.x = vx;
            s.y = vy;
        }
        shot
    }
}

/// 抓指定窗口（用于调试截图）。
#[allow(dead_code)]
pub fn capture_window(hwnd: HWND) -> Option<Screen> {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return None;
        }
        let dc = GetWindowDC(hwnd);
        if dc.is_null() {
            return None;
        }
        let shot = grab(dc, 0, 0, r.right - r.left, r.bottom - r.top);
        ReleaseDC(hwnd, dc);
        shot
    }
}
