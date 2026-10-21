//! 钉图窗口：置顶、可拖动、滚轮缩放、Ctrl+滚轮调透明度。
//!
//! 阴影做法：
//!   窗口比图片大一圈（PAD），外圈是软件算出来的柔和阴影 —— 像素到图片
//!   矩形的距离映射成 alpha，平方衰减。图片本身不透明，直接覆盖上去。
//!
//! 因为要逐像素控制 alpha，这里不能用 SetLayeredWindowAttributes +
//! StretchBlt（那是整窗一个透明度、且画不了半透明边缘），
//! 全程走 UpdateLayeredWindow：整张图预乘好 alpha 后一次提交。

use crate::win::{hinstance, to_wide};
use image::imageops::FilterType;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowRect, RegisterClassW,
    SetWindowPos, ShowWindow, UpdateLayeredWindow, SWP_NOZORDER, SW_SHOW, ULW_ALPHA,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONUP,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const CLASS_NAME: &str = "SmartPastePin";
/// 图片四周留给阴影的边距
const PAD: i32 = 18;
/// 阴影最深处的 alpha（贴着图片边缘那一圈，会被图片盖住）
const SHADOW_ALPHA: f32 = 110.0;

pub struct Pin {
    pub hwnd: HWND,
    img: image::RgbaImage,
    scale: f32,
    alpha: u8,
    dc: HDC,
    bmp: HBITMAP,
    dragging: bool,
    drag_cursor: POINT,
    drag_win: POINT,
}

impl Drop for Pin {
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

impl Pin {
    /// 重建合成位图并提交给窗口。dst 是窗口左上角（已含 PAD 偏移）。
    fn render(&mut self, dst_x: i32, dst_y: i32) -> bool {
        let iw = self.img.width() as i32;
        let ih = self.img.height() as i32;
        let dw = ((iw as f32 * self.scale).round() as i32).max(8);
        let dh = ((ih as f32 * self.scale).round() as i32).max(8);
        let w = dw + PAD * 2;
        let h = dh + PAD * 2;

        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(std::ptr::null_mut(), screen);
            if dc.is_null() {
                return false;
            }

            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h; // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                DeleteDC(dc);
                return false;
            }
            SelectObject(dc, bmp as HGDIOBJ);

            let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);

            // 1. 阴影：像素到图片矩形的距离 → alpha
            let reach = PAD as f32;
            for y in 0..h {
                let dy = if y < PAD {
                    (PAD - y) as f32
                } else if y >= PAD + dh {
                    (y - (PAD + dh) + 1) as f32
                } else {
                    0.0
                };
                let row = (y * w) as usize;
                for x in 0..w {
                    let dx = if x < PAD {
                        (PAD - x) as f32
                    } else if x >= PAD + dw {
                        (x - (PAD + dw) + 1) as f32
                    } else {
                        0.0
                    };
                    if dx == 0.0 && dy == 0.0 {
                        continue; // 图片区域，稍后被覆盖
                    }
                    let d = (dx * dx + dy * dy).sqrt();
                    if d >= reach {
                        continue;
                    }
                    let t = 1.0 - d / reach;
                    let a = (t * t * SHADOW_ALPHA) as u32;
                    px[row + x as usize] = a << 24; // 纯黑，alpha 预乘后 RGB 仍是 0
                }
            }

            // 2. 图片外沿 1px 亮边。
            //    深色壁纸上纯黑阴影几乎不可见，得靠这条白线把边界划出来；
            //    浅色背景上它又刚好补足阴影的过渡，两头都不突兀。
            let ring_a = 132u32;
            let ring = (ring_a << 24) | (ring_a << 16) | (ring_a << 8) | ring_a;
            let (x0, y0) = (PAD - 1, PAD - 1);
            let (x1, y1) = (PAD + dw, PAD + dh);
            for x in x0..=x1 {
                px[(y0 * w + x) as usize] = ring;
                px[(y1 * w + x) as usize] = ring;
            }
            for y in y0..=y1 {
                px[(y * w + x0) as usize] = ring;
                px[(y * w + x1) as usize] = ring;
            }

            // 3. 图片本体，不透明，直接盖上
            let scaled = image::imageops::resize(&self.img, dw as u32, dh as u32, FilterType::Lanczos3);
            for (y, line) in scaled.rows().enumerate() {
                let base = (y as i32 + PAD) * w + PAD;
                for (x, p) in line.enumerate() {
                    px[(base + x as i32) as usize] =
                        0xFF00_0000 | ((p[2] as u32) << 16) | ((p[1] as u32) << 8) | p[0] as u32;
                }
            }

