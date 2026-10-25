//! 光标合成。
//!
//! 录屏时 DDA 给的帧里没有光标，得自己叠。做法不是在整帧上画，
//! 而是只处理光标覆盖的那一小块（默认 64×64）：
//! 背景从 GPU 读回 → `DrawIconEx` 混合 → 贴回 GPU。
//! 这样整帧永远不进 CPU，也不需要 shader。

use std::ffi::c_void;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING,
    DI_NORMAL, HICON, ICONINFO, SM_CXCURSOR, SM_CYCURSOR,
};

/// 光标盒子的最小边长。高 DPI 下系统光标能到 64px，再大就截断。
const MIN_BOX: i32 = 64;

/// 一帧内光标的位置。`x/y` 已经是左上角（热点已扣掉）。
#[derive(Clone, Copy)]
pub struct CursorState {
    pub icon: HICON,
    pub x: i32,
    pub y: i32,
}

pub struct CursorBlender {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    pub size: i32,
}

impl Drop for CursorBlender {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

impl CursorBlender {
    pub fn new() -> Option<Self> {
        unsafe {
            let size = GetSystemMetrics(SM_CXCURSOR)
                .max(GetSystemMetrics(SM_CYCURSOR))
                .max(MIN_BOX);

            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(std::ptr::null_mut(), screen);
            if dc.is_null() {
                return None;
            }

            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = size;
            bi.bmiHeader.biHeight = -size; // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;

            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap as HGDIOBJ);

            Some(Self {
                dc,
                bitmap,
                old,
                bits: bits as *mut u32,
                size,
            })
        }
    }

    /// 查一次光标。没有光标（或不可见）返回 None。
    pub fn query(&self) -> Option<CursorState> {
        unsafe {
            let mut ci: CURSORINFO = std::mem::zeroed();
            ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut ci) == 0 || ci.flags & CURSOR_SHOWING == 0 {
                return None;
            }

            let mut ii: ICONINFO = std::mem::zeroed();
            if GetIconInfo(ci.hCursor, &mut ii) == 0 {
                return None;
            }
            // GetIconInfo 会创建两个位图，必须还回去，否则每帧泄漏
            if !ii.hbmMask.is_null() {
                DeleteObject(ii.hbmMask as HGDIOBJ);
            }
            if !ii.hbmColor.is_null() {
                DeleteObject(ii.hbmColor as HGDIOBJ);
            }

            Some(CursorState {
                icon: ci.hCursor,
                x: ci.ptScreenPos.x - ii.xHotspot as i32,
                y: ci.ptScreenPos.y - ii.yHotspot as i32,
            })
        }
    }

    /// 在小块 `data`（BGRA，`w*h`）上画光标。
    ///
    /// `(ox, oy)` 是小块左上角的屏幕坐标；`state` 来自 `query()`。
    /// 返回 true 表示确实改了像素。
    pub fn blend(&mut self, state: &CursorState, data: &mut [u32], w: i32, h: i32, ox: i32, oy: i32) -> bool {
        if w <= 0 || h <= 0 || data.len() < (w * h) as usize {
            return false;
        }
        unsafe {
            let left = state.x - ox;
            let top = state.y - oy;

            let x0 = left.max(0);
            let y0 = top.max(0);
            let x1 = (left + self.size).min(w);
            let y1 = (top + self.size).min(h);
            if x0 >= x1 || y0 >= y1 {
                return false;
            }

            let bw = x1 - x0;
            let bh = y1 - y0;

            // 1. 背景拷进 DIB（放在左上角）
            for row in 0..bh {
                let src = ((y0 + row) * w + x0) as usize;
                let d = self.bits.add((row * self.size) as usize);
                std::ptr::copy_nonoverlapping(data.as_ptr().add(src), d, bw as usize);
            }

            // 2. 在 DIB 上画光标。DIB 坐标要减掉被裁掉的那部分
            DrawIconEx(
                self.dc,
                x0 - left,
                y0 - top,
                state.icon,
                0,
                0,
                0,
                std::ptr::null_mut(),
                DI_NORMAL,
            );

            // 3. 拷回，并把 alpha 拉满（GDI 会把 alpha 写成 0）
            for row in 0..bh {
                let d = ((y0 + row) * w + x0) as usize;
                let s = self.bits.add((row * self.size) as usize);
                std::ptr::copy_nonoverlapping(s, data.as_mut_ptr().add(d), bw as usize);
                for px in data[d..d + bw as usize].iter_mut() {
                    *px |= 0xFF00_0000;
                }
            }

            true
        }
    }
}