            // 4. 提交
            let dst = POINT { x: dst_x, y: dst_y };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: self.alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let ok = UpdateLayeredWindow(
                self.hwnd,
                std::ptr::null_mut(),
                &dst,
                &size,
                dc,
                &src,
                0,
                &blend,
                ULW_ALPHA,
            );

            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
            if !self.bmp.is_null() {
                DeleteObject(self.bmp as HGDIOBJ);
            }
            self.dc = dc;
            self.bmp = bmp;

            ok != 0
        }
    }
}

static mut PINS: Vec<Box<Pin>> = Vec::new();

#[allow(dead_code)]
fn lo(v: LPARAM) -> i32 {
    (v as usize & 0xFFFF) as u16 as i16 as i32
}
#[allow(dead_code)]
fn hi(v: LPARAM) -> i32 {
    ((v as usize >> 16) & 0xFFFF) as u16 as i16 as i32
}

extern "system" fn pin_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                if let Some(p) = find_mut(hwnd) {
                    p.dragging = true;
                    let mut pt: POINT = std::mem::zeroed();
                    GetCursorPos(&mut pt);
                    p.drag_cursor = pt;
                    let mut wr: RECT = std::mem::zeroed();
                    GetWindowRect(hwnd, &mut wr);
                    p.drag_win = POINT {
                        x: wr.left,
                        y: wr.top,
                    };
                    SetCapture(hwnd);
                }
                0
            }
            WM_MOUSEMOVE => {
                if let Some(p) = find_mut(hwnd) {
                    if p.dragging {
                        let mut pt: POINT = std::mem::zeroed();
                        GetCursorPos(&mut pt);
                        let nx = p.drag_win.x + (pt.x - p.drag_cursor.x);
                        let ny = p.drag_win.y + (pt.y - p.drag_cursor.y);
                        SetWindowPos(hwnd, std::ptr::null_mut(), nx, ny, 0, 0, SWP_NOZORDER | 0x0001);
                    }
                }
                0
            }
            WM_LBUTTONUP => {
                if let Some(p) = find_mut(hwnd) {
                    p.dragging = false;
                }
                ReleaseCapture();
                0
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32;
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                let mut wr: RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut wr);
                let (wx, wy) = (wr.left, wr.top);

                if let Some(p) = find_mut(hwnd) {
                    if ctrl {
                        let a = (p.alpha as i32 + if delta > 0 { 16 } else { -16 }).clamp(40, 255);
                        p.alpha = a as u8;
                    } else {
                        let step = if delta > 0 { 1.1 } else { 1.0 / 1.1 };
                        p.scale = (p.scale * step).clamp(0.1, 6.0);
                    }
                    // 缩放和透明度都得重画整张合成位图
                    p.render(wx, wy);
                }
                0
            }
            WM_RBUTTONUP | WM_LBUTTONDBLCLK => {
                DestroyWindow(hwnd);
                0
            }
            windows_sys::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
                if let Some(p) = find_mut(hwnd) {
                    p.hwnd = std::ptr::null_mut();
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn find_mut(hwnd: HWND) -> Option<&'static mut Pin> {
    PINS.iter_mut()
        .find(|p| p.hwnd == hwnd)
        .map(|b| b.as_mut())
}

pub fn register() {
    unsafe {
        let class = to_wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: 0x0008, // CS_DBLCLKS
            lpfnWndProc: Some(pin_wndproc),
            hInstance: hinstance(),
            hCursor: windows_sys::Win32::UI::WindowsAndMessaging::LoadCursorW(
                std::ptr::null_mut(),
                windows_sys::Win32::UI::WindowsAndMessaging::IDC_SIZEALL,
            ),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
    }
}

/// 把一张图钉到屏幕上 (x, y)。x/y 是图片左上角，不是窗口左上角。
pub fn create(img: &image::RgbaImage, x: i32, y: i32) -> bool {
    unsafe {
        let class = to_wide(CLASS_NAME);
        let title = to_wide("SmartPaste Pin");
        let w = (img.width() as i32 + PAD * 2).max(16);
        let h = (img.height() as i32 + PAD * 2).max(16);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_VISIBLE,
            x - PAD,
            y - PAD,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance(),
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return false;
        }

        PINS.push(Box::new(Pin {
            hwnd,
            img: img.clone(),
            scale: 1.0,
            alpha: 255,
            dc: std::ptr::null_mut(),
            bmp: std::ptr::null_mut(),
            dragging: false,
            drag_cursor: POINT { x: 0, y: 0 },
            drag_win: POINT { x: 0, y: 0 },
        }));

        let idx = PINS.len() - 1;
        if !PINS[idx].render(x - PAD, y - PAD) {
            DestroyWindow(hwnd);
            return false;
        }

        ShowWindow(hwnd, SW_SHOW);
        true
    }
}

/// 清理已销毁的钉图，释放 GDI 资源。
pub fn reap() {
    unsafe {
        PINS.retain(|p| !p.hwnd.is_null());
    }
}
